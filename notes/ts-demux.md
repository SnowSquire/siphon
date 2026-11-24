# TS demux and access units

Turning Twitch's containers into the elementary stream a sink consumes: access-unit
boundaries, codec configuration, timestamps and seek points. Two demuxers live in
`crates/daemon/src/media/`: `ts.rs` for the MPEG-TS source (H.264 + AAC) and `fmp4.rs`
for the fMP4 source (H.264 or H.265 + AAC), which is the only path H.265 can take.
The elementary stream leaves through Riffle (`notes/riffle.md`).

## Access units

- **PES boundaries are not access-unit boundaries.** Twitch's muxer can start a PES
  with the tail of the previous AU, followed by the next AU's AUD. The demuxer
  concatenates PES payloads and splits at AUD NALs; the PTS/DTS of the PES
  containing the AUD is assigned to that AU. Assuming one AU per PES drops frames
  and corrupts every later reference.
- H.264 arrives Annex B. One segment's samples live in a `SampleStore`: one
  growing byte buffer for every access unit and AAC frame, one flat list of NAL
  ranges, and one list of `Sample` records into both. The demuxer appends; a
  `Sample` is `Copy` and carries `(pts, dts, video, keyframe, byte range, nal
  range)`. NAL ranges are relative to the sample's bytes, so packetization takes
  `(data, nals)` unchanged.
- That is one allocation per segment, reused, instead of one `Vec` per access
  unit, one `Vec` per NAL list and one `Vec` per AAC frame. Allocating the access
  unit with `mem::take` also leaked a buffer per AU; the store has no such path.
- mediaconvert drops the access unit delimiter before packetization. Dropping it
  from a NAL range is a bump of the range's start, not a `Vec::remove(0)`.
- Video and audio bytes are kept in separate buffers. Twitch interleaves the two
  PES streams, so one shared buffer put audio bytes inside the access unit still
  being assembled; `audio_between_pes_packets_stays_out_of_the_access_unit` is
  the regression test.
- Measured on this box (`cargo test -p daemon --release -- --ignored --nocapture
  throughput`): 2006 s of 1080p60 media went from 0.653 s to 0.485 s
  (3072x -> 4136x realtime on one core); demux 0.453 s -> 0.343 s. The RTP
  packet count stayed exactly 1 142 090, which is the identity check that the
  demuxed bytes did not change.
- AAC arrives as ADTS frames: strip the header, keep raw frames, capture the
  AudioSpecificConfig.
- fMP4 needs no carry: a sample's bytes are contiguous in `mdat`, so a segment is fed
  whole and nothing spans a call. `trun`/`tfhd`/`trex` defaults resolve each sample's
  size, duration, flags and composition offset, video is rewritten from length prefixes
  to the same Annex B start codes the TS path emits, and a segment that fails halfway
  is rolled back out of the store. Keyframes are read from the bitstream — an IDR NAL
  for H.264, any IRAP for H.265 — while `trun`/`sdtp` only supply the separate `sync`
  bit.

| source | demux | elementary stream |
|---|---|---|
| TS, H.264 | PES → Annex B start-code split | NALUs + AUD |
| TS, AAC | ADTS frames | raw AAC + AudioSpecificConfig |
| fMP4, HEVC | `moof`/`trun` → length-prefixed NALUs (`hvcC` length size) | NALUs |
| fMP4, AV1 | `moof`/`trun` → OBUs | OBUs |
| fMP4, AAC | `moof`/`trun` → raw frames + `esds` config | raw AAC |

## Timestamps

- TS: PES PTS/DTS are already 90 kHz.
- fMP4: DTS from `tfdt` + `trun` durations, PTS = DTS + composition offsets, in the
  track's own media timescale (`Sample` carries counts, `Track::timescale` converts).
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
- **Sources stay as they are:** TS carries H.264 + AAC. fMP4 carries anything:
  H.264 (a multigroup/multitrack transmux, which is what a non-partner stream is
  served as today), HEVC (the portrait 1080p60 track on a dual-format broadcast),
  AV1 later. The container is chosen by the feature that needs it, not by the
  partnership: disable multigroup and the same channel is MPEG-TS.

## Code to adapt (MIT)

- `mediacommon/pkg/formats/mpegts/reader.go` — TS demux for the H.264/AAC source.
- `mediacommon/pkg/codecs/{h264,h265,av1,mpeg4audio}` — SPS/avcC/hvcC/av1C/esds/ADTS.
- `mediacommon/pkg/formats/fmp4` — init/part parsing.
- Rust: `mpeg2ts-reader` (MIT/Apache) if porting the TS demux is not wanted.

## Open risks

- The TS source has no MAP, so configuration has to come from the first segment.
- Init changes mid-stream (discontinuities, ads).
- AAC AudioSpecificConfig changes are the least forgiving.
