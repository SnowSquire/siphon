# ES-RTP / RTSP output design

Status: implemented for the TS source (H.264 + AAC) in `crates/twitch/src/rtsp/`.
Disk output is disabled while this is the only sink. fMP4 / HEVC / AV1 are still
design-only.

## Implementation notes (TS)

- Packetizers are ports of gortsplib `rtph264` / `rtpmpeg4audio` (MIT): single
  NAL, STAP-A and FU-A for H.264; AAC-hbr AU headers for audio; 1450-byte
  payload budget.
- **PES boundaries are not access-unit boundaries.** Twitch's muxer can start a
  PES with the tail of the previous AU, followed by the next AU's AUD. The
  demuxer concatenates PES payloads and splits at AUD NALs; the PTS/DTS of the
  PES containing the AUD is assigned to that AU. Assuming one AU per PES drops
  frames and corrupts every later reference.
- The media consumer paces at 1x from a shared 90 kHz epoch; RTCP sender
  reports map that media clock (not the wall clock at send time) to NTP. RTSP
  consumers register as live-only (`ConsumerRegistration { backlog: false }`)
  so a late client starts at the edge instead of replaying the window behind the
  pacer. Prefetch messages are played immediately; their playlist timestamp is
  extrapolated from the last complete segment.
- Absolute position / live signalling: the playhead is tracked from the
  playlist's elapsed time (`Segment::timestamp_ms`, from
  `#EXT-X-TWITCH-ELAPSED-SECS`). DESCRIBE puts it in the SDP session-level
  `a=range:npt=<seconds>-` (open-ended, so players treat the stream as live and
  leave ffmpeg's duration unset), and PLAY repeats it in `Range: npt=<seconds>-`,
  which ffmpeg adds to every RTP timestamp (`range_start_offset`). Clients that
  do not rebase start times then display the absolute NPT. mpv rebases by
  default; `--rebase-start-time=no` shows the absolute position, and
  `demuxer-start-time` exposes it either way.
- RTSP `Scale` catch-up was considered and dropped: ffmpeg's RTSP demuxer (and
  therefore mpv) does not parse `Scale`, so asking a late starter to play 1.3x
  cannot work.
- UDP and interleaved-TCP transports are both supported; RTCP SRs are sent once
  per second (SR + SDES CNAME), including throughout VOD. The SR's NTP is
  `origin + media presentation time`, so the mapping is the same regardless of
  how far ahead the VOD pacer has sent; a wall-clock NTP would make ffmpeg pull
  the timeline backwards on every report.
- RTCP fields are big-endian 32-bit words. `ntp >> 32` is still a `u64` in Rust,
  and the compound-packet length is words-minus-one; both mistakes silently
  shift the RTP/NTP mapping.

## VOD / DVR seeking (implemented)

- The live playlist only has the recent window. `rtsp/vod.rs` ports twitch-dlp's
  `getM3u8.ts` to find the whole-broadcast playlist: a public archive through
  `FFZ_RecentBroadcasts` + `PlaybackAccessToken` + usher, otherwise the private
  `{domain}/{sha1("{channel}_{stream_id}_{start}")[..20]}_…/chunked/index-dvr.m3u8`
  probed across Twitch's VOD domains for start offsets `0, -1, +1`. The archive
  master comes from usher's `/vod/v2/` endpoint, which publishes the same
  `STABLE-VARIANT-ID` as the live master. It is fetched in parallel with live
  variant resolution, but only for its media playlist URLs (the signature stays
  on the master); the archive variant is chosen later by matching that stable
  variant id. Debug mode points at the mock's `/index-dvr.m3u8`. A background
  task refreshes and re-parses the playlist every 10 s (Twitch publishes a new
  group roughly that often), in parallel with the live scheduler, so a seek
  never has to fetch a playlist first.
- A normal join stays on the live scheduler spools (live edge). Prefetch
  segments are played as soon as the scheduler downloads them; their playlist
  timestamp is extrapolated from the last complete segment. A real seek
  (`PAUSE` then `PLAY` with `Range: npt=X-`) makes the media engine fetch the
  DVR segment containing X, start at the last keyframe at or before X, and play
  the DVR timeline forward.
- The DVR playlist ends about one group (~10 s) behind the live edge. The
  scheduler publishes a snapshot of its live playback window (`Vec<Segment>`,
  Readers into the same spools) to the RTSP engine. At the DVR end the engine
  replays that window from `position_ms` (the DVR playhead) up to the live edge
  at 4x, then continues with the live consumer queue. It re-reads the window as
  the scheduler refreshes it, so segments that arrive during the replay (or
  messages dropped by the lossy consumer queue during a download stall) do not
  leave holes. The gap cannot come from the queue: a live-only consumer never
  receives segments published before it joined. `push_handover` inserts by
  sequence and deduplicates, because prefetch and main-playlist messages can
  arrive out of order.
- The RTSP consumer is `ConsumerRegistration { lossy: true }`: when its bounded
  queue is full it loses one message instead of being disconnected. Normal live
  playback only needs the newest segment and the DVR handover heals holes from
  the live window, whereas closing the consumer permanently killed the output.
- A seek bumps a per-session epoch; the writer drops older-epoch packets still
  queued from the live burst. A `Session::seeking` flag additionally suppresses
  the fan-out between the PLAY response and `set_media_base`, so no packet with
  the old sequence/timestamps leaks into the new timeline. Keep the RTP
  **sequence** running across the seek: ffmpeg resets `s->seq` but not its RTP
  statistics, so restarting at 1 is a 64k jump that is rejected as
  `bad cseq 0001 expected=0001`; continuing at `max_seq + 1` is accepted.
- VOD is paced at **16x**, not 1x. The pacer fills the bounded per-session
  outbound queue (4096 RTP packets); when it is full the media engine waits for
  capacity instead of dropping the client, so the queue acts as the player's
  buffer and the send rate settles at the client's drain rate. A 2x player
  therefore stays ahead instead of starving, and mpv's duration (which it grows
  from the highest demuxed PTS) grows with the live edge. The queue also hides
  the next segment's on-demand fetch; fetching it *before* the first emit made
  every seek wait for two full segments.
- A seek `PLAY` deliberately omits `RTP-Info`. The header seeds ffmpeg's
  `base_timestamp`, but the media engine has not reset the RTP epoch yet, so the
  value would be the old live timestamp; ffmpeg then rejects every post-seek
  packet until the next `PLAY` (the "click pause/play to make it go" symptom).
  Without it, the first packet establishes the base.
- While serving VOD, RTCP sender reports are sent with the media-clock NTP
  described above, so the mapping stays consistent even though the pacer sends
  ahead of wall time. For live, the same formula reads as real time and removes
  B-frame timestamp jitter: the SR's RTP value is the last packet actually
  sent, never a separately computed DTS playhead.
- VOD playback aborts and returns to live once no RTSP client is connected, so
  a closed player cannot keep downloading the archive.
- Seek packets start a new RTP/RTCP epoch: the shared `base_pts` and both
  `Track` mappings are reset, and `Range: npt=X-` becomes ffmpeg's
  `range_start_offset`. `media_delta` is signed: only a delta near ±2^33 is a
  wrap, because the small negative audio-vs-video offset at a keyframe is real.
  Both tracks anchor on the same minimum PES PTS of the first segment played.
- The SDP advertises a finite `a=range:npt=0-<live playhead>` so players enable
  seeking; the initial PLAY is still treated as live (only PAUSE+PLAY is a
  seek). mpv's watch-later resume is a real PAUSE+PLAY and is honored; disable
  it client-side if a fixed RTSP URL should always start live.
- `SegmentMessage::Seek` is the control message from the RTSP plane to the media
  engine. It travels through the existing ordered consumer channel because
  kanal async receivers are not safe in `select`.

## Decisions

- **No FFmpeg.** Codec config and SPS/sequence-header parsing replace the probe.
- **No MPEG-TS muxer.** fMP4 is never remuxed to TS.
- **Containers cannot cross disconnect protection.** mpegts ↔ fmp4 is not
  upgradable or downgradable mid-stream (`notes.txt`).
- **RTSP with ES-RTP only.** One RTP track per codec; no TS-over-RTP.
- **Sources stay as they are:**
  - TS source = H.264 + AAC (Twitch only puts these in TS).
  - fMP4 source = HEVC / AV1 + AAC.
- **RTCP Sender Reports** for A/V sync and clock mapping.
- **SDP is built from the init segment** (fMP4) or from in-band config (TS).
- Seek via the existing `.index` + per-track RTP timestamp handling.

## Source → RTP

| source | demux | RTP payload |
|---|---|---|
| TS, H.264 | PES → Annex B start-code split → NALUs | RFC 6184 (`rtph264`) |
| TS, AAC | ADTS frames → strip header → raw AAC + AudioSpecificConfig | RFC 3640 (`rtpmpeg4audio`) |
| fMP4, HEVC | `moof`/`trun` → length-prefixed NALUs (`hvcC` length size) | RFC 7798 (`rtph265`) |
| fMP4, AV1 | `moof`/`trun` → OBUs | AV1 RTP spec (`rtpav1`) |
| fMP4, AAC | `moof`/`trun` → raw frames + `esds` config | RFC 3640 (`rtpmpeg4audio`) |

No container is emitted on the wire. The init/MAP bytes are consumed by the server; only
codec configuration reaches the client, via SDP.

## `#EXT-X-MAP` / init handling

`#EXT-X-MAP` exists only for fMP4. It is an init segment (`ftyp` + `moov`) that
describes the tracks; it is not a media track and is never sent over RTP.

Parse it once per stream / per init change:

- `moov` → `trak` → `mdia` → `mdhd`: per-track timescale, language.
- `stsd` sample entries: `hvc1`/`hev1`, `av01`, `mp4a`.
- Codec config boxes: `hvcC`, `av1C`, `esds`.
- `trex` defaults and `mvex` for sample defaults.
- Optional: `BYTERANGE` on the MAP URI (the current scheduler doesn't handle it yet).

What goes into SDP:

| track | SDP |
|---|---|
| H.264 | `a=rtpmap:96 H264/90000` + `a=fmtp:96 profile-level-id=<hex>;sprop-parameter-sets=<sps_b64>,<pps_b64>;packetization-mode=1` |
| H.265 | `a=rtpmap:97 H265/90000` + `a=fmtp:97 sprop-vps=<b64>;sprop-sps=<b64>;sprop-pps=<b64>` |
| AV1 | `a=rtpmap:98 AV1/90000` + `a=fmtp:98 profile=<n>;level-idx=<n>;tier=<n>` |
| AAC | `a=rtpmap:99 MPEG4-GENERIC/<rate>/<ch>` + `a=fmtp:99 streamtype=5;profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config=<hex AudioSpecificConfig>` |

What stays internal (not SDP):

- media timescales → used to convert sample times to the 90 kHz RTP clock,
- `lengthSizeMinusOne` (AVCC/HVCC) → NAL splitting,
- default sample durations/flags → packetization,
- `av1C` config OBUs → sequence header insertion at random access points,
- `esds` AudioSpecificConfig → AAC RTP framing and SDP `config`.

So: **codec config goes in SDP; timing and sample-table details do not.** The init
segment itself is not streamed.

### When the init is available

- fMP4: the scheduler already fetches `#EXT-X-MAP` as a separate init fetch, so the
  SDP can be built before the first media segment and before a client `DESCRIBE`.
- TS: there is no MAP. PAT/PMT, SPS/PPS and ADTS headers are in-band in the first
  segment. The server must parse the first segment to build the SDP; an RTSP client
  that connects before that has to wait (or get an error). This is the TS counterpart
  of the MAP fetch and should be done at stream start.

### Init changes

Twitch can change the init on discontinuities/session changes (resolution changes, ad
breaks). Options:

- Build a new SDP for new sessions.
- Keep existing sessions alive with in-band config updates: H.264/HEVC SPS/PPS and the
  AV1 sequence header can be re-sent at the next keyframe; AAC config changes are harder
  and may require a new session.
- The index already stores init records with a counter, so the server can look up the
  init for any segment/time when seeking.

## Timestamps

- Video RTP clock is 90 kHz; audio clock is the sample rate (48 kHz for Twitch AAC,
  but read it from `mdhd`/`esds`/ADTS).
- fMP4: DTS from `tfdt` + `trun` durations, PTS = DTS + composition offsets, scaled
  from the media timescale to 90 kHz.
- TS: PES PTS/DTS are already 90 kHz.
- Each track has its own base RTP timestamp and SSRC. RTCP SR (NTP ↔ RTP) every ~5 s
  gives the receiver the mapping needed for A/V sync.
- On seek, rebase the RTP timestamp (or continue); the SR carries the mapping either way.

## Seeking

- RTSP `PLAY` with `Range: npt=<time>`.
- TS: the index gives the segment byte offset; open `.segs` there and continue
  emitting PES/RTP. Re-parse PAT/PMT if starting at a segment boundary.
- fMP4: find the segment covering the time, parse its `tfdt`, start from the first
  sample at/after the target (prefer a keyframe), emit the current config in-band.
- Random access: start at an IDR/keyframe; send SPS/PPS/sequence header before it.

## Tracks

- One `m=` section per track; multiple video tracks are legal (separate `SETUP`s,
  transports, SSRCs, payload types).
- Multiple formats inside one `m=` line are alternatives, not simultaneous tracks.
- A Twitch variant is one video + one audio; audio-only variants are a single `m=audio`.

## Code to adapt (MIT)

- `gortsplib/pkg/format/rtp{h264,h265,av1,mpeg4audio}` — RTP packetizers.
- `gortsplib/pkg/rtpsender`, `pkg/rtptime` — RTCP SR and timestamp mapping.
- `gortsplib` server subset (or `shiguredo_rtsp`, Apache-2.0, sans-I/O) — RTSP control.
- `mediacommon/pkg/formats/fmp4` — init/part parsing.
- `mediacommon/pkg/formats/mpegts/reader.go` — TS demux for the H.264/AAC source.
- `mediacommon/pkg/codecs/{h264,h265,av1,mpeg4audio}` — SPS/avcC/hvcC/av1C/esds/ADTS.
- Rust: `mpeg2ts-reader` (MIT/Apache) if porting the TS demux is not wanted.

## Dropped from the plan

- FFmpeg (`ffmpeg-next`, the probe, the blocking thread).
- The fMP4 → MPEG-TS muxer (mediabunny / mediacommon `mpegts/writer.go`).
- AV1-in-TS.
- TS-over-RTP.

## Client memory / IO (uninitialized reads)

The chunks live in a zero-initialized slot arena (next section), so every byte
of the payload buffer is always a valid `u8`. That removes the old
`writable_slice().assume_init_mut()` -> `Read::read(&mut [u8])` UB without any
nightly features: `writable_slice()` is `&mut [u8]`, and `Read`/`AsyncRead` can
write into it directly.

If the one-time arena zeroing ever becomes measurable, the uninit path is:

- `#![feature(read_buf, core_io_borrowed_buf)]` on nightly (both are needed;
  `core_io_borrowed_buf` alone does not give you `Read::read_buf`).
- `Read::read_buf(&mut BorrowedCursor)` with
  `BorrowedBuf::from(&mut [MaybeUninit<u8>])`. The default `read_buf` impl
  zero-initializes via `ensure_init`; an impl that overrides it writes into the
  cursor without initializing.
- rustls: enable `rustls = { features = ["read_buf"] }`. Its build script sets
  `cfg(read_buf)` only on nightly, and `Reader::read_buf` appends plaintext
  directly into the cursor (`cursor.append(...)`), no memset.
- compio plaintext: `AsyncRead::read<B: IoBufMut>` already takes uninit buffers;
  the driver writes through raw pointers. `IoBuf` is `'static`, and `IoBufMut`
  is implemented for `Uninit<T>`/`Slice<T>` but not `Arc`, so wrap a slot in a
  `ChunkBuf { id: SlotId, offset: usize }` owner that implements `IoBufMut`.
- TLS boundary: `compio-tls` only exposes futures
  `AsyncRead::poll_read(&mut [u8])` and does not expose the underlying
  `rustls::ClientConnection`, so `read_buf` requires driving rustls directly
  (`read_tls`, `process_new_packets`, `reader().read_buf(cursor)`).
- `BorrowedBuf`/`read_buf` is still unstable (rust issue #117693) as of 1.99.

With the zeroed arena none of this is required; it is recorded as the fallback,
not the plan.

## Slot chunk allocator

Replaces `Arc<Chunk>` (`Chunk::new`, `OnceLock<next>`, drop-chain) with a global
arena of fixed slots. The compio reactor is single-threaded, so this needs no
atomics and no locks: `Cell`/`NonNull` are enough.

- 32 slots per arena, 32-bit free bitmap.
- Chunk size `192_512` = `188 * 1024` = 47 pages. The old 16 KiB factor was a
  ureq buffering artefact; 188 is the only real constraint (TS alignment), and
  `192_512` is the smallest size that is also 4 KiB page-aligned.
- Arena data = `32 * 192_512 = 6_160_384 B` (~5.9 MiB), allocated with
  `alloc_zeroed` so every slot byte is initialized from the start.
- `SlotMeta { refs: Cell<u32>, next: Cell<Option<SlotId>> }` in a separate array
  from `data`, so the bitmap/metadata scan does not touch the payload.
- Intrusive refcount per slot. A slice is live while held by the writer (tail),
  any reader, or the predecessor's `next` link. The `next` link must hold a
  reference, otherwise a lagging reader can follow a recycled slot.
- Free path: refcount `1 -> 0`; take `next` and release it iteratively (never
  recursively, a long chain would overflow the stack); set the bitmap bit
  (`debug_assert` it was clear, so double-free is impossible); if the arena
  bitmap is full, unlink and deallocate the arena.
- Allocation: `trailing_zeros` on the free bitmap; if empty, push a new arena
  onto the chain.
- Guarantees: `SlotRef` (readers) and `SlotMut` (writer) pin a slot and deref to
  `&[u8]`/`&mut [u8]`. No mutex is needed; the stream frontier provides the
  visibility edge.
- Global allocator shared by all channels in the daemon. Arenas are reclaimed
  when fully free; the free sets advance in stream order so the head is usually
  the one that empties. A doubly-linked list makes unlink O(1).
- Buffer depth: 32 slots is ~6 MiB, roughly 8 s at the measured ~0.75 MB/s while
  the live prefetch is in flight. Arena growth covers catch-up and slow readers.
- No blocking consumers: FFmpeg is dropped from this plan, and RTSP/RTP
  packetization plus compio-fs run on the reactor. If a blocking consumer is
  ever added, the only sound pattern is: pin on the reactor, pass a `Send`
  `SlotView`, and release only on an explicit completion message. compio's
  `spawn_blocking` does **not** cancel the task when the future is dropped, so a
  guard held by the cancellable future would be a use-after-free.

## Open risks

- AV1 RTP is AOM `av1-rtp-spec` v1.0, not an RFC; player support is assumed acceptable.
- TS source has no MAP, so SDP discovery has to wait for the first segment.
- Init changes mid-stream (discontinuities, ads).
- AAC AudioSpecificConfig changes are the least forgiving.
- Multiple video tracks are supported but multiply bandwidth; RTSP has no ABR switch.
