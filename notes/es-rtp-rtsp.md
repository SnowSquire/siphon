# ES-RTP / RTSP output design

Status: decided direction, not implemented.

## Decisions

- **No FFmpeg.** Codec config and SPS/sequence-header parsing replace the probe.
- **No MPEG-TS muxer.** fMP4 is never remuxed to TS.
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

## Open risks

- AV1 RTP is AOM `av1-rtp-spec` v1.0, not an RFC; player support is assumed acceptable.
- TS source has no MAP, so SDP discovery has to wait for the first segment.
- Init changes mid-stream (discontinuities, ads).
- AAC AudioSpecificConfig changes are the least forgiving.
- Multiple video tracks are supported but multiply bandwidth; RTSP has no ABR switch.
