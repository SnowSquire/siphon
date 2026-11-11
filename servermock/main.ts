import { readFile } from "node:fs/promises";
import { createServer, type ServerResponse } from "node:http";
import { xxh3 } from "@node-rs/xxhash";
import segments from "./segments.json" with { type: "json" };

interface Segment {
  filename: string;
  length_ms: number;
  present_time_ms: number;
  sequence?: number;
  pdt?: string;
  label?: string;
  ad?: boolean;
}

interface DvrGroup {
  index: number;
  start: number;
  end: number;
  start_ms: number;
  duration_ms: number;
  segments: Segment[];
}

const segment_list: Segment[] = segments;
const segment_by_filename = new Map(segment_list.map((s) => [s.filename, s]));

const LIVE_WINDOW_MS = 30_000;
const DVR_TARGET_MS = 10_000;
// Wall-clock multiplier for the simulated broadcast. Set SPEED=1 to replay at
// real time (the RTSP pacer honors it); the default races ahead.
const SPEED = Number(process.env.SPEED ?? 8);

const session_id = String(Math.floor(Date.now() * 1000 + Math.random() * 1000));
const dvr_groups = buildDvrGroups(segment_list);

function buildDvrGroups(segments: Segment[]): DvrGroup[] {
  const groups: DvrGroup[] = [];
  let i = 0;

  while (i < segments.length) {
    const start = i;
    let duration = 0;
    do {
      duration += segments[i].length_ms;
      i += 1;
    } while (i < segments.length && duration < DVR_TARGET_MS);

    groups.push({
      index: groups.length,
      start,
      end: i,
      start_ms: segments[start].present_time_ms,
      duration_ms: duration,
      segments: segments.slice(start, i),
    });
  }

  return groups;
}

function streamDurationMs(segments: Segment[]): number {
  const last = segments.at(-1);
  return last ? last.present_time_ms + last.length_ms : 0;
}

function iso(date_ms: number): string {
  return new Date(date_ms).toISOString();
}

function mediaSequence(segments: Segment[], index: number): number {
  return segments[index]?.sequence ?? index;
}

function generateLivePlaylist(
  spark_time_ms: number,
  segments: Segment[],
  origin: string,
): string | undefined {
  const now = Date.now();
  const sim_now = spark_time_ms + (now - spark_time_ms) * SPEED;
  const content_now = Math.min(sim_now - spark_time_ms, streamDurationMs(segments));

  // Only fully produced segments belong in the main live list. The segment
  // currently being written (and the next one) are advertised as prefetches.
  const next_segment_index = segments.findIndex(
    (seg) => seg.present_time_ms + seg.length_ms > content_now,
  );

  if (next_segment_index === 0) {
    return undefined;
  }

  const available_end =
    next_segment_index === -1 ? segments.length : next_segment_index;
  const window_start_ms = Math.max(0, content_now - LIVE_WINDOW_MS);

  let first_sequence = segments.findIndex(
    (seg) => seg.present_time_ms + seg.length_ms > window_start_ms,
  );

  if (first_sequence < 0 || first_sequence >= available_end) {
    first_sequence = Math.max(0, available_end - 15);
  }

  const available_segs = segments.slice(first_sequence, available_end);
  const prefetch_segs =
    next_segment_index === -1
      ? []
      : segments.slice(next_segment_index, next_segment_index + 2);

  if (available_segs.length === 0) {
    return undefined;
  }

  const sequence = mediaSequence(segments, first_sequence);
  const session_unix = Math.floor(spark_time_ms / 1000);
  const stream_start = iso(spark_time_ms);
  const target_duration = Math.max(
    6,
    Math.ceil(Math.max(...available_segs.map((seg) => seg.length_ms)) / 1000),
  );

  let m3u8 = `#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:${target_duration}
#EXT-X-MEDIA-SEQUENCE:${sequence}
#EXT-X-TWITCH-LIVE-SEQUENCE:${sequence}
#EXT-X-TWITCH-ELAPSED-SECS:${(available_segs[0].present_time_ms / 1000).toFixed(3)}
#EXT-X-TWITCH-TOTAL-SECS:${(content_now / 1000).toFixed(3)}
#EXT-X-DATERANGE:ID="playlist-creation-${session_unix}",CLASS="timestamp",START-DATE="${stream_start}",END-ON-NEXT=YES,X-SERVER-TIME="${(spark_time_ms / 1000).toFixed(2)}"
#EXT-X-DATERANGE:ID="playlist-session-${session_unix}",CLASS="twitch-session",START-DATE="${stream_start}",END-ON-NEXT=YES,X-TV-TWITCH-SESSIONID="${session_id}"
#EXT-X-DATERANGE:ID="source-${session_unix}",CLASS="twitch-stream-source",START-DATE="${stream_start}",END-ON-NEXT=YES,X-TV-TWITCH-STREAM-SOURCE="live"
#EXT-X-DATERANGE:ID="trigger-${session_unix}",CLASS="twitch-trigger",START-DATE="${stream_start}",END-ON-NEXT=YES,X-TV-TWITCH-TRIGGER-URL="${origin}/trigger"`;

  for (const segment of available_segs) {
    m3u8 += `
#EXT-X-PROGRAM-DATE-TIME:${iso(spark_time_ms + segment.present_time_ms)}
#EXTINF:${(segment.length_ms / 1000).toFixed(3)},live
${origin}/${segment.filename}`;
  }

  if (next_segment_index === -1) {
    m3u8 += `
#EXT-X-ENDLIST
`;
    return m3u8;
  }

  for (const prefetch of prefetch_segs) {
    m3u8 += `
#EXT-X-TWITCH-PREFETCH:${origin}/${prefetch.filename}`;
  }

  return m3u8;
}

function generateDvrPlaylist(
  spark_time_ms: number,
  segments: Segment[],
  groups: DvrGroup[],
  origin: string,
): string {
  const now = Date.now();
  const sim_now = spark_time_ms + (now - spark_time_ms) * SPEED;
  const content_now = Math.min(sim_now - spark_time_ms, streamDurationMs(segments));

  // Only advertise groups that have fully landed. The in-progress group is the
  // DVR/live edge; that is what gives the DVR playlist its natural ~0-10s lag.
  const available_groups = groups.filter(
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
#ID3-EQUIV-TDTG:${new Date(now).toISOString().slice(0, 19)}
#EXT-X-PLAYLIST-TYPE:EVENT
#EXT-X-MEDIA-SEQUENCE:0
#EXT-X-TWITCH-ELAPSED-SECS:0.000
#EXT-X-TWITCH-TOTAL-SECS:${(content_now / 1000).toFixed(3)}
#EXT-X-PROGRAM-DATE-TIME:${iso(spark_time_ms)}`;

  for (const group of available_groups) {
    // The header PDT above belongs to the first segment. Twitch repeats PDT
    // for every subsequent segment.
    m3u8 +=
      group.index === 0
        ? ""
        : `\n#EXT-X-PROGRAM-DATE-TIME:${iso(spark_time_ms + group.start_ms)}`;
    m3u8 += `
#EXTINF:${(group.duration_ms / 1000).toFixed(3)},
${origin}/dvr/${group.index}.ts`;
  }

  if (content_now >= streamDurationMs(segments)) {
    m3u8 += `
#EXT-X-ENDLIST
`;
  }

  return m3u8;
}

async function concatDvrGroup(group: DvrGroup): Promise<Buffer> {
  const parts = await Promise.all(
    group.segments.map((segment) => readFile(`segments/${segment.filename}`)),
  );
  return Buffer.concat(parts);
}

const TS_PACKET_SIZE = 188;

function currentContentMs(spark_time_ms: number, segments: Segment[]): number {
  const now = Date.now();
  const sim_now = spark_time_ms + (now - spark_time_ms) * SPEED;
  return Math.min(sim_now - spark_time_ms, streamDurationMs(segments));
}

function segmentComplete(segment: Segment, content_now: number): boolean {
  return content_now >= segment.present_time_ms + segment.length_ms;
}

/**
 * Twitch serves prefetch segments while they are still being produced. The
 * response is chunked and grows over time. This mock does the same: it only
 * publishes the fraction of the file that should exist at the simulated time.
 * Constant bitrate is assumed, so byte position is proportional to time.
 */
function streamPrefetchSegment(
  res: ServerResponse,
  bytes: Buffer,
  segment: Segment,
  spark_time_ms: number,
): void {
  res.setHeader("content-type", "video/mp2t");
  res.setHeader("cache-control", "no-cache, no-store, private");
  res.setHeader("access-control-allow-origin", "*");
  res.flushHeaders();

  const total = bytes.length;
  const segment_start_ms = spark_time_ms + segment.present_time_ms;
  let sent = 0;
  let closed = false;

  const timer = setInterval(() => {
    if (closed || res.destroyed) {
      return;
    }

    const sim_now = spark_time_ms + (Date.now() - spark_time_ms) * SPEED;
    const fraction = Math.max(
      0,
      Math.min(1, (sim_now - segment_start_ms) / segment.length_ms),
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
    Math.min(1, (sim_now - segment_start_ms) / segment.length_ms),
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

function segment(res: ServerResponse, bytes: Buffer): void {
  res.setHeader("content-type", "video/mp2t");
  res.setHeader("access-control-allow-origin", "*");
  res.end(bytes);
}

function notFound(res: ServerResponse): void {
  res.statusCode = 404;
  res.end("dummy");
}

const spark_time_ms = Date.now();

const server = createServer(async (req, res) => {
  const url = new URL(req.url ?? "/", `http://${req.headers.host ?? "localhost"}`);
  const origin = `http://${url.host}`;

  try {
    if (url.pathname === "/index.m3u8") {
      const body = generateLivePlaylist(spark_time_ms, segment_list, origin);
      if (body === undefined) {
        notFound(res);
        return;
      }
      playlist(res, body);
      return;
    }

    if (url.pathname === "/index-dvr.m3u8") {
      playlist(
        res,
        generateDvrPlaylist(spark_time_ms, segment_list, dvr_groups, origin),
      );
      return;
    }

    const dvr_match = /^\/dvr\/(\d+)\.ts$/.exec(url.pathname);
    if (dvr_match) {
      const group = dvr_groups[Number(dvr_match[1])];
      if (!group) {
        notFound(res);
        return;
      }

      const bytes = await concatDvrGroup(group);
      console.log(`/dvr/${group.index}.ts ${group.segments.length} segments`);
      console.log(xxh3.xxh128(bytes).toString(16).padStart(32, "0"));
      segment(res, bytes);
      return;
    }

    const live_match = /^\/([0-9]{6}\.ts)$/.exec(url.pathname);
    if (live_match) {
      const found = segment_by_filename.get(live_match[1]);
      if (!found) {
        notFound(res);
        return;
      }

      const bytes = await readFile(`segments/${found.filename}`);
      const content_now = currentContentMs(spark_time_ms, segment_list);
      console.log(url.pathname);
      console.log(xxh3.xxh128(bytes).toString(16).padStart(32, "0"));

      if (segmentComplete(found, content_now)) {
        segment(res, bytes);
      } else {
        const produced = Math.max(
          0,
          Math.min(
            100,
            ((content_now - found.present_time_ms) / found.length_ms) * 100,
          ),
        );
        console.log(
          `prefetch ${found.filename} (${produced.toFixed(1)}% produced)`,
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
  console.log("mock server listening on http://127.0.0.1:8000");
});
