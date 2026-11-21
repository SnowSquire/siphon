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
// dvr-master.m3u8) and the first variant playlist (live-first-media.m3u8).
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
  url: string;
}

type Source = "watch" | "gql" | "hermes" | "master" | "spawn";

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
const started = Date.now();

function record(observation: Observation): void {
  appendFileSync(
    timeline,
    `${JSON.stringify({ t: Date.now() / 1000, age: (Date.now() - started) / 1000, ...observation })}\n`,
  );
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
  return response.text();
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

function stopChildren(): void {
  for (const child of children) {
    if (child.process.exitCode === null) child.process.kill("SIGTERM");
  }
  // Give them a moment to flush, then make sure they are gone.
  setTimeout(() => {
    for (const child of children) {
      if (child.process.exitCode === null) child.process.kill("SIGKILL");
    }
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
      record({
        src: "master",
        event: "first",
        variant: first.id,
        media_sequence: media.match(/^#EXT-X-MEDIA-SEQUENCE:(.*)$/m)?.[1]?.trim() ?? null,
        first_pdt: media.match(/^#EXT-X-PROGRAM-DATE-TIME:(.*)$/m)?.[1]?.trim() ?? null,
        segments: media.split("\n").filter((line) => line.startsWith("http")).length,
      });
    } catch (error) {
      record({ src: "master", event: "first_failed", error: String(error) });
    }
  }
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
  for (;;) {
    const stream = await waitForStream(10000);
    if (!stream) {
      console.error("capture: no broadcast metadata yet; will keep trying for the DVR");
    } else {
      const startSeconds = Math.floor(Date.parse(stream.createdAt) / 1000);
      if (spawned.size === 0) {
        console.log(
          `capture: broadcast ${stream.id} started ${new Date(startSeconds * 1000).toISOString()}`,
        );
      }

      // The per-variant private path is the only way to reach the portrait group
      // when the archive master does not list it.
      const choices: DvrChoice[] = [];
      for (const kind of wanted) {
        const live = kind === "landscape" ? liveLandscape : livePortrait;
        const id = variants.find((variant) => variant.url === live)?.id;
        if (!id) continue;
        const url = await probeDvr(stream.id, startSeconds, `${id}/index-dvr.m3u8`);
        if (url) choices.push({ kind, url });
      }

      if (choices.length < wanted.length) {
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
              if (choices.some((choice) => choice.kind === kind)) continue;
              const url = pick(archiveVariants, kind);
              if (url) choices.push({ kind, url });
            }
          }
        } catch (error) {
          record({ src: "watch", event: "archive_failed", error: String(error) });
        }
      }

      // `chunked/` is the source group, and on a dual-format broadcast that is
      // the landscape source (the portrait group lives under its own id).
      if (!choices.some((choice) => choice.kind === "landscape") && liveLandscape) {
        const chunked = await probeDvr(stream.id, startSeconds, "chunked/index-dvr.m3u8");
        if (chunked) {
          console.log("capture: attributing chunked/ to landscape (the source group)");
          choices.push({ kind: "landscape", url: chunked });
        }
      }

      for (const choice of choices) {
        if (spawned.has(choice.kind)) continue;
        spawned.add(choice.kind);
        console.log(`capture: DVR ${choice.kind} ${choice.url}`);
        record({ src: "watch", event: "dvr", kind: choice.kind, url: choice.url });
        spawnRecorder(`${options.channel}-${choice.kind}-dvr`, choice.url, [
          "--live-wait-time",
          "5",
          "--live-take-count",
          "1000000",
        ]);
      }
    }

    if (spawned.size >= wanted.length || Date.now() >= dvrDeadline) break;
    console.log(
      `capture: waiting for the DVR playlist(s) (${spawned.size}/${wanted.length}); retrying in ${options.dvrRetry}s`,
    );
    record({ src: "watch", event: "dvr_wait", spawned: spawned.size, wanted: wanted.length });
    await sleep(options.dvrRetry * 1000);
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
  console.log(`capture: ${children.length} recorder(s) running; Ctrl-C stops them`);

  await new Promise<void>((resolve) => {
    const pending = children.filter(
      (child) => child.process.exitCode === null && child.process.signalCode === null,
    );
    if (pending.length === 0) {
      resolve();
      return;
    }
    let remaining = pending.length;
    for (const child of pending) {
      child.process.once("close", () => {
        remaining -= 1;
        if (remaining === 0) resolve();
      });
    }
  });
}

main().catch((error: unknown) => {
  console.error(`capture: ${String(error)}`);
  process.exit(1);
});
