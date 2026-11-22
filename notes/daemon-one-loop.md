# Daemon: one loop, one owner

Status: the first two passes are implemented and committed in the
`daemon-one-loop` worktree (`otwutqxw`, `qullqoys`). `cargo test -p daemon` is
141 green, clippy is clean, and the mock's new routes were exercised by hand with
curl. This is the plan we settled on in the design conversation, written down so
the reasoning survives the session that produced it.

**MORNING: read § "The runtime-struct question" at the end. That is the one you
said you were too tired to work through, and it is the only open decision left.**

## The shape

Everything that owns state runs in **one loop**: config, channel data, the stream
registry, the admin connections, Hermes, notifications, timers. Spawned tasks are
only the things that genuinely have their own state: per-stream scheduler, media
engine, DVR refresher, disk recorder, downloader workers, HTTP client. They talk
to the loop through the bus (updates) and kanal queues (work).

That is the whole point: a synchronous call is possible because there is one
owner. `App`/`AppHandle`/`EventSender`/`AppEvent` are gone. An admin connection
is a slot in a `Vec<Option<Conn>>` owned by the loop, not a task.

### Connections

- The listener task does one thing: `accept`, then push the raw stream into a
  kanal queue.
- The loop holds `rpc::Conn { stream: Rc<AdminStream>, decoder, out: Vec<u8>,
  ready, writing }`.
- Two operations per connection, armed one at a time: `rpc::read` and
  `rpc::write`. They own an `Rc<AdminStream>` and the buffer, and return both, so
  nothing in the loop is borrowed across an `await`.
- `FuturesUnordered<Op>` holds connection ops, the bus wake, and jobs. The loop
  `select!`s that set against the Hermes socket read and one deadline future.
  The socket cannot move into the set: one `WsStream` serves both directions and
  a send must not queue behind a read.

### Snapshot

`Daemon::snapshot(&self) -> Snapshot` is a plain function returning an owned
value. On `Hello` the loop builds it and queues `Welcome` + `Snapshot` on that
connection. No reply channel, no timeout, no round trip.

### Bus

One queue, not one per connection. `Bus::publish` coalesces by entity and wakes
the loop; the loop drains, encodes each update **once**, and appends the same
bytes to every ready connection's `out`. Backpressure is `out.len() > MAX_OUT` →
drop the connection; the UI resyncs from a snapshot. The 512-line log ring stays
a separate snapshot-only structure.

### Registry

- `channels: Vec<Channel>` — one row per channel the registry serves, with
  `login`, `record` (the persisted preference, mirrored from config), `stopped:
  Option<NonZeroU64>` (the broadcast id the user stopped) and lazily fetched
  `data: Option<Rc<ChannelData>>` (title, variants, broadcast id).
- `streams: HashMap<StreamKey, Rc<StreamRuntime>>` — `(login, quality)` for now.
- Deleted: `monitors`, `once`, `suppressed`, `max_streams`, `ensure_capacity`,
  the capacity eviction, the `debug` flag.
- `set_record(login, bool)` is the whole monitor concept: the bool is the intent.
  `monitor_live(login)` is what Hermes stream-up and the reconnect baseline call.
  `record_once(key)` spawns a recording now and stores nothing. `stop_pipeline`
  writes the current broadcast id into `stopped`; a restream has a new id, so it
  is not suppressed.
- The gate (one async mutex per key) is keyed by `xxh3(login) ^ xxh3(quality)`,
  so no string is allocated per resolve.
- `StreamRuntime.recording` is `RefCell<Option<Recorder { disk, metrics }>>`, so
  "recording but no metrics" is unrepresentable.

### Scheduler

Poll only while someone is reading. When the last consumer leaves (viewers and
the disk's pre-fill registration), the scheduler parks on the registration
channel instead of polling and fetching. That is the fix for a recording-only
stream downloading every live segment for nobody. The registration sender stays
open, so a later viewer resumes it without respawning.

### Timers

No 1-second tick. The loop computes the nearest deadline — Hermes welcome /
keepalive, reconnect, update check, telemetry flush — and sleeps exactly that
long. Telemetry is still needed only while a UI is connected, so it is a deadline
armed at 1 Hz only then.

### Constants

Tunables live in `constants.rs`: segment ring, RPC backlog cap, log cap,
notification cap, edge poll bounds, scheduler backoff, Hermes liveness, update
cadence. Format-local values (TS packet size, NAL types, index magic) stay in
their modules.

### Debug

`--debug` means "the endpoints point at `servermock`", nothing else. The
behavioural branches go: fake variants, seeded channels, skipped GQL, faked
connection health, forced DVR URLs. The mock grows a GQL route for the queries
`gql.rs` actually makes. `--debug` is compiled out of release builds
(`cfg(debug_assertions)`).

### Disk and demux

- Disk: the live registration already ends at handoff (`drain_live`). The missing
  half was that the live scheduler kept polling and fetching afterwards; the
  scheduler parking above fixes it.
- Demux: a flat sample store — one byte buffer plus records of ranges, with AU
  buffers recycled — instead of one `Vec` per access unit, one `Vec` per NAL list
  and one `Vec` per AAC frame. Not started.

## What landed

- `otwutqxw` — one loop owns the backend: connections as slots with one read and
  one write op each, synchronous snapshot, one coalesced bus queue encoded once
  per update, registry without the sets/cap, constants moved, no tick.
- `qullqoys` — the scheduler parks with no consumers and resumes at the live
  edge; `--debug` is only endpoint selection (GQL, usher, OAuth, Hermes), and
  servermock answers `UserByLogin`, `UsersByIds`, `PlaybackAccessToken`, the
  usher master path and an avatar.
- The RPC round-trip test now drives the real listener, the real daemon loop and
  the real registry over a Unix socket.

## What is not decided / not done

- Stream identity is still `(login, quality)` internally. Keying by channel id
  needs a login→id lookup in the resolve path, which `get_variants` does not
  provide; it is blocked on a GQL query (or usher data) that returns it.
- Demux sample store (flat buffer + ranges, recycled AU buffers) not started.
  The existing per-AU allocation and per-AAC-frame `to_vec()` are still there.
- `notes/architecture.md`, `notes/rpc.md` and `notes/daemon-mode.md` still
  describe the old loop, the per-connection queues and the monitor sets.
- The Windows named-pipe listener path was rewritten by analogy and not built or
  run on this box.

---

## The runtime-struct question

This is the one that got skipped. The question is what the per-stream runtime
struct holds after all of the above, and specifically whether its independent
booleans should become a state enum.

Today (`StreamRuntime`) it is an AoS of cells: `recording: Cell<bool>`,
`disk: RefCell<Option<Rc<DiskOutput>>>`, `disk_metrics:
RefCell<Option<Rc<DiskMetrics>>>`, `media_spawned: Cell<bool>`,
`dvr_spawned: Cell<bool>`, plus `debug` and `broadcast_id`.

The combinations those fields allow but never mean are the problem: recording
true with no disk, a disk with no metrics, a media engine spawned with no
registration, two spawn flags whose lifetimes are unrelated.

Three options:

1. **Collapse the coupled ones** — `recording: Option<Recorder { disk, metrics }>`
   and a spawn token per task. Two independent booleans stay two booleans.
   Cheapest, removes the invalid combinations that actually occur.
2. **A state enum** — `enum Runtime { Watching, Recording, Both }` and make each
   task's existence derive from the state. Strongest invariants, but the states
   are not really exclusive: viewers and recording are independent, and the
   second orientation exists only while the primary records. The enum would need
   variants that encode the product anyway.
3. **Leave the fields** and only delete what the rest of the plan deletes
   (`debug`, `broadcast_id`, the sets).

Recommendation, for the morning: **1**. `Option<Recorder>` is the real coupling
(the disk and its metrics are one thing), and the spawn flags are genuinely
independent facts about two different tasks, not states of a machine. The enum in
(2) buys nothing that the type system was not already giving us, and it would
have to model the dual-orientation case as an extra variant to stay honest.
