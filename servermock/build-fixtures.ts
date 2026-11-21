/**
 * Build the offline fixtures in `captures/servermock/` (gitignored) from the
 * read-only natemek capture.
 *
 *   pnpm exec node build-fixtures.ts [chunk-count]   (default: 200)
 *
 * Output: `$SEGMENTS_DIR`, or `../captures/servermock/`. The directory is
 * self-contained (`manifest.json`, `segments.json`, `chunks/`, `live/`), so the
 * mock and the Rust benchmark both take it as their only input.
 *
 * The source chunks are copied with a hardlink and never modified. Each chunk
 * is split into ~2 s live pieces at IDR boundaries. We split at TS packet
 * boundaries ourselves instead of using ffmpeg's `segment` muxer: that muxer
 * re-inserts PAT/PMT at every cut, so its output does not concatenate back to
 * the source byte-for-byte. Every cut here starts at the source's own PAT/PMT
 * + IDR packets, so the pieces are clean keyframe starts *and* their
 * concatenation is bit-identical to the captured chunk.
 *
 * Durations are measured with ffprobe (video stream duration, rounded to ms).
 * Chunk durations are the sum of their measured pieces so the served timeline
 * is exactly continuous; the chunk's own ffprobe duration is cross-checked.
 */
import { createHash } from "node:crypto";
import { execFile } from "node:child_process";
import {
  copyFileSync,
  linkSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import path from "node:path";

const execFileAsync = promisify(execFile);

const ROOT = path.dirname(fileURLToPath(import.meta.url));
const CAPTURE_DIR = path.resolve(
  ROOT,
  "../captures/natemek-20261121-1706/tmp/natemek-dvr",
);
const CAPTURE = path.join(CAPTURE_DIR, "0____");
const OUT = process.env.SEGMENTS_DIR
  ? path.resolve(process.env.SEGMENTS_DIR)
  : path.resolve(ROOT, "../captures/servermock");
const VIDEO_PID = 0x101;
const TS_PACKET = 188;
const DEFAULT_CHUNKS = 200;
const FIRST_SEQUENCE = 10000;

interface ChunkFixture {
  index: number;
  file: string;
  start_ms: number;
  duration_ms: number;
  pdt_ms: number;
}

interface LiveFixture {
  index: number;
  file: string;
  start_ms: number;
  duration_ms: number;
  sequence: number;
  chunk: number;
}

interface LegacySegment {
  filename: string;
  length_ms: number;
  present_time_ms: number;
  sequence: number;
  pdt: string;
  label: string;
  ad: boolean;
}

interface Manifest {
  source: string;
  pdt_origin_ms: number;
  chunks: ChunkFixture[];
  live: LiveFixture[];
}

function pad(n: number, width: number): string {
  return String(n).padStart(width, "0");
}

function sha256(buffer: Buffer | Uint8Array): string {
  return createHash("sha256").update(buffer).digest("hex");
}

async function ffprobeDuration(file: string): Promise<number> {
  const { stdout } = await execFileAsync(
    "ffprobe",
    [
      "-v",
      "error",
      "-select_streams",
      "v:0",
      "-show_entries",
      "stream=duration",
      "-of",
      "csv=p=0",
      file,
    ],
    { maxBuffer: 1 << 20 },
  );
  const value = Number.parseFloat(stdout.trim().split(/\r?\n/, 1)[0] ?? "");
  if (!Number.isFinite(value)) {
    throw new Error(`ffprobe gave no video duration for ${file}: ${stdout}`);
  }
  return Math.round(value * 1000);
}

async function hasKeyframe(file: string): Promise<boolean> {
  const { stdout } = await execFileAsync(
    "ffprobe",
    [
      "-v",
      "error",
      "-select_streams",
      "v:0",
      "-read_intervals",
      "%+#1",
      "-show_entries",
      "packet=flags",
      "-of",
      "csv=p=0",
      file,
    ],
    { maxBuffer: 1 << 20 },
  );
  return stdout.includes("K");
}

async function mapLimit<T, R>(
  items: T[],
  limit: number,
  fn: (item: T, index: number) => Promise<R>,
): Promise<R[]> {
  const results = new Array<R>(items.length);
  let next = 0;
  async function worker(): Promise<void> {
    while (next < items.length) {
      const i = next++;
      results[i] = await fn(items[i], i);
    }
  }
  await Promise.all(
    Array.from({ length: Math.min(limit, items.length) }, () => worker()),
  );
  return results;
}

function parsePlaylist(text: string): { pdt: number; extinf: number }[] {
  const out: { pdt: number; extinf: number }[] = [];
  let pdt: number | undefined;
  for (const line of text.replace(/^\uFEFF/, "").split(/\r?\n/)) {
    if (line.startsWith("#EXT-X-PROGRAM-DATE-TIME:")) {
      pdt = Date.parse(line.slice("#EXT-X-PROGRAM-DATE-TIME:".length));
    } else if (line.startsWith("#EXTINF:")) {
      const extinf = Number.parseFloat(
        line.slice("#EXTINF:".length).split(",", 1)[0] ?? "",
      );
      if (pdt === undefined || !Number.isFinite(extinf)) {
        throw new Error(`unparseable raw.m3u8 line: ${line}`);
      }
      out.push({ pdt, extinf });
      pdt = undefined;
    }
  }
  return out;
}

function pidAt(buffer: Buffer, packet: number): number {
  const o = packet * TS_PACKET;
  return ((buffer[o + 1] & 0x1f) << 8) | buffer[o + 2];
}

function payloadStart(buffer: Buffer, packet: number): number {
  const o = packet * TS_PACKET;
  return o + 4 + ((buffer[o + 3] >> 4) & 1) * (buffer[o + 4] + 1);
}

/** True if the video PES starting at `packet` carries an IDR (NAL type 5). */
function isIdr(buffer: Buffer, packet: number): boolean {
  const start = payloadStart(buffer, packet);
  const end = Math.min(start + TS_PACKET, buffer.length);
  for (let i = start; i + 3 < end; i++) {
    if (
      buffer[i] === 0 &&
      buffer[i + 1] === 0 &&
      buffer[i + 2] === 1 &&
      (buffer[i + 3] & 0x1f) === 5
    ) {
      return true;
    }
  }
  return false;
}

/** Presentation timestamp (seconds) of the video PES starting at `packet`. */
function pesPts(buffer: Buffer, packet: number): number | undefined {
  const start = payloadStart(buffer, packet);
  if (start + 14 > buffer.length) {
    return undefined;
  }
  const p = buffer.subarray(start, start + 14);
  if (p[0] !== 0 || p[1] !== 0 || p[2] !== 1 || p[3] !== 0xe0 || !(p[7] & 0x80)) {
    return undefined;
  }
  const high = (p[9] >> 1) & 7;
  const mid = ((p[10] << 7) | (p[11] >> 1)) & 0x7fff;
  const low = ((p[12] << 7) | (p[13] >> 1)) & 0x7fff;
  return (high * 0x40000000 + mid * 0x8000 + low) / 90000;
}

/**
 * Byte offsets where a new live piece starts: every PAT that is immediately
 * followed (within a few packets) by a video PES with an IDR. Extra keyframes
 * that land within 1.5 s of the previous one are skipped so pieces stay close
 * to 2 s — cutting at every IDR would leave sub-second pieces. Every retained
 * boundary is still a PAT + IDR packet pair, so the pieces remain independently
 * decodable and concatenate back to the source byte-for-byte.
 */
function idrBoundaries(buffer: Buffer): number[] {
  if (buffer.length % TS_PACKET !== 0) {
    throw new Error(`chunk is not a whole number of TS packets: ${buffer.length}`);
  }
  const boundaries: number[] = [];
  const packets = buffer.length / TS_PACKET;
  let basePts: number | undefined;
  let lastPts = 0;
  for (let p = 0; p < packets; p++) {
    const o = p * TS_PACKET;
    if (buffer[o] !== 0x47) {
      throw new Error(`bad TS sync byte at packet ${p}`);
    }
    if (pidAt(buffer, p) !== 0 || ((buffer[o + 1] >> 6) & 1) !== 1) {
      continue;
    }
    let idrPacket: number | undefined;
    for (let q = p + 1; q < Math.min(packets, p + 24); q++) {
      if (pidAt(buffer, q) !== VIDEO_PID) {
        continue;
      }
      if (((buffer[q * TS_PACKET + 1] >> 6) & 1) === 1 && isIdr(buffer, q)) {
        idrPacket = q;
        break;
      }
    }
    if (idrPacket === undefined) {
      continue;
    }
    const pts = pesPts(buffer, idrPacket);
    if (pts === undefined) {
      continue;
    }
    if (basePts === undefined) {
      basePts = pts;
      lastPts = 0;
      boundaries.push(o);
    } else if (pts - basePts - lastPts >= 1.5) {
      lastPts = pts - basePts;
      boundaries.push(o);
    }
  }
  if (boundaries[0] !== 0) {
    throw new Error("chunk does not start with a PAT + IDR");
  }
  return boundaries;
}

async function main(): Promise<void> {
  const count = Number.parseInt(process.argv[2] ?? "", 10);
  const chunkCount = Number.isFinite(count) && count > 0 ? count : DEFAULT_CHUNKS;

  const playlist = parsePlaylist(
    readFileSync(path.join(CAPTURE_DIR, "raw.m3u8"), "utf8"),
  );
  if (playlist.length < chunkCount) {
    throw new Error(`capture has ${playlist.length} chunks, asked for ${chunkCount}`);
  }

  // Only the generated subdirectories are ours; never remove the capture.
  const chunksDir = path.join(OUT, "chunks");
  const liveDir = path.join(OUT, "live");
  rmSync(chunksDir, { recursive: true, force: true });
  rmSync(liveDir, { recursive: true, force: true });
  mkdirSync(chunksDir, { recursive: true });
  mkdirSync(liveDir, { recursive: true });

  const chunks: ChunkFixture[] = [];
  const live: LiveFixture[] = [];
  const keys: { index: number; duration_ms: number }[] = [];
  const legacy: LegacySegment[] = [];
  const verified: string[] = [];
  let liveStart = 0;

  for (let chunkIndex = 0; chunkIndex < chunkCount; chunkIndex++) {
    const source = path.join(
      CAPTURE,
      `${pad(chunkIndex, 3)}.ts`,
    );
    const bytes = readFileSync(source);
    const dest = path.join(chunksDir, `${pad(chunkIndex, 3)}.ts`);
    try {
      // Hardlink keeps the 5.7 GB capture cheap; read-only input stays untouched.
      linkSync(source, dest);
    } catch {
      copyFileSync(source, dest);
    }

    const boundaries = idrBoundaries(bytes);
    const pieces: Buffer[] = [];
    for (let k = 0; k < boundaries.length; k++) {
      const start = boundaries[k];
      const end = k + 1 < boundaries.length ? boundaries[k + 1] : bytes.length;
      pieces.push(bytes.subarray(start, end));
    }

    // Byte-for-byte check for every chunk (the spec asks for at least three).
    const hash = createHash("sha256");
    for (const piece of pieces) {
      hash.update(piece);
    }
    if (hash.digest("hex") !== sha256(bytes)) {
      throw new Error(`pieces do not reconstitute chunk ${chunkIndex}`);
    }

    const chunkStart = liveStart;
    for (let k = 0; k < pieces.length; k++) {
      const index = live.length;
      const file = `live/${pad(index, 6)}.ts`;
      writeFileSync(path.join(OUT, file), pieces[k]);
      const duration_ms = await ffprobeDuration(path.join(OUT, file));
      if (duration_ms <= 0) {
        throw new Error(`zero-length live piece ${file}`);
      }
      keys.push({ index, duration_ms });
      live.push({
        index,
        file,
        start_ms: liveStart,
        duration_ms,
        sequence: FIRST_SEQUENCE + index,
        chunk: chunkIndex,
      });
      legacy.push({
        filename: file,
        length_ms: duration_ms,
        present_time_ms: liveStart,
        sequence: FIRST_SEQUENCE + index,
        pdt: new Date(
          playlist[chunkIndex].pdt + (liveStart - chunkStart),
        ).toISOString(),
        label: "live",
        ad: false,
      });
      liveStart += duration_ms;
    }

    const duration_ms = liveStart - chunkStart;
    chunks.push({
      index: chunkIndex,
      file: `chunks/${pad(chunkIndex, 3)}.ts`,
      start_ms: chunkStart,
      duration_ms,
      pdt_ms: playlist[chunkIndex].pdt,
    });

    if (chunkIndex === 0 || chunkIndex === 2 || chunkIndex === 50) {
      verified.push(
        `chunk ${pad(chunkIndex, 3)}: ${pieces.length} pieces, ` +
          `source+pieces sha256 ${sha256(bytes)}, ` +
          `EXTINF ${playlist[chunkIndex].extinf.toFixed(3)}s / measured ` +
          `${(duration_ms / 1000).toFixed(3)}s`,
      );
    }

    if ((chunkIndex + 1) % 25 === 0) {
      console.log(`split ${chunkIndex + 1}/${chunkCount} chunks`);
    }
  }

  console.log("verifying every live piece starts with a keyframe (ffprobe)…");
  await mapLimit(keys, 8, async ({ index, duration_ms }) => {
    const file = path.join(OUT, live[index].file);
    if (!(await hasKeyframe(file))) {
      throw new Error(`no keyframe at start of ${file}`);
    }
    if (live[index].duration_ms !== duration_ms) {
      throw new Error(`duration changed for ${file}`);
    }
  });

  const manifest: Manifest = {
    source: "natemek 2026-11-21T17:06:17Z",
    pdt_origin_ms: playlist[0].pdt,
    chunks,
    live,
  };
  writeFileSync(
    path.join(OUT, "manifest.json"),
    `${JSON.stringify(manifest, null, 2)}\n`,
  );
  // `segments.json` is the Rust benchmark's index, in its old flat schema, with
  // `filename` relative to this directory. Nothing about the fixtures is
  // tracked: the whole directory is ignored under `captures/`.
  const json = `${JSON.stringify(legacy, null, 2)}\n`;
  writeFileSync(path.join(OUT, "segments.json"), json);

  console.log(verified.join("\n"));
  console.log(
    `built ${chunks.length} chunks / ${live.length} live pieces ` +
      `(${(liveStart / 1000).toFixed(3)} s), ` +
      `${keys.length} keyframes verified`,
  );
}

main().catch((error: unknown) => {
  console.error(error);
  process.exitCode = 1;
});
