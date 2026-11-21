# RTSP / RTP output

The sink: RTSP control, RTP packetization, RTCP clock mapping, SDP and the DVR
seek plane. Implemented for the TS source (H.264 + AAC) in
`crates/daemon/src/rtsp/`; fMP4 / HEVC / AV1 are design-only.

RTSP is the external harness (`ffplay`, `ffprobe`) and nothing else's transport:
the planned UI channel carries elementary streams over the admin RPC
(`notes/media-transport.md`), so this is slated to become a compile-time optional
feature. Keep it correct; do not invest in its hot path. Demuxing, codec config
and timestamps are in `notes/ts-demux.md`.

## Packetization

- Ports of gortsplib `rtph264` / `rtpmpeg4audio` (MIT): single NAL, STAP-A and
  FU-A for H.264; AAC-hbr AU headers for audio; 1450-byte payload budget.
- One RTP track per codec; no TS-over-RTP. Payloads are RFC 6184 (H.264), RFC
  7798 (H.265), the AV1 RTP spec (AOM, not an RFC) and RFC 3640 (AAC). Only codec
  configuration reaches the client, via SDP; no container is ever on the wire.

## Clock and RTCP

- The media consumer paces at 1x from a shared 90 kHz epoch. RTCP sender reports
  map that media clock — not the wall clock at send time — to NTP. The SR's NTP is
  `origin + media presentation time`, so the mapping is the same however far the
  VOD pacer has sent ahead; a wall-clock NTP makes ffmpeg pull the timeline
  backwards on every report.
- For live, the same formula reads as real time and removes B-frame timestamp
  jitter: the SR's RTP value is the last packet actually sent, never a separately
  computed DTS playhead.
- RTCP fields are big-endian 32-bit words. `ntp >> 32` is still a `u64` in Rust,
  and the compound-packet length is words-minus-one; both mistakes silently shift
  the RTP/NTP mapping.
- Sender reports go once per second (SR + SDES CNAME), including throughout VOD.
- UDP and interleaved-TCP transports are both supported.

## Live signalling

- RTSP consumers register as live-only (`ConsumerRegistration { backlog: false }`),
  so a late client starts at the edge instead of replaying the window behind the
  pacer. Prefetch messages are played immediately; their playlist timestamp is
  extrapolated from the last complete segment.
- Absolute position comes from the playlist's elapsed time
  (`Segment::timestamp_ms`, from `#EXT-X-TWITCH-ELAPSED-SECS`). DESCRIBE puts it in
  the SDP session-level `a=range:npt=<seconds>-` (open-ended, so players treat the
  stream as live and leave ffmpeg's duration unset), and PLAY repeats it in
  `Range: npt=<seconds>-`, which ffmpeg adds to every RTP timestamp
  (`range_start_offset`). Clients that do not rebase start times then display the
  absolute NPT; mpv rebases by default, `--rebase-start-time=no` shows it.
- RTSP `Scale` catch-up was considered and dropped: ffmpeg's RTSP demuxer (and
  therefore mpv) does not parse `Scale`, so a late starter cannot be played at
  1.3x.

## VOD / DVR seeking (implemented)

- The live playlist only has the recent window. `rtsp/dvr.rs` ports twitch-dlp's
  `getM3u8.ts` to find the whole-broadcast playlist: a public archive through
  `FFZ_RecentBroadcasts` + `PlaybackAccessToken` + usher, otherwise the private
  `{domain}/{sha1("{channel}_{stream_id}_{start}")[..20]}_…/chunked/index-dvr.m3u8`
  probed across Twitch's VOD domains for start offsets `0, -1, +1`. The archive
  master comes from usher's `/vod/v2/` endpoint, which publishes the same
  `STABLE-VARIANT-ID` as the live master. It is fetched in parallel with live
  variant resolution, but only for its media playlist URLs (the signature stays on
  the master); the archive variant is chosen later by matching that stable variant
  id. Debug mode points at the mock's `/index-dvr.m3u8`. A background task
  refreshes and re-parses the playlist every 10 s (Twitch publishes a new group
  roughly that often), in parallel with the live scheduler, so a seek never has to
  fetch a playlist first.
- A normal join stays on the live scheduler spools (live edge). Prefetch segments
  are played as soon as the scheduler downloads them; their playlist timestamp is
  extrapolated from the last complete segment. A real seek (`PAUSE` then `PLAY`
  with `Range: npt=X-`) makes the media engine fetch the DVR segment containing X,
  start at the last keyframe at or before X, and play the DVR timeline forward.
- The DVR playlist ends about one group (~10 s) behind the live edge. The
  scheduler publishes a snapshot of its live playback window (`Vec<Segment>`,
  Readers into the same spools) to the RTSP engine. At the DVR end the engine
  replays that window from `position_ms` (the DVR playhead) up to the live edge at
  4x, then continues with the live consumer queue. It re-reads the window as the
  scheduler refreshes it, so segments that arrive during the replay (or messages
  dropped by the lossy consumer queue during a download stall) do not leave holes.
  The gap cannot come from the queue: a live-only consumer never receives segments
  published before it joined. `push_handover` inserts by sequence and
  deduplicates, because prefetch and main-playlist messages can arrive out of
  order.
- The RTSP consumer is `ConsumerRegistration { lossy: true }`: when its bounded
  queue is full it loses one message instead of being disconnected. Normal live
  playback only needs the newest segment, and the DVR handover heals holes from the
  live window, whereas closing the consumer permanently killed the output.
- A seek bumps a per-session epoch; the writer drops older-epoch packets still
  queued from the live burst. A `Session::seeking` flag additionally suppresses the
  fan-out between the PLAY response and `set_media_base`, so no packet with the old
  sequence/timestamps leaks into the new timeline. Keep the RTP **sequence**
  running across the seek: ffmpeg resets `s->seq` but not its RTP statistics, so
  restarting at 1 is a 64k jump rejected as `bad cseq 0001 expected=0001`;
  continuing at `max_seq + 1` is accepted.
- VOD is paced at **16x**, not 1x. The pacer fills the bounded per-session outbound
  queue (4096 RTP packets); when it is full the media engine waits for capacity
  instead of dropping the client, so the queue acts as the player's buffer and the
  send rate settles at the client's drain rate. A 2x player therefore stays ahead
  instead of starving, and mpv's duration (which it grows from the highest demuxed
  PTS) grows with the live edge. The queue also hides the next segment's on-demand
  fetch; fetching it *before* the first emit made every seek wait for two full
  segments.
- A seek `PLAY` deliberately omits `RTP-Info`. The header seeds ffmpeg's
  `base_timestamp`, but the media engine has not reset the RTP epoch yet, so the
  value would be the old live timestamp; ffmpeg then rejects every post-seek packet
  until the next `PLAY` (the "click pause/play to make it go" symptom). Without it,
  the first packet establishes the base.
- Seek packets start a new RTP/RTCP epoch: the shared `base_pts` and both `Track`
  mappings are reset, and `Range: npt=X-` becomes ffmpeg's `range_start_offset`.
  `media_delta` is signed: only a delta near ±2^33 is a wrap, because the small
  negative audio-vs-video offset at a keyframe is real. Both tracks anchor on the
  same minimum PES PTS of the first segment played.
- The SDP advertises a finite `a=range:npt=0-<live playhead>` so players enable
  seeking; the initial PLAY is still treated as live (only PAUSE+PLAY is a seek).
  mpv's watch-later resume is a real PAUSE+PLAY and is honored; disable it
  client-side if a fixed RTSP URL should always start live.
- `SegmentMessage::Seek` is the control message from the RTSP plane to the media
  engine. It travels through the existing ordered consumer channel because kanal
  async receivers are not safe in `select`.
- VOD playback aborts and returns to live once no RTSP client is connected, so a
  closed player cannot keep downloading the archive.

## SDP

Codec configuration only; timing and sample-table details stay inside the demuxer
(`notes/ts-demux.md`).

| track | SDP |
|---|---|
| H.264 | `a=rtpmap:96 H264/90000` + `a=fmtp:96 profile-level-id=<hex>;sprop-parameter-sets=<sps_b64>,<pps_b64>;packetization-mode=1` |
| H.265 | `a=rtpmap:97 H265/90000` + `a=fmtp:97 sprop-vps=<b64>;sprop-sps=<b64>;sprop-pps=<b64>` |
| AV1 | `a=rtpmap:98 AV1/90000` + `a=fmtp:98 profile=<n>;level-idx=<n>;tier=<n>` |
| AAC | `a=rtpmap:99 MPEG4-GENERIC/<rate>/<ch>` + `a=fmtp:99 streamtype=5;profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config=<hex AudioSpecificConfig>` |

When the config is available:

- fMP4: the scheduler already fetches `#EXT-X-MAP` as a separate init fetch, so
  the SDP can be built before the first media segment and before a client
  `DESCRIBE`.
- TS: there is no MAP. PAT/PMT, SPS/PPS and ADTS headers are in-band in the first
  segment, so the server must parse that segment before it can answer DESCRIBE; a
  client that connects earlier has to wait (or get an error).

Init changes on discontinuities/session changes (resolution changes, ad breaks)
need a new SDP for new sessions; existing sessions keep going on in-band config
updates, with AAC changes the least forgiving. `notes/ts-demux.md` owns the
parsing, the in-band re-send rules and the index lookup.

## Tracks

- One `m=` section per track; multiple video tracks are legal (separate `SETUP`s,
  transports, SSRCs, payload types).
- Multiple formats inside one `m=` line are alternatives, not simultaneous tracks.
- A Twitch variant is one video + one audio; audio-only variants are a single
  `m=audio`.

## Decisions

- **RTSP with ES-RTP only.** One RTP track per codec; no TS-over-RTP.
- **No MPEG-TS muxer.** fMP4 is never remuxed to TS.
- **RTCP Sender Reports** for A/V sync and clock mapping.
- **SDP is built from the init segment** (fMP4) or from in-band config (TS).
- Seek via the existing `.index` + per-track RTP timestamp handling.

## Code to adapt (MIT)

- `gortsplib/pkg/format/rtp{h264,h265,av1,mpeg4audio}` — RTP packetizers.
- `gortsplib/pkg/rtpsender`, `pkg/rtptime` — RTCP SR and timestamp mapping.
- `gortsplib` server subset (or `shiguredo_rtsp`, Apache-2.0, sans-I/O) — RTSP
  control.

## Open risks

- AV1 RTP is AOM `av1-rtp-spec` v1.0, not an RFC; player support is assumed
  acceptable.
- The TS source has no MAP, so SDP discovery has to wait for the first segment.
- Init changes mid-stream (discontinuities, ads).
- AAC AudioSpecificConfig changes are the least forgiving.
- Multiple video tracks are supported but multiply bandwidth; RTSP has no ABR
  switch.
