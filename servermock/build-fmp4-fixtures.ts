/**
 * Build the fMP4 fixtures in `captures/servermock-fmp4/` (gitignored) from the
 * read-only ozeniken capture.
 *
 *   pnpm run build:fmp4-fixtures
 *
 * Input:  $CAPTURE_DIR, default `../captures/ozeniken-first-segment`.
 * Output: $SEGMENTS_DIR, default `../captures/servermock-fmp4/`.
 *
 * The capture has one init segment and two fragment tracks per group: a live
 * stream of ~2 s fragments and an EVENT/DVR stream of ~10 s chunks. The DVR
 * chunks are the very same bytes, just grouped five fragments at a time, so we
 * hardlink both views and verify their byte sums with SHA-256. Nothing is
 * rewritten: init + fragment(s) is a playable fMP4 stream.
 *
 * Fragment names carry the PDT in milliseconds (`17<PDT>.m4s`). Durations are
 * the gaps between consecutive PDTs; the last live piece takes its duration
 * from the trailing empty EOS fragment, and the last DVR chunk from the
 * captured media playlist's total.
 */
import { createHash } from "node:crypto";
import {
  copyFileSync,
  existsSync,
  linkSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = path.dirname(fileURLToPath(import.meta.url));
const CAPTURE_DIR = path.resolve(
  process.env.CAPTURE_DIR ?? path.join(ROOT, "../captures/ozeniken-first-segment"),
);
const OUT = process.env.SEGMENTS_DIR
  ? path.resolve(process.env.SEGMENTS_DIR)
  : path.resolve(ROOT, "../captures/servermock-fmp4");

const GROUPS = ["landscape", "portrait"] as const;
type GroupName = (typeof GROUPS)[number];

const NAME_RE = /^(\d{13})\.(?:mp4|m4s)$/;
/**
 * The capture ends with an 8-byte empty fragment. It is an EOS marker, not
 * media: the DVR chunks do not contain it, so it must not enter the fixtures.
 * Its PDT is still the timeline end (the last real fragment's duration).
 */
const EMPTY_FRAGMENT_MAX_BYTES = 32;
const FIRST_SEQUENCE = 10000;

interface Fragment {
  pdt: number;
  name: string;
  size: number;
}

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

interface GroupFixture {
  init: string;
  live: LiveFixture[];
  chunks: ChunkFixture[];
}

interface Manifest {
  source: string;
  pdt_origin_ms: number;
  groups: Record<GroupName, GroupFixture>;
}

function pad(n: number, width: number): string {
  return String(n).padStart(width, "0");
}

function sha256(bytes: Uint8Array): string {
  return createHash("sha256").update(bytes).digest("hex");
}

/** Hardlink keeps the ~150 MB capture cheap; the read-only source is untouched. */
function linkOrCopy(source: string, dest: string): void {
  try {
    linkSync(source, dest);
  } catch {
    copyFileSync(source, dest);
  }
}

/** The captured master's attribute line for this group, if the capture has one. */
function streamInfFor(group: GroupName): string | undefined {
  const file = path.join(CAPTURE_DIR, "live-master.m3u8");
  if (!existsSync(file)) {
    return undefined;
  }
  for (const line of readFileSync(file, "utf8").split("\n")) {
    const attributes = line.trim().slice("#EXT-X-STREAM-INF:".length);
    if (!line.trim().startsWith("#EXT-X-STREAM-INF:")) {
      continue;
    }
    const resolution = /RESOLUTION=(\d+)x(\d+)/.exec(attributes);
    if (!resolution) {
      continue;
    }
    const portrait = Number(resolution[2]) > Number(resolution[1]);
    if (portrait === (group === "portrait")) {
      return attributes;
    }
  }
  return undefined;
}

function fragmentDir(group: GroupName, kind: "live" | "dvr"): string {
  const name = `ozeniken-${group}-${kind}`;
  return path.join(CAPTURE_DIR, name, "tmp", name, "0____");
}

function discover(dir: string): Fragment[] {
  // The builder is a one-shot tool, so sync fs is fine here.
  return readdirSync(dir)
    .map((name) => {
      const match = NAME_RE.exec(name);
      if (!match) {
        return undefined;
      }
      return {
        pdt: Number(match[1]),
        name,
        size: statSync(path.join(dir, name)).size,
      };
    })
    .filter((item): item is Fragment => item !== undefined)
    .sort((a, b) => a.pdt - b.pdt);
}

/** The captured DVR playlist's total, in ms (last chunk is shorter than 10 s). */
function playlistTotalMs(group: GroupName): number {
  const file = path.join(CAPTURE_DIR, `dvr-${group}-media.m3u8`);
  const text = readFileSync(file, "utf8");
  const match = /^#EXT-X-TWITCH-TOTAL-SECS:([\d.]+)$/m.exec(text);
  if (!match) {
    throw new Error(`no EXT-X-TWITCH-TOTAL-SECS in ${file}`);
  }
  return Math.round(Number(match[1]) * 1000);
}

function buildGroup(name: GroupName): GroupFixture {
  const liveDir = fragmentDir(name, "live");
  const dvrDir = fragmentDir(name, "dvr");
  const liveInit = readFileSync(path.join(liveDir, "_init.mp4"));
  const dvrInit = readFileSync(path.join(dvrDir, "_init.mp4"));
  if (sha256(liveInit) !== sha256(dvrInit)) {
    throw new Error(`${name}: live and DVR init segments differ`);
  }

  const foundLive = discover(liveDir);
  const chunks = discover(dvrDir);
  if (foundLive.length < 2 || chunks.length === 0) {
    throw new Error(`${name}: not enough fragments/chunks to build fixtures`);
  }

  const tail = foundLive[foundLive.length - 1];
  const emptyTail = tail.size <= EMPTY_FRAGMENT_MAX_BYTES;
  const live = emptyTail ? foundLive.slice(0, -1) : foundLive;
  // Twitch closes a live playlist with an entry whose body is just this 8-byte
  // `skip` box, declaring #EXTINF:1.000 for it and counting that second in its
  // own TOTAL-SECS - the DVR does not have it. Both captures agree on 1.000 s.
  const eos = emptyTail
    ? { pdt: tail.pdt, file: `${name}/eos.m4s`, duration_ms: 1000 }
    : undefined;

  if (live.length % chunks.length !== 0) {
    throw new Error(
      `${name}: ${live.length} live fragments do not divide into ${chunks.length} DVR chunks`,
    );
  }
  const perChunk = live.length / chunks.length;
  for (let i = 0; i < chunks.length; i++) {
    if (live[i * perChunk].pdt !== chunks[i].pdt) {
      throw new Error(`${name}: DVR chunk ${i} starts at a different PDT`);
    }
  }

  // The DVR playlist's total is the broadcast's true end: the live playlist's own
  // total is inflated by the empty EOS entry, and the EOS fragment's PDT can be a
  // millisecond off the chunk boundary. Both views must end on the same instant or
  // the mock never advertises the final chunk (it only lists chunks that have
  // fully landed) and the recording is silently one chunk short.
  const total = playlistTotalMs(name);
  const liveEndPdt = live[0].pdt + total;

  const liveFixtures: LiveFixture[] = live.map((fragment, i) => {
    const nextPdt = i + 1 < live.length ? live[i + 1].pdt : liveEndPdt;
    const duration_ms = nextPdt - fragment.pdt;
    if (duration_ms <= 0) {
      throw new Error(`${name}: fragment ${fragment.name} has a bad PDT gap`);
    }
    return {
      index: i,
      file: `${name}/live/${pad(i, 6)}.m4s`,
      start_ms: fragment.pdt - live[0].pdt,
      duration_ms,
      sequence: FIRST_SEQUENCE + i,
      chunk: Math.floor(i / perChunk),
    };
  });

  const chunkDurations = chunks.map((chunk, i) =>
    i + 1 < chunks.length ? chunks[i + 1].pdt - chunk.pdt : 0,
  );
  chunkDurations[chunks.length - 1] =
    total - chunkDurations.reduce((sum, ms) => sum + ms, 0);
  if (chunkDurations[chunks.length - 1] <= 0) {
    throw new Error(`${name}: DVR playlist total does not cover the chunks`);
  }
  const chunkFixtures: ChunkFixture[] = chunks.map((chunk, i) => ({
    index: i,
    file: `${name}/chunks/${pad(i, 3)}.m4s`,
    start_ms: chunk.pdt - chunks[0].pdt,
    duration_ms: chunkDurations[i],
    pdt_ms: chunk.pdt,
  }));

  // Both views must be the same media: hash each chunk against the exact
  // concatenation of its live fragments, not just the summed byte counts.
  for (let i = 0; i < chunks.length; i++) {
    const hash = createHash("sha256");
    for (let j = i * perChunk; j < (i + 1) * perChunk; j++) {
      hash.update(readFileSync(path.join(liveDir, live[j].name)));
    }
    const expected = sha256(readFileSync(path.join(dvrDir, chunks[i].name)));
    if (hash.digest("hex") !== expected) {
      throw new Error(`${name}: DVR chunk ${i} is not its live fragments`);
    }
  }

  const liveSpan = liveFixtures.reduce((sum, seg) => sum + seg.duration_ms, 0);
  const dvrSpan = chunkFixtures.reduce((sum, seg) => sum + seg.duration_ms, 0);
  if (liveSpan !== dvrSpan) {
    throw new Error(
      `${name}: live (${liveSpan} ms) and DVR (${dvrSpan} ms) spans differ`,
    );
  }

  // Only the generated subtrees are ours; never remove the capture.
  const groupOut = path.join(OUT, name);
  rmSync(path.join(groupOut, "live"), { recursive: true, force: true });
  rmSync(path.join(groupOut, "chunks"), { recursive: true, force: true });
  mkdirSync(path.join(groupOut, "live"), { recursive: true });
  mkdirSync(path.join(groupOut, "chunks"), { recursive: true });
  linkOrCopy(path.join(liveDir, "_init.mp4"), path.join(groupOut, "init.mp4"));
  if (eos) {
    linkOrCopy(path.join(liveDir, tail.name), path.join(OUT, eos.file));
  }
  live.forEach((fragment, i) =>
    linkOrCopy(
      path.join(liveDir, fragment.name),
      path.join(OUT, liveFixtures[i].file),
    ),
  );
  chunks.forEach((chunk, i) =>
    linkOrCopy(
      path.join(dvrDir, chunk.name),
      path.join(OUT, chunkFixtures[i].file),
    ),
  );

  console.log(
    `${name}: ${live.length} live fragments + ${chunks.length} DVR chunks ` +
      `(${(liveSpan / 1000).toFixed(3)} s), ${perChunk} fragments/chunk, ` +
      `init sha256 ${sha256(liveInit).slice(0, 16)}…`,
  );
  const streamInf = streamInfFor(name);
  return {
    init: `${name}/init.mp4`,
    ...(streamInf && { streamInf }),
    ...(eos && {
      eos: {
        file: eos.file,
        start_ms: eos.pdt - live[0].pdt,
        duration_ms: eos.duration_ms,
      },
    }),
    live: liveFixtures,
    chunks: chunkFixtures,
  };
}

function main(): void {
  const groups = {} as Record<GroupName, GroupFixture>;
  // A capture only has the groups that were actually downloaded; a
  // single-orientation stream is a perfectly good fixture set.
  const present = GROUPS.filter((name) =>
    existsSync(fragmentDir(name, "live")),
  );
  if (present.length === 0) {
    throw new Error(`no live fragments under ${CAPTURE_DIR}`);
  }
  for (const name of present) {
    groups[name] = buildGroup(name);
  }
  if (present.length === GROUPS.length) {
    // The two groups really are different media: H.264 vs HEVC init boxes.
    if (
      sha256(readFileSync(path.join(OUT, groups.landscape.init))) ===
      sha256(readFileSync(path.join(OUT, groups.portrait.init)))
    ) {
      throw new Error("groups have identical init bytes");
    }
  }

  const manifest: Manifest = {
    source: "ozeniken 2026-11-21T21:54:34.600Z (dual-format fMP4)",
    pdt_origin_ms: groups.landscape.chunks[0].pdt_ms,
    groups,
  };
  mkdirSync(OUT, { recursive: true });
  writeFileSync(
    path.join(OUT, "manifest.json"),
    `${JSON.stringify(manifest, null, 2)}\n`,
  );
  console.log(`built fMP4 fixtures in ${OUT}`);
}

try {
  main();
} catch (error) {
  console.error(error);
  process.exitCode = 1;
}
