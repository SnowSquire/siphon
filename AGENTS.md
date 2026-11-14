# AGENTS.md

Twitch HLS live-stream client (Rust, single-threaded `compio` reactor) plus
`servermock/`, a Node mock of Twitch's edge used to test it offline. RTSP is the
only sink; disk output is disabled and being rebuilt from
`notes/disk-complete-recording.md`.

## Who writes what

- `AGENTS.md` — this contract. Read it first; keep it short.
- `notes.txt`, `todo.md` — the human maintainer's scratch. Read them freely;
  **ask before editing** (adding an item when asked is fine; never "tidy" them).
- `notes/` — the agent's design docs. Read the relevant one before touching a
  subsystem, and update it when the design changes.

## Notes

- `architecture.md` — pipeline, scheduler, downloader, daemon, listeners.
- `slot-arena-rework.md` — slots, arenas, refcounts, spools, wakeups.
- `es-rtp-rtsp.md` — TS demux, packetizers, SDP, timestamps, seeking.
- `rtsp-daemon.md` — daemon registry, sessions, queues, SR/epoch, handover.
- `tls-client-api.md` — the HTTP/1.1 + rustls client (and why it is unbuffered).
- `hls-transport-probe.md` — measurements of how Twitch actually delivers.
- `disk-complete-recording.md` — the disk-recording design.
- `rpc.md` — the daemon RPC, Siphon backend/UI split, state updates and monitor lifecycle.

## Commands

- Build/check: `cargo clippy -p daemon`, `cargo build -p daemon`
- Lint: `cargo clippy -p daemon --all-targets` — Cargo.toml enables `pedantic`/`nursery`/`suspicious`/`cargo` groups as warn; keep it clean.
- Format: `cargo +nightly fmt` — `crates/daemon/rustfmt.toml` uses unstable options (import grouping/granularity, `reorder_impl_items`, LF) that stable `cargo fmt` silently ignores.
- Tests: `cargo test -p daemon` (arena/spool, playlist/utils/variant parsing, an HTTP client suite against a loopback server).
- Benchmark: `cargo test -p daemon --release -- --ignored --nocapture throughput` (demux/packetize over `servermock/segments`). Cross-compile: `cargo zigbuild --release --tests -p daemon --target x86_64-unknown-linux-musl`.
- Miri: `cargo +nightly miri test -p daemon` — the check for the arena's `unsafe` blocks; network tests are `cfg(not(miri))`.
- UI: `cargo run -p siphon-ui` (binary `siphon`). It starts `daemon.exe` next to itself if `127.0.0.1:7418` is not listening; the tray has Open / Restart service / Quit. `siphon.exe --restart-service` does the same process-level restart headlessly.

## Terminology

- **broadcast** — one Twitch channel's live stream (`stream_url` is the manifest URL, `stream_id` is Twitch's `BROADCAST-ID`).
- **slot** — fixed 192 512 B zeroed buffer in the arena. Never "chunk": *chunk* means an HTTP transfer-encoding chunk.
- **segment** — one HLS media playlist entry (~2 s), one HTTP fetch.
- **spool** — the byte stream of one segment: one `Writer`, any number of `Reader`s, backed by a chain of slots.
- **consumer** — the task/sink reading a spool (RTSP, disk). A consumer owns one `ConsumerWake`.
- **reader** — a consumer's cursor into a spool.

## Local dev loop (servermock)

`--debug` bypasses Twitch and fetches `http://127.0.0.2:8000/index.m3u8`.

1. In `servermock/`: `pnpm install`, then `pnpm dev` (`node --watch main.ts`). It needs real files in `servermock/segments/` (~556 MB, gitignored); the worktree hook hardlinks `segments/` and `captures/` from the base checkout. `SPEED=8` by default (`SPEED=1` for a real-time RTSP test); it ends with `#EXT-X-ENDLIST`, and `touch servermock/main.ts` restarts it from the top.
2. `cargo run -p daemon -- play -c anything -d` — `-c` is required even in debug mode; broadcast id `0`.
3. `play` serves `rtsp://127.0.0.1:7417/` and `rtsp://[::1]:7417/`; `daemon` binds `--bind` (default `127.0.0.1:7417`) and serves `rtsp://host/<channel>/<quality>/`; DESCRIBE creates the stream.
4. `ffplay -nodisp rtsp://127.0.0.1:7417/` and `ffprobe -rtsp_transport tcp -i rtsp://127.0.0.1:7417/` are the end-to-end checks (`.../sunnys/1080p/` under `daemon -d`).
5. `cargo run -p siphon-ui` opens the Siphon window against the running daemon; it starts one when the admin port is idle.

## Architecture

Single-threaded `compio` reactor (`#[compio::main]`): everything is a task on one
thread, so there are no locks, atomics or `Send` bounds. `!Send` tasks are
deliberate — do not "fix" `clippy::future_not_send`. Pipeline and listener
detail: `notes/architecture.md`.

- `arena.rs`, `spool.rs` — slots and spools, intrusive refcounts, zero-copy `IoBuf` views (`notes/slot-arena-rework.md`).
- `http/` — hand-rolled HTTP/1.1 over `rustls::unbuffered` (`notes/tls-client-api.md`); `http/ws.rs` is the RFC 6455 client Hermes uses.
- `scheduler.rs` — manifest poll, Twitch tags, fetch dispatch, consumer fan-out; the `#EXT-X-TWITCH-*` quirks are in `notes.txt`.
- `variant.rs` — master-playlist resolution through Twitch GQL + usher.
- `config.rs`, `gql.rs`, `hermes.rs`, `matcher.rs`, `notifier.rs`, `update.rs` — the Siphon backend: persisted config, GQL, Hermes pubsub, word filter, toasts/webhook, update check (`notes/rpc.md`).
- `app.rs`, `bus.rs`, `rpc.rs`, `metrics.rs` — the backend app loop, entity update bus, admin RPC server and telemetry.
- `registry.rs` — the stream registry and monitors behind `daemon` mode (`notes/rtsp-daemon.md`).
- `downloader.rs` — bounded fetch workers streaming into spools.
- `rtsp/` — RTSP server and ES over RTP (`notes/es-rtp-rtsp.md`).
- `disk.rs` — complete-broadcast recording: live runs, DVR holes, v4 index (`notes/disk-complete-recording.md`).
- `crates/rpc` (`siphon-rpc`) — the typed protocol, postcard framing and the UI-side client.
- `crates/ui` (`siphon-ui`, binary `siphon`) — the Slint window and tray, the RPC client worker, and the daemon lifecycle (auto-start, PID-file restart). Windows-only. `notes/rpc.md` has the split; the ZGUI rewrite uses the `video` crate's scaffolding later.

## Rules (do not "simplify")

1. One thread, one reactor. Never block it: a blocking channel send is a deadlock, not "slow".
2. Unregister a consumer only on `is_closed()`; `try_send`'s "full" is not "closed".
3. One `ConsumerWake` per consumer, woken on **publish**, never only on close.
4. `.instrument(span)`, never `enter()` held across an `.await`.
5. `Reader: Clone` duplicates the cursor and never rewinds; never clone an advancing consumer's reader to replay.
6. Loopback only: `socket2`, v4+v6 on the same port, never `[::]`; `AddrInUse` moves the pair to ephemeral ports.
7. A completion read learns its size from the buffer capacity; `reserve` a fresh `Vec` first.
8. Every panic is fatal (`abort_on_panic`); `main` returns a `Result`.
9. The manifest poll is the only polling; everything else is event-driven.
10. `crates/daemon/src/index.hexpat` mirrors the on-disk format; keep both in sync.

## Repo conventions

- Colocated `jj` + git repo; work with `jj`, not git. Open a commit at the start of a task, commit each step, `jj squash` back down; `jj undo` recovers.
- Style: efficient code, not borrow-checker appeasement; few dependencies (`compio`, `rustls`, `http`, `httparse`, `futures-util`); fixed-size storage when the bound is structural, `Vec` only when it is data-dependent (pre-reserve).
