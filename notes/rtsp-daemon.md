# RTSP daemon mode

Status: implemented (`daemon.rs`, `rtsp::StreamResolver`). The arena rework it
depended on landed first.

## Goal

One long-lived RTSP server; the channel is chosen by the request URI. Nothing
remote is fetched at startup: no GQL, no variant resolution, no DVR discovery,
no scheduler. Startup is: tracing, `Client`, one `ArenaSet`, the downloader
workers, the listeners.

## CLI

`twitch-hls-client2 daemon [--bind <addr>] [--max-streams N]`. The old
behaviour moved under an explicit `play` subcommand (`-c/--channel` and
`-q/--quality` live there). `--bind` defaults to `127.0.0.1:7417`; `daemon`
binds exactly that address, so the dual-loopback-family fallback is only in
`play`. `--max-streams` defaults to `constants::MAX_STREAMS` (8).

## URI

`rtsp://host/<channel>/<quality>/`

- Path segments, never query parameters: ffmpeg joins a relative
  `a=control:trackID=0` by string append (`libavformat/rtsp.c:615`), so a query
  would swallow the track fragment (`...?quality=x/trackID=0`).
- `<channel>` is the Twitch login, lowercased.
- `<quality>` is a `STABLE-VARIANT-ID` (`chunked`, `720p60`, `audio_only`, ...).
  Missing means the first variant. An unknown id is a 404.
- `a=control:trackID=0` stays relative; `Content-Base` is the request URI plus a
  trailing slash.

## Resolution and lifetime

`Registry` keyed by `(channel, quality)`.

1. DESCRIBE resolves the key through a small keyed async gate (one mutex per
   stream key / channel). The channel entry does one GQL variant fetch per
   login and caches the title plus the variant list.
2. The stream entry picks the variant by id and builds the per-variant pipeline:
   `Stream`, `WakeSet`, consumer-registration channel, `Dvr::spawn`,
   `spawn_media`, scheduler task.
3. `Session` holds `RefCell<Option<Rc<Stream>>>`; the accept loop hands the
   resolver down instead of a single `Rc<Stream>`. A session attaches on
   DESCRIBE, not on connect.
4. **Last viewer disconnects: drop the stream immediately.** `Stream::stop`
   sets the stop flag and clears the seek sender; `StreamRuntime::stop` also
   stops the `Dvr`, pokes the scheduler latch and drops the registration
   sender. `next_reader`/`handle_new_readers` treat "closed and empty" as
   `Stopped`, so the scheduler returns, and the media engine's `recv` ends when
   the scheduler drops its consumer. The media loop, `play_vod` and the
   watchdog additionally check `stopped()`.
5. The channel entry is dropped with the channel's last stream (decided). A
   reconnect re-resolves the variant list instead of playing a stale one; there
   is no separate expiry timer.

## Sharing

- Global: `Client`, one `ArenaSet` (already sized as a pool), the downloader
  queue, the RTSP listener.
- Per channel: login, title, variant list.
- Per stream (one variant): scheduler, `Dvr`, `Stream`, `WakeSet`, media engine,
  consumer-registration channel, stop flags.

`Dvr` is per variant, not per channel: it stores the chosen `live_variant` and
resolves that variant's archive media playlist.

## Capacity

- `--max-streams` (defaults to `MAX_STREAMS`). Streams with no attached
  sessions are dropped before the count is checked; a stream someone is
  watching is never evicted. When full, DESCRIBE gets a 503.
- The capacity check and the registry insert are in the same synchronous block
  (`spawn_stream` has no await), so the count is exact even when resolutions
  interleave.
- The slot budget is still global (`SEGMENT_SLOTS_MAX` is per spool, not per
  stream), so a per-stream reservation remains open.

## Naming

`Hub` became `Stream` (one variant's RTSP presentation). "Broadcast" keeps its
meaning from `AGENTS.md`: one channel's live stream. There was no
`MAX_BROADCASTS` left to rename; the new cap is `MAX_STREAMS`.

## Open questions

- Per-stream slot reservation.
- Per-stream spans for tracing (the scheduler/media spans are currently
  stream-agnostic).
- Debug mode resolves every login to the mock playlist, so an unknown channel
  cannot be exercised offline.
