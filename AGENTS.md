# AGENTS.md

Twitch HLS live-stream client (Rust) + `servermock/` (Node mock Twitch edge server) used to test it offline. No README; `todo.md` and `notes.txt` are the maintainer's live scratch notes — read them before touching scheduler/disk logic. `notes/` has the longer design documents (transport probe, slot arena / RTSP direction, TLS client API).

## Commands

- Build/check: `cargo clippy -p twitch-hls-client2`, `cargo build -p twitch-hls-client2`
- Lint: `cargo clippy -p twitch-hls-client2 --all-targets` — Cargo.toml enables `pedantic`/`nursery`/`suspicious`/`cargo` groups as warn; keep it clean.
- Format: `crates/twitch/rustfmt.toml` uses **unstable** rustfmt options (`group_imports`, `imports_granularity`, `imports_layout`, `format_code_in_doc_comments`, `reorder_impl_items`) and pins LF; stable `cargo fmt` silently ignores them. Use `cargo +nightly fmt`.
- Tests: `cargo test -p twitch-hls-client2` (arena/spool, playlist/utils/variant parsing, and an HTTP client suite against a loopback mock server).
- Miri: `cargo +nightly miri test -p twitch-hls-client2` runs everything except the network tests (`cfg(not(miri))`); it is the check for the arena's handful of `unsafe` blocks.
- `servermock/unused/main_test.ts` is stale boilerplate — don't rely on it.

## Terminology

- **broadcast** — one Twitch channel's live stream. (Was "stream"; `stream_url` is the `manifest_url`, `stream_id` is Twitch's `BROADCAST-ID`.)
- **slot** — fixed 192 512 B zeroed buffer in the arena. Never "chunk": *chunk* means an HTTP transfer-encoding chunk.
- **segment** — one HLS media playlist entry (~2 s), one HTTP fetch.
- **spool** — the byte stream of one segment: one `Writer`, any number of `Reader`s, backed by a chain of slots.
- **consumer** — the task/sink reading a spool (disk, each TCP client). A consumer owns one `ConsumerWake`.
- **reader** — a consumer's cursor into a spool.

## Local dev loop (servermock)

`--debug` mode bypasses Twitch and fetches `http://127.0.0.2:8000/index.m3u8`.

1. In `servermock/`: `pnpm install` first (installs `@node-rs/xxhash`); `pnpm dev` runs `node --watch main.ts`. It needs real segment files in `servermock/segments/` (gitignored). The mock replays at `SPEED = 8` and ends with `#EXT-X-ENDLIST`; `touch servermock/main.ts` restarts it from the top.
2. `cargo run -p twitch-hls-client2 -- -c anything -d` — `-c` is still required by clap even in debug mode. Debug mode uses a fixed broadcast id (`0`) so restarts resume the same files.
3. Client serves the raw TS stream over TCP on `localhost:7417` (v4 and v6; ephemeral ports as fallback) and persists to `manifest/{broadcast_id}.segs` + `.index`.
4. `ffprobe -i tcp://127.0.0.1:7417` is the quick end-to-end check.

## Architecture

Single-threaded `compio` reactor (`#[compio::main]`). Everything below runs as tasks on one thread, so there are no locks, no atomics and no `Send` bounds; threads exist only inside the driver (IOCP on Windows, io_uring on Linux). `!Send` tasks are deliberate — do not "fix" `clippy::future_not_send`.

- `arena.rs` — two fixed slabs, both allocation-free after startup:
  - **slots**: fixed `192_512` B (188 × 1024) zeroed slots, 32 per lazily chained arena, intrusive refcounts where the `next` link holds a reference so a lagging reader can never follow a recycled slot. Arenas are `alloc_zeroed` and every view derives from one base pointer (Miri-checked under Stacked Borrows).
  - **spools**: one small record per live spool (`refs`, `closed`, `published`) in a fixed table of `MAX_SLOTS` entries with an intrusive free list, because a live spool always owns at least one slot. No per-segment `Rc` allocation.
- `spool.rs` — `Spool::open()` returns a `Writer`/`Reader` pair. The writer appends into a slot tail and publishes; readers get owned, pinned `SlotView`s (`IoBuf`) that go straight to `write_all` / `write_at`, so the fan-out is zero-copy.
- **Wakeups**: a spool has no wait list. Each consumer task owns one `ConsumerWake` (one `Waker`, allocated once) registered in the scheduler's `WakeSet`; a publish wakes every registered consumer and each re-checks its own spool frontier. Never wait on a per-spool list — that was the old design and it also silently only completed on close.
- `http/` — hand-rolled HTTP/1.1 client. Pool keyed by `(tls, host, port)` with 4 idle/host, 8 total, 30 s; chunked-first framing decoder that reads payload straight into the caller's buffer; `rustls::unbuffered` driven over compio completion reads (no `compio-tls`, no futures-io bridge). rustls types are confined to `http/tls.rs`.
- `scheduler.rs` — polls the manifest through the client, parses Twitch tags, dispatches segment/prefetch/init fetches, fans spools out to registered consumers. `variant.rs` resolves master playlists via Twitch GQL + usher (`utils::query_string` percent-encodes the signed usher URL). Segment URLs are shared as `Rc<str>` between the fetch message and the prefetch entry.
- `downloader.rs` — `constants::DOWNLOAD_TASKS` worker tasks pulling from a bounded queue; bodies stream into the spool; retries with backoff only while the spool is still empty (a retry after bytes would corrupt the segment).
- `disk.rs` — `compio-fs` appends to `manifest/{broadcast_id}.segs` and record-encodes `manifest/{broadcast_id}.index` (magic `"OZENIKEN"`, version 3, little-endian). No file cursor: explicit offsets tracked in memory; the record buffer is reused. Restarts resume by reading the index once and replaying it.
- `tcp.rs` — one listener + accept loop per loopback family, one task per client; `Discontinuity` resets the client's sequence filter.

No native FFmpeg dependency. The discontinuity probe is gone; `StreamRecord::Discontinuity` keeps its encoder/decoder so v3 index files stay compatible, and the init/SPS parser will produce it later (TODO in `disk.rs`).

## Gotchas (hard-won, do not "simplify")

- Twitch manifest parsing (`scheduler.rs::reload`): `#EXT-X-TWITCH-LIVE-SEQUENCE` appears multiple times during dropout protection; only the value immediately after `#EXT-X-MEDIA-SEQUENCE` (or after an ad segment) is trustworthy. `#EXT-X-TWITCH-ELAPSED-SECS` invalidates it. See `notes.txt`.
- mpegts ↔ fmp4 containers cannot cross disconnect protection (`notes.txt`).
- The index header is 24 bytes (magic + version + stream id); records start after it. Getting this wrong silently overwrites `stream_id` and breaks resume.
- A reader must be woken on **publish**, not only on close, or live streaming degenerates into one burst per segment. See `spool.rs::Wait`.
- compio's `TcpListener::bind` takes the *first* address that works, so each loopback family needs its own listener/accept loop.
- A completion read learns its size from the buffer's capacity; a fresh `Vec` needs `reserve` before `read`.
- Spans must be attached with `.instrument(span)`, never `span.enter()` held across an `.await` — everything shares one thread.
- `crates/twitch/src/index.hexpat` (ImHex pattern) mirrors `disk.rs`; keep both in sync when changing the on-disk format.

## Repo conventions

- Colocated `jj` (Jujutsu) + git repo; git shows a detached HEAD. Work with `jj`, not git.
- jj workflow: open a new commit at the start of a task; commit each step and `jj squash` back down into it before moving on; `jj undo` recovers mistakes.
- Style: write efficient code — don't slow things down just to appease the borrow checker. Prefer fewer dependencies; `compio`, `rustls`, `http`, `httparse` and `futures-util` are the HTTP/runtime set.
- Prefer fixed-size storage when the bound is structural (slots, spool records, arenas); use a `Vec` only when the size is genuinely data-dependent (playlist window, consumer list), and pre-reserve it.
