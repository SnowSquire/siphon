# Disk output: complete-broadcast recording

Status: design.  `disk.rs` is still disabled (`// mod disk;` in `main.rs`).  This
document replaces the v3 on-disk format and the plain append-only writer with a
writer that produces the **whole broadcast** in one `segs`/`index` pair: the DVR
prefix downloaded in the background, the live tail recorded as it airs, and the
two spliced together in place.  Every span the live path cannot record — the
part before the client started, ad breaks, client downtime, restarts — is filled
from the DVR, so the finished recording has no gaps and no timestamp jumps.

`--output <dir>` enables it.  There is no separate switch, no storage cap and no
configurable catch-up window.  The recording lives at
`<dir>/<stream_id>/` (one folder per broadcast; the stream id is globally
unique, debug mode uses `0`).

## Files

| path | role |
| --- | --- |
| `<dir>/<id>/segs` | canonical byte stream |
| `<dir>/<id>/index` | canonical record stream |
| `<dir>/<id>/live.segs` | live bytes recorded while a fill is in flight |
| `<dir>/<id>/live.index` | live records for the same window |

The canonical pair always "owns" the name: the DVR downloader is the canonical
writer whenever a DVR playlist exists, so the recording is never renamed from
live to DVR after the fact.  `live.*` is the merge source and is deleted once
its content has been copied into the canonical pair and the splice marker is
durable.  Between splices the live writer appends straight to the canonical
pair; on a hole it switches back to a fresh `live.*`.

## Index v4

`index.hexpat` mirrors this; keep both in sync.  Backwards compatibility is
deliberate: a v3 file (or any header mismatch) on the target stream is a **fatal**
error and the program exits.  Switching software versions mid-download is not
supported.

```
Header { magic: "OZENIKEN", version: u32 = 4, stream_id: u64 }
```

Records are a `u8` tag followed by packed little-endian fields.

| tag | record |
| --- | --- |
| 0 | `Segment { source: u8, sequence: u64, timestamp_ms: u32, duration_ms: u32, byte_offset: u64, byte_length: u32, hash: u128, flags: u8 }` |
| 1 | `Init { source: u8, counter: u64, byte_offset: u64, byte_length: u32, hash: u128 }` |
| 2 | `Discontinuity { width: u16, height: u16, rate_num: u16, rate_denom: u16 }` |
| 3 | `Splice { dvr_segs_offset: u64, segs_len: u64, live_segs_offset: u64, live_index_offset: u64, record_len: u32, complete: u8 }` |

- `source`: `0` = live playlist numbering, `1` = DVR playlist numbering.  The
  two numberings are unrelated (the DVR starts its own sequence and never sees
  ads), so `sequence` is only meaningful inside its source's domain.
- `timestamp_ms` is the authoritative key: broadcast-relative presentation
  start, from `EXT-X-TWITCH-ELAPSED-SECS` on both sides.  Broadcasts are capped
  at 48 h, so `u32` milliseconds is safe (50 days of headroom).
- `duration_ms` is stored because resume needs the exact end of the last record
  and any reader needs it; `timestamp + duration` is the record's end.
- `hash` is xxh3-128 over the record's bytes; resume uses it to detect a
  half-written tail.
- `flags` bit 0 = muted.
- No `Gap` record: a successful recording has no gaps at all.  Every span the
  live path cannot record — ad breaks, client downtime, restarts — is filled
  from the DVR, so the timestamp sequence stays continuous.  The only exception
  is a run with no DVR at all (discovery timeout), where there is nothing to
  fill an ad span with; that is a degraded recording, not a different format.
- No live-sequence field on DVR records: the live playlist that covered the
  prefix is gone, so the mapping cannot be recovered.  Nothing consumes it.

Segment sizes are not uniform across a splice: DVR records are ~10 s chunks
(one record per chunk), live records are ~2 s playlist segments.  Mixed
granularity is accepted; the timestamp is the ordering key.

### Splice record

The `Splice` record sits at the start of the reserved index gap and makes the
merge crash-recoverable without a progress cursor:

- `dvr_segs_offset` – target offset in the canonical `segs`; the copied bytes go
  exactly there, and new live bytes start at `dvr_segs_offset + segs_len`.
- `segs_len` – bytes to copy out of `live.segs`.
- `live_segs_offset` – source offset in `live.segs`.
- `live_index_offset` – source offset in `live.index` (past its header), so the
  index gap can be regenerated without rescanning the whole live index.
- `record_len` – bytes reserved in the canonical index for the rebased records;
  the writer's next record starts after it.
- `complete` – 0 pending, 1 done.  At a fixed offset so completion is a one-byte
  positional write after both gaps are fsynced.

Readers stop at a pending marker: everything after a pending `Splice` is either
zero-filled reservation or data whose validity is not yet established.

### Muted flag

Twitch mutes DVR (and VOD) segments **after** a broadcast ends, never during it,
so a prefix downloaded live is unmuted while a resume against a finished
broadcast may race the mute detection.  A muted source is recorded rather than
skipped, with `flags` bit 0 set.  Detecting muting (playlist marker vs. probing)
is a TODO; until then the bit is always clear.  The field exists so the archive
is honest when it does get populated.

## What the scheduler already gives us

Three existing behaviours decide how much hole-filling machinery is needed:

- Segments are pushed to consumers **at dispatch**, in playlist order, with the
  `Reader` doing the waiting (`dispatch_main`).  Delivery order is therefore
  the playlist order, not the download-completion order.
- `segment_tail` is the next *undispatched* sequence.  A short reconnect is a
  transport error (window preserved), so any segment still inside the 30 s
  window is re-dispatched and re-fetched while its CDN URL is still alive
  (live segment URLs live ~2 min).  A hole only exists when the playlist front
  has moved past the missed segments.
- `reset()` (non-transport error) sets `segment_tail = 0` and re-dispatches the
  whole window, so the disk writer can receive **duplicate** timestamps.

Consequences for the writer: track `last_end`; drop `timestamp_ms < last_end`;
append on equality; start a fill on `timestamp_ms > last_end` (client downtime,
a restart, or an ad break — all three are spans the live path cannot record and
the DVR can supply).

`Segment` gains `duration_ms` (known at dispatch) so `last_end` is exact.

## Ad breaks

Ads occupy live playlist time but are never part of the archive.  The DVR chunks
the source stream, which keeps running through an ad break, so the DVR **does**
have content for that span — the content the live playlist replaced with the ad.
The disk writer cannot record the ad (ad segments are skipped in `reload` before
dispatch) and must not record a hole either: the recording has to stay
continuous, with no timestamp jump.

An ad break is therefore a fill like any other, with the DVR supplying the
missing content.  Each ad segment emits:

```rust
SegmentMessage::Ad { start_ms: u64, duration_ms: u32 }
```

Per segment, not per break: the end of a break is not known until content
resumes.  The first `Ad` moves the writer into the fill state at `last_end` (the
end of the last recorded content record), so the DVR download for the ad span
starts while the break is still playing instead of after it.  `run_start` only
exists once the first post-ad content record arrives; the splice waits for that
record and for the DVR to cover it.  Ad bytes are never written; the recording
gets the DVR's content for the span.  Other consumers can ignore the message
(the RTSP playhead could use it later).

A run without a DVR has no content for the span, so the jump remains — the one
case where the archive is not continuous.  There is still no marker for it.

## Startup: wait for DVR discovery

The DVR playlist is published some time after a broadcast starts (~30 s in
practice).  The disk output does not write until it knows which of the two
outcomes applies:

- deadline `max(now + 15 s, broadcast_start + 30 s)`, where
  `broadcast_start = now - first_segment.timestamp_ms`;
- DVR found → canonical pair is the DVR target, live writes go to `live.*`;
- timeout → canonical pair *is* the live recording and the DVR is ignored for
  the rest of the run (no rename later); ad spans then remain as timestamp
  jumps, since nothing can supply their content.

Waiting costs nothing: the disk consumer registers first and drains its bounded
queue during the wait, holding each arriving segment's spool `Reader` — no heap
copy — until the decision, then writes them in order to the chosen pair.  The
18-segment backlog (~36 s) covers the delay, but the wait needs a bigger arena
budget: backlog (~36 s) plus up to 30 s of wait is ~66 s, i.e. ~80 MB at 10 Mb/s.
`ARENAS_PER_BROADCAST` goes from 6 to 16 (~98 MB per broadcast, allocated lazily,
`ArenaSet` drops arenas when empty).

When disk output is enabled, DVR discovery stops retrying after that deadline;
when it is disabled it keeps retrying forever, because the RTSP seek path still
depends on the timeline.

A bad `--output` directory (or a failure to create it) logs and disables disk
output; RTSP is unaffected.

## DVR download

- Same media playlist Twitch publishes for the archive (the SHA-1 path and the
  public archive path yield the same media playlist; the live `STABLE-VARIANT-ID`
  picks it from the archive master).
- Chunks are ~10 s groups of the source stream's segments, concatenated.  The
  live playlist shows the same segments except across ad breaks, where it shows
  ads instead — which is exactly why the DVR can fill them.
- Fetch from the playlist start on a fresh recording, otherwise from the chunk
  boundary described below, in order, and stop once the DVR's covered end `E`
  reaches `run_start`: the timestamp of the first live record written after
  entering the fill (not the first record in `live.segs`, which may be a
  previous run's tail that this fill supersedes).
- `DVR_DOWNLOAD_TASKS` starts at 1 and is benchmarked upward (parallel CloudFront
  fetches sustain ~70 Mb/s); the reorder buffer is the existing `spool`, in a
  **dedicated** `ArenaSet` sized per task (~18 MB for a 10 s chunk at 12 Mb/s).
  The live set is untouched.
- `Content-Length` is not used to precompute offsets (live segments are chunked;
  DVR chunks are static files, but in-order writing is simpler and equally fast
  on a local disk).  A short body is not rejected.
- A chunk becomes one `Segment` record with `source = 1`, the DVR playlist
  sequence, and an xxh3 hash over the bytes as they are written.
- 404 mid-prefix (the broadcaster deleted the VOD; it pulls the segments off
  CloudFront) stops the DVR download and leaves the canonical pair as a
  truncated prefix plus `live.*` as the tail, **separate**.  No marker, no
  repair: continuing to write output is what matters.

## Fill state machine

Two states, cycled for the initial pre-roll, every ad break, every restart gap,
and every mid-run hole:

- **A — live appends to the canonical pair.**  The steady state after a splice.
- **B — canonical frozen, live appends to `live.*`.**  Entered at startup (with
  `live.*` empty or holding a previous run's tail), on the first `Ad` message of
  a break, or when a hole is detected.  A fresh recording is the degenerate
  case: the canonical is empty, `C = 0`, there is no overlap to verify, and the
  fill downloads from the first chunk.

`B → A` when the DVR's covered end `E >= run_start`, where `run_start` is the
timestamp of the first live record written after entering state B.  Using the
file's first record would be wrong when a previous run's tail was appended:
that tail is superseded by the DVR and must not be copied.

### Leading edge: verify, don't truncate

The canonical ends at `L` (timestamp), `L_off` (segs length).  The DVR chunk
covering it starts at `C < L`, because chunks are 10 s and live records are 2 s.
The overlap is a whole number of already-recorded live segments, so the chunk's
prefix duplicates bytes we already have.

Rather than truncating the canonical tail and re-downloading it:

1. `C` = largest DVR chunk start `<= L`; if `C == L` the overlap is zero and the
   chunk is appended whole.
2. `C_off` = `byte_offset` of the first canonical record with `timestamp >= C`.
   `C` is a source-segment boundary, so it is also a record boundary: `C_off`
   is exact, `overlap = L_off - C_off` is a whole number of records, and it is a
   multiple of 188 (TS packet size).
3. Download chunk `C` whole.  Stream-compare the canonical's bytes
   `[C_off, L_off)` against the chunk's first `overlap` bytes, in lockstep with
   consuming the chunk into the writer, xxh3 over the full range.  Hashing the
   whole overlap costs nothing next to the reads.
4. Match → append `chunk[overlap..]` at `L_off`.  The first record of the fill is
   therefore a *partial* chunk record: `timestamp_ms = L` (its bytes start
   there), `sequence` = the chunk's own sequence, `byte_length` = the suffix.
   The timeline stays monotonic and the byte stream stays packet-aligned.
5. Mismatch → **abort the fill** and leave the canonical and `live.*` separate.
   Never truncate on a guess: a mismatch means the segmentation assumption is
   wrong and a merge would be corrupt.

### Trailing edge: splice and the reserved gap

At `E >= run_start` the live tail accumulated during the fill must be appended
after the DVR's end.  The live pair is frozen at that moment (`segs` length
`live_len`), and the suffix that still matters is the contiguous run of live
records starting at the first record with `timestamp >= E`: the copy stops at
the first timestamp break in that run, because anything after a break is another
span the DVR has to supply, not something to append now.

1. Write `Splice { complete: 0 }` at the canonical index end and **fsync the
   index**, before a single byte of new live data is written past the gap.
2. Read `live.index` from `live_index_offset`, parse, keep records with
   `timestamp >= E`, rebase `byte_offset += dvr_segs_offset - live_segs_offset`
   (and `Init.byte_offset` likewise), renumber `Init.counter` continuing the
   canonical's counter, and write the result into the reserved index gap.  The
   live index is small (~47 B per 2 s, ~85 KB per hour), so this is synchronous.
3. Spawn the segs copy: `live.segs[live_segs_offset .. live_len]` →
   `dvr_segs_offset`, chunked.  It runs at full speed in its own task because a
   GB-sized copy inside the writer loop would stall live writes past the arena
   budget.
4. New live records and bytes are written after the gap (`dvr_segs_offset +
   segs_len`) while the copy runs.  Both writers use explicit positional
   offsets, so there is no shared cursor and no lock.
5. On completion: fsync the canonical `segs`, flip `complete`, fsync the index,
   delete `live.*`.  The live pair is never deleted before `complete` is
   durable, which is what makes recovery cheap.

### Why the copy exists

Splicing at `run_start` — the earliest point at which the prefix is complete —
means the copy is the live tail recorded during the entire catch-up.  Waiting
for the DVR to reach the live edge would make the copy small, but it re-downloads
content the live path already fetched; that trade was rejected.  Catch-up is a
local copy, not bandwidth, and the DVR sustains ~70 Mb/s in parallel.  Peak disk
usage is `DVR + live` until the copy completes and `live.*` is deleted.

## Resume

1. Open the canonical pair.  Header magic/version/stream_id mismatch → fatal,
   exit the program.
2. Truncate a torn tail: parse records until one is short or malformed, truncate
   the index to the last complete record, truncate `segs` to
   `max(byte_offset + byte_length)` over the surviving records (not to the file
   length, which can run past the records while a splice gap is pending), then
   read the last record's bytes back and verify its xxh3; walk back while it
   fails.
3. `Splice` with `complete = 0` → redo: recompute the index gap from
   `live_index_offset` (the init counter base comes from the canonical records
   before the marker) and re-copy
   `live.segs[live_segs_offset .. +segs_len]`; fsync, flip, delete `live.*`.
   Full-gap redo, not a progress cursor: the live pair is frozen and only deleted
   after `complete`, so redoing is always possible and idempotent.
4. `Splice` with `complete = 1` → the canonical is authoritative; delete any
   stray `live.*`.
5. Start state B: the fill begins at the chunk boundary `<=` the canonical's end.
   A previous run's `live.*`, if any, is appended to rather than recreated; the
   `timestamp >= E` filter naturally drops the records the DVR now supersedes,
   and the file is still there if the DVR download fails.

## Errors

- **Fatal** (exit): index magic/version/stream id mismatch, or an index that
  truncation cannot repair.  `spawn_disk_output` returns a handle that `main`
  selects against `scheduler.main()`; a fatal disk error returns from `main`
  after dropping the runtime.
- **Non-fatal** (log, stop the disk subsystem, drop the consumer): any write
  error including a full disk, the DVR fetch, the gap copy.  RTSP keeps serving.
  There is no storage cap and no preflight.
- DVR 404 mid-prefix → stop the DVR, leave the pairs separate.
- Discovery timeout → live-only canonical pair.

## Supporting changes

- `arena.rs`: `ArenaSet::new(slots)`; the arena array becomes a `Vec`, the spool
  table takes the slot count.  The live set's budget grows
  (`ARENAS_PER_BROADCAST = 16`, so `MAX_ARENAS = 64`, `MAX_SLOTS = 2048`); the
  DVR reorder buffer gets its own set sized from `DVR_DOWNLOAD_TASKS`.
- `scheduler.rs`: `Segment.duration_ms`; `SegmentMessage::Ad`; emit it at the ad
  skip in `reload` so the writer can start filling an ad span while it plays.
  Prefetch messages keep being delivered (the RTSP path plays them) and the disk
  writer keeps ignoring them: a prefetched segment is re-dispatched as a main
  segment, so recording it would duplicate it.
- `disk.rs`: re-enabled, rewritten around the two-state machine, the DVR fetch
  task, the copy task, the marker codec and the resume repair.
- `main.rs`: `--output <dir>`; the disk handle; the module re-enabled behind it.
- `rtsp/vod.rs` → `rtsp/dvr.rs`, `VodTimeline`/`VodSegment` renamed.  The refresh
  loop stays while the disk-backed seek reader does not exist yet.
- `index.hexpat`: v4.
- `servermock`: a start-offset knob (`START_MS`) so the debug loop can join
  mid-broadcast and exercise the fill/splice.  Note the mock's Node server sets
  `Content-Length`; Twitch's live CDN mostly does not, so tests must not lean on
  it.

## Testing

- Unit: v4 record round-trip; the copy transform (rebase, init renumbering,
  partial-chunk suffix, `timestamp = L`); the marker's field offsets; the
  overlap arithmetic (`C`, `C_off`, `overlap`).
- Mock integration: `SPEED=1`, join at a non-zero offset, kill and restart the
  client; assert the canonical index is contiguous and monotonic, that the
  splice's gap was filled, and that `live.*` is gone.  `ffprobe` the `segs`.
- `cargo +nightly miri test -p twitch-hls-client2` for the arena changes.

## Deferred

- **Disk-backed RTSP seeking.**  Once the recording exists, seeking should read
  the index and `segs` instead of fetching from Twitch, and the DVR timeline
  refresh can stop when disk is on.  The format is designed for it: records are
  timestamp-ordered, byte ranges are exact, and a reader stops at a pending
  splice or a torn tail.  Separate change.
- **Live-window replay for sub-window holes.**  Not needed today: the scheduler
  already re-dispatches anything still inside the 30 s window.  A scheduler
  "replay retained segments from timestamp" would only help if the 30 s window
  ever closed faster than the reconnect.
- fMP4/HEVC/AV1 prefixes.  The record format is container-agnostic and `Init`
  records are copied generically; the chunking assumption is TS.

## Rejected alternatives

- **Chained part files** (an index record naming the next file): no copy and no
  gap, but every reader — including the external splitter this archive exists
  for — would have to follow the chain.  One `segs` was a hard requirement.
- **Splicing at the live edge** (wait for the DVR to reach the playhead): turns
  the copy into a few segments, but re-downloads content the live path already
  fetched.
- **Truncating the tail at the leading edge**: loses up to a chunk of recording,
  and cannot detect a wrong segmentation assumption.  Verify-and-skip does
  neither.
- **An in-place progress cursor on the splice**: only needed when the copy runs
  while new live data is written past it *and* the source may disappear.  Here
  the source is frozen and deleted only after `complete`, so a full redo is
  simpler and equally safe.
- **A heap buffer for the pre-decision segments**: copies data and adds a second
  memory pool.  Holding the `Reader`s and growing the arena does the same job
  with no copy.
- **One `AdGap` span per break**: the end of a break is unknown when it starts.
- **Storing the live sequence on DVR records**: unrecoverable for the prefix and
  consumed by nothing.
