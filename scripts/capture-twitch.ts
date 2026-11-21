#!/usr/bin/env node
// Capture one Twitch broadcast in both orientations, live and DVR, and record
// how each observation path sees the broadcast start.
//
//   node scripts/capture-twitch.ts -c ozeniken [-o DIR] [--dry-run]
//     [--interval MS] [--watch-timeout SEC]
//
// Up to four N_m3u8DL-RE recorders: landscape and portrait, each once from the
// live playlist (the ~2 s segments as published) and once from the archive/DVR
// playlist (the whole broadcast in ~10 s chunks from segment 0). That matrix is
// what lets us compare what Twitch serves against what the disk recorder
// assumes, and the reason to start before going live is the very first segment.
//
// While offline it watches for the stream from three directions and timestamps
// each observation into <out>/timeline.jsonl, so the latency of each can be
// compared:
//
//   gql    - the StreamMetadata query the daemon uses, every --interval ms;
//   hermes - the daemon's wss://hermes.twitch.tv/v1 socket, subscribed to
//            video-playback-by-id.<id> and broadcast-settings-update.<id>;
//   master - the live multivariant playlist, whose first variant is also the
//            trigger to start the recorders.
//
// It also saves the multivariant playlists it sees (live-master.m3u8 and
// dvr-master.m3u8), the first variant playlist (live-first-media.m3u8) with the
// first segment and its init in first/, and every distinct revision of that
// variant playlist in live-media/ (hashed, so only changes are written): the
// end-of-stream shape is only visible in the live playlist.
import { spawn, type ChildProcess } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import {
  appendFileSync,
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  writeFileSync,
} from "node:fs";
import path from "node:path";

const CLIENT_ID = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const GQL = "https://gql.twitch.tv/gql";
const HERMES = `wss://hermes.twitch.tv/v1?clientId=${CLIENT_ID}`;
const USHER = "https://usher.ttvnw.net/api/v2/channel/hls";
const VOD_USHER = "https://usher.ttvnw.net/vod/v2";
const STREAM_METADATA = "b57f9b910f8cd1a4659d894fe7550ccc81ec9052c01e438b290fd66a040b9b93";
const PLAYBACK_TOKEN = "ed230aa1e33e07eebb8928504583da78a5173989fadfb1ac94be06a04f3cdbe9";
const RECENT_BROADCASTS = "a92c0ad34fdba9b3f5e6125dce4b0acc9373f2b2c06c74c92187749d12f73d4c";
/// The live playlist only advances when a segment closes, so 1 s is enough to
/// see every revision without the 300 ms master-poll cadence.
const REVISION_INTERVAL = 1000;
const VOD_DOMAINS = [
  "https://ds0h3roq6wcgc.cloudfront.net",
  "https://d2nvs31859zcd8.cloudfront.net",
  "https://d2aba1wr3818hz.cloudfront.net",
  "https://d3c27h4odz752x.cloudfront.net",
  "https://dgeft87wbj63p.cloudfront.net",
  "https://d1m7jfoe9zdc1j.cloudfront.net",
  "https://d3vd9lfkzbru3h.cloudfront.net",
  "https://ddacn6pr5v0tl.cloudfront.net",
  "https://d3aqoihi2n8ty8.cloudfront.net",
  "https://d3fi1amfgojobc.cloudfront.net",
  "https://d2vi6trrdongqn.cloudfront.net",
  "https://d3stzm2eumvgb4.cloudfront.net",
];

interface Options {
  channel: string;
  out: string;
  dryRun: boolean;
  interval: number;
  watchTimeout: number;
  dvrOnly: boolean;
  dvrRetry: number;
  dvrWatch: number;
  endGrace: number;
}

interface Variant {
  kind: "landscape" | "portrait" | "audio";
  bandwidth: number;
  id: string;
  resolution: string;
  source: string;
  url: string;
}

interface DvrChoice {
  kind: "landscape" | "portrait";
  /// Which route resolved it: the archive master, a private object, or chunked/.
  via: string;
  url: string;
}

type Source = "watch" | "gql" | "hermes" | "master" | "live" | "spawn";

interface Observation {
  src: Source;
  event: string;
  [field: string]: unknown;
}

interface HermesMessage {
  type?: string;
  welcome?: { keepaliveSec?: number };
  subscribeResponse?: { result?: string; subscription?: { id?: string } };
  notification?: {
    subscription?: { id?: string };
    pubsub?: { type?: string; viewers?: number; stream_title?: string; game?: string };
  };
}

interface StreamMetadataResponse {
  data?: {
    user?: {
      id?: string;
      stream?: {
        id?: string;
        createdAt?: string;
        viewersCount?: number;
        title?: string;
        game?: { name?: string };
      } | null;
    };
  };
}

const options: Options = {
  channel: "ozeniken",
  out: "",
  dryRun: false,
  interval: 300,
  watchTimeout: 1800,
  dvrOnly: false,
  dvrRetry: 30,
  dvrWatch: 3600,
  endGrace: 20,
};
for (let index = 2; index < process.argv.length; index += 1) {
  const arg = process.argv[index]!;
  const value = process.argv[index + 1];
  switch (arg) {
    case "-c":
    case "--channel":
      options.channel = value ?? options.channel;
      index += 1;
      break;
    case "-o":
    case "--out":
      options.out = value ?? options.out;
      index += 1;
      break;
    case "--interval":
      options.interval = Number(value ?? options.interval);
      index += 1;
      break;
    case "--watch-timeout":
      options.watchTimeout = Number(value ?? options.watchTimeout);
      index += 1;
      break;
    case "--dvr-only":
      options.dvrOnly = true;
      break;
    case "--dvr-retry":
      options.dvrRetry = Number(value ?? options.dvrRetry);
      index += 1;
      break;
    case "--dvr-watch":
      options.dvrWatch = Number(value ?? options.dvrWatch);
      index += 1;
      break;
    case "--end-grace":
      options.endGrace = Number(value ?? options.endGrace);
      index += 1;
      break;
    case "-n":
    case "--dry-run":
      options.dryRun = true;
      break;
    default:
      console.error(`capture: unknown argument ${arg}`);
      console.error("usage: node scripts/capture-twitch.ts [-c channel] [-o dir] [--dry-run]");
      process.exit(2);
  }
}
options.out ||= `captures/${options.channel}-${new Date()
  .toISOString()
  .replace(/[-:]/g, "")
  .replace(/\..*/, "")
  .replace("T", "-")}`;
mkdirSync(options.out, { recursive: true });

const timeline = path.join(options.out, "timeline.jsonl");
const revisionsDir = path.join(options.out, "live-media");
const started = Date.now();

interface TimelineEntry extends Observation {
  t: number;
  age: number;
}

/**
 * When each path first said the stream was up, on *our* clock. Process start is
 * whenever the script was launched, so `age` in the timeline cannot say how long
 * Twitch took to announce a broadcast; and the manifest's
 * `#EXT-X-PROGRAM-DATE-TIME` is Twitch's clock, which need not agree with ours.
 * The reference is therefore the earlier of the two live-state observations:
 * whoever told us first was told at time zero.
 */
let firstGqlLive: TimelineEntry | null = null;
let firstStreamUp: TimelineEntry | null = null;
let firstVariants: TimelineEntry | null = null;
let firstPlaylist: TimelineEntry | null = null;
let latencyPrinted = false;

const signed = (seconds: number): string => `${seconds >= 0 ? "+" : ""}${seconds.toFixed(2)}s`;

function printLatency(): void {
  const gql = firstGqlLive;
  const hermes = firstStreamUp;
  const anchors = [gql, hermes].filter((entry): entry is TimelineEntry => entry !== null);
  if (anchors.length === 0) return;
  const reference = Math.min(...anchors.map((entry) => entry.t));
  console.log("capture: when the live state arrived (our clock, not the manifest's):");
  const rows: Array<[string, TimelineEntry | null]> = [
    ["gql live", gql],
    ["hermes stream-up", hermes],
    ["master first variant", firstVariants],
    ["master first playlist", firstPlaylist],
  ];
  for (const [label, entry] of rows) {
    console.log(
      `  ${label.padEnd(22)} ${entry ? signed(entry.t - reference) : "not seen"}`,
    );
  }
  if (gql && hermes) {
    console.log(
      `  ${"hermes - gql".padEnd(22)} ${signed(hermes.t - gql.t)} (${hermes.t < gql.t ? "hermes" : "gql"} first)`,
    );
  }
}

/** Print once both halves are in; print what is known if one never arrives. */
function checkLatency(entry: TimelineEntry): void {
  if (latencyPrinted) return;
  const both = firstGqlLive !== null && firstStreamUp !== null;
  const only = firstGqlLive ?? firstStreamUp;
  if (both || (only && entry.t - only.t >= 60)) {
    latencyPrinted = true;
    printLatency();
  }
}

function record(observation: Observation): void {
  const entry: TimelineEntry = {
    t: Date.now() / 1000,
    age: (Date.now() - started) / 1000,
    ...observation,
  };
  appendFileSync(timeline, `${JSON.stringify(entry)}\n`);
  if (entry.src === "gql" && entry.event === "live") firstGqlLive ??= entry;
  else if (entry.src === "hermes" && entry.event === "stream-up") firstStreamUp ??= entry;
  else if (entry.src === "master" && entry.event === "variants") firstVariants ??= entry;
  else if (entry.src === "master" && entry.event === "first") firstPlaylist ??= entry;
  checkLatency(entry);
}

/**
 * The end of the stream is observable three ways, in order of speed: the live
 * playlist gains `ENDLIST` (within a second, and the only one that is exact),
 * Hermes sends `stream-down` (~4 s later), and GQL flips to offline (~9 s later).
 * Whichever arrives first wins; a single observer is not enough because the
 * playlist poller only runs once the stream is known and Hermes can drop.
 */
let ended = false;
const endWaiters: Array<() => void> = [];

function onEnded(callback: () => void): () => void {
  if (ended) {
    callback();
    return () => {};
  }
  endWaiters.push(callback);
  return () => {
    const index = endWaiters.indexOf(callback);
    if (index >= 0) endWaiters.splice(index, 1);
  };
}

function markEnded(reason: string): void {
  if (ended) return;
  ended = true;
  record({ src: "watch", event: "stream_ended", reason });
  console.log(`capture: stream ended (${reason}); ${options.endGrace}s for the tail`);
  for (const wake of endWaiters.splice(0)) wake();
}

/** Sleep that returns as soon as the stream ends. */
function sleepUnlessEnded(ms: number): Promise<void> {
  return new Promise((resolve) => {
    if (ended) {
      resolve();
      return;
    }
    let detach = (): void => {};
    const timer = setTimeout(() => {
      detach();
      resolve();
    }, ms);
    detach = onEnded(() => {
      clearTimeout(timer);
      resolve();
    });
  });
}

const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

async function post<T>(body: unknown): Promise<T> {
  const response = await fetch(GQL, {
    method: "POST",
    headers: { "Client-ID": CLIENT_ID, "Content-Type": "text/plain;charset=UTF-8" },
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(15000),
  });
  return (await response.json()) as T;
}

/** The daemon posts these as a one-element array, so the reply is an array too. */
async function streamMetadata(): Promise<StreamMetadataResponse> {
  const raw = await post<StreamMetadataResponse | StreamMetadataResponse[]>([
    {
      operationName: "StreamMetadata",
      variables: { channelLogin: options.channel, includeIsDJ: false },
      extensions: { persistedQuery: { version: 1, sha256Hash: STREAM_METADATA } },
    },
  ]);
  return Array.isArray(raw) ? (raw[0] ?? {}) : raw;
}

/** The broadcast record, which can lag the master by seconds when it starts. */
async function waitForStream(
  timeoutMs: number,
): Promise<{ id: string; createdAt: string } | null> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      const meta = await streamMetadata();
      const stream = meta.data?.user?.stream;
      if (stream?.id && stream.createdAt) return { id: stream.id, createdAt: stream.createdAt };
    } catch {
      // transient; retry until the deadline
    }
    if (Date.now() >= deadline) return null;
    await sleep(500);
  }
}

async function get(url: string, timeout = 15000): Promise<string> {
  const response = await fetch(url, {
    headers: { "User-Agent": "siphon-capture" },
    signal: AbortSignal.timeout(timeout),
  });
  // A 403 from an unpublished DVR object and a 404 from an expired playlist
  // both read as bodies if this is skipped, and a playlist-shaped parser would
  // happily save "Forbidden" as a revision.
  if (!response.ok) throw new Error(`HTTP ${response.status} ${url.slice(0, 80)}`);
  return response.text();
}

/** GET for bytes: the init segment and the first media fragment. */
async function getBytes(url: string, timeout = 30000): Promise<Buffer> {
  const response = await fetch(url, {
    headers: { "User-Agent": "siphon-capture" },
    signal: AbortSignal.timeout(timeout),
  });
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return Buffer.from(await response.arrayBuffer());
}

async function head(url: string): Promise<number> {
  try {
    const response = await fetch(url, {
      method: "HEAD",
      headers: { "User-Agent": "siphon-capture" },
      signal: AbortSignal.timeout(8000),
    });
    return response.status;
  } catch {
    return 0;
  }
}

// ------------------------------------------------------------------- variants

function parseMaster(text: string): Variant[] {
  const variants: Variant[] = [];
  let info: string | null = null;
  for (const line of text.split("\n")) {
    if (line.startsWith("#EXT-X-STREAM-INF:")) {
      info = line.slice("#EXT-X-STREAM-INF:".length);
      continue;
    }
    if (!info || !line || line.startsWith("#")) continue;
    const attribute = (name: string): string =>
      info?.match(new RegExp(`${name}="?([^",]+)"?`))?.[1] ?? "";
    const resolution = attribute("RESOLUTION");
    const [width, height] = resolution.split("x").map(Number);
    const kind =
      !resolution || !width || !height
        ? "audio"
        : height > width
          ? "portrait"
          : "landscape";
    variants.push({
      kind,
      bandwidth: Number(attribute("BANDWIDTH")),
      id: attribute("STABLE-VARIANT-ID"),
      resolution: resolution || "?",
      source: attribute("IVS-VARIANT-SOURCE"),
      url: line,
    });
    info = null;
  }
  return variants.sort((left, right) => right.bandwidth - left.bandwidth);
}

function printVariants(title: string, variants: Variant[]): void {
  console.log(`capture: ${title}`);
  for (const variant of variants) {
    console.log(
      `  ${variant.kind.padEnd(9)} ${String(variant.bandwidth).padStart(10)}  ` +
        `${variant.id.padEnd(18)} ${variant.resolution.padEnd(10)} ${variant.source}`,
    );
  }
}

/** The highest-bandwidth variant of one orientation. */
function pick(variants: Variant[], kind: "landscape" | "portrait"): string | null {
  return variants.find((variant) => variant.kind === kind)?.url ?? null;
}

function sessionData(master: string, id: string): string | null {
  const match = master.match(new RegExp(`DATA-ID="${id}",VALUE="([^"]*)"`));
  return match?.[1] ?? null;
}

// ------------------------------------------------------------ live revisions

interface MediaSegment {
  duration: number | null;
  pdt: string | null;
  uri: string;
}

interface MediaPlaylist {
  sequence: number | null;
  targetDuration: number | null;
  elapsed: number | null;
  total: number | null;
  endlist: boolean;
  segments: MediaSegment[];
}

function parseMedia(text: string): MediaPlaylist {
  const playlist: MediaPlaylist = {
    sequence: null,
    targetDuration: null,
    elapsed: null,
    total: null,
    endlist: false,
    segments: [],
  };
  let duration: number | null = null;
  let pdt: string | null = null;
  const value = (line: string, tag: string): string => line.slice(tag.length).trim();
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (line.startsWith("#EXTINF:")) {
      duration = Number.parseFloat(value(line, "#EXTINF:"));
    } else if (line.startsWith("#EXT-X-PROGRAM-DATE-TIME:")) {
      pdt = value(line, "#EXT-X-PROGRAM-DATE-TIME:");
    } else if (line.startsWith("#EXT-X-MEDIA-SEQUENCE:")) {
      playlist.sequence = Number(value(line, "#EXT-X-MEDIA-SEQUENCE:"));
    } else if (line.startsWith("#EXT-X-TARGETDURATION:")) {
      playlist.targetDuration = Number(value(line, "#EXT-X-TARGETDURATION:"));
    } else if (line.startsWith("#EXT-X-TWITCH-ELAPSED-SECS:")) {
      playlist.elapsed = Number.parseFloat(value(line, "#EXT-X-TWITCH-ELAPSED-SECS:"));
    } else if (line.startsWith("#EXT-X-TWITCH-TOTAL-SECS:")) {
      playlist.total = Number.parseFloat(value(line, "#EXT-X-TWITCH-TOTAL-SECS:"));
    } else if (line === "#EXT-X-ENDLIST") {
      playlist.endlist = true;
    } else if (line && !line.startsWith("#")) {
      playlist.segments.push({ duration, pdt, uri: line });
      duration = null;
      pdt = null;
    }
  }
  return playlist;
}

/**
 * Every distinct state of the live media playlist, hashed so an unchanged
 * playlist is not rewritten. The live playlist is the only place the
 * end-of-stream shape is visible: the trailing entry Twitch appends after the
 * last real segment, its `EXTINF`, and whether Twitch emits `ENDLIST` at all or
 * the URLs simply start 404ing. The DVR playlist is append-only, so it does not
 * need this.
 */
const revision = { hash: "", count: 0 };

function saveRevision(text: string): void {
  const hash = createHash("sha1").update(text).digest("hex");
  if (hash === revision.hash) return;
  revision.hash = hash;
  const name = `${String(revision.count).padStart(4, "0")}.m3u8`;
  mkdirSync(revisionsDir, { recursive: true });
  writeFileSync(path.join(revisionsDir, name), text);
  const parsed = parseMedia(text);
  const last = parsed.segments.at(-1);
  // Segment names are long CDN tokens; the extension is the interesting part
  // (.mp4 while live, .m4s once the playlist has ended).
  const lastUri = last ? path.basename(new URL(last.uri, "https://x/").pathname) : null;
  record({
    src: "live",
    event: "revision",
    file: `live-media/${name}`,
    media_sequence: parsed.sequence,
    target_duration: parsed.targetDuration,
    elapsed: parsed.elapsed,
    total: parsed.total,
    segments: parsed.segments.length,
    last_extinf: last?.duration ?? null,
    last_pdt: last?.pdt ?? null,
    last_uri: lastUri,
    endlist: parsed.endlist,
    hash: hash.slice(0, 12),
  });
  console.log(
    `capture: live revision ${name} seq=${parsed.sequence} segments=${parsed.segments.length}` +
      (last ? ` last=${last.duration?.toFixed(3) ?? "?"}s ${lastUri ? path.extname(lastUri) : ""}` : "") +
      (parsed.endlist ? " ENDLIST" : ""),
  );
  revision.count += 1;
}

/** Poll one live media playlist until ENDLIST, or until it stops answering. */
async function logRevisions(playlistUrl: string): Promise<void> {
  let failures = 0;
  for (;;) {
    try {
      const text = await get(playlistUrl);
      failures = 0;
      saveRevision(text);
      if (parseMedia(text).endlist) {
        record({ src: "live", event: "endlist" });
        console.log("capture: live playlist reached ENDLIST");
        markEnded("endlist");
        return;
      }
    } catch (error) {
      failures += 1;
      record({ src: "live", event: "revision_failed", error: String(error) });
      if (failures >= 10) {
        console.log("capture: live playlist stopped answering; revision log closed");
        markEnded("live playlist gone");
        return;
      }
    }
    await sleep(REVISION_INTERVAL);
  }
}

// ---------------------------------------------------------------- live master

let masterUrl: string | null = null;
let channelId: string | null = null;

async function resolveChannel(): Promise<void> {
  const meta = await streamMetadata();
  channelId = meta.data?.user?.id ?? null;
  const token = await post<{
    data?: { streamPlaybackAccessToken?: { signature?: string; value?: string } | null };
  }>({
    operationName: "PlaybackAccessToken",
    extensions: { persistedQuery: { version: 1, sha256Hash: PLAYBACK_TOKEN } },
    variables: {
      isLive: true,
      isVod: false,
      login: options.channel,
      playerType: "site",
      platform: "site",
      vodID: "",
    },
  });
  const issued = token.data?.streamPlaybackAccessToken;
  if (!issued?.signature || !issued.value) throw new Error("no playback token");
  const params: Record<string, string> = {
    allow_source: "true",
    allow_audio_only: "true",
    browser_family: "chrome",
    browser_version: "151.0",
    cdm: "wv",
    enable_score: "true",
    fast_bread: "false",
    include_unavailable: "true",
    lang: "en",
    multigroup_video: "true",
    os_name: "Windows",
    os_version: "NT 10.0",
    p: "1234567",
    platform: "web",
    play_session_id: "abcdef0123456789abcdef0123456789",
    player_backend: "mediaplayer",
    playlist_include_framerate: "true",
    reassignments_supported: "true",
    sig: issued.signature,
    supported_codecs: "av1,h265,h264",
    token: issued.value.trim(),
    transcode_mode: "cbr_v1",
  };
  const query = Object.entries(params)
    .map(([key, value]) => `${key}=${encodeURIComponent(value)}`)
    .join("&");
  masterUrl = `${USHER}/${options.channel}.m3u8?${query}`;
  record({ src: "watch", event: "resolved", channel_id: channelId });
}

// -------------------------------------------------------------------- archive

function dvrObject(streamId: string, startSeconds: number, offset: number): string {
  const name = `${options.channel}_${streamId}_${startSeconds + offset}`;
  return `${createHash("sha1").update(name).digest("hex").slice(0, 20)}_${name}`;
}

/** One private DVR media playlist by its path inside the object directory. */
async function probeDvr(streamId: string, startSeconds: number, sub: string): Promise<string | null> {
  for (const offset of [0, -1, 1]) {
    const object = dvrObject(streamId, startSeconds, offset);
    for (const domain of VOD_DOMAINS) {
      const url = `${domain}/${object}/${sub}`;
      if ((await head(url)) === 200) return url;
    }
  }
  return null;
}

/** The archive multivariant playlist: lists every DVR media playlist. */
async function archiveMaster(userId: string, startSeconds: number): Promise<string | null> {
  const recent = await post<
    Array<{
      data?: {
        user?: {
          videos?: { edges?: Array<{ node?: { id?: string; createdAt?: string } }> };
        };
      };
    }>
  >([
    {
      operationName: "FFZ_RecentBroadcasts",
      variables: { id: userId, type: "ARCHIVE", sort: "TIME", limit: 1 },
      extensions: { persistedQuery: { version: 1, sha256Hash: RECENT_BROADCASTS } },
    },
  ]);
  const edge = (Array.isArray(recent) ? recent[0] : recent)?.data?.user?.videos?.edges?.[0];
  const vod = edge?.node;
  if (!vod?.id || !vod.createdAt) return null;
  if (Date.parse(vod.createdAt) / 1000 < startSeconds) return null;

  const token = await post<{
    data?: { videoPlaybackAccessToken?: { signature?: string; value?: string } | null };
  }>({
    operationName: "PlaybackAccessToken",
    extensions: { persistedQuery: { version: 1, sha256Hash: PLAYBACK_TOKEN } },
    variables: {
      isLive: false,
      isVod: true,
      login: "",
      vodID: vod.id,
      playerType: "site",
      platform: "web",
    },
  });
  const issued = token.data?.videoPlaybackAccessToken;
  if (!issued?.signature || !issued.value) return null;

  const params: Record<string, string> = {
    allow_audio_only: "true",
    allow_source: "true",
    browser_family: "chrome",
    browser_version: "157.0",
    cdm: "wv",
    enable_score: "true",
    include_unavailable: "true",
    lang: "en",
    multigroup_video: "true",
    os_name: "Windows",
    os_version: "NT 10.0",
    p: "3642389",
    platform: "web",
    player_backend: "mediaplayer",
    player_version: "1.59.0-rc.1",
    playlist_include_framerate: "true",
    reassignments_supported: "true",
    sig: issued.signature,
    supported_codecs: "av1,h265,h264",
    token: issued.value.trim(),
    transcode_mode: "cbr_v1",
  };
  const query = Object.entries(params)
    .map(([key, value]) => `${key}=${encodeURIComponent(value)}`)
    .join("&");
  return get(`${VOD_USHER}/${vod.id}.m3u8?${query}`);
}

// -------------------------------------------------------------------- hermes

async function hermes(): Promise<void> {
  for (;;) {
    await new Promise<void>((resolve) => {
      let socket: WebSocket;
      try {
        socket = new WebSocket(HERMES);
      } catch (error) {
        record({ src: "hermes", event: "connect_failed", error: String(error) });
        setTimeout(resolve, 2000);
        return;
      }
      const topics = new Map<string, string>();
      const subscribe = (topic: string): void => {
        const id = randomBytes(16).toString("base64url");
        topics.set(id, topic);
        socket.send(
          JSON.stringify({
            type: "subscribe",
            id: randomBytes(16).toString("base64url"),
            subscribe: { id, type: "pubsub", pubsub: { topic } },
            timestamp: new Date().toISOString(),
          }),
        );
      };
      socket.addEventListener("open", () => record({ src: "hermes", event: "open" }));
      socket.addEventListener("message", (event: MessageEvent<string>) => {
        let message: HermesMessage;
        try {
          message = JSON.parse(event.data) as HermesMessage;
        } catch {
          return;
        }
        if (message.type === "welcome") {
          // Subscriptions only count after the welcome, as in hermes.rs.
          record({
            src: "hermes",
            event: "welcome",
            keepalive: message.welcome?.keepaliveSec ?? null,
          });
          if (channelId) {
            subscribe(`video-playback-by-id.${channelId}`);
            subscribe(`broadcast-settings-update.${channelId}`);
          }
          return;
        }
        if (message.type === "subscribeResponse") {
          record({
            src: "hermes",
            event: "subscribed",
            result: message.subscribeResponse?.result ?? null,
            topic: topics.get(message.subscribeResponse?.subscription?.id ?? "") ?? null,
          });
          return;
        }
        if (message.type !== "notification") return;
        // The payload is a JSON *string*, not an object: hermes.rs parses
        // message["notification"]["pubsub"].as_str() the same way.
        let pubsub: { type?: string; viewers?: number } = {};
        const raw = message.notification?.pubsub as unknown;
        try {
          pubsub = typeof raw === "string" ? JSON.parse(raw) : ((raw as typeof pubsub) ?? {});
        } catch {
          return;
        }
        record({
          src: "hermes",
          event: pubsub.type ?? "notification",
          topic: topics.get(message.notification?.subscription?.id ?? "") ?? null,
          viewers: pubsub.viewers ?? null,
        });
        if (pubsub.type === "stream-down") markEnded("hermes stream-down");
      });
      socket.addEventListener("close", () => {
        record({ src: "hermes", event: "close" });
        resolve();
      });
      socket.addEventListener("error", () => {
        record({ src: "hermes", event: "error" });
        try {
          socket.close();
        } catch {}
        resolve();
      });
    });
    await sleep(2000);
  }
}

// ------------------------------------------------------------------ recording

const children: Array<{ label: string; process: ChildProcess }> = [];

function runningChildren(): Array<{ label: string; process: ChildProcess }> {
  return children.filter(
    (child) => child.process.exitCode === null && child.process.signalCode === null,
  );
}

function stopChildren(): void {
  const running = runningChildren();
  for (const child of running) child.process.kill("SIGTERM");
  if (running.length === 0) {
    // Nothing left to wait for, and the GQL poll and Hermes socket would keep
    // the loop alive forever.
    process.exit(0);
    return;
  }
  // Give them a moment to flush, then make sure they are gone.
  setTimeout(() => {
    for (const child of runningChildren()) child.process.kill("SIGKILL");
    process.exit(0);
  }, 1500);
}

for (const signal of ["SIGINT", "SIGTERM"] as const) {
  process.on(signal, () => {
    console.log(`capture: stopping ${children.length} recorder(s)`);
    stopChildren();
  });
}

function spawnRecorder(label: string, url: string, extra: string[]): void {
  const dir = path.join(options.out, label);
  if (options.dryRun) {
    console.log(`would run: ${label} -> ${dir}\n  ${url}\n  ${extra.join(" ")}`);
    return;
  }
  mkdirSync(path.join(dir, "tmp"), { recursive: true });
  const logPath = path.join(dir, `${label}.log`);
  if (existsSync(logPath)) {
    // Re-running into the same --out would put two recorders on one directory.
    console.log(`capture: ${label} already recorded here, not starting another`);
    return;
  }
  const log = openSync(logPath, "w");
  let closed = false;
  const closeLog = (): void => {
    if (!closed) {
      closed = true;
      closeSync(log);
    }
  };
  const child = spawn(
    "N_m3u8DL-RE",
    [
      url,
      "--save-dir",
      dir,
      "--save-name",
      label,
      "--tmp-dir",
      path.join(dir, "tmp"),
      "--skip-merge",
      "--del-after-done",
      "false",
      "--live-keep-segments",
      "true",
      "--thread-count",
      "4",
      "--check-segments-count",
      "false",
      "--no-ansi-color",
      "--disable-update-check",
      "--log-level",
      "INFO",
      ...extra,
    ],
    { stdio: ["ignore", log, log] },
  );
  child.on("close", closeLog);
  child.on("error", (error) => {
    console.error(`capture: ${label} failed to start: ${error.message}`);
    closeLog();
  });
  children.push({ label, process: child });
  console.log(`capture: ${label} -> ${dir} (pid ${child.pid})`);
  record({ src: "spawn", event: label, pid: child.pid ?? null });
}

// ----------------------------------------------------------------------- main

async function main(): Promise<void> {
  await resolveChannel();
  hermes().catch((error: unknown) => record({ src: "hermes", event: "fatal", error: String(error) }));

  // Poll GQL the whole time: it is how the daemon sees a channel, and its
  // timestamps are what the Hermes/manifest ones are compared against.
  let live = false;
  void (async () => {
    for (;;) {
      const polled = Date.now();
      try {
        const meta = await streamMetadata();
        const stream = meta.data?.user?.stream ?? null;
        record({
          src: "gql",
          event: stream ? "live" : "offline",
          latency_ms: Date.now() - polled,
          stream_id: stream?.id ?? null,
          viewers: stream?.viewersCount ?? null,
        });
        if (!stream && live) markEnded("gql offline");
        live = stream !== null;
      } catch (error) {
        record({ src: "gql", event: "error", error: String(error) });
      }
      await sleep(options.interval);
    }
  })();
  /** One master fetch. `null` while the channel has no variants. */
  const pollMaster = async (): Promise<{ text: string; variants: Variant[] } | null> => {
    if (!masterUrl) return null;
    const polled = Date.now();
    try {
      const text = await get(masterUrl);
      const parsed = parseMaster(text);
      record({
        src: "master",
        event: parsed.length > 0 ? "variants" : "empty",
        variants: parsed.length,
        latency_ms: Date.now() - polled,
      });
      return parsed.length > 0 ? { text, variants: parsed } : null;
    } catch (error) {
      record({ src: "master", event: "error", error: String(error) });
      return null;
    }
  };

  let ready = await pollMaster();
  const waited = ready === null;
  if (!ready) {
    console.log(`capture: ${options.channel} is not live yet; watching for the first variant`);
    const deadline = Date.now() + options.watchTimeout * 1000;
    let announced = false;
    while (Date.now() < deadline) {
      if (!announced && live) {
        announced = true;
        console.log("capture: the channel is live, waiting for the first variant");
      }
      await sleep(options.interval);
      ready = await pollMaster();
      if (ready) break;
    }
    if (!ready) {
      console.error(`capture: no live variant after ${options.watchTimeout}s, giving up`);
      process.exit(1);
    }
  }

  const { text: master } = ready;
  let variants = ready.variants;
  writeFileSync(path.join(options.out, "live-master.m3u8"), master);

  if (waited) {
    // The first variant playlist is the one that lists segment 1.
    const first = variants[0]!;
    try {
      const media = await get(first.url);
      writeFileSync(path.join(options.out, "live-first-media.m3u8"), media);
      saveRevision(media);
      const parsed = parseMedia(media);
      record({
        src: "master",
        event: "first",
        variant: first.id,
        media_sequence: parsed.sequence,
        first_pdt: parsed.segments[0]?.pdt ?? null,
        segments: parsed.segments.length,
      });
      // The segment itself, so the capture does not depend on fetching it by
      // hand afterwards. `live-first-media.m3u8` above is the evidence that it
      // is segment 0.
      const firstDir = path.join(options.out, "first");
      mkdirSync(firstDir, { recursive: true });
      const map = media.match(/^#EXT-X-MAP:URI="([^"]+)"/m)?.[1];
      if (map) {
        const bytes = await getBytes(new URL(map, first.url).toString());
        writeFileSync(path.join(firstDir, "init.mp4"), bytes);
        record({ src: "live", event: "init_saved", bytes: bytes.length });
      }
      const segment = parsed.segments[0];
      if (segment) {
        const bytes = await getBytes(new URL(segment.uri, first.url).toString());
        const name = `${String(parsed.sequence ?? 0).padStart(6, "0")}.seg`;
        writeFileSync(path.join(firstDir, name), bytes);
        record({
          src: "live",
          event: "segment_saved",
          file: `first/${name}`,
          bytes: bytes.length,
          duration: segment.duration,
          pdt: segment.pdt,
        });
      }
    } catch (error) {
      record({ src: "master", event: "first_failed", error: String(error) });
    }
  }

  // From here the live playlist is polled until ENDLIST: the first revision is
  // already saved above, and each later one only when it changes.
  void logRevisions(variants[0]!.url).catch((error: unknown) =>
    record({ src: "live", event: "revision_failed", error: String(error) }),
  );
  console.log(`capture: live master saved to ${path.join(options.out, "live-master.m3u8")}`);
  const features = sessionData(master, "CHANNEL-METADATA");
  const stack = sessionData(master, "TRANSCODESTACK");
  if (features) console.log(`capture: channel features: ${features}`);
  if (stack) console.log(`capture: transcode stack:  ${stack}`);
  printVariants(`${options.channel} variants (kind, bandwidth, id, resolution, source)`, variants);

  const liveLandscape = pick(variants, "landscape");
  const livePortrait = pick(variants, "portrait");
  record({ src: "watch", event: "variants", variants: variants.length });
  console.log(`capture: output ${options.out}`);

  // Live first: it is the time-critical half and needs nothing from GQL.
  // `--live-take-count` is not "how many to download": on the first pass the
  // tool keeps the LAST N segments of the playlist (FilterUtil.SyncStreams skips
  // everything before the tail), and only later refreshes append. The default
  // 16 happens to cover a 30 s window of 2 s segments, but that is incidental -
  // asking for more makes "start at the first segment" explicit.
  if (options.dvrOnly) {
    console.log("capture: --dvr-only, not starting the live recorders");
  } else if (liveLandscape) {
    spawnRecorder(`${options.channel}-landscape-live`, liveLandscape, [
      "--live-wait-time",
      "4",
      "--live-take-count",
      "100",
    ]);
  } else {
    console.error("capture: no landscape variant, skipping live landscape");
  }
  if (!options.dvrOnly && livePortrait) {
    spawnRecorder(`${options.channel}-portrait-live`, livePortrait, [
      "--live-wait-time",
      "4",
      "--live-take-count",
      "100",
    ]);
  } else if (!options.dvrOnly) {
    console.error("capture: no portrait variant, skipping live portrait");
  }

  // The DVR needs the broadcast's id and start time, and StreamMetadata lags the
  // master by seconds on a fresh broadcast. The private DVR object is not
  // published immediately either (every path 403s for the first minutes), so the
  // whole resolution retries until each live orientation has a playlist.
  const wanted: Array<"landscape" | "portrait"> = [];
  if (liveLandscape) wanted.push("landscape");
  if (livePortrait) wanted.push("portrait");
  const spawned = new Set<string>();
  const dvrDeadline = Date.now() + options.dvrWatch * 1000;
  // StreamMetadata starts returning null the moment the broadcast ends, but the
  // DVR object is usually published after that, so the id and start time are
  // cached and the last probe runs on the cached copy.
  let broadcast: { id: string; createdAt: string } | null = null;
  let announced = false;
  for (;;) {
    const stream = await waitForStream(ended ? 2000 : 10000);
    if (stream) broadcast = stream;
    if (!broadcast) {
      if (ended) {
        console.log("capture: stream ended with no broadcast metadata; giving up on the DVR");
        break;
      }
      console.error("capture: no broadcast metadata yet; will keep trying for the DVR");
    } else {
      const startSeconds = Math.floor(Date.parse(broadcast.createdAt) / 1000);
      if (!announced) {
        announced = true;
        console.log(
          `capture: broadcast ${broadcast.id} started ${new Date(startSeconds * 1000).toISOString()}`,
        );
      }

      // Resolution order mirrors the daemon's `dvr::discover`: the public
      // archive master first, then the per-variant private object, then
      // `chunked/`. They are not interchangeable: a group's live id is not
      // necessarily its DVR object id. The landscape source is `chunked/` and
      // its live id 403s, while portrait's live id is its own object and
      // answers. Only the archive master states which object belongs to which
      // group (`IVS-GROUPS`), so it has to be the primary route.
      const choices: DvrChoice[] = [];
      try {
        const archive = channelId ? await archiveMaster(channelId, startSeconds) : null;
        if (archive) {
          writeFileSync(path.join(options.out, "dvr-master.m3u8"), archive);
          console.log(`capture: DVR master saved to ${path.join(options.out, "dvr-master.m3u8")}`);
          const archiveVariants = parseMaster(archive);
          printVariants(
            "archive variants (kind, bandwidth, id, resolution, source)",
            archiveVariants,
          );
          for (const kind of wanted) {
            const url = pick(archiveVariants, kind);
            if (!url) continue;
            const id = archiveVariants.find((variant) => variant.url === url)?.id;
            choices.push({ kind, url, via: `archive:${id || "?"}` });
          }
        }
      } catch (error) {
        record({ src: "watch", event: "archive_failed", error: String(error) });
      }

      // The private object is the fallback, not the primary: it needs nothing
      // from the VOD endpoint, and for a secondary group it is the same object
      // the archive master points at anyway.
      for (const kind of wanted) {
        if (choices.some((choice) => choice.kind === kind)) continue;
        const live = kind === "landscape" ? liveLandscape : livePortrait;
        const id = variants.find((variant) => variant.url === live)?.id;
        if (!id) continue;
        const url = await probeDvr(broadcast.id, startSeconds, `${id}/index-dvr.m3u8`);
        if (url) choices.push({ kind, url, via: `private:${id}` });
      }

      // `chunked/` is the source group, and on a dual-format broadcast that is
      // the landscape source (the portrait group lives under its own id).
      if (!choices.some((choice) => choice.kind === "landscape") && liveLandscape) {
        const chunked = await probeDvr(broadcast.id, startSeconds, "chunked/index-dvr.m3u8");
        if (chunked) {
          console.log("capture: attributing chunked/ to landscape (the source group)");
          choices.push({ kind: "landscape", url: chunked, via: "chunked/" });
        }
      }

      for (const choice of choices) {
        if (spawned.has(choice.kind)) continue;
        spawned.add(choice.kind);
        console.log(`capture: DVR ${choice.kind} via ${choice.via}: ${choice.url}`);
        record({
          src: "watch",
          event: "dvr",
          kind: choice.kind,
          via: choice.via,
          url: choice.url,
        });
        spawnRecorder(`${options.channel}-${choice.kind}-dvr`, choice.url, [
          "--live-wait-time",
          "5",
          "--live-take-count",
          "1000000",
        ]);
      }
    }

    if (spawned.size >= wanted.length || Date.now() >= dvrDeadline) break;
    // One probe after the stream ends (the object is often published only then),
    // then stop rather than waiting out --dvr-watch.
    if (ended) break;
    console.log(
      `capture: waiting for the DVR playlist(s) (${spawned.size}/${wanted.length}); retrying in ${options.dvrRetry}s`,
    );
    record({ src: "watch", event: "dvr_wait", spawned: spawned.size, wanted: wanted.length });
    await sleepUnlessEnded(options.dvrRetry * 1000);
  }
  if (spawned.size === 0) {
    console.error("capture: no DVR playlist found; the live recorders are unaffected");
  }

  console.log(
    `capture: gql ${live ? "live" : "not live"} at ${(
      (Date.now() - started) /
      1000
    ).toFixed(2)}s; timeline in ${timeline}`,
  );

  if (options.dryRun) {
    console.log("capture: dry run, nothing started");
    process.exit(0);
  }
  if (children.length === 0) {
    console.error("capture: nothing to record");
    process.exit(1);
  }
  console.log(
    `capture: ${children.length} recorder(s) running; they stop on ENDLIST, Ctrl-C stops them now`,
  );

  // The live recorders stop themselves when the playlist ends. The DVR ones sit
  // on an EVENT playlist that will never grow again and wait forever, so the end
  // of the stream starts a grace period after which everything is stopped.
  await new Promise<void>((resolve) => {
    let remaining = runningChildren().length;
    if (remaining === 0) {
      resolve();
      return;
    }
    let grace: NodeJS.Timeout | null = null;
    const detach = onEnded(() => {
      console.log(
        `capture: ${remaining} recorder(s) still running; stopping in ${options.endGrace}s`,
      );
      grace = setTimeout(() => resolve(), options.endGrace * 1000);
    });
    for (const child of children) {
      child.process.once("close", () => {
        remaining -= 1;
        if (remaining > 0) return;
        detach();
        if (grace) clearTimeout(grace);
        resolve();
      });
    }
  });

  const running = runningChildren();
  if (running.length > 0) {
    console.log(`capture: stopping ${running.length} recorder(s)`);
    stopChildren();
    return;
  }
  console.log("capture: all recorders finished");
  process.exit(0);
}

main().catch((error: unknown) => {
  console.error(`capture: ${String(error)}`);
  process.exit(1);
});
