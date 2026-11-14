# Daemon RPC and the Siphon backend/UI split

Status: phases 0–2 backend landed. `crates/rpc` (`siphon-rpc`) owns the typed
protocol, postcard framing and client; `crates/daemon` owns config, GQL,
Hermes, the app loop, monitors, telemetry and the admin RPC server. The UI has
not moved into this workspace yet. A loopback test covers a monitor add, a
pipeline update and a pipeline stop over RPC.

## Goal

The whole app is Siphon. The long-lived process is the **Siphon backend** (today
`daemon`); the tray/window is the **Siphon UI**. The backend owns all behaviour
and the UI is a dumb renderer:

- backend: config load/save/migrate, GQL resolution, Hermes, live/title
  decisions, filtered words, notifications, monitor list, pipelines, disk,
  telemetry;
- UI: render the latest state, send user intents, show the notification center
  and tray. No GQL, no Hermes, no config writes, no decisions.

The two talk over one typed RPC. The protocol types live in one crate so the
compiler catches drift; no field layout is duplicated between client and server.

```
Siphon UI (ZGUI)  ── typed RPC ──  Siphon backend (compio)
                                      ├─ app state (config, channels, notifications)
                                      ├─ GQL + Hermes
                                      ├─ monitors → pipelines (scheduler, disk, media)
                                      └─ RTSP listener
```

`play` stays a separate CLI mode and does not serve RPC.

## Transport

- TCP on loopback, separate from RTSP: `daemon --admin-bind`, default
  `127.0.0.1:7418`. RTSP `--bind` may be `0.0.0.0`; the admin surface must not
  follow it. `--no-admin` disables it.
- No HTTP, WebSocket or gRPC: the client and server are both compio in this
  workspace, and the only missing part is an envelope. No auth on loopback.
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
    notifications: Vec<NotificationView>,
    error: Option<String>,
    update: Option<UpdateView>,
}

enum Update {
    Config(Config),
    Conn(ConnHealth),
    ChannelUpsert(ChannelView),
    ChannelRemove(u64),
    Pending(Vec<String>),
    PipelineUpsert(PipelineView),
    PipelineRemove(StreamKey),
    Segment { key: StreamKey, record: SegmentRecord },
    Arena(ArenaSummary),
    Downloader(DownloaderSummary),
    Notification(NotificationView),
    NotificationDismissed(u64),
    Error(Option<String>),
    Update(Option<UpdateView>),
}
```

`ChannelView` carries what the UI renders: id, login, display name, avatar,
live, title, game, viewers, stream start. `PipelineView` carries key, state
(live/offline/stopped), pinned, recording, sessions, position, live edge,
segment tail, window size, counters and last error. `SegmentRecord` is only
scalars; the segment ring is queryable and updates append to it.

Requests:

- intents: `AddLogin`, `RemoveChannel`, `SetNotifyTitleChanges`, `SetSound`,
  `AddFilteredWord`, `RemoveFilteredWord`, `ClearError`, `DismissNotification`,
  `MonitorAdd { login, quality, record }`, `MonitorRemove { key, force }`,
  `MonitorUpdate { key, record, quality }`, `PipelineStop { key, force }`,
  `PipelineRestart { key }`, `DisconnectViewers { key }`, update actions;
- queries: `GetSnapshot`, `GetStreamDetail { key }`, `ListSegments { key,
  limit }`, `GetArena`, `GetDownloader`, `GetDisk { key }`.

An intent returns an ack or a typed `RpcError`; the resulting state change
arrives as an `Update`. The UI may optimistically render a toggle, but the
backend remains authoritative.

## Ordering and backpressure

- One reader task per connection handles requests; one writer task drains a
  bounded outbound queue and the per-connection control queue. Never block the
  reactor: `try_send` only, exactly like RTSP `Session::out`.
- A slow UI is disconnected, never queued indefinitely. It reconnects and
  resyncs from `Snapshot`.
- Updates are published through an event bus of weak subscribers
  (`RefCell<Vec<Weak<Subscriber>>>`, same shape as `WakeSet`). Publishing must
  not keep a stream or a pipeline alive.
- No `RefCell` borrow is held across an `.await` in a handler.

## Backend app

The registry stops being the top-level owner. A new app/backend struct owns the
Siphon `Worker` logic and the registry as a subsystem; the RPC and RTSP
listeners are tasks around it.

### Config

- The daemon owns `Config` (channels, filtered words, notification
  preferences, sound, recording defaults) and its migrations.
- Reuse Siphon's `%APPDATA%\com.iken.siphon\config.json` schema and path so
  existing installs keep their channels; the UI stops touching the file.
- Save asynchronously on compio. The UI receives config in the snapshot and
  mutates it only through intents.

### GQL, Hermes and notifications

- Port Siphon's GQL and Hermes code into the daemon. compio already has
  `ws`/`ws-connect`; no second runtime.
- Hermes `stream-up` activates the desired monitor; `stream-down` stops the
  pipeline but keeps the monitor; title/game/viewcount events update
  `ChannelView`s and drive the matcher.
- Notifications are decided and displayed by the daemon, so they work with the
  UI closed. The OS toast (Windows) and the webhook are outputs; the
  notification center is daemon state and is rendered from the snapshot.
- Until Hermes lands in the daemon, the same API works with an external bridge:
  a monitor is desired state, and activation is an explicit request/event.

### Monitors and pipelines

A monitor is desired state, not a stream. The key is `(login, quality)` exactly
like `StreamKey`; changing quality is remove + add.

- `Registry` keeps `monitors: HashSet<StreamKey>` in addition to live
  runtimes.
- A monitor-only runtime runs the scheduler and optional disk, but **no media
  engine and no DVR refresh**. Segments are downloaded and discarded; this is
  the "which segments arrived" view.
- On DESCRIBE, `Runtime::upgrade()` creates the `Dvr` if needed and spawns the
  media engine with the existing registration sender, `WakeSet`, live window
  and reader wake. `media_spawned: Cell<bool>` makes it idempotent; DESCRIBE's
  `wait_ready` covers the join.
- `ensure_capacity`, `release_stream`, `finish_stream` and `prune_channel` must
  treat pinned/monitored runtimes like recording ones: never evict, never drop
  the channel entry while a monitor exists.
- When `--max-streams` is full, `MonitorAdd` fails with a distinct status. A
  viewer is never evicted to make room.
- Offline at activation: keep the desired monitor and its status; retry on the
  next Hermes event or `PipelineRestart`. No new polling loop in v1.

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

- `crates/rpc`: serde message types, `postcard` codec, frame decoder, compio
  client. No ZGUI and no daemon internals, so the UI and the daemon can share
  it. Package `siphon-rpc`.
- `crates/daemon`: the backend app and the RPC server module; `registry.rs`
  owns pipelines, `app.rs` owns everything else.
- UI, still external: the Slint app moves in as `crates/ui` (`siphon-ui`) with
  a binary named `siphon`, depending on `siphon-rpc`. `crates/tray` goes away
  when it lands.
- `play` keeps its current pipeline and needs no protocol types.

## Phases

0. **Protocol crate**: messages, postcard codec, incremental decoder, frame
   limits, Hello/Welcome, loopback client/server tests. **Done.**
1. **Backend state**: config load/save/migrate, GQL, Hermes, matcher,
   notifications; snapshot and entity updates. **Done** (the UI is still
   external, so it does not render them yet).
2. **Pipelines**: monitor-only runtimes, upgrade on DESCRIBE, capacity rules,
   segment ring, arena/downloader telemetry, `ListSegments`/`GetStreamDetail`.
   **Done**, except `GetDisk` (returns `Unsupported`).
3. **Full lifecycle**: monitor add/remove/update, stop/restart, forced viewer
   disconnect, per-monitor record start/stop. **Open**; stop/restart exists,
   forced disconnect and ephemeral recording do not.
4. **Disk and polish**: run introspection/release, update ownership,
   multi-client behavior, update throttling/coalescing. **Open.**

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

## Open questions

- Update checking/applying: daemon or UI? Applying an MSI restarts processes.
- Launch model: daemon autostart at login (needed for notifications with the UI
  closed) versus the UI starting it on demand.
- Multiple UI clients: the protocol supports it, but only one tray/window is
  expected; decide whether to enforce single-instance at the RPC layer.
- Disk API shape: run listing, release semantics and canonical progress.
- Per-stream slot reservation (already open in `rtsp-daemon.md`).
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
- Quality is part of `StreamKey`; a quality change is remove + add, with
  `force` if the old viewer must go.
- The UI never writes config; the daemon acks an intent and publishes the
  resulting config update.
- Hermes viewcount traffic can be frequent; coalesce `ChannelUpsert` per channel
  before the writer.

## UI and video context

The UI is planned to move from Slint to ZGUI (fine-grained `reactive_graph`
signals), so `Snapshot`/`Update` map directly onto per-entity signals; the
protocol stays free of ZGUI types.

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
separate `notes/video.md` when the player work starts.
