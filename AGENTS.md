# AGENTS.md

Twitch HLS live-stream client (Rust) + `servermock/` (Node mock Twitch edge server) used to test it offline. No README; `todo.md` and `notes.txt` are the maintainer's live scratch notes — read them before touching scheduler/segment/disk logic.

## Commands

- Build/check: `cargo check -p twitch-hls-client2`, `cargo build -p twitch-hls-client2`
- Lint: `cargo clippy -p twitch-hls-client2` — Cargo.toml enables `pedantic`/`nursery`/`suspicious`/`cargo` groups as warn; keep it clean.
- Format: `crates/twitch/rustfmt.toml` uses **unstable** rustfmt options (`group_imports`, `imports_granularity`, `imports_layout`, `format_code_in_doc_comments`, `reorder_impl_items`); stable `cargo fmt` silently ignores them. Use `cargo +nightly fmt`.
- No Rust tests exist. `servermock/unused/main_test.ts` is stale boilerplate — don't rely on it.

## Local dev loop (servermock)

`--debug` mode bypasses Twitch and fetches `http://127.0.0.2:8000/index.m3u8` (main.rs:82).

1. In `servermock/`: `pnpm install` first (installs `@node-rs/xxhash`); `pnpm dev` runs `node --watch main.ts`. It needs real segment files in `servermock/segments/` (gitignored).
2. `cargo run -p twitch-hls-client2 -- -c anything -d` — `-c` is still required by clap even in debug mode.
3. Client serves the raw TS stream over TCP on localhost:7417 and persists to `manifest/{stream_id}.segs` + `.index`.

## Architecture

Thread-per-module, wired through unbounded `kanal` channels in `main.rs`:

- `scheduler.rs` — polls the HLS manifest, parses Twitch tags, dispatches segment/prefetch/init fetches to downloaders, fans out segment data to registered readers (tcp + disk). `variant.rs` resolves master playlists via Twitch GQL + usher. `downloader.rs` — 8 worker threads (`constants::DOWNLOAD_THREADS`); retries with backoff;
- `segment/` — lock-free chunked stream (`CHUNK_SIZE = 770_048 = LCM(16384, 188)`; ureq buffers 16 KiB, TS packets are 188 B). Uses **heavy `unsafe`** (`MaybeUninit` data, manual Send/Sync, `wait_on_address::AtomicWait`). Invariants documented inline in writer.rs/reader.rs — preserve them.
- `disk.rs` — appends to `manifest/{stream_id}.segs`, appends records to `manifest/{stream_id}.index` (magic `"OZENIKEN"`, version 3, little-endian). Restarts resume by replaying the index. Requires native FFmpeg (`ffmpeg-next`) at build/runtime.

## Gotchas (hard-won, do not "simplify")

- Twitch manifest parsing (`scheduler.rs::reload`): `#EXT-X-TWITCH-LIVE-SEQUENCE` appears multiple times during dropout protection; only the value immediately after `#EXT-X-MEDIA-SEQUENCE` (or after an ad segment) is trustworthy. `#EXT-X-TWITCH-ELAPSED-SECS` invalidates it. See `notes.txt`.
- mpegts ↔ fmp4 containers cannot cross disconnect protection (`notes.txt`).
- `crates/twitch/src/index.hexpat` (ImHex pattern) mirrors `disk.rs`; keep both in sync when changing the on-disk format.

## Repo conventions

- Colocated `jj` (Jujutsu) + git repo; git shows a detached HEAD. Work with `jj`, not git.
- jj workflow: open a new commit at the start of a task; commit each step and `jj squash` back down into it before moving on; `jj undo` recovers mistakes.
- Style: write efficient code — don't slow things down just to appease the borrow checker. Avoid adding dependencies; the project is lean at MVP stage and already somewhat bloated.
