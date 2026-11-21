import { readFile } from "node:fs/promises";
import { createServer, type ServerResponse } from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { xxh3 } from "@node-rs/xxhash";
import { startHermes } from "./hermes.ts";

// #EXT-X-MAP (fMP4) is out of scope: the capture is MPEG-TS only, so every
// advertised segment is a .ts file here.

interface Chunk {
  index: number;
  file: string;
  start_ms: number;
  duration_ms: number;
  pdt_ms: number;
}

interface Live {
  index: number;
  file: string;
  start_ms: number;
  duration_ms: number;
  sequence: number;
  chunk: number;
}

interface Manifest {
  source: string;
  pdt_origin_ms: number;
  chunks: Chunk[];
  live: Live[];
}

interface Entry {
  /** Assigned when the playlist is emitted: ads shift the live sequence. */
  sequence: number;
  start_ms: number;
  duration_ms: number;
  title: string;
  url: string;
  ad: boolean;
  disc: boolean;
  live?: Live;
}

interface DvrGroup {
  index: number;
  start_ms: number;
  duration_ms: number;
  pdt_ms: number;
  disc: boolean;
  /** Real captured chunk (default), or the live pieces to concatenate. */
  file?: string;
  pieces?: Live[];
}

// Fixtures are never committed. The capture-derived segments live under
// `captures/servermock/` (gitignored); `SEGMENTS_DIR` points the mock at any
// other build of them.
const SEGMENTS_DIR =
  process.env.SEGMENTS_DIR ??
  fileURLToPath(new URL("../captures/servermock", import.meta.url));
const manifest: Manifest = JSON.parse(
  await readFile(path.join(SEGMENTS_DIR, "manifest.json"), "utf8"),
) as Manifest;

// Wall-clock multiplier for the simulated broadcast. Set SPEED=1 to replay at
// real time (the RTSP pacer honors it); the default races ahead.
const SPEED = Number(process.env.SPEED ?? 8);
// Pretend the broadcast started this many milliseconds ago, so the debug loop
// joins mid-broadcast and exercises prefill/holes/copies.
const START_MS = Number(process.env.START_MS ?? 0);

function envNumber(name: string): number | undefined {
  const raw = process.env[name];
  if (raw === undefined || raw === "") {
    return undefined;
  }
  const value = Number(raw);
  return Number.isFinite(value) ? value : undefined;
}

const LIVE_WINDOW_MS = envNumber("LIVE_WINDOW_MS") ?? 30_000;
const AD_AT_MS = envNumber("AD_AT_MS");
const AD_LEN_MS = envNumber("AD_LEN_MS") ?? 30_000;
const AD_DISC = (process.env.AD_DISC ?? "1") !== "0";
const DVR_403_BEFORE_MS = envNumber("DVR_403_BEFORE_MS");
const DISC_AT_MS = envNumber("DISC_AT_MS");
const CHUNK_SEGS = Math.max(0, Math.floor(envNumber("CHUNK_SEGS") ?? 0));

// Both groups share one simulated timeline and one set of segment URLs, so the
// same generators serve both; only the advertised paths differ. The extra
// aliases are the per-variant URLs the daemon derives from STABLE-VARIANT-ID
// when it walks the master playlist (`chunked` is the debug landscape id).
const LIVE_PATHS = new Set([
  "/index.m3u8",
  "/chunked.m3u8",
  "/portrait.m3u8",
  "/1080p60-portrait.m3u8",
]);
const DVR_PATHS = new Set([
  "/index-dvr.m3u8",
  "/chunked-dvr.m3u8",
  "/portrait-dvr.m3u8",
  "/1080p60-portrait-dvr.m3u8",
]);

const session_id = String(Math.floor(Date.now() * 1000 + Math.random() * 1000));
const streamDuration = manifest.live.length
  ? manifest.live[manifest.live.length - 1].start_ms +
    manifest.live[manifest.live.length - 1].duration_ms
  : 0;

function basename(file: string): string {
  return file.slice(file.lastIndexOf("/") + 1);
}

function iso(date_ms: number): string {
  return new Date(date_ms).toISOString();
}

function currentContentMs(spark_time_ms: number): number {
  const now = Date.now();
  const sim_now = spark_time_ms + (now - spark_time_ms) * SPEED;
  return Math.min(sim_now - spark_time_ms, streamDuration);
}

function segmentComplete(segment: Live, content_now: number): boolean {
  return content_now >= segment.start_ms + segment.duration_ms;
}

/** All live segments, as playlist entries with a stable absolute timeline. */
const realEntries: Entry[] = manifest.live.map((live) => ({
  sequence: live.sequence,
  start_ms: live.start_ms,
  duration_ms: live.duration_ms,
  title: "live",
  url: `/${basename(live.file)}`,
  ad: false,
  disc: false,
  live,
}));
const liveByUrl = new Map(
  manifest.live.map((live) => [`/${basename(live.file)}`, live]),
);

// An ad break replaces real content in the live playlist only. The DVR
// playlist always keeps the real chunks, so a live recorder sees a gap that
// the DVR side can fill. The timeline keeps advancing during the break.
const adEntries: Entry[] = [];
const adSkipped = new Set<number>();
if (AD_AT_MS !== undefined && AD_LEN_MS > 0) {
  const ad_end_ms = AD_AT_MS + AD_LEN_MS;
  for (const live of manifest.live) {
    if (live.start_ms < ad_end_ms && live.start_ms + live.duration_ms > AD_AT_MS) {
      adSkipped.add(live.index);
    }
  }
  const ad_count = Math.ceil(AD_LEN_MS / 2000);
  const ad_base =
    realEntries.find((entry) => entry.start_ms >= AD_AT_MS)?.sequence ??
    manifest.live[0].sequence;
  for (let i = 0; i < ad_count; i++) {
    const start_ms = AD_AT_MS + i * 2000;
    adEntries.push({
      sequence: ad_base + i,
      start_ms,
      duration_ms: Math.min(2000, ad_end_ms - start_ms),
      title: `ad|break|${i}`, // Twitch ad titles contain a literal '|'.
      url: `/ad/${i}.ts`,
      ad: true,
      disc: AD_DISC && i === 0,
    });
  }
  const first_after = realEntries.find((entry) => entry.start_ms >= ad_end_ms);
  if (first_after && AD_DISC) {
    first_after.disc = true;
  }
}

/** #EXT-X-DISCONTINUITY before the first entry at/after DISC_AT_MS. */
function markDiscontinuity<T extends { start_ms: number; disc: boolean }>(
  entries: T[],
): void {
  if (DISC_AT_MS === undefined) {
    return;
  }
  const first = entries.find((entry) => entry.start_ms >= DISC_AT_MS);
  if (first) {
    first.disc = true;
  }
}
markDiscontinuity([...realEntries, ...adEntries]);

function buildDvrGroups(): DvrGroup[] {
  if (CHUNK_SEGS > 0) {
    // Re-chunking after a restart: group live pieces by count, not by the
    // captured chunk boundaries. Absolute times — not group numbers — line up.
    const groups: DvrGroup[] = [];
    for (let i = 0; i < manifest.live.length; i += CHUNK_SEGS) {
      const pieces = manifest.live.slice(i, i + CHUNK_SEGS);
      const last = pieces[pieces.length - 1];
      groups.push({
        index: groups.length,
        start_ms: pieces[0].start_ms,
        duration_ms: last.start_ms + last.duration_ms - pieces[0].start_ms,
        pdt_ms: manifest.chunks[pieces[0].chunk]?.pdt_ms ?? manifest.pdt_origin_ms,
        disc: false,
        pieces,
      });
    }
    markDiscontinuity(groups);
    return groups;
  }

  const groups: DvrGroup[] = manifest.chunks.map((chunk) => ({
    index: chunk.index,
    start_ms: chunk.start_ms,
    duration_ms: chunk.duration_ms,
    pdt_ms: chunk.pdt_ms,
    disc: false,
    file: chunk.file,
  }));
  markDiscontinuity(groups);
  return groups;
}

const dvr_groups = buildDvrGroups();

function generateLivePlaylist(
  spark_time_ms: number,
  origin: string,
): string | undefined {
  const content_now = currentContentMs(spark_time_ms);
  // Only fully produced segments belong in the main live list. The segment
  // currently being written (and the next one) are advertised as prefetches.
  const next_segment_index = manifest.live.findIndex(
    (seg) => seg.start_ms + seg.duration_ms > content_now,
  );

  if (next_segment_index === 0) {
    return undefined;
  }

  const available_end =
    next_segment_index === -1 ? manifest.live.length : next_segment_index;
  const window_start_ms = Math.max(0, content_now - LIVE_WINDOW_MS);

  let first_sequence = manifest.live.findIndex(
    (seg) => seg.start_ms + seg.duration_ms > window_start_ms,
  );
  if (first_sequence < 0 || first_sequence >= available_end) {
    first_sequence = Math.max(0, available_end - 15);
  }

  const available: Entry[] = realEntries
    .slice(first_sequence, available_end)
    .filter((entry) => !adSkipped.has(entry.live!.index));
  for (const ad of adEntries) {
    if (
      ad.start_ms + ad.duration_ms > window_start_ms &&
      ad.start_ms + ad.duration_ms <= content_now
    ) {
      available.push(ad);
    }
  }
  available.sort((a, b) => a.start_ms - b.start_ms);
  if (available.length === 0) {
    return undefined;
  }

  const prefetch = (next_segment_index === -1
    ? []
    : manifest.live.slice(next_segment_index, next_segment_index + 2)
  )
    .filter((live) => !adSkipped.has(live.index))
    .map((live) => `/${basename(live.file)}`);

  const sequence = available[0].sequence;
  const session_unix = Math.floor(spark_time_ms / 1000);
  const stream_start = iso(spark_time_ms);
  const target_duration = Math.max(
    6,
    Math.ceil(Math.max(...available.map((seg) => seg.duration_ms)) / 1000),
  );

  let m3u8 = `#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:${target_duration}
#EXT-X-MEDIA-SEQUENCE:${sequence}
#EXT-X-TWITCH-LIVE-SEQUENCE:${sequence}
#EXT-X-TWITCH-ELAPSED-SECS:${(available[0].start_ms / 1000).toFixed(3)}
#EXT-X-TWITCH-TOTAL-SECS:${(content_now / 1000).toFixed(3)}
#EXT-X-DATERANGE:ID="playlist-creation-${session_unix}",CLASS="timestamp",START-DATE="${stream_start}",END-ON-NEXT=YES,X-SERVER-TIME="${(spark_time_ms / 1000).toFixed(2)}"
#EXT-X-DATERANGE:ID="playlist-session-${session_unix}",CLASS="twitch-session",START-DATE="${stream_start}",END-ON-NEXT=YES,X-TV-TWITCH-SESSIONID="${session_id}"
#EXT-X-DATERANGE:ID="source-${session_unix}",CLASS="twitch-stream-source",START-DATE="${stream_start}",END-ON-NEXT=YES,X-TV-TWITCH-STREAM-SOURCE="live"
#EXT-X-DATERANGE:ID="trigger-${session_unix}",CLASS="twitch-trigger",START-DATE="${stream_start}",END-ON-NEXT=YES,X-TV-TWITCH-TRIGGER-URL="${origin}/trigger"`;

  let position = 0;
  for (const segment of available) {
    // Ads shift the sequence numbers: renumber the emitted window so they
    // stay strictly increasing even though the ad entries have no source
    // sequence of their own.
    segment.sequence = sequence + position++;
    if (segment.disc) {
      m3u8 += "\n#EXT-X-DISCONTINUITY";
    }
    m3u8 += `
#EXT-X-PROGRAM-DATE-TIME:${iso(spark_time_ms + segment.start_ms)}
#EXTINF:${(segment.duration_ms / 1000).toFixed(3)},${segment.title}
${origin}${segment.url}`;
  }

  if (next_segment_index === -1) {
    m3u8 += `
#EXT-X-ENDLIST
`;
    return m3u8;
  }

  for (const prefetch_url of prefetch) {
    m3u8 += `
#EXT-X-TWITCH-PREFETCH:${origin}${prefetch_url}`;
  }

  return m3u8;
}

function generateDvrPlaylist(spark_time_ms: number, origin: string): string {
  const content_now = currentContentMs(spark_time_ms);
  // Only advertise groups that have fully landed. The in-progress group is the
  // DVR/live edge; that is what gives the DVR playlist its natural ~0-10s lag.
  const available_groups = dvr_groups.filter(
    (group) => group.start_ms + group.duration_ms <= content_now,
  );
  const target_duration = Math.max(
    12,
    Math.ceil(
      Math.max(0, ...available_groups.map((group) => group.duration_ms)) / 1000,
    ),
  );

  let m3u8 = `#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:${target_duration}
#ID3-EQUIV-TDTG:${new Date().toISOString().slice(0, 19)}
#EXT-X-PLAYLIST-TYPE:EVENT
#EXT-X-MEDIA-SEQUENCE:0
#EXT-X-TWITCH-ELAPSED-SECS:0.000
#EXT-X-TWITCH-TOTAL-SECS:${(content_now / 1000).toFixed(3)}
#EXT-X-PROGRAM-DATE-TIME:${iso(spark_time_ms)}`;

  for (const group of available_groups) {
    if (group.disc) {
      m3u8 += "\n#EXT-X-DISCONTINUITY";
    }
    // The header PDT above belongs to the first segment. Twitch repeats PDT
    // for every subsequent segment.
    m3u8 += group.index === 0 ? "" : `\n#EXT-X-PROGRAM-DATE-TIME:${iso(spark_time_ms + group.start_ms)}`;
    m3u8 += `
#EXTINF:${(group.duration_ms / 1000).toFixed(3)},
${origin}/dvr/${group.index}.ts`;
  }

  if (content_now >= streamDuration) {
    m3u8 += `
#EXT-X-ENDLIST
`;
  }

  return m3u8;
}

async function dvrBytes(group: DvrGroup): Promise<Buffer> {
  if (group.file) {
    return readFile(path.join(SEGMENTS_DIR, group.file));
  }
  const parts = await Promise.all(
    (group.pieces ?? []).map((piece) => readFile(path.join(SEGMENTS_DIR, piece.file))),
  );
  return Buffer.concat(parts);
}

const TS_PACKET_SIZE = 188;

/**
 * Twitch serves prefetch segments while they are still being produced. The
 * response is chunked and grows over time. This mock does the same: it only
 * publishes the fraction of the file that should exist at the simulated time.
 * Constant bitrate is assumed, so byte position is proportional to time.
 */
function streamPrefetchSegment(
  res: ServerResponse,
  bytes: Buffer,
  segment: Live,
  spark_time_ms: number,
): void {
  res.setHeader("content-type", "video/mp2t");
  res.setHeader("cache-control", "no-cache, no-store, private");
  res.setHeader("access-control-allow-origin", "*");
  res.flushHeaders();

  const total = bytes.length;
  const segment_start_ms = spark_time_ms + segment.start_ms;
  let sent = 0;
  let closed = false;

  const timer = setInterval(() => {
    if (closed || res.destroyed) {
      return;
    }

    const sim_now = spark_time_ms + (Date.now() - spark_time_ms) * SPEED;
    const fraction = Math.max(
      0,
      Math.min(1, (sim_now - segment_start_ms) / segment.duration_ms),
    );

    const target =
      fraction >= 1
        ? total
        : Math.min(
            total,
            Math.floor((total * fraction) / TS_PACKET_SIZE) * TS_PACKET_SIZE,
          );

    if (target > sent) {
      res.write(bytes.subarray(sent, target));
      sent = target;
    }

    if (sent >= total) {
      closed = true;
      clearInterval(timer);
      res.end();
    }
  }, 10);

  res.on("close", () => {
    closed = true;
    clearInterval(timer);
  });

  // Do not wait for the first timer tick when the segment is already partly
  // produced.
  const sim_now = spark_time_ms + (Date.now() - spark_time_ms) * SPEED;
  const fraction = Math.max(
    0,
    Math.min(1, (sim_now - segment_start_ms) / segment.duration_ms),
  );
  const target = Math.floor((total * fraction) / TS_PACKET_SIZE) * TS_PACKET_SIZE;
  if (target > 0) {
    res.write(bytes.subarray(0, target));
    sent = target;
  }
}

function playlist(res: ServerResponse, body: string): void {
  res.setHeader("content-type", "application/vnd.apple.mpegurl");
  res.setHeader("cache-control", "no-cache, no-store, private");
  res.setHeader("access-control-allow-origin", "*");
  res.end(body);
}

/**
 * A two-group live master, for exercising dual-format resolution. Twitch uses
 * absolute CDN URLs here; the daemon's live-master parser also ignores lines
 * that do not start with `http`, so relative paths would drop both variants.
 */
function masterPlaylist(origin: string): string {
  return `#EXTM3U
#EXT-X-VERSION:6
#EXT-X-STREAM-INF:BANDWIDTH=8000000,RESOLUTION=1920x1080,STABLE-VARIANT-ID="chunked",IVS-NAME="1080p60",IVS-GROUPS="landscape",IVS-VARIANT-SOURCE="source"
${origin}/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=8000000,RESOLUTION=1080x1920,STABLE-VARIANT-ID="1080p60-portrait",IVS-NAME="1080p60",IVS-GROUPS="portrait",IVS-VARIANT-SOURCE="source"
${origin}/portrait.m3u8
`;
}

function segment(res: ServerResponse, bytes: Buffer): void {
  res.setHeader("content-type", "video/mp2t");
  res.setHeader("access-control-allow-origin", "*");
  res.end(bytes);
}

function notFound(res: ServerResponse): void {
  res.statusCode = 404;
  res.end("dummy");
}

const spark_time_ms = Date.now() - START_MS / SPEED;

const server = createServer(async (req, res) => {
  const url = new URL(req.url ?? "/", `http://${req.headers.host ?? "localhost"}`);
  const origin = `http://${url.host}`;

  try {
    if (url.pathname === "/master.m3u8") {
      playlist(res, masterPlaylist(origin));
      return;
    }

    if (LIVE_PATHS.has(url.pathname)) {
      const body = generateLivePlaylist(spark_time_ms, origin);
      if (body === undefined) {
        notFound(res);
        return;
      }
      playlist(res, body);
      return;
    }

    if (DVR_PATHS.has(url.pathname)) {
      playlist(res, generateDvrPlaylist(spark_time_ms, origin));
      return;
    }

    const dvr_match = /^\/dvr\/(\d+)\.ts$/.exec(url.pathname);
    if (dvr_match) {
      const group = dvr_groups[Number(dvr_match[1])];
      if (!group) {
        notFound(res);
        return;
      }
      if (
        DVR_403_BEFORE_MS !== undefined &&
        group.start_ms < DVR_403_BEFORE_MS
      ) {
        res.statusCode = 403;
        res.end("forbidden");
        return;
      }

      const bytes = await dvrBytes(group);
      console.log(
        `/dvr/${group.index}.ts ${group.file ?? `${group.pieces?.length} live pieces`}`,
      );
      console.log(xxh3.xxh128(bytes).toString(16).padStart(32, "0"));
      segment(res, bytes);
      return;
    }

    const ad_match = /^\/ad\/(\d+)\.ts$/.exec(url.pathname);
    if (ad_match) {
      // The daemon never fetches ad segments; any decodable bytes will do.
      const bytes = await readFile(path.join(SEGMENTS_DIR, manifest.live[0].file));
      segment(res, bytes);
      return;
    }

    const live_match = /^\/([0-9]{6}\.ts)$/.exec(url.pathname);
    if (live_match) {
      const found = liveByUrl.get(`/${live_match[1]}`);
      if (!found) {
        notFound(res);
        return;
      }

      const bytes = await readFile(path.join(SEGMENTS_DIR, found.file));
      const content_now = currentContentMs(spark_time_ms);
      console.log(url.pathname);
      console.log(xxh3.xxh128(bytes).toString(16).padStart(32, "0"));

      if (segmentComplete(found, content_now)) {
        segment(res, bytes);
      } else {
        const produced = Math.max(
          0,
          Math.min(
            100,
            ((content_now - found.start_ms) / found.duration_ms) * 100,
          ),
        );
        console.log(
          `prefetch ${found.file} (${produced.toFixed(1)}% produced)`,
        );
        streamPrefetchSegment(res, bytes, found, spark_time_ms);
      }
      return;
    }

    notFound(res);
  } catch (error) {
    console.error(error);
    if (!res.headersSent) {
      res.statusCode = 500;
    }
    res.end("error");
  }
});

server.listen(8000, () => {
  console.log(
    `mock server listening on http://127.0.0.1:8000 (speed ${SPEED}x, start ${START_MS} ms in, ${manifest.live.length} live segments, ${dvr_groups.length} DVR groups)`,
  );
  console.log(
    `knobs: ad=${AD_AT_MS ?? "off"}+${AD_LEN_MS} disc=${AD_DISC} ` +
      `dvr403=${DVR_403_BEFORE_MS ?? "off"} disc-at=${DISC_AT_MS ?? "off"} ` +
      `chunk-segs=${CHUNK_SEGS || "captured"} window=${LIVE_WINDOW_MS}`,
  );
});

startHermes(() => currentContentMs(spark_time_ms) >= streamDuration);
