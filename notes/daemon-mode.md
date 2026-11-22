# Daemon mode

Status: implemented (`registry.rs`, `rtsp::StreamResolver`).

## Goal

One long-lived RTSP server; the channel is chosen by the request URI. Nothing
remote is fetched at startup: no GQL, no variant resolution, no DVR discovery,
no scheduler. Startup is: tracing, `Client`, one `ArenaSet`, the downloader
workers, the listeners.

## CLI

`daemon daemon [--bind <addr>] [--admin <endpoint>] [--no-admin]`.
`--bind` defaults to `127.0.0.1:7417`; `daemon` binds exactly that address, so
the dual-loopback-family fallback is only in `play`. There is no stream limit
(`--max-streams` was removed). `play` keeps `-c/--channel` and `-q/--quality`.

## URI

`rtsp://host/<channel>/<quality>/`

- Path segments, never query parameters: ffmpeg joins a relative
  `a=control:trackID=0` by string append (`libavformat/rtsp.c:615`), so a query
  would swallow the track fragment (`...?quality=x/trackID=0`).
- `<channel>` is the Twitch login, lowercased, and it must be a followed
  channel: the daemon owns the login to id mapping (`config.channels[].id`), the
  pipeline is keyed by `(channel id, quality)`, and any other login is a 404.
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
5. A followed channel keeps its row; `stream-down` clears the fetched variant
   data so the next broadcast re-resolves. An entry created by an RTSP DESCRIBE
   for an unfollowed channel is dropped with its last stream. There is no expiry
   timer.

## Sharing

- Global: `Client`, one `ArenaSet` (already sized as a pool), the downloader
  queue, the RTSP listener.
- Per channel: login, title, variant list.
- Per stream (one variant): scheduler, `Dvr`, `Stream`, `WakeSet`, media engine,
  consumer-registration channel, stop flags.

`Dvr` is per variant, not per channel: it stores the chosen `live_variant` and
resolves that variant's archive media playlist.

## Capacity

- No cap, no eviction. `SEGMENT_SLOTS_MAX` bounds one spool, the arena bounds the
  process, and a failed allocation is one failed segment (`ArenaFull`).
- A runtime is created by a DESCRIBE, by `record` on a live channel, or by
  `RecordOnce`; it goes away when the last viewer leaves and nothing is
  recording.
- The fixed per-stream cost is the manifest poll (and, while somebody reads, the
  segment fetches). A parked scheduler does neither, so a recording-only stream
  costs the DVR refresh rate.

## Naming

`Stream` is one variant's RTSP presentation; "broadcast" keeps its meaning from
`AGENTS.md`: one channel's live stream. The cap is `MAX_STREAMS`.

## Gotchas

Most of the protocol-level traps (RTP-Info on seek, the SR media-clock anchor,
the seek epoch, the 16x queue, the handover, the lossy consumer) are in
`rtsp-output.md`; these are the ones that live in this layer.

- A joining session must wait for its first video IDR on **every** track, not
  just video (`Stream::send`). ffmpeg copies the first sender report's
  `first_rtcp_ntp_time`/`rtcp_ts_offset` into every stream that has not seen one,
  assuming all streams' first packets are the same instant; starting audio while
  video waits for the keyframe makes the audio timeline jump on its first SR.
  Audio-only sessions (no video SETUP) are not gated.
- One RTP/RTCP packet is an `Rc<Vec<u8>>` shared by every session's queue:
  `Track::packet` builds it once per access unit and the fan-out is a refcount
  (copying per session was ~20k allocations/s at 16x with two viewers). The
  interleaved-TCP writer keeps one framing buffer and gets it back through
  `write_all` (`BufResult.1`), so the four-byte header costs a copy but no
  allocation.
- Never block the reactor. `Session::out` is an `AsyncSender` (no blocking
  `send`); RTSP responses go through `Session::control` (unbounded, drained
  before every message, poked with `try_send(Outbound::Flush)`); setup and the
  fan-out use `try_send` too. A blocking send parks the only thread, so the task
  that would free the slot can never be polled.
- `push_out` unregisters only on a closed channel: "full" is not "closed", and
  `RTCP_HEADROOM` keeps slots free for the reports queued between samples.
- `Session::seek_started` gates the stream from the PLAY reply until
  `set_media_base`; the watchdog releases the session after `SEEK_TIMEOUT` and
  logs the state, so a broken seek degrades to a glitch. A watchdog line that
  never appears while a stream is silent means the thread is blocked or
  spinning.

## Open questions

- Per-stream slot reservation.
- Per-stream spans for tracing (the scheduler/media spans are currently
  stream-agnostic).
- Debug mode resolves every login to the mock playlist, so an unknown channel
  cannot be exercised offline.
