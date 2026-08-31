import { xxh3 } from "@node-rs/xxhash";
import segments from "./segments.json" with { type: "json" };

interface Segment {
  present_time_ms: number;
  filename: string;
  length_ms: number;
}

function generatePlaylist(
  spark_time_ms: number,
  segments: Segment[],
  origin: string,
): string | null | undefined {
  const now = Date.now();
  const sim_now = spark_time_ms + (now - spark_time_ms) * 8;
  const lead_segment_index = segments.findIndex(
    (seg) => spark_time_ms + seg.present_time_ms > sim_now,
  );

  if (lead_segment_index == 0) {
    return undefined;
  }
  const first_sequence =
    lead_segment_index === -1
      ? Math.max(segments.length - 15, 0)
      : Math.max(0, lead_segment_index - 15);

  const avail_end = lead_segment_index === -1 ? segments.length : lead_segment_index;
  const avail_segs = segments.slice(first_sequence, avail_end);
  const prefetch_segs = segments.slice(lead_segment_index, lead_segment_index + 2);

  let m3u8 = `#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:6
#EXT-X-MEDIA-SEQUENCE:${first_sequence}
#EXT-X-TWITCH-LIVE-SEQUENCE:${first_sequence}
#EXT-X-TWITCH-ELAPSED-SECS:${(avail_segs[0].present_time_ms / 1000).toFixed(3)}
#EXT-X-TWITCH-TOTAL-SECS:${((sim_now - spark_time_ms) / 1000).toFixed(3)}`;

  for (const segment of avail_segs) {
    m3u8 += `
#EXT-X-PROGRAM-DATE-TIME:${new Date(spark_time_ms + segment.present_time_ms).toISOString()}
#EXTINF:${(segment.length_ms / 1000).toFixed(3)},live
${origin}/${segment.filename}`;
  }
  if (lead_segment_index == -1) {
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

async function main() {
  const spark_time_ms = Date.now();

  async function handler(req: Request): Promise<Response> {
    const url = new URL(req.url);

    if (url.pathname === "/index.m3u8") {
      return new Response(generatePlaylist(spark_time_ms, segments, `http://${url.host}`), {});
    }
    try {
      const lol = await Deno.open(`segments${url.pathname}`);
      const bytes = await Deno.readFile(`segments${url.pathname}`);
      console.log(url.pathname);
      console.log(xxh3.xxh128(bytes).toString(16).padStart(32, "0"));
      return new Response(lol.readable, {});
    } catch {
      return new Response("dummy", { status: 404 });
    }
  }
  const server = Deno.serve(handler);
}

if (import.meta.main) {
  main();
}
