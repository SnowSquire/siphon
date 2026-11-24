# Riffle: the daemon↔UI protocol

Status: Part I is designed and nothing is built; `play` still serves RTSP and the
UI has no video path. Parts II and III are implemented. The ALPN is `riffle/1`.
It is one protocol: the RPC is the control plane (`Class::Control`) and media is
the same framing with media classes, over a carrier that can lose datagrams.

Part I is the transport and the media model. Part II is the control plane the
daemon and UI already speak. Part III is the UI's presentation path.

## Why RTSP goes

RTSP/RTP is built for a client across a network. It costs SDP negotiation, a
session state machine, RTP 90 kHz timestamps mapped to NTP, RTCP sender reports
once a second, `Range` signalling for the live position, TCP interleaving, and a
jitter/reorder buffer on the far side. The UI wants one thing: the next access
unit, or an older one when it is behind.

The daemon already holds exactly that. `rtsp/h264.rs` splits Annex-B NALs and
knows keyframes; `rtsp/ts.rs` demuxes and splits at AUD boundaries (PES is *not*
an AU boundary — `notes/ts-demux.md`). RTSP then shatters each AU into
STAP-A/FU-A datagrams under a 1450-byte budget and the UI puts them back
together. That round trip exists only to satisfy RTSP.

## Two carriers, one transport

| carrier | when | what it gives |
|---|---|---|
| pipe / domain socket | same machine | reliable ordered bytes, no loss, no reorder |
| QUIC over UDP | any other machine | streams + DATAGRAM, ACKs, pacing, congestion control, PMTU |

`compio-quic` 0.8.x (quinn-proto 0.11 + rustls 0.23) on the compio reactor,
pinned. The pipe is not the second-class path — it is the fast one, and the only
one that can hand over arena slots.

Only elementary stream crosses either carrier. No MPEG-TS, no fMP4: containers
exist on disk and inside the fetch path only. The demuxer is incremental and
emits an access unit as soon as the AU is complete, never waiting for a whole
segment. The AU is the atomic unit; within one, the transport fragments at
~1200 bytes.

## Connection, lane, object

- **connection** — one per UI. Control and media share it; QUIC multiplexing
  removes the head-of-line blocking that argued for a second one.
- **lane** — one per watched stream, unlimited. A lane is a reader position and a
  rate, not a queue.
- **object** — one access unit, one audio frame, one blob. The unit the policy
  bits apply to.

### One header

Datagram: 18 B, ~1.5% of a 1200-byte payload.

```
u8 class | u8 flags | u64 obj_id | u32 total_len | u32 frag_offset | payload
```

Stream and pipe frames prepend `u32 len`. `class` doubles as the datagram lane
tag, because a QUIC DATAGRAM frame carries no stream association. `obj_id` is
monotonic for the life of the connection and **never reset**, so a seek needs no
epoch field and "drop if `obj_id <= last_delivered`" survives one. `flags` is
`supersede | keyframe | last_fragment`.

### Policy, not delivery classes

Reliability and ordering are per object, not per connection.

| class | retransmit incomplete | cross-object | note |
|---|---|---|---|
| video AU | no | supersede | datamosh on loss |
| audio frame | no — sent twice | supersede | absolute priority in the lane |
| control, acks | yes | ordered | bidi stream 0 |
| blob | yes | independent | one unidirectional stream per object |
| state update | no | supersede | versioned, coalesced |

That is the whole taxonomy. Neither "reliable unordered" nor "unreliable ordered"
needs to exist as a delivery class: order *inside* an object is free, and order
*between* objects is the `supersede` bit.

## Push, not pull

The daemon pushes; a UI cannot ask for what has not been sent. It reports where
it is:

```
StreamPlayback { position, rate, buffer_ms }    // on change, plus a 5 s heartbeat
```

The daemon keeps one estimate — reported position advanced by rate and elapsed
time — and pushes while

```
sent_position < predicted_playhead + buffer_ms
```

and never faster than data exists. That single rule produces all three
behaviours:

- **at the edge** — "data exists" is the binding constraint, so AUs leave as the
  demuxer emits them. Chunk by chunk, no lookahead, no added latency.
- **below 1×** — the playhead falls behind, `sent_position` runs ahead until the
  buffer is full, and the daemon stops reading until it catches up.
- **a DVR seek** — it fills `buffer_ms` forward from the seek point.

`buffer_ms` defaults to 30 s, clamps to 1–120 s, and is **the client's RAM**: the
daemon retains nothing per lane. The 5 s heartbeat exists for clock drift between
the two estimates, not for liveness — a stall at the edge is already known to the
daemon, because it has nothing more to send.

There is no credit protocol and no window on the wire. This rule *is* the flow
control, which is also why "unlimited lanes" is cheap on the daemon side.

## Video: unreliable ordered, and it never gives up

Drops are whole AUs. A truncated AU is not a broken picture, it is a broken
bitstream — CABAC desync, garbage, or a hard error.

On loss the UI keeps decoding against a stale reference and lets the picture
smear: it does not freeze and does not flush. Concretely, `AV_EF_EXPLODE` off,
`FF_EC_GUESS_MVS | FF_EC_DEBLOCK` on, and **no** `avcodec_flush_buffers`. At
Twitch's ~2 s GOP a smear heals itself within a GOP, and live Twitch has no
keyframe-request API anyway, so gating on the next IDR buys a freeze and nothing
else.

There is no drift threshold. The picture may look wrong for a while after a bad
patch and recovers at the next IDR.

Keyframes are one `keyframe` bit — the same fact `AV_PKT_FLAG_KEY` carries, set on
any IRAP including HEVC CRA. The UI drops RASL pictures itself when it starts at a
CRA; the transport does not model IDR vs CRA vs BLA.

## Audio

Separate class, same lane, never sharing video's fate. ~400 B per AAC frame, so it
is **sent twice** rather than retransmitted: +16 KB/s buys near-zero loss without
a round trip, and a round trip is the only thing that could make it late.

Audio has absolute priority inside the lane: when the congestion window cannot
carry both, a video AU is dropped. Audio also drives the playback clock; video is
presented to match it.

## Seek, join and the window

- **Seek** is GOP-aligned by the daemon and precise by the client. `StreamSeek {
  target }` → `StreamStarted { start_pts, start_segment }`; the client discards
  decoded frames before its target. Trimming in the daemon would remove the
  keyframe needed to decode the target, so it is not merely expensive — it is
  wrong.
- **Joining live** starts at the *previous* IDR from the window, runs above 1×
  until it reaches the edge, then settles to 1×. Join is instant; the cost is
  being one GOP behind, which is invisible on a live stream.
- **A position outside the retained window** is clamped forward to the oldest
  retained segment and reported through `StreamStarted`, so the client knows it
  jumped. This is a rare path: a DVR 403, or a DVR URL that cannot be resolved.

## Codecs

- v1: H.264 over MPEG-TS.
- Then: H.265, which Twitch carries **only in fMP4**, so it needs an fMP4 demuxer
  (`moof`/`trun`/`mdat`, init-segment `hvcC`, length-prefixed NALs converted to
  Annex-B). That is a deliverable of its own and is not allowed to block the
  transport. The same path later carries AV1.
- Codec config is one opaque blob — the raw `avcC` / `hvcC` / `av1C` record —
  which is bit-for-bit what `avcodec_open2` takes as `extradata`. No per-codec
  parameter-set modelling.

`StreamConfig { video: { codec, extradata, timescale, width, height }, audio: {
extradata, timescale, sample_rate, channels } }`, sent once, reliably, before any
object.

## Multiple lanes

Unlimited, bounded by the network, the client's decoder, and the arena. Two
streamers in the same room means two lanes with independent playheads, and the
client's playback modulator nudges each with small rate corrections (0.98–1.02) —
which is why a rate change must never cause a burst or a gap.

Edge lanes get strict priority over catching-up lanes. A catch-up has no deadline;
a live lane does.

`StreamKey` becomes `{ channel, stream_id, quality }`. The stream id (Twitch's
`BROADCAST-ID`) is the identity, so a restream is a different stream and an old
lane never inherits a new broadcast. The channel rides along for attribution,
because a lane whose broadcast just ended must still be able to name its channel.

## Blobs and avatars

`ChannelView` carries `avatar { id, rev }` where `id` is a content hash, not a
path. `GetAvatar { id, px }` on the control plane returns pre-scaled bytes; the UI
caches them by hash in its own directory and hands ZGUI a local file, because
ZGUI's image element reads files and not URLs. A blob is an object with
`retransmit + independent`, and nothing else.

## Zero copy ends at the pipe

The spool is a chain of 192 KiB arena slots with cursor readers. On the same
machine a message can carry a slot range instead of bytes, so the media path
copies nothing. That is what the pipe is for, and it is why the local path is not
"QUIC over loopback".

QUIC copies by construction — it encrypts — so the handle path is a carrier
property, not a protocol assumption.

## Trust: pinned keys, typed code

TLS 1.3, a self-signed certificate on each side, no CA. The pin is the SPKI's
SHA-256, i.e. the key rather than the certificate wrapper, so a renewal is not a
false alarm.

Pairing, once per client:

1. `siphon-daemon pair` opens a 120 s window: one attempt at a time, no RPC
   served.
2. The UI connects, and both sides derive a short authentication string from the
   TLS exporter, which is bound to the full transcript including both
   certificates. An active MITM produces two different strings.
3. The daemon prints its string; the user types it into the UI; the UI sends it
   back over the provisional connection.
4. Match: the daemon pins the client's SPKI in `peers.json` and the UI pins the
   daemon's. Mismatch: everything is torn down and forgotten.

The typed code is proof, not comparison — an MITM on the UI's leg cannot produce
the daemon's string. Revocation is deleting an entry from `peers.json`: the
operator of a remote daemon is someone who edits JSON.

No session resumption and no 0-RTT. Resumption skips certificate verification, so
it would bypass the pin on exactly the reconnects it exists to speed up, and 0-RTT
early data is replayable. What a roamed UI needs is QUIC connection migration,
which needs neither.

Keys are 0600 on Unix and DPAPI on Windows, and are never regenerated by an
update: a new daemon key is an explicit re-pair for every client.

## Reaching the daemon

`network.listen: Vec<String>` in `config.json`, already in the schema and already
migrated. Empty means no UDP listener at all, and a local install uses the pipe.
Non-empty binds those `host:port`s, and **remote clients get full capability** —
`ApplyUpdate` and `SetRecordDir` included, because the operator of a remote daemon
is the same person. `network.remoteControl` and `RemoteCapability` are deleted;
the gate is `listen.is_empty()`.

The pipe stays bound regardless. `ui.json` already has `Connection::Remote {
endpoint, fingerprint, label }` and `active_connection`; nothing reads them yet,
and that is the change.

## Telemetry

`ConnHealth` gains `rtt_ms`, `cwnd_bytes`, `lost`, `dropped_video`, `dropped_audio`,
`last_drop_age_ms`, and a derived `recovering`. Datamosh makes drops normal, so
they have to be visible, or "smearing because lossy" is indistinguishable from
"smearing because broken".

`Update::Lane(LaneView)` and `Update::LaneRemove(StreamKey)`, coalesced per key:

```
LaneView { key, state, position_ms, playhead_ms, buffered_ahead_ms, rate,
           dropped_video, dropped_audio, recovering, started_ms }
```

The snapshot carries `lanes: Vec<LaneView>` for this connection's lanes only.

## What stays

`play` keeps its RTSP server: it is the external harness (`ffplay`, `ffprobe`) and
the only way to check the packetizers against a real client. Removing the UI's use
of RTSP does not delete `crates/daemon/src/rtsp/`; the packetizers just stop being
on the critical path.

## Open

- **AV1** — the same fMP4 path, a different config record and OBU framing.
- **Quality downgrade under pressure** — audio-only today, a different video track
  later.
- **Cross-lane alignment** — the client's modulator owns it and is audio-driven; a
  manual offset knob first. The protocol carries no anchor for it.
- **Frame-accurate seek** — needs an AU index in the demuxer.
- **DVR 403** — the one path that turns a clamp into a restart.

## Rejected

- **TQUIC** — a better `!Send` shape than quinn, but DATAGRAM exists only on an
  unmerged branch, there is no custom certificate verifier and no way to require a
  client certificate, no TLS exporter, the peer certificate is unreachable from a
  connection, TLS config takes file paths only, and a git dependency cannot build
  because BoringSSL is a submodule. It is an HTTP/3 client, not a transport.
- **quiche** — sans-IO, and the only option with RFC 7250 raw public keys, i.e.
  literally "present a key, no certificate". Killed by BoringSSL in a hermetic
  Nix cross-build to MSVC.
- **A hand-rolled protocol over UDP** — that is a congestion controller, ACK
  ranges, RTT estimation, flow control and PMTU discovery to own and debug, for a
  6–40 Mbps workload that every library serves at 6–13 Gbps.
- **Noise instead of TLS** — dropping TLS was the goal, not the mechanism. QUIC
  makes the handshake mandatory, and the SAS falls out of the exporter.
- **SRT / RIST** — reliable ordered only, which deletes latest-wins and datamosh.
- **A subchannel per channel for RPC ordering** — versions make order irrelevant;
  `Update` is already an idempotent entity value.
- **Four delivery classes** — two policy bits on an object cover every class.
- **Credit, or any window on the wire** — the playhead estimate is the flow
  control.
- **A daemon-side lookahead buffer** — the buffer is the client's RAM.
- **A datamosh drift threshold** — it recovers at the next IDR.

---

# Part II: the control plane

Status: implemented. `crates/shared` (`siphon-shared`) owns the typed protocol,
postcard framing and client in its `riffle` module, the per-client settings schema
in `settings`, and the JSON file layer in `store`; `crates/daemon` owns config,
GQL, Hermes, the daemon loop, the stream registry, telemetry and the admin RPC
server. The UI
is `crates/ui` (ZGUI, binary `siphon`) and talks only over the RPC. It presents
through the DXGI flip chain extracted from the video spike and the libplacebo
pass (SDR unless the client's HDR setting asks otherwise), starts the daemon
when the admin port is idle and restarts it at the process level (PID file +
terminate), never through the socket: a frozen daemon must still be restartable
from the tray.

## Goal

The whole app is Siphon. The long-lived process is the **Siphon backend** (today
`daemon`); the tray/window is the **Siphon UI**. The backend owns all behaviour
and the UI is a dumb renderer:

- backend: config load/save/migrate, GQL resolution, Hermes, live/title
  transitions, monitor list, pipelines, disk, telemetry;
- UI: render the latest state, send user intents, decide and show notifications,
  own the notification centre and the tray. No GQL, no Hermes, no daemon-config
  writes.

The two talk over one typed RPC. The protocol types live in one crate so the
compiler catches drift; no field layout is duplicated between client and server.

```
Siphon UI (ZGUI)  ── typed RPC ──  Siphon backend (compio)
                                      ├─ app state (config, channels, transitions)
                                      ├─ GQL + Hermes
                                      ├─ channels (record flags) → pipelines
                                      └─ RTSP listener
```

`play` stays a separate CLI mode and does not serve RPC.

## Transport

- A Windows named pipe (`\\.\pipe\siphon-<app id>`) or a Unix domain socket
  (`$XDG_RUNTIME_DIR/siphon-<app id>.sock`, 0600), separate from RTSP.
  `daemon --admin <endpoint>` overrides it; `--no-admin` disables it. RTSP
  `--bind` may be `0.0.0.0`; the admin surface cannot follow it, and OS access
  control is the process user's.
- No HTTP, WebSocket or gRPC: the client and server are both compio in this
  workspace, and the only missing part is an envelope. No auth beyond the
  pipe/socket permissions.
- Frame: `u32` big-endian payload length, then a `postcard`-encoded message.
  Cap the frame (`RPC_MAX_FRAME`, 1 MiB) and reject before allocating. The
  codec is one module, so the encoding can be swapped without touching the
  message types.
- First exchange is `Hello`/`Welcome`: magic, `PROTOCOL_VERSION`, app version.
  A mismatched version is rejected with a typed error, not a decode panic.

```text
ClientMessage::Hello { magic, protocol, app_version }
ServerMessage::Welcome { protocol, server_version }
ClientMessage::Request { id, request }
ServerMessage::Response { id, result: Result<Response, RpcError> }
ServerMessage::Update { revision, update }
```

## State model

The UI is fine-grained reactive (ZGUI), so the steady state is **entity-level
updates**, not whole snapshots and not per-field patches:

- On connect: `Welcome`, then one full `Snapshot` with a monotonic `revision`.
- Then `Update`s. Each update carries a complete entity value (a channel row, a
  pipeline row, the config, the arena summary), so it is idempotent and can be
  coalesced by entity without losing correctness.
- On reconnect or a revision gap: `GetSnapshot` → `Snapshot` → updates. TCP is
  ordered, so a gap only happens after the backend disconnected the UI.
- The snapshot is the resync mechanism; it is not sent on every change.

```rust
struct Snapshot {
    revision: u64,
    config: Config,
    conn: ConnHealth,
    channels: Vec<ChannelView>,
    pending: Vec<String>,
    pipelines: Vec<PipelineView>,
    arena: ArenaSummary,
    downloader: DownloaderSummary,
    logs: Vec<LogEntry>,
    error: Option<String>,
    update: Option<UpdateView>,
}

enum Update {
    Config(Config),
    Conn(ConnHealth),
    ChannelUpsert(Box<ChannelView>),
    ChannelRemove(u64),
    Pending(Vec<String>),
    PipelineUpsert(PipelineView),
    PipelineRemove(StreamKey),
    Segment { key: StreamKey, record: SegmentRecord },
    Arena(ArenaSummary),
    Downloader(DownloaderSummary),
    /// Append-only: never coalesced, because the transition is the value. The
    /// client decides whether it is worth a toast.
    StreamEvent(Box<StreamEvent>),
    Log(LogEntry),
    Error(Option<String>),
    Update(Option<UpdateView>),
}
```

`ChannelView` carries what the UI renders: id, login, display name, the resolved
switch colour (`chatColor`, or a generated `oklch(...)` fallback), the local
`avatarPath` the daemon cached, live, title, game, the
game id, viewers, stream start as milliseconds, when it was last live
(`last_live_ms`), Twitch's `BROADCAST-ID`, the last per-channel failure
(`error`), and the per-channel `record` switch — the notification switches are
the client's and never cross this boundary. `last_live_ms` is Twitch's
`lastBroadcast.startedAt`, and the name is
a misnomer: while the channel is live it is roughly the broadcast's start,
which Twitch reports about five seconds after the real one, and while offline
it is when that broadcast *ended*. `stream_start_ms` (from `stream.createdAt`)
is the accurate start, so an uptime or a duration reads that and never
`last_live_ms`. `stream-down` writes the local end time first so the row never
shows the start of a stream that just ended, and the next baseline poll
overwrites it with Twitch's own end time; they differ by seconds, which the
UI's minute-granular relative text hides. That makes the field an ordering key:
newest live first for the live tier, most recently live first for the offline
tier — which is what puts a channel that has been dark for weeks at the bottom.
`error` is the channel's own resolve failure, cleared by the next successful
refresh; pipeline and disk failures stay on their own entities and the UI
merges them by login. ZGUI's
image element reads files, not URLs, so `avatar::ensure` downloads each picture
once into the app data directory and the row is re-published when the path
exists. The workspace version (`0.5.0`) is shared by `daemon`, `siphon-shared` and
`siphon-ui` because the Hello/Welcome carries it; `libplacebo-sys` and the
`video` prototype keep their own `0.1.0`. `PipelineView` carries key, state (live/offline/stopped),
pinned, recording, sessions, position, live edge, segment tail, window size,
counters, last error and the attached recorder's `disk` counters. `SegmentRecord`
is only scalars; the segment ring is queryable and updates append to it.

Pins are the one ordering fact the daemon never sees: `ui.json`'s `channels`
map holds a `pinned` flag per channel id (`siphon_shared::settings`), and a
snapshot only decides which rows are drawn, not what is pinned — an entry for a
channel that is no longer followed is inert rather than pruned. Ordering
therefore differs per window, which is the point. There is no separate
`pins.json`.

`LogEntry` is the daemon's bounded event ring (`LOG_CAP`, 512): `seq`, `at_ms`,
level, subsystem target, message, and the stream identity beside it (`login`,
`stream_id`) so the Logs tab can correlate a line with the recording on disk. `Update::Log` is append-only and never coalesced. The ringlives on the bus, not the app, so the registry can log a recording start/stop
and the snapshot still carries the whole history.

Requests:

- intents: `AddLogin { login }`, `RemoveChannel`, `SetRecordDir(Option<String>)`,
  `SetRecordDualFormat(bool)`, `SetRecord { channel, record }`, `ClearError`,
  `ApplyUpdate`, `PipelineStop { key, force }`, `PipelineRestart(StreamKey)`,
  `DisconnectViewers(StreamKey)`, `RecordOnce { login, quality }`. The
  notification switches and the word filter are not here: they are client
  settings, written to the client's own `ui.json`;
- queries: `GetSnapshot`, `GetStreamDetail { key }`, `ListSegments { key,
  limit }`, `GetArena`, `GetDownloader`, `GetDisk { key }`.

An intent returns an ack or a typed `RpcError`; the resulting state change
arrives as an `Update`. The UI may optimistically render a toggle, but the
backend remains authoritative.

## Ordering and backpressure

- Connections are slots in the daemon loop, not tasks: each `riffle::Conn` owns its
  frame decoder and outbound `Vec<u8>`, and the loop arms one read op and one
  write op at a time. A request is handled inline with `&mut` access to the
  backend state, so the response is queued before the next loop pass. A snapshot
  is a plain function returning an owned value.
- The listener is a task that only accepts; it hands the raw stream to the loop
  through one kanal queue.
- The bus is one coalesced queue, not one per connection. The loop encodes each
  update once and appends the same bytes to every ready connection. Append-only
  updates (`Segment`, `Log`) are never coalesced.
- A connection whose outbound buffer exceeds `riffle::MAX_OUT` is dropped. It
  reconnects and resyncs from `Snapshot`; nothing is queued indefinitely.
- Background jobs (GQL refresh, saves, update checks, avatars) finish on their
  own task and post through one kanal queue the loop drains. The loop's biased
  select puts the Hermes socket read first, so a completed websocket read is
  never dropped for a queued event.
- No `RefCell` borrow is held across an `.await` in a handler.

## Backend app

The registry stops being the top-level owner. A new app/backend struct owns the
Siphon `Worker` logic and the registry as a subsystem; the RPC and RTSP
listeners are tasks around it.

### Config

- The daemon owns `Config` (the follow list and its `record` switch, the
  recording defaults, the cached `account` and the `network` section) and its
  migrations. Everything a notification is made of moved out of it.
- Two files, one owner each: `config.json` in `%APPDATA%\com.iken.siphon`
  (`$XDG_CONFIG_HOME` on Unix) is the daemon's, `ui.json` in the same directory
  is a client's — display, notification policy, the per-channel switches and
  pins, and `connections`/`activeConnection`. The one secret is `auth.json` in
  the state directory (`%LOCALAPPDATA%` / `$XDG_STATE_HOME`), which the daemon
  reads and never puts on a command line. The module docs in `settings.rs` and
  `auth.rs` own the details.
- Reuse Siphon's config path so existing installs keep their channels; the UI
  stops touching the daemon's file.
- Save asynchronously on compio. The UI receives config in the snapshot and
  mutates it only through intents.

### GQL, Hermes and notifications

- Port Siphon's GQL and Hermes code into the daemon. compio already has
  `ws`/`ws-connect`; no second runtime.
- Hermes `stream-up` activates the desired monitor; `stream-down` stops the
  pipeline but keeps the monitor; title/game/viewcount events update
  `ChannelView`s.
- The daemon **publishes** transitions rather than deciding notifications:
  `stream-up`, `stream-down` and a title/game move each publish one
  `StreamEvent` (kind, `previous`, and the row as it stood), uncoalesced. The
  client owns the switches, the word filter and the mute window and raises the
  OS toast itself, so the notification centre is client state and does not
  survive a window restart. This is safe because the UI autostarts at login and
  owns the daemon. The webhook in `notify.rs` stays: it reports
  the disk output's own conditions, not the user's notifications.
- Until Hermes lands in the daemon, the same API works with an external bridge:
  a monitor is desired state, and activation is an explicit request/event.

### Record state and pipelines

There is no monitor set. The key is `(channel id, quality)`, the same pair `StreamKey` carries.

- A followed channel is a row with `record: bool` (mirrored from config),
  `stopped: Option<NonZeroU64>` (the broadcast id the user stopped) and lazily
  fetched variant data. `set_record` writes the bool; `monitor_live` is what
  Hermes `stream-up` and the reconnect baseline call.
- A one-shot recording is a pipeline started now and not persisted:
  `RecordOnce` resolves the channel and records the live broadcast.
- Stop writes the current broadcast id into `stopped`. A restream has a new id,
  so it is not suppressed; `stream-down` clears the latch and the fetched
  variant data.
- On DESCRIBE, a new runtime attaches the media engine immediately (a DESCRIBE
  is a viewer); `Runtime::upgrade()` is idempotent through `media_spawned`.
- No capacity check and no eviction: a runtime goes away when the last viewer
  leaves and nothing is recording. A scheduler with no consumers parks, so an
  idle stream costs neither polls nor fetches.
- Offline at activation: publish the error; the next Hermes event or
  `PipelineRestart` retries. No polling loop.

### Lifecycle

- `MonitorRemove` unpins. If nobody is watching and nothing is recording, stop
  the runtime; otherwise leave it and let the last viewer/recording release it.
- `force` is the only thing that disconnects viewers. Add a per-session close
  signal and a cancel-safe `select` in the RTSP read loop; `Session::kill()`
  closes the outbound channel and wakes both halves.
- `PipelineStop` stops the pipeline but keeps the monitor; `PipelineRestart`
  stops and re-resolves.
- Per-monitor `record` starts/stops the disk output on a live runtime. Disk
  introspection (runs, canonical progress, release) is phase 2: `DiskOutput`
  currently exposes only `stop`/`wait`, so it needs a shared metrics handle and
  a control channel.

## Telemetry

- `GlobalMetrics`: arena live slots, arena count, high-water mark, alloc
  failures, spool-cap failures, arena backoffs; downloader queue depth,
  capacity, in-flight count, retries.
- `StreamMetrics`, one per runtime: scheduler counters (manifest polls,
  failures, segment tail, prefetch tail, window size), segment ring (fixed
  size, scalars only), fetch outcome/bytes/elapsed/retries, last error. Media
  stats (sessions, position, live edge) are read from `Stream` accessors at
  snapshot time.
- `ToDownloader::FetchURL` gains an optional stats handle; the worker updates it
  on completion. `Writer` gains a published-byte accessor for the byte count.
- Never copy a `Segment` or a `Reader` into metrics: `Segment` contains a
  `Reader`, and cloning it pins arena slots. Copy scalars.
- The snapshot is built on demand from app state + registry + metrics; the
  update bus emits at the mutation sites.

## Crates

- `crates/shared` (package `siphon-shared`): everything both sides agree on. The
  `riffle` module holds the serde message types, the `postcard` codec, the frame
  decoder and the compio client — no ZGUI and no daemon internals. `settings`
  is the per-client schema a UI persists for itself, `store` the versioned-JSON
  file layer it and the daemon's config both read and write through.
- `crates/daemon`: the backend app and the RPC server module; `registry.rs`
  owns pipelines, `app.rs` owns everything else.
- `crates/ui` (`siphon-ui`, binary `siphon`): the ZGUI window presented through
  the DXGI/libplacebo renderer, the native tray and the Riffle worker. It owns the daemon lifecycle (`service.rs`): auto-start on launch and
  a process-level restart (`daemon.pid` in the app data dir, image-name check,
  `taskkill` fallback).
- `play` keeps its current pipeline and needs no protocol types.

## Tests

- Codec: roundtrip, partial header/payload, oversized frame, bad version,
  truncated postcard.
- Loopback server/client: Hello, GetSnapshot, intent ack, update delivery,
  slow-client disconnect.
- State projection: apply a snapshot plus a sequence of updates and compare
  against a freshly built snapshot (debug hash).
- Monitor lifecycle in `--debug`: add, activate, DESCRIBE upgrade, remove,
  restart; no network.
- `cargo clippy -p daemon --all-targets`, `cargo +nightly fmt`.
  No new unsafe in the RPC path, so no Miri requirement.

## Remote clients

Status: designed, not implemented. `ui.json`'s `connections` and
`activeConnection` exist and nothing reads them; `config.json`'s
`network.listen` is stored and not bound. The protocol is Part I; this is what
the control plane becomes when the UI is pointed at another machine.

### Local

The pipe/socket stays the local transport. It *is* the access control (pipe
DACL, 0600 socket), there is no listener on a machine that does not need one,
and `probe`, `ensure_running` and restart are built on it. `Connection::Local`
means "this install's endpoint", not "a URL that says loopback": trusting
loopback would let any local process impersonate a daemon.

### Remote

One QUIC connection per UI carries both planes: the control plane on a
bidirectional stream, media as datagrams on the same connection. No second
credential, no second listener.

`network.listen: Vec<String>` is the only switch. Empty means no UDP listener at
all, and a local install uses the pipe. Non-empty binds those `host:port`s, and
**remote clients get full capability** — `ApplyUpdate` and `SetRecordDir`
included, because the operator of a remote daemon is the same person. There is
no per-intent capability gate; the gate is `listen.is_empty()`, and
`RemoteCapability` is deleted.

### Identity and pinning

- A self-signed certificate per side, no CA. The pin is the SPKI's SHA-256: the
  key, not the certificate wrapper, so a renewal is not a false alarm.
- Pairing is a typed code, not a pasted string. `siphon-daemon pair` opens a
  120 s window, the UI connects, the daemon prints the short authentication
  string, and the user types it into the UI. Part I has the sequence and why the
  code is proof rather than comparison.
- A mismatch offers "pair again", never a "connect anyway" button.

### What remote costs

- `ensure_running` must not spawn, and `restart` cannot be process-level: the
  tray Restart degrades to an RPC exit request, or to "go restart it there".
- `probe` needs a connect plus handshake with a timeout, and "unreachable" must
  be distinguishable from "wrong fingerprint".
- Applying an update runs an installer on the daemon's machine. That is the
  operator's own machine, so it is allowed; the blast radius is still different
  from a local update's.
- No NAT traversal, no rendezvous. An address and a port, plus "use a tailnet or
  forward a port", is the honest scope.

### Version skew

One build today, two machines later. Because every install carries a daemon, a
mismatch is never a dead end — the UI can fall back to its own local daemon. So
the contract is exact-match `PROTOCOL_VERSION` with both versions named in the
refusal, rather than compatibility shims.

The protocol version now gates framing as well as messages, so a mismatch that
slipped through would be a corrupt picture rather than a clean error: gate on
the handshake, never open media before `Welcome`, and carry a capability set
(media framing, HDR, what the client can decode) because the two machines'
renderers genuinely differ.


## Open questions

- Update checking/applying: daemon or UI? Applying an installer restarts processes.
- Launch model: settled — the UI autostarts at login and starts the daemon.
  There is no service and nothing runs while the user is logged out; the
  installer's Run value and `--minimized` are in `notes/windows-installer.md`.
- Multiple UI clients: the protocol supports it, but only one tray/window is
  expected; decide whether to enforce single-instance at the RPC layer.
- Disk API shape: run listing, release semantics and canonical progress.
- Per-stream slot reservation (already open in `daemon-mode.md`).
- Video/HDR: separate note; see below.

## Gotchas

- One thread, one reactor. A blocking send is a deadlock. `try_send` + writer
  task + control queue.
- Never hold a registry/app `RefCell` borrow across an `.await`.
- Updates are complete entity values; do not turn them into per-field patches.
  Coalesce by entity, never drop a revision silently.
- Do not put `Rc<Stream>` or `Reader` in events or snapshots; copy scalars and
  use `Weak` subscribers.
- `pinned` must be checked in every eviction path, or the first DESCRIBE drops
  every monitor-only runtime.
- `StreamKey` is `{ channel, quality }`: the channel id is the identity the
  daemon already holds in `config.channels[].id`. A login appears only where a
  user or an RTSP URI names one, and the UI resolves it for display from its
  channel list.
- Quality is part of `StreamKey`; a quality change is remove + add, with
  `force` if the old viewer must go.
- The UI never writes config; the daemon acks an intent and publishes the
  resulting config update.
- Hermes viewcount traffic can be frequent; coalesce `ChannelUpsert` per channel
  before the writer.


# Part III: the UI

## UI and video context

The window is four tabs plus a disabled fifth: **Channels** (the table,
per-row notify/record switches, expandable detail, add bar), **Filter** (global
title/sound switches and the word list), **Recordings** (live `PipelineView`
recorders plus the one-off `RecordOnce` form), **Logs** (connection, arena and
downloader counters, pipeline state, notifications and the event ring), and
**Watch**, which is drawn with a `not-allowed` cursor until the video surface
exists. zgui-ui controls are themed through one `ThemeProvider` token block in
`css.rs`'s `THEME_CSS`, so switches, inputs and tooltips share the hand-written
palette.
Hovering a row's name, title or game shows its channel id, broadcast id and
game id; the expanded panel shows those and the stream's start time as fields.

The UI is ZGUI (fine-grained `reactive_graph` signals); `Snapshot`/`Update` map
onto the reactive state, and the protocol stays free of ZGUI types. The renderer
is the video spike's path without the video: ZGUI renders into a `Bgra8Unorm`
texture, libplacebo composites it into a DXGI `FLIP_DISCARD` backbuffer
(three buffers, frame-latency waitable object, interval 1), and DXGI presents.
The swapchain is imported into Vulkan with `pl_vulkan_import`, so there is no
`VkSurfaceKHR`/`VkSwapchainKHR`. A drag emits an extent per resize event, so
the presentation is rebuilt on the per-frame path and has to be cheap. The ZGUI
texture and the inner renderer are rebuilt for the exact window extent every
time, which is what makes the content reflow at the display's rate. The chain's
backbuffers are the expensive part (`ResizeBuffers` costs ~3-6 ms), so the chain
is sized in two steps rather than one:

- **At rest** it is the window's own extent (`dxgi::idle_canvas`), presented 1:1
  at its top-left. 1000x760 costs 9 MB of backbuffers where a virtual-screen
  canvas cost 77 MB, and nothing is scaled or cropped.
- **While the size is moving** it covers the monitor the window is on
  (`dxgi::resize_canvas`, from `MonitorFromWindow`), grown when an event first
  outgrows it. `ResizeBuffers` is paid once at the start of a drag; every event
  after that is a repaint of the interface texture alone. `DXGI_SCALING_NONE`
  makes DWM show the chain's top-left corner cropped to the window, and the
  overlay stretches the UI's edge texels over the rest, so the area a growing
  window uncovers is the page background rather than black for the frame before
  the new extent is drawn.
- **`SETTLE` (400 ms) after the last event** the chain shrinks back to the window
  in `UiRenderer::draw`. The interface texture is kept and composed whole, so
  the new backbuffers need nothing painted into them first; measured, the shrink
  is ~3 ms. It waits while the desktop has the mouse captured — a live drag or
  border resize — and re-arms the deadline instead, so it happens once the drag
  is over. `ResizeBuffers` in the middle of a drag is the one moment this drawing
  was seen to lose the device (below).

So a drag costs two chain resizes: the monitor one (~5 ms) when it starts and the
window one (~3 ms) after it stops. A pause in the middle of a drag looks like the
end of one and pays the same two.

`DXGI_SCALING_NONE` fills whatever part of the client area the chain does not
cover with the chain's *background colour*, which defaults to opaque black —
not with the window's background brush, and not with the page colour. An
occasional one-frame black bar across the client area during a live resize was
tracked down to that fill and nothing else in the path can produce pure black:
a flip-model `Present` is atomic, the canvas is drawn whole before it, and the
UI texture is rebuilt and fully damaged on the resize frame. `SetBackgroundColor`
puts the page colour (`#12141a`) in that slot, so whatever the compositor fills
is the page rather than a black bar. A window-class background brush is set to
the page colour as well, for `WM_ERASEBKGND` paths. (The chain-size and
background knobs that made those two fills visible on demand were removed once
the causes were known.)

The D3D12 device, command queue and DXGI factory are created once per window by
`dxgi::D3d12` and shared by every swapchain. Recreating them per resize cost
~280 ms, almost all of it `D3D12CreateDevice`, which is what made a drag crawl.
The dev profile optimises the dependencies (ZGUI's layout and paint, wgpu, the
Win32 bindings) and `siphon-ui`, because a drag is a CPU-bound frame loop.

A rebuild replaces nothing until its replacement exists. It used to free the
interface view first; when the renderer for the new extent could not be built,
the frame that followed composited through the freed texture, and libplacebo
took the process down with it. `renderer_supplied` asking `SharedGraphics` for
a device that has been lost is the way that happens, so the failure is reachable
from a real state and not only from a bug in this file.

Closing the window puts it away in the tray: ZGUI's close request is vetoed and
the surface hidden, and the tray's Open (or a second launch of the program, which
posts the same message) shows it again. Two facts about this platform decide how
the hidden state is handled:

- ZGUI's `occluded` signal never comes true on Windows, because winit's Windows
  backend emits no `Occluded` event.
- A hidden window is handed no frames at all: winit paints visible windows only,
  and "a window that is hidden later waits for the paint that shows it again".
  Nothing that runs in a frame — the 100 ms interval included — runs while the
  window is in the tray.

Hiding therefore ends with `SetProcessWorkingSetSize(-1, -1)`, not with a chain
park. What a window makes resident is mostly driver code and first-frame work it
never touches again: the measured private working set goes from 134 MB to about
1 MB while hidden, and only ~2-5 MB faults back when the tray shows the window.
From inside, the window never sees the difference; from Task Manager it is the
difference between a tray app and a browser tab. Only a hidden window asks:
on a visible one the same call would turn the next pointer move into faults.

The swapchain is deliberately *not* released on hide, and the reason is a
measurement that corrected an earlier one. `ResizeBuffers(1, 1)` does give the
chain back, and every re-import of the three backbuffers raises the process's own
GPU accounting by about three times the chain — 9 MB of chain at rest, so ~27 MB;
a drag's monitor chain is 25-44 MB, so ~75-130 MB — and none of it comes down.
It looked like a leak and was written up as one. It is not: over 40 chain resizes
the *process* counter went 89 MB -> 1,207 MB while the adapter's dedicated usage
stayed flat (7,086 MB -> 7,070 MB), and `VK_EXT_memory_budget` reports each
import and its free balancing to the byte. What grows is per-process attribution
for imported resources; after a day of dragging, Task Manager would show
gigabytes this process does not hold. The chain stays, and the working set — the
number the tray is judged on — is handled by the trim instead.

Worth knowing: a drag re-imports the chain twice (monitor at the start, the
window at the end), so its accounting climbs ~100-160 MB per drag while device
memory does not move.

One `DXGI_ERROR_DEVICE_REMOVED` was seen in a scripted stress of the two-step
sizing: the settle shrink's `ResizeBuffers` succeeded, the `Present1` after it
came back with the device gone, and every frame after that retried the same
failing `ResizeBuffers` — which reads as a frozen window. Two things came out of
it. The shrink no longer runs while the window holds the mouse capture; and
`DxgiSwapchain` latches a removed device (`is_removed`), logs one error and stops
asking, so a removal degrades to a window that stops updating instead of one that
freezes. It has not been seen again in repeated stress, so the capture guard is a
precaution rather than a proven fix.

Tray actions run in the tray's window procedure, not through a channel, for the
same no-frames reason: an action queued for a frame would wait for the window it
was meant to open. The procedure calls the handler the window installs, so Show,
Restart and Quit all work while hidden — Quit wakes the loop through ZGUI's own
command waker.

Tray actions run in the tray's window procedure, not through a channel, for the
same no-frames reason: an action queued for a frame would wait for the window it
was meant to open. The procedure calls the handler the window installs, so Show,
Restart and Quit all work while hidden — Quit wakes the loop through ZGUI's own
command waker.

The window icon is set from the bundled `icon.ico` with `WM_SETICON` (a small
image for the title bar, a large one for Alt-Tab and the taskbar, each the size
the desktop asks for). The `.ico` rather than `icon.png` because the window wants
an `HICON` and the file already holds the images Windows asks for; the PNG would
need a decoder to become the RGBA rows `Surface::set_icon` takes.

What the window costs, measured rather than guessed (debug build, private
working set as Task Manager's Memory column reports it):

| what | private working set | commit | VRAM |
| --- | --- | --- | --- |
| a process that creates only this wgpu Vulkan device | 45 MB | 260 MB | 200 MB |
| the same window on ZGUI's own wgpu renderer | 133 MB | 466 MB | — |
| this window, `MemoryHints::Performance` (the default) | 138 MB | 418 MB | 474 MB |
| this window, `MemoryHints::Manual { 4..8 MiB }` | 134 MB | 228 MB | 247 MB |
| the same window hidden in the tray | ~1 MB | 228 MB | 247 MB |

So the presentation layer (`dxgi.rs`, `placebo.rs`) is about 5 MB of the working
set, and everything else is what a GPU UI is: the Vulkan instance and device the
driver sets up for any client, and ZGUI's renderer, text engine and scene, whose
own textures are 12 MB (composed target 3.1 MB, atlases 5.2 MB, buffers 0.6 MB,
interface 3.0 MB). The daemon has none of it: 4 MB, because it is sockets and
buffers.

`MemoryHints::Manual` with 4-8 MiB suballocation blocks is the allocator's own
half of that: `Performance`, the default, has gpu-allocator carve 128 MiB device
and 64 MiB host blocks, which is where the 190 MB of commit and 111 MB of VRAM
above came from. wgpu's `MemoryUsage` scores the same here; `Manual` states the
block size this UI actually wants.

The inner renderer's atlas textures are moved onto the replacement: the atlas*policy* (which tile holds which glyph) lives in the window's content cache,
above the renderer, and it believes every tile it placed is resident. A fresh
empty atlas would therefore draw the rectangles and nothing else — the scene
still names glyph tiles no texture backs. The colour space, HDR metadata and SDR
white point are decisions about the display and survive a resize; only a window
that moved to another output changes them, which is the next feature's problem.
There are no external textures to lose: RPC events are drained by a 100 ms
`set_interval` in the view, not by a surface pump. That interval checks the
channel before touching the state, so an idle tick costs nothing. The protocol is incremental — an `Update` names one
entity and carries it whole — so the signals *are* the state: `riffle::UiState`
holds one per thing the view draws and applies an update to the signal it names.
A snapshot exists only for the first connect and a reconnect. That is what makes
ZGUI's reactivity pay: a viewer count touches `channels` alone, and no closure
clones the login list to read the connection label.
HDR is off for
now (`HdrMode::Off`), but the SDR white point still comes from DisplayConfig;
the video pass and HDR transfer functions are the later work.

ZGUI's `surface` element is the video seam: a producer with its own cadence gets
the shared `wgpu::Device`/`Queue`, creates textures on that device and
`present`s them; ZGUI composites them as external textures. The SDR player can
use that directly. For HDR/DXGI, the UI is rendered offscreen and libplacebo
composites video + UI into an imported DXGI backbuffer. That path has no CPU
copies: ZGUI's normal full-frame GPU blit is the only copy, and it can be
removed by exposing the composed `SceneTexture`; the UI overlay is one
libplacebo pass. libplacebo imports the same Vulkan device via
`pl_vulkan_import`; it requires `hostQueryReset` and `timelineSemaphore`, which
wgpu 29.0.4 does not enable by default. ZGUI pins wgpu 29.0.4/ash 0.38, so the
existing video plan's wgpu 30.0.1 patch must be rebased. Write this up in a
separate `notes/video-decode.md` when the player work starts.
