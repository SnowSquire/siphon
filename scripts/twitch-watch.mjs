#!/usr/bin/env node
// Poll one Twitch channel from two directions and write every observation with
// a timestamp, so the two paths can be compared:
//
//   * `gql`    - the StreamMetadata GraphQL query the daemon uses for channels
//                (`hermes.rs`/`dvr.rs` persisted hash), polled every --interval.
//   * `hermes` - the same pubsub websocket the daemon uses
//                (`wss://hermes.twitch.tv/v1`), subscribed to
//                `video-playback-by-id.<id>` and `broadcast-settings-update.<id>`.
//   * `master` - the live multivariant playlist, so the moment it grows its
//                first variant is on the timeline too. The first non-empty
//                master and the first variant media playlist are saved next to
//                the timeline, and `live-master.ready` is created - that file is
//                what the capture script waits for.
//
//   node scripts/twitch-watch.mjs --channel ozeniken --out captures/ozeniken-x
//
// Everything is appended to <out>/timeline.jsonl as one JSON object per line.
import { appendFileSync, mkdirSync, writeFileSync } from "node:fs";
import { randomBytes } from "node:crypto";
import path from "node:path";

const CLIENT_ID = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const GQL = "https://gql.twitch.tv/gql";
const HERMES = `wss://hermes.twitch.tv/v1?clientId=${CLIENT_ID}`;
const USHER = "https://usher.ttvnw.net/api/v2/channel/hls";
const STREAM_METADATA = "b57f9b910f8cd1a4659d894fe7550ccc81ec9052c01e438b290fd66a040b9b93";
const PLAYBACK_TOKEN = "ed230aa1e33e07eebb8928504583da78a5173989fadfb1ac94be06a04f3cdbe9";

const options = { channel: "", out: "captures/watch", interval: 300 };
for (let index = 2; index < process.argv.length; index += 2) {
  const key = process.argv[index].replace(/^--/, "");
  options[key] = process.argv[index + 1];
}
options.interval = Number(options.interval);
if (!options.channel) {
  console.error("usage: twitch-watch.mjs --channel <login> --out <dir> [--interval ms]");
  process.exit(2);
}
mkdirSync(options.out, { recursive: true });
const timeline = path.join(options.out, "timeline.jsonl");

function record(event) {
  appendFileSync(timeline, `${JSON.stringify({ t: Date.now() / 1000, ...event })}\n`);
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function post(body) {
  const response = await fetch(GQL, {
    method: "POST",
    headers: { "Client-ID": CLIENT_ID, "Content-Type": "text/plain;charset=UTF-8" },
    body: JSON.stringify(body),
  });
  return response.json();
}

function gql(body) {
  return post(body).then((data) => (Array.isArray(data) ? data[0] : data));
}

let channelId = null;
let masterUrl = null;
let liveMaster = null; // saved once

function masterUrlFor(signature, value) {
  const params = {
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
    sig: signature,
    supported_codecs: "av1,h265,h264",
    token: value,
    transcode_mode: "cbr_v1",
  };
  const query = Object.entries(params)
    .map(([key, val]) => `${key}=${encodeURIComponent(val)}`)
    .join("&");
  return `${USHER}/${options.channel}.m3u8?${query}`;
}

async function resolve() {
  const meta = await gql([
    {
      operationName: "StreamMetadata",
      variables: { channelLogin: options.channel, includeIsDJ: false },
      extensions: { persistedQuery: { version: 1, sha256Hash: STREAM_METADATA } },
    },
  ]);
  channelId = meta?.data?.user?.id ?? null;
  const token = await gql({
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
  const issued = token?.data?.streamPlaybackAccessToken;
  if (issued) {
    masterUrl = masterUrlFor(issued.signature, issued.value.trim());
  }
  record({ src: "watch", event: "resolved", channel_id: channelId, has_master: !!masterUrl });
}

function variants(text) {
  return text.split("\n").filter((line) => line.startsWith("#EXT-X-STREAM-INF")).length;
}

async function pollGql() {
  const started = Date.now();
  const data = await gql([
    {
      operationName: "StreamMetadata",
      variables: { channelLogin: options.channel, includeIsDJ: false },
      extensions: { persistedQuery: { version: 1, sha256Hash: STREAM_METADATA } },
    },
  ]);
  const stream = data?.data?.user?.stream ?? null;
  record({
    src: "gql",
    event: stream ? "live" : "offline",
    latency_ms: Date.now() - started,
    stream_id: stream?.id ?? null,
    viewers: stream?.viewersCount ?? null,
    game: stream?.game?.name ?? null,
    title: stream?.title ?? null,
  });
  return !!stream;
}

async function pollMaster() {
  if (!masterUrl) return false;
  const started = Date.now();
  const response = await fetch(masterUrl, { headers: { "User-Agent": "siphon-latency" } });
  const text = await response.text();
  const count = variants(text);
  record({ src: "master", event: count ? "variants" : "empty", variants: count, latency_ms: Date.now() - started });
  if (count === 0 || liveMaster) return count > 0;

  liveMaster = text;
  writeFileSync(path.join(options.out, "live-master.m3u8"), text);
  const first = text.split("\n").find((line) => line.startsWith("https://"));
  if (first) {
    const media = await fetch(first, { headers: { "User-Agent": "siphon-latency" } }).then((r) => r.text());
    writeFileSync(path.join(options.out, "live-first-media.m3u8"), media);
    const sequence = media.match(/^#EXT-X-MEDIA-SEQUENCE:(.*)$/m)?.[1]?.trim();
    const pdt = media.match(/^#EXT-X-PROGRAM-DATE-TIME:(.*)$/m)?.[1]?.trim();
    const segments = media.split("\n").filter((line) => line.startsWith("http")).length;
    record({ src: "master", event: "first", media_sequence: sequence, first_pdt: pdt, segments });
  }
  writeFileSync(path.join(options.out, "live-master.ready"), "1");
  return true;
}

function nano() {
  return randomBytes(16).toString("base64url");
}

async function hermes() {
  for (;;) {
    await new Promise((resolve) => {
      let socket;
      try {
        socket = new WebSocket(HERMES);
      } catch (error) {
        record({ src: "hermes", event: "connect_failed", error: String(error) });
        return setTimeout(resolve, 2000);
      }
      const subscriptions = new Map();
      const subscribe = (topic) => {
        const id = nano();
        subscriptions.set(id, topic);
        socket.send(
          JSON.stringify({
            type: "subscribe",
            id: nano(),
            subscribe: { id, type: "pubsub", pubsub: { topic } },
            timestamp: new Date().toISOString(),
          }),
        );
      };
      socket.addEventListener("open", () => {
        record({ src: "hermes", event: "open" });
      });
      socket.addEventListener("message", (event) => {
        let message;
        try {
          message = JSON.parse(event.data);
        } catch {
          return;
        }
        if (message.type === "welcome") {
          record({ src: "hermes", event: "welcome", keepalive: message.welcome?.keepaliveSec ?? null });
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
            topic: subscriptions.get(message.subscribeResponse?.subscription?.id) ?? null,
          });
          return;
        }
        if (message.type !== "notification") return;
        const pubsub = message.notification?.pubsub ?? {};
        const topic = subscriptions.get(message.notification?.subscription?.id) ?? null;
        record({
          src: "hermes",
          event: pubsub.type ?? "notification",
          topic,
          viewers: pubsub.viewers ?? null,
          title: pubsub.stream_title ?? null,
          game: pubsub.game ?? null,
        });
      });
      socket.addEventListener("close", () => {
        record({ src: "hermes", event: "close" });
        resolve();
      });
      socket.addEventListener("error", (error) => {
        record({ src: "hermes", event: "error", error: String(error.message ?? error) });
        try {
          socket.close();
        } catch {}
        resolve();
      });
    });
    await sleep(2000);
    if (!channelId) await resolve();
  }
}

async function main() {
  await resolve();
  // Hermes runs on its own; the polling loop below owns the timeline ordering.
  hermes().catch((error) => record({ src: "hermes", event: "fatal", error: String(error) }));

  let live = false;
  let ready = false;
  for (;;) {
    try {
      live = await pollGql();
    } catch (error) {
      record({ src: "gql", event: "error", error: String(error) });
    }
    if (!ready) {
      try {
        ready = await pollMaster();
      } catch (error) {
        record({ src: "master", event: "error", error: String(error) });
      }
    }
    await sleep(options.interval);
  }
}

main().catch((error) => {
  record({ src: "watch", event: "fatal", error: String(error) });
  process.exit(1);
});
