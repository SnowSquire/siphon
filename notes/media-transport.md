# Media transport: the UI does not use RTSP

Status: idea. The only trace so far is one bullet in `notes/linux-port.md:133` —
"transport is not settled and RTSP is probably not it". Nothing is designed or
built; `play` still serves RTSP and the UI still has no video path.

## Why RTSP goes

RTSP/RTP is built for a client on the network. It costs SDP negotiation, a
session state machine, RTP 90 kHz timestamps mapped to NTP, RTCP sender reports
once a second, RTP range signalling for the live position, TCP interleaving, and
a jitter/reorder buffer on the far side. The UI is on loopback, in order, and
wants one thing: the next access unit, or a keyframe if it fell behind.

The daemon already holds the truth in exactly that form. `rtsp/h264.rs` splits
Annex-B NALs and knows keyframes; `rtsp/ts.rs` demuxes the transport stream and
splits at AUD boundaries (PES is *not* an AU boundary — see
`notes/es-rtp-rtsp.md`). RTSP then shatters each AU into STAP-A/FU-A datagrams
under a 1450-byte budget and the UI puts them back together. That round trip
exists only to satisfy RTSP, and `notes/es-rtp-rtsp.md` spends a page and a half
on the consequences.

## What replaces it

A media channel alongside the existing RPC (`crates/rpc`: typed messages,
postcard framing, loopback, `Hello`/`Welcome`), carrying **elementary stream, not
decoded frames** — decode belongs to the UI because it needs the Vulkan device.

| message | direction | payload |
|---|---|---|
| `StreamOpen { broadcast, quality }` | UI → daemon | request a stream |
| `StreamConfig { sps, pps, width, height, timescale }` | daemon → UI | parameter sets (`H264Config` already tracks them) |
| `AccessUnit { pts, dts, keyframe, bytes }` | daemon → UI | one Annex-B AU |
| `StreamSeek { position }` | UI → daemon | DVR / late join; resumes at a keyframe |
| `StreamClose { reason }`, `StreamError` | both | teardown |

Three semantics that RTSP fights and this expresses directly:

- **Latest wins.** The UI is a live consumer; when it is behind, the daemon
  drops. Drops must be in units the decoder tolerates: never mid-GOP, so a drop
  runs from a non-keyframe AU to just before the next keyframe, and the keyframe
  itself always goes through.
- **Credit-based flow control.** The UI grants a byte window and the daemon never
  queues beyond it — no RTCP, no jitter buffer, no reordering, because HLS
  segments arrive ordered.
- **Seek is a restart.** `StreamSeek` picks a segment and the daemon resumes at
  its keyframe. RTSP could only approximate this with `Range`, and `Scale`
  catch-up was already tried and dropped because ffmpeg does not parse it.

## Zero copy is the real prize

The spool is already a chain of fixed 192 KiB slots in an arena with cursor
readers (`notes/slot-arena-rework.md`). A UI on the same machine can map those
slots instead of receiving copies, so a `AccessUnit` message could carry a slot
range rather than bytes, and the media path copies nothing at all. RTSP can
never do this — everything becomes a datagram.

## What stays

`play` keeps its RTSP server: it is the external harness (`ffplay`, `ffprobe`)
and the only way to check the packetizers against a real client. Removing the
UI's use of RTSP does not delete `crates/daemon/src/rtsp/`; the packetizers just
stop being on the critical path.

## Open

- **Audio.** AAC wants continuity and video wants latest-wins; same channel with
  a separate policy, or a second channel?
- **Sockets.** The admin RPC is request/response with a 1 MiB frame cap, which
  does not suit a firehose. A second connection for media (or a message class
  with its own windowing) is probably cleaner than bending the admin protocol.
- **Remote UI.** The moment the UI can run on another machine, this protocol
  needs framing, authentication and probably TLS of its own — the same job RTSP
  does today, which is the argument for keeping RTSP as the fallback for that
  case rather than growing both.
- **Seek granularity.** Segment-aligned is what the scheduler has. Frame-accurate
  needs the demuxer to expose an AU index.
