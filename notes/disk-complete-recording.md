# Disk output: complete-broadcast recording

Status: implemented in `crates/twitch/src/disk.rs` (v4 index).  `--output` on
both `play` and `daemon`; `index.hexpat` mirrors the format.  `disk.rs` replaces
the disabled v3 implementation wholesale.

Goal: with `--output <dir>`, produce one **contiguous, ad-free archive** of a
broadcast at `<dir>/<stream_id>/segs` + `<dir>/<stream_id>/index` (debug uses
stream id `0`): the part before the client joined, every ad break, and any live
downtime are all filled from the DVR playlist.  The recording has no gaps and no
timestamp jumps when the DVR is available.

## Two sources, one file

- **Live** — the scheduler's consumer fan-out, recorded from the moment the
  client starts.  It is the safety net: if no DVR URL ever appears, the live
  recording *is* the archive.  It is split into **runs** (`live.NNN`) at every
  gap in the live playlist (ads, reconnects).
- **DVR** — the archive playlist (`rtsp/vod.rs`, to be renamed `rtsp/dvr.rs`),
  which chunks the *source* stream: it has content for ad spans the live
  playlist replaced with ads.  The DVR owns the canonical file: it writes every
  span the live path cannot record and leaves a **hole** where each live run
  sits; each run is copied into its hole in the background.

The DVR download never waits on a copy, and the live recorder never waits on the
DVR; only the canonical assembly waits, and it waits for a hole, not for the
network.

## Files

| path | role |
| --- | --- |
| `segs`, `index` | canonical; the DVR file, holes filled from runs |
| `live.NNN.segs`, `live.NNN.index` | one pair per live run |

A run pair is deleted only after its `Run` record is durable, so the source of
every hole is always on disk while the hole is pending.  `--keep-runs` keeps the
pairs instead of deleting them.

## Flow

1. **Live first.** The disk consumer registers immediately with `backlog: true`
   (the last 18 segments, ~36 s) and records **run 0** from `live_start`.  There
   is no discovery wait, no in-memory hold and no arena reservation.
2. **Discovery in parallel.** A DVR URL is looked for until
   `max(now + 15 s, broadcast_start + 30 s)`.
   - No URL: close run 0, **promote** it to `segs`/`index` (close, rename,
     reopen) and write the live stream directly into the canonical from then on.
     No assembly happens in this mode, so a gap (an ad, a long reconnect) cannot
     be filled: the index simply has no records for it.  A DVR URL appearing
     after the promotion is an anomaly: notify.
   - URL found: the canonical is the DVR file from now on.
3. **Pre-fill.** The DVR downloader writes `[0, chunk boundary covering
   live_start)` into the canonical, keeping the chunk that crosses `live_start`
   whole.  The live recorder keeps recording run 0.
4. **Runs become holes.** The DVR's position jumps over each live run: it writes
   the whole chunks for the spans not covered by a run (the pre-fill and each ad
   gap), skips the run, and continues at the chunk boundary at the run's end.
   The canonical gets a byte hole of the run's *uncovered middle* in both `segs`
   and `index`; the DVR writes everything after it at the reserved offset.
5. **Ads.** An ad during the pre-fill ends the current run, the DVR downloads the
   whole chunks covering the ad (content the live playlist never had), the next
   run starts, and the same hole/copy happens for it.  An ad is just a gap: no
   special message and no special record.
6. **Handoff.** When the DVR's position reaches the live playlist's newest
   segment, it tells the live recorder to stop: the last run closes, its hole is
   copied, and the DVR follows the publication edge from then on.  The live
   recorder never resumes on its own; a DVR failure after the handoff ends the
   recording (see *Errors*).
7. **Background copies.** Each run's uncovered middle is copied into its hole
   while the DVR keeps downloading.  Copies are independent of network
   progress and independent of each other.

## Holes and trimming

The DVR writes **whole chunks only**: a chunk that overlaps a run's edge is
written in full.  Therefore:

- a hole starts and ends on DVR chunk boundaries;
- the run's leading and trailing records outside those boundaries are trimmed
  and are supplied by the DVR's chunks (the DVR downloads them anyway);
- the copy is exactly the run's uncovered middle, `run_len - leading - trailing`,
  i.e. up to two chunks shorter than the run — which is also why it is fast;
- nothing is duplicated and nothing is lost: the DVR supplies the edges, the live
  run supplies the middle.

The hole's size is computed from the run's index (kept record byte lengths) when
the run closes, i.e. when the trailing chunk is chosen, and it is reserved in
both the `segs` file (bytes) and the `index` (records) before the DVR writes
anything past it.

## Index v4

`index.hexpat` mirrors this.  Backwards compatibility is deliberately dropped: a
v3 index (or any header mismatch) on the target stream is a **fatal** error and
the program exits.

```
Header { magic: "OZENIKEN", version: u32 = 4, stream_id: u64 }
```

| tag | record |
| --- | --- |
| 0 | `Segment { source: u8, sequence: u64, elapsed_ms: u32, pdt_ms: u64, duration_ms: u32, byte_offset: u64, byte_length: u32, hash: u128, flags: u8 }` |
| 1 | `Init { source: u8, counter: u64, byte_offset: u64, byte_length: u32, hash: u128 }` |
| 2 | `Discontinuity { width: u16, height: u16, rate_num: u16, rate_denom: u16 }` |
| 3 | `Run { run: u32, dst_segs_offset: u64, segs_len: u64, source_segs_offset: u64, source_index_offset: u64, record_len: u32, complete: u8 }` |

`Run.run` is the one field beyond the original table: without it a resume has no
way to name the `live.NNN` pair that backs a pending hole.  It sits immediately
after the tag, so the rest of the layout is unchanged.

- `source`: `0` live, `1` DVR.  Each record stores **its own source's sequence
  number**.  There is no mapping between DVR and live numbering — the
  inconsistencies make it unrecoverable, so nothing tries.
- `pdt_ms` is the absolute `EXT-X-PROGRAM-DATE-TIME` (Unix epoch ms) and is the
  alignment key.  `elapsed_ms` is the playlist's broadcast-relative time (what
  NPT uses).  Both are stored because neither alone is enough: the reader needs
  NPT, and the merge needs an absolute clock.
- `duration_ms` is the record's length, so `end = start + duration` is exact on
  resume and for any reader.
- `hash` is xxh3-128 over the record's bytes; resume uses it to detect a
  half-written tail.
- `flags` bit 0 = muted (Twitch mutes DVR/VOD segments *after* a broadcast ends,
  never during; a resume can race the mute detection, so record it rather than
  skip it.  Detecting it is a TODO; the bit is clear until then).
- No `Gap` record: a successful run/hole leaves no gap, and an unfilled hole is
  visible as a timestamp jump in the index (no records) with zeros in `segs`.
- No live↔DVR sequence field: nothing can populate it and nothing consumes it.

DVR records are ~10 s chunks, live records ~2 s segments; mixed granularity is
accepted, `timestamp`/`pdt` is the ordering key.

### `Run` record

Sits at the start of the reserved index hole and makes the assembly
crash-recoverable without a progress cursor:

- `dst_segs_offset` — where the copied middle goes; new DVR bytes start at
  `dst_segs_offset + segs_len`.
- `segs_len` — bytes in the run's uncovered middle.
- `source_segs_offset` — source offset in `live.NNN.segs`.
- `source_index_offset` — source offset in `live.NNN.index` of the first kept
  record, so the reserved records can be regenerated without rescanning.
- `record_len` — bytes reserved in the canonical index for the rebased records.
- `complete` — 0 pending, 1 done, at a fixed offset for a one-byte flip.

While pending, a reader stops at the marker.  The copy writes the rebased,
renumbered run records into the index hole and the middle bytes into the segs
hole; on completion the segs are fsynced, `complete` is flipped, the index is
fsynced and the run pair deleted (unless `--keep-runs`).

## Alignment and the sanity check

- The trim is by `pdt_ms` (a lexer event, below).  PDT is the only absolute clock
  and survives playlist re-chunking; `elapsed_ms` is per-playlist and must not be
  used to align the two sources.
- `pdt - elapsed` is the broadcast's wall-clock origin and should be the same
  constant on both sides.  At every hole boundary compare the DVR chunk's offset
  with the live record's offset; if they differ by more than **1 s**, do not
  merge that hole: notify, leave it zero-filled (sparse), and write no records
  for it, so the archive has an honest hole rather than a silent splice.
- Content hashing at the boundary is unnecessary: the DVR writes the whole
  chunk, so no bytes are ever copied over bytes from the other source.  Segment
  hashes remain for resume integrity only.

## Resume

1. Parse the header.  Magic/version/stream_id mismatch → fatal, exit.
2. Repair a torn tail: truncate the index to the last complete record, truncate
   `segs` to `max(byte_offset + byte_length)` over the surviving records (not to
   the file length, which runs past hole reservations), then read the last
   record's bytes back and verify its xxh3; walk back while it fails.
3. Pending `Run` → redo it in place: write the rebased records into the index
   hole and the middle bytes into the segs hole, fsync, flip `complete`, delete
   the pair (unless `--keep-runs`).  Later DVR records are left untouched (the
   hole is below them), so a crash during a background copy costs only that
   copy.  A complete `Run` (even an abandoned, zero-filled one) is final; a
   missing source pair turns a pending marker into exactly that abandoned hole.
4. Complete `Run` → delete any leftover run pair unless `--keep-runs`.
5. Decide the mode from the index:
   - DVR records and a DVR URL now → continue the DVR from the last record
     (re-chunked or not, matching by `pdt_ms`); recover any pending runs.
   - Only live records (a promoted live recording) and a DVR URL now → **rename
     the canonical pair to a run** (`live.NNN`), start a fresh canonical, prefill
     `[0, live_start)`, and treat the renamed pair as that run: this is the
     live → DVR switch.
   - DVR records and no DVR URL → stop and notify.  DVR → live is not automatic:
     that would mean a wrong assumption about Twitch, and the files are merged
     by hand.
   - Only live records and no DVR URL → continue recording live (new run, or the
     canonical directly if this run was already promoted).

## Errors and notifications

- **Fatal** (exit): index magic/version/stream id mismatch, or an index that
  truncation cannot repair.  `spawn_disk_output` returns a handle that `main`
  selects against `scheduler.main()`; a fatal disk error returns from `main`.
- **Non-fatal** (log, stop the disk subsystem, keep RTSP): any write error
  including a full disk, the DVR fetch, a copy.  No retries for `ENOSPC`.
- Early DVR chunks can 403 when dropout protection fires often.  Skip them,
  notify, leave the prefill hole at the start; recent chunks still work, which
  is what makes the run/hole model viable.  A missing gap chunk at the end is
  the same: leave the hole, notify.
- `notify()` is for anything that contradicts the assumptions about Twitch:
  skipped 403s, a PDT/elapsed mismatch, a DVR URL after promotion, a DVR failure
  mid-pre-fill, a DVR gone on resume, and the disk subsystem stopping.  It must
  not block the reactor: hand it to a channel/task.  `--notify <url>` on `play`;
  daemon reads it from its config.

## Daemon

- `--output` applies to daemon streams: one folder per broadcast, several
  recorders at once.
- A recording keeps the stream entry alive (its URLs stored), and counts toward
  the `--max-streams` accounting, but it does **not** keep the live manifest
  poller alive by itself: the poller may pause once the pre-fill is finished and
  nobody is watching.  It may **not** pause while the pre-fill is incomplete, so a
  no-DVR recording keeps the poller alive for its whole life (and that is the
  only way it can keep recording).
- After the handoff the recording is DVR-only, so a paused poller costs the
  archive nothing.

## Supporting changes

- `playlist.rs`: `Event::ProgramDateTime(u64)` (RFC 3339 → epoch ms, hand-rolled,
  no new dependency); a pending PDT is applied to the next media item.  Prefetch
  tags carry no PDT.
- `scheduler.rs`: `Segment.pdt_ms` and `Segment.duration_ms`; the disk writer
  detects a gap from the timestamp jump (no `Ad` message: an ad is just a gap
  with content in the DVR).
- `spool.rs`/`arena.rs`: the per-spool slot cap (`SEGMENT_SLOTS_MAX = 64` ≈
  12.3 MB) is below a 12 Mbps 10 s DVR chunk (~15 MB); give the DVR spool its own
  cap (~96 slots) and its own `ArenaSet` for the reorder buffer.  Arenas are not
  globally capped, so nothing else changes.
- `disk.rs`: re-enabled and rewritten around the two sources, runs, holes, the
  `Run` marker, the copies and the resume repair.
- `main.rs`/`daemon.rs`: `--output`, `--notify`, `--keep-runs`; the disk handle
  and the daemon wiring.
- `rtsp/vod.rs` → `rtsp/dvr.rs`; `VodTimeline`/`VodSegment` renamed.  The refresh
  loop keeps running (the RTSP seek path still uses it until the disk-backed
  reader exists).
- `index.hexpat`: v4.
- `servermock`: a `START_MS` knob so the debug loop can join mid-broadcast and
  exercise prefill/holes/copies.  The mock (Node) sets `Content-Length`;
  Twitch's live CDN mostly does not, so tests must not depend on it.

## Testing

- Unit: v4 round-trip; the hole arithmetic (`run_len - leading - trailing`), the
  trim by `pdt_ms`, the copy transform (rebase `byte_offset`, renumber `Init`
  counters), `Run` recovery.
- **Mock integration:** `SPEED=1`, join at a non-zero offset (`START_MS`),
  kill and restart the client; assert the canonical index is contiguous and
  monotonic, every pending `Run` was completed, and the run pairs are gone (or
  kept with `--keep-runs`).  `NO_DVR=1` exercises the promotion path.  The
  implementation was checked with `START_MS=120000` / `SPEED=16` for prefill +
  hole copies, a mid-prefill kill + restart, a promoted-live kill + restart
  with the DVR back on (the live → DVR switch), and daemon mode through
  `ffprobe`.
- `ffprobe` the `segs`; `cargo +nightly miri test -p twitch-hls-client2` for the
  arena/spool changes.

## Deferred

- **Disk-backed RTSP seeking**: read the index/`segs` instead of fetching from
  Twitch; then the DVR timeline refresh can stop when disk output is on.  The
  format is designed for it (timestamp-ordered records, exact byte ranges, stop
  at a pending `Run`).
- fMP4/HEVC/AV1 archives: the record format is container-agnostic and `Init`
  records copy generically; the chunking assumption is TS.
- DVR → live automatic fallback (deliberately manual).

## Rejected alternatives

- **Merging at the DVR's published edge**: re-downloads the live window.
- **Truncating the live tail to avoid a copy**: loses recorded bytes; the whole
  DVR chunk is written and the run's edges are trimmed instead.
- **Content hashing at a hole boundary**: nothing overlaps, so there is nothing
  to compare; the elapsed/PDT check catches a wrong alignment.
- **Chained part files**: every reader would have to follow the chain; one
  canonical file is the point.
- **A `Gap` record**: no gaps are produced; an unfilled hole is a timestamp jump.
- **Mapping DVR sequence numbers to live ones**: the numbering is inconsistent;
  store each source's own number and never convert.
- **An `Ad` message**: the timestamp jump already identifies the gap, and the DVR
  supplies its content.
- **Waiting for DVR discovery before recording**: the live recording must start
  immediately in case no URL ever arrives.
