# TS demux and access units

Turning Twitch's containers into the elementary stream a sink consumes: access-unit
boundaries, codec configuration, timestamps and seek points. Implemented for the TS
source (H.264 + AAC) in `crates/daemon/src/rtsp/{ts,h264,aac}.rs`. This is not
RTSP-specific — `media-transport.md` plans to reuse `ts.rs` and the H.264/AAC
splitters for the UI media channel. fMP4 (HEVC / AV1) is design-only. RTP/RTCP, SDP
and RTSP sessions are `notes/rtsp-output.md`.

## Access units

- **PES boundaries are not access-unit boundaries.** Twitch's muxer can start a PES
  with the tail of the previous AU, followed by the next AU's AUD. The demuxer
  concatenates PES payloads and splits at AUD NALs; the PTS/DTS of the PES
  containing the AUD is assigned to that AU. Assuming one AU per PES drops frames
  and corrupts every later reference.
- H.264 arrives Annex B. NAL units are kept as ranges into the sample's bytes, so a
  stream allocates once per AU rather than once per NAL; the AUD is included.
  mediaconvert drops the delimiter before packetization, which is harmless.
- AAC arrives as ADTS frames: strip the header, keep raw frames, capture the
  AudioSpecificConfig.

| source | demux | elementary stream |
|---|---|---|
| TS, H.264 | PES → Annex B start-code split | NALUs + AUD |
| TS, AAC | ADTS frames | raw AAC + AudioSpecificConfig |
| fMP4, HEVC | `moof`/`trun` → length-prefixed NALUs (`hvcC` length size) | NALUs |
| fMP4, AV1 | `moof`/`trun` → OBUs | OBUs |
| fMP4, AAC | `moof`/`trun` → raw frames + `esds` config | raw AAC |

## Timestamps

- TS: PES PTS/DTS are already 90 kHz.
- fMP4: DTS from `tfdt` + `trun` durations, PTS = DTS + composition offsets,
  scaled from the media timescale.
- The per-track RTP base, SSRC and clock-to-NTP mapping live in the sink
  (`notes/rtsp-output.md`).

## `#EXT-X-MAP` / init handling

`#EXT-X-MAP` exists only for fMP4. It is an init segment (`ftyp` + `moov`) that
describes the tracks; it is not a media track. Parse it once per stream and per
init change:

- `moov` → `trak` → `mdia` → `mdhd`: per-track timescale, language.
- `stsd` sample entries: `hvc1`/`hev1`, `av01`, `mp4a`.
- Codec config boxes: `hvcC`, `av1C`, `esds`.
- `trex` defaults and `mvex` for sample defaults.
- Optional: `BYTERANGE` on the MAP URI (the scheduler does not handle it yet).

What stays internal: media timescales (only used to convert to the sink's clock),
`lengthSizeMinusOne` (AV1/HEVC NAL splitting), default sample durations and flags
(packetization), `av1C` config OBUs (sequence header insertion at random access
points), `esds` AudioSpecificConfig (AAC framing and the sink's config parameter).

So: **codec config goes to the sink; timing and sample-table details do not.** The
init segment itself is never streamed.

Availability:

- fMP4: the scheduler fetches `#EXT-X-MAP` as a separate init fetch, so config
  exists before the first media segment.
- TS: there is no MAP. PAT/PMT, SPS/PPS and ADTS headers are in-band in the first
  segment, so that segment has to be parsed before any sink can be configured.

Init changes on discontinuities/session changes (resolution changes, ad breaks):
new config for new sessions, in-band SPS/PPS (and the AV1 sequence header) re-sent
at the next keyframe, and AAC config changes are the least forgiving. The index
stores init records with a counter, so the init for any segment or time can be
looked up when seeking.

## Seeking

- TS: the index gives the segment byte offset; open `.segs` there and continue
  reading PES. Re-parse PAT/PMT when starting at a segment boundary.
- fMP4: find the segment covering the time, parse its `tfdt`, start from the first
  sample at/after the target (prefer a keyframe), and emit the current config
  in-band.
- Random access: start at an IDR/keyframe and send SPS/PPS (or the sequence header)
  before it.

## Decisions

- **No FFmpeg.** Codec config and SPS/sequence-header parsing replace the probe.
- **Containers cannot cross disconnect protection.** mpegts ↔ fmp4 is not
  upgradable or downgradable mid-stream (`notes.txt`).
- **Sources stay as they are:** TS = H.264 + AAC (Twitch only puts these in TS),
  fMP4 = HEVC / AV1 + AAC.

## Code to adapt (MIT)

- `mediacommon/pkg/formats/mpegts/reader.go` — TS demux for the H.264/AAC source.
- `mediacommon/pkg/codecs/{h264,h265,av1,mpeg4audio}` — SPS/avcC/hvcC/av1C/esds/ADTS.
- `mediacommon/pkg/formats/fmp4` — init/part parsing.
- Rust: `mpeg2ts-reader` (MIT/Apache) if porting the TS demux is not wanted.

## Open risks

- The TS source has no MAP, so configuration has to come from the first segment.
- Init changes mid-stream (discontinuities, ads).
- AAC AudioSpecificConfig changes are the least forgiving.
