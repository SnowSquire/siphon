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
- `ts-demux.md` — access units, codec config, timestamps, seek points.
- `rtsp-output.md` — RTP packetization, RTCP, SDP, sessions, DVR seeking.
- `daemon-mode.md` — daemon registry, capacity, stream lifecycle, session gotchas.
- `tls-client-api.md` — the HTTP/1.1 + rustls client (and why it is unbuffered).
- `hls-transport-probe.md` — measurements of how Twitch actually delivers.
- `disk-complete-recording.md` — the disk-recording design.
- `rpc.md` — the daemon RPC, Siphon backend/UI split, state updates and monitor lifecycle.
- `ui-design-direction.md` — the ZGUI surface: tokens, density, marquees, row states.
- `linux-port.md` — the UI on Linux: present path, buffer counts, build constraints,
  and what the hardware on the dev box can and cannot do.
- `windows-installer.md` — the per-user NSIS installer, the update hand-off and
  the FFmpeg/libplacebo payload the installer has to ship.
- `dxgi-hell.md` — the Windows DXGI band that is still open: the share-vs-blit
  seam, the measurements, the experiments already ruled out, and the rules.
- `video-decode.md` — the planned UI video path: VA-API / D3D12 Video, libav.
- `media-transport.md` — the planned replacement for RTSP between daemon and UI
  (idea only; nothing built).

## Commands

- Nix (both platforms from one host): `nix build .#linux` / `nix build
  .#windows` / `nix build .#windows-installer` / `nix develop` — hermetic Linux
  binaries, the cross-compiled Windows payload, and the setup + portable zip +
  symbols zip. The flake is the only build system for Windows: the release
  workflow installs Nix and calls it, so there are no xwin/shaderc/libplacebo
  shell scripts to keep in sync. `notes/nix-build.md` has the pieces and pins.
- Build/check: `cargo clippy -p daemon`, `cargo build -p daemon`
- Lint: `cargo clippy -p daemon --all-targets` — Cargo.toml enables `pedantic`/`nursery`/`suspicious`/`cargo` groups as warn; keep it clean. Whole-workspace lints (what CI runs) are `nix build .#checks.linux-clippy` and `.#checks.windows-clippy`, which reuse the dependency layer.
- Format: `cargo +nightly fmt` — `crates/daemon/rustfmt.toml` uses unstable options (import grouping/granularity, `reorder_impl_items`, LF) that stable `cargo fmt` silently ignores.
- One-channel recorder: `siphon-daemon record <login> [variant] [--mode live|dvr|both]
  [--list] --output <dir>` — one variant, no config, no RPC, no RTSP. Without
  `[variant]` it takes the highest-scoring one; `--list` prints the live master's
  variants and exits. `--mode` picks the live playlist, the DVR, or both (the
  default). It waits for the channel to go live and exits when the recording ends.
  In `-d` it needs `SIPHON_HERMES_URL` (the mock's `ws://127.0.0.1:8001/v1`).
- `cargo build --release` uses the quick profile (no LTO, 16 codegen units).
  The shipped binaries come from `nix build .#windows`, whose derivation sets
  `CARGO_PROFILE_RELEASE_LTO=fat` and `..._CODEGEN_UNITS=1`, so a Nix build is
  much slower than the local one and is worth backgrounding.
- Tests: `cargo test -p daemon` (arena/spool, playlist/utils/variant parsing, an HTTP client suite against a loopback server).
- Benchmark: `cargo test -p daemon --release -- --ignored --nocapture throughput` (demux/packetize over `captures/servermock`, the built fixtures). Cross-compile: `cargo zigbuild --release --tests -p daemon --target x86_64-unknown-linux-musl`.
- Miri: `cargo +nightly miri test -p daemon` — the check for the arena's `unsafe` blocks; network tests are `cfg(not(miri))`.
- Windows cross-check: `nix develop .#windows -c cargo clippy --workspace
  --all-targets` (or `nix build .#windows` for the payload). The shell exports
  the pinned MSVC CRT/SDK, `clang-cl`/`llvm-lib`/`lld-link`, libplacebo's prefix
  and `CARGO_BUILD_TARGET`, so no `--target` or hand-set environment is needed.
  A plain `cargo check -p daemon --target x86_64-pc-windows-msvc` still works
  against the global `~/.cargo/config.toml` + `~/.cache/xwin` setup. No Windows
  machine is involved.
- UI: `cargo run -p siphon-ui` (binary `siphon`, ZGUI + DXGI). It starts `siphon-daemon.exe` next to itself if the admin named pipe is not listening; the tray has Open / Restart service / Quit. `siphon.exe --restart-service` does the same process-level restart headlessly.
- Dev iteration against Nix: the Rust builds are split by crane
  (`nix/windows.nix`, `nix/linux.nix`) into a dependency layer plus one
  derivation per binary, so editing a crate rebuilds that crate, not the ~500
  dependencies, and the layers are shared across worktrees and CI. The payload's
  source is filtered to `Cargo.toml`/`Cargo.lock`/`crates/`, so doc edits do not
  invalidate it. For hot iteration use `nix develop .#windows -c cargo build`
  (incremental `./target`, quick profile — the LTO knobs are derivation-only).
  Never point `CARGO_TARGET_DIR` at a persistent directory for a Nix build:
  mtime-based freshness plus 1970-stamped sources yields stale binaries.
  `notes/nix-build.md` has the layer table.
- Installer: `nix build .#windows-installer` — stages the payload and runs
  `makensis` inside the derivation, producing the setup, the portable zip and
  the symbols zip in `result/`. `notes/windows-installer.md` has the NSIS
  contract; there is no packaging script any more.
- UI check (native): `cargo clippy -p siphon-ui --all-targets` — the Linux present path builds against the system libplacebo through pkg-config (`LIBPLACEBO_SOURCE=system` is the default there). Run it for any UI change; it does not need Windows.
- UI check (Windows target): `nix develop .#windows -c cargo clippy -p siphon-ui
  --all-targets`. The shell's libplacebo prefix is the one `nix/msvc.nix`
  builds, so there is no `win-prefix` under `~/Projects/Code/Libs` to rebuild
  when libplacebo or the Vulkan headers move: bump the pin in
  `nix/sources.nix` and Nix rebuilds it.

## Terminology

- **broadcast** — one Twitch channel's live stream (`stream_url` is the manifest URL, `stream_id` is Twitch's `BROADCAST-ID`).
- **slot** — fixed 192 512 B zeroed buffer in the arena. Never "chunk": *chunk* means an HTTP transfer-encoding chunk.
- **segment** — one HLS media playlist entry (~2 s), one HTTP fetch.
- **spool** — the byte stream of one segment: one `Writer`, any number of `Reader`s, backed by a chain of slots.
- **consumer** — the task/sink reading a spool (RTSP, disk). A consumer owns one `ConsumerWake`.
- **reader** — a consumer's cursor into a spool.

## Local dev loop (servermock)

`--debug <base>` (dev builds only; a release binary has no such flag) points GQL,
usher and OAuth at a mock edge such as `http://127.0.0.2:8000` and replays its
playlists. The mock's Hermes socket is on its own port, so point the daemon at it
with `SIPHON_HERMES_URL=ws://127.0.0.1:8001/v1`.

1. In `servermock/`: `pnpm install`, then `pnpm dev` (`node --watch main.ts`). It needs the built fixtures in `captures/servermock/` (`pnpm run build:fixtures`; ~2.8 GB, gitignored — `SEGMENTS_DIR` points it elsewhere). `SPEED=8` by default (`SPEED=1` for a real-time RTSP test); it ends with `#EXT-X-ENDLIST`, and `touch servermock/main.ts` restarts it from the top.
2. `cargo run -p daemon -- play -c anything -d http://127.0.0.2:8000` — `-c` is required even in debug mode; broadcast id `0`.
3. `play` serves `rtsp://127.0.0.1:7417/` and `rtsp://[::1]:7417/`; `daemon` binds `--bind` (default `127.0.0.1:7417`) and serves `rtsp://host/<channel>/<quality>/`; DESCRIBE creates the stream.
4. `ffplay -nodisp rtsp://127.0.0.1:7417/` and `ffprobe -rtsp_transport tcp -i rtsp://127.0.0.1:7417/` are the end-to-end checks (`.../sunnys/1080p/` under `daemon -d http://127.0.0.2:8000`).
5. `cargo run -p siphon-ui` opens the Siphon window against the running daemon; it starts one when the admin port is idle.

## Worktrees

OpenCode v2 has no worktree setup hook (`worktree.directory` is the only
setting), so a fresh worktree has no media and no node deps. Run the script
once, by hand, right after `worktree_create`:

```
OPENCODE_WORKTREE_BASE=<main checkout> OPENCODE_WORKTREE_PATH=<new worktree> \
  bash <new worktree>/scripts/worktree-setup.sh
```

It clones `captures/` copy-on-write with `--reflink=auto` (btrfs: no extra
blocks, and writes stay private), hardlinks it on Windows, and runs
`pnpm install`.

## Architecture

Single-threaded `compio` reactor (`#[compio::main]`): everything is a task on one
thread, so there are no locks, atomics or `Send` bounds. `!Send` tasks are
deliberate — do not "fix" `clippy::future_not_send`. Pipeline and listener
detail: `notes/architecture.md`.

- `arena.rs`, `spool.rs` — slots and spools, intrusive refcounts, zero-copy `IoBuf` views; the design rationale is in the module docs.
- `http/` — hand-rolled HTTP/1.1 over `rustls::unbuffered` (`notes/tls-client-api.md`); `http/ws.rs` is the RFC 6455 client Hermes uses.
- `scheduler.rs` — manifest poll, Twitch tags, fetch dispatch, consumer fan-out; the `#EXT-X-TWITCH-*` quirks are in `notes.txt`.
- `variant.rs` — master-playlist resolution through Twitch GQL + usher.
- `config.rs`, `gql.rs`, `hermes.rs`, `matcher.rs`, `notifier.rs`, `update.rs` — the Siphon backend: persisted config, GQL, Hermes pubsub, word filter, toasts/webhook, update check (`notes/rpc.md`).
- `logging.rs` — the daemon's stdout/stderr, appended to `daemon.log` in the
  per-user state dir (`%LOCALAPPDATA%`, `~/.local/state`) and rotated at 4 MiB.
- `app.rs`, `bus.rs`, `rpc.rs`, `metrics.rs` — the backend app loop, entity update bus, admin RPC server and telemetry.
- `registry.rs` — the stream registry and monitors behind `daemon` mode (`notes/daemon-mode.md`).
- `downloader.rs` — bounded fetch workers streaming into spools.
- `rtsp/` — RTSP server and ES over RTP (`notes/rtsp-output.md`; demux is `notes/ts-demux.md`). Sink only, and slated to become a compile-time optional feature: keep it correct, but do not invest in its hot path.
- `disk.rs` — complete-broadcast recording: live runs, DVR holes, v4 index (`notes/disk-complete-recording.md`).
- `crates/shared` (`siphon-shared`) — what both sides agree on: `rpc/` (the
  typed protocol, postcard framing, the UI-side client), `settings/` (the
  per-client settings schema) and `store/` (the JSON file layer).
- `crates/ui` (`siphon-ui`, binary `siphon`) — the ZGUI window presented through the DXGI flip chain ported from the video spike, the native tray, the RPC client worker, and the daemon lifecycle (auto-start, PID-file restart). Windows-only; `notes/rpc.md` has the split and the SDR/HDR state.

## Rules (do not "simplify")

1. One thread, one reactor. Never block it: a blocking channel send is a deadlock, not "slow".
2. Unregister a consumer only on `is_closed()`; `try_send`'s "full" is not "closed".
3. One `ConsumerWake` per consumer, woken on **publish**, never only on close.
4. `.instrument(span)`, never `enter()` held across an `.await`.
5. `Reader: Clone` duplicates the cursor and never rewinds; never clone an advancing consumer's reader to replay.
6. Loopback only: `socket2`, v4+v6 on the same port, never `[::]`; `AddrInUse` moves the pair to ephemeral ports.
7. A compio `read` learns its length from the **capacity** of the buffer it is handed, not from a count: a `Vec::new()` has none, so the read returns 0 and looks like EOF. `reserve` capacity before handing it over — on the buffer you already own (`mem::take`, `clear`, `reserve`), never by allocating one per read; `reserve` is a no-op when it is already big enough.
8. Every panic is fatal (`abort_on_panic`); `main` returns a `Result`.
9. The manifest poll is the only polling; everything else is event-driven.
10. `crates/daemon/src/index.hexpat` mirrors the on-disk format; keep both in sync.
11. Allocate in setup, not in the loop. A `Vec::with_capacity` on a path that runs per packet or per frame is a defect: reuse a buffer the task or struct owns (`mem::take` → `clear` → `reserve`, or a completion that hands the buffer back), or use a fixed array sized by the format's structural bound. `downloader::fetch` is the model — `writer.tail()` hands it arena capacity, so a segment is read once, straight into its slot. Per-segment and control-path allocations are fine.

## Repo conventions

- Colocated `jj` + git repo; work with `jj`, not git. Open a commit at the start of a task, commit each step, `jj squash` back down; `jj undo` recovers.
- Name a commit by its **change ID** (`change_id.shortest(8)`, e.g. `kkmppsq`), not by its git commit hash. A hash names one snapshot and changes on every rewrite, so it is useless to whoever wants to find or amend the work; `jj show <change>` / `jj edit <change>` only work with the change ID. Quote a hash only when that exact snapshot is the point.
- Read history with an explicit revset: `jj chain` (the `fork_point(@ | trunk())::@` alias), `jj log -r '::@'`, or `trunk()..@` when trunk is known. Bare `jj log` interleaves every head, so its order is not a chain.
- Push only described commits. If `jj git push` refuses one, describe it (`jj describe -r <change>`; add `--ignore-immutable` when it is already published) and push again. Never use `--allow-empty-description`.
- Style: efficient code, not borrow-checker appeasement; few dependencies (`compio`, `rustls`, `http`, `httparse`, `futures-util`); fixed-size storage when the bound is structural, `Vec` only when it is data-dependent (pre-reserve).
