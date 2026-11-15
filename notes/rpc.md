# Daemon RPC and the Siphon backend/UI split

Status: phases 0–2 backend landed. `crates/rpc` (`siphon-rpc`) owns the typed
protocol, postcard framing and client; `crates/daemon` owns config, GQL,
Hermes, the app loop, monitors, telemetry and the admin RPC server. The UI is
now `crates/ui` (ZGUI, binary `siphon`) and talks only over the RPC. It presents
through the video spike's DXGI flip chain and libplacebo pass (SDR for now; the
HDR path is kept dormant), starts the daemon when the admin port is idle and
restarts it at the process level (PID file + terminate), never through the
socket: a frozen daemon must still be restartable from the tray.

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
  `AddFilteredWord`, `RemoveFilteredWord`, `SetRecord { channel, record }`,
  `ClearError`, `DismissNotification`, `ApplyUpdate`, `PipelineStop { key,
  force }`, `PipelineRestart(StreamKey)`, `DisconnectViewers(StreamKey)`,
  `RecordOnce { login, quality }`;
- queries: `GetSnapshot`, `GetStreamDetail { key }`, `ListSegments { key,
  limit }`, `GetArena`, `GetDownloader`, `GetDisk { key }`.

An intent returns an ack or a typed `RpcError`; the resulting state change
arrives as an `Update`. The UI may optimistically render a toggle, but the
backend remains authoritative.

## Ordering and backpressure

- One reader task per connection handles requests; one writer task drains a
  bounded outbound queue. Control responses and updates share that queue: a
  writer that selected between two receive queues would drop an already-
  completed receive when both became ready. Never block the reactor: `try_send`
  only, exactly like RTSP `Session::out`.
- The app loop's own event wait is lossless too: senders wake a `ConsumerWake`
  and the loop drains with `try_recv`; background jobs are delivered through
  the same queue. The loop's biased select puts the Hermes socket read first,
  so a completed websocket read is never dropped for a queued request.
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
- `crates/ui` (`siphon-ui`, binary `siphon`): the ZGUI window presented through
  the DXGI/libplacebo renderer, the native tray and the RPC worker. It owns the daemon lifecycle (`service.rs`): auto-start on launch and
  a process-level restart (`daemon.pid` in the app data dir, image-name check,
  `taskkill` fallback). `crates/tray` is obsolete.
- `play` keeps its current pipeline and needs no protocol types.

## Phases

0. **Protocol crate**: messages, postcard codec, incremental decoder, frame
   limits, Hello/Welcome, named-pipe/socket client/server tests. **Done.**
1. **Backend state**: config load/save/migrate, GQL, Hermes, matcher,
   notifications; snapshot and entity updates. **Done** (the UI is still
   external, so it does not render them yet).
2. **Pipelines**: monitor-only runtimes, upgrade on DESCRIBE, capacity rules,
   segment ring, arena/downloader telemetry, `ListSegments`/`GetStreamDetail`.
   **Done**, except `GetDisk` (returns `Unsupported`).
3. **Full lifecycle**: monitor add/remove/update, stop/restart, forced viewer
   disconnect, per-monitor record start/stop. **Done**, including one-shot
   `RecordOnce` and `PipelineStop { force }`; the per-channel `record` flag
   starts and stops the disk on a live runtime.
4. **Disk and polish**: run introspection/release, update ownership,
   multi-client behavior, update throttling/coalescing. **Mostly done**:
   `GetDisk` reports live recorder counters and an endpoint test covers two UI
   clients; releasing finished runs by hand and per-run history are open.

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
entity and carries it whole — so the signals *are* the state: `rpc::UiState`
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
separate `notes/video.md` when the player work starts.
