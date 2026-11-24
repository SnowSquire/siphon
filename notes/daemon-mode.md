# Daemon mode

Status: implemented (`registry.rs`, the Riffle control plane).

## Goal

One long-lived daemon; the stream is chosen by the control plane — a
`StreamOpen` carrying a `StreamKey`. Nothing remote is fetched at startup: no
GQL, no variant resolution, no DVR discovery, no scheduler. Startup is: tracing,
`Client`, one `ArenaSet`, the downloader workers, the listeners.

## CLI

`daemon daemon [--admin <endpoint>] [--no-admin] [--output <dir>] [--notify <url>]
[--keep-runs]`. The admin endpoint defaults to the per-app pipe/socket. There is
no address to bind: a remote listener exists only when `network.listen` is set in
the config, and it is empty — no network surface — by default. There is no stream
limit (`--max-streams` went with RTSP). `pair` opens the window that approves a
remote client's certificate; `record <login>` is the one-channel recorder with no
control plane at all.

## Stream key

`StreamKey { channel, quality }`

- `channel` is the Twitch **channel id**, not the login: the daemon owns the
  login↔id mapping (`config.channels[].id`), the pipeline is keyed by
  `(channel id, quality)`, and a login the daemon cannot resolve is `NotFound`.
- `quality` is a `STABLE-VARIANT-ID` (`chunked`, `720p60`, `audio_only`, ...).
  Missing means the first variant; an id the master playlist does not carry is a
  `BadRequest` naming the login.

## Resolution and lifetime

`Registry` keyed by `(channel id, quality)`.

1. `attach` resolves the key through a small keyed async gate (one mutex per
   stream key / channel). The channel entry does one GQL variant fetch per login
   and caches the title plus the variant list. An offline channel is `Failed`
   ("is offline"), an unknown one `NotFound`.
2. The stream entry picks the variant by id and builds the per-variant pipeline:
   `StreamRuntime`, `WakeSet`, consumer-registration channel, `Dvr`, scheduler
   task.
3. A lane claims the runtime with `attach`; a key already attached takes a second
   claim instead of spawning a second pipeline. The lane set itself is per
   connection.
4. **Last lane detaches: drop the stream immediately** unless something is
   recording or the channel's record preference is on. `StreamRuntime::stop` sets
   the stop flag, stops the `Dvr` and the disk output, and drops the registration
   sender; the scheduler sees its consumer gone and returns.
5. A followed channel keeps its row; `stream-down` clears the fetched variant
   data so the next broadcast re-resolves. An entry created by an ad-hoc request
   for an unfollowed channel is dropped with its last pipeline. There is no
   expiry timer.

## Sharing

- Global: `Client`, one `ArenaSet` (already sized as a pool), the downloader
  queue, the admin listener (and the QUIC listener when `network.listen` is set).
- Per channel: login, title, variant list.
- Per stream (one variant): scheduler, `Dvr`, `StreamRuntime`, `WakeSet`, the
  consumer-registration channel, stop flags.

`Dvr` is per variant, not per channel: it stores the chosen `live_variant` and
resolves that variant's archive media playlist.

## Capacity

- No cap, no eviction. `SEGMENT_SLOTS_MAX` bounds one spool, the arena bounds the
  process, and a failed allocation is one failed segment (`ArenaFull`).
- A runtime is created by `StreamOpen`, by `record` on a live channel, by
  `RecordOnce`, or by the record preference (`monitor_live`); it goes away when
  the last lane leaves and nothing is recording.
- The fixed per-stream cost is the manifest poll (and, while somebody reads, the
  segment fetches). A parked scheduler does neither, so a recording-only stream
  costs the DVR refresh rate.

## Naming

`StreamRuntime` is one variant's pipeline; "broadcast" keeps its meaning from
`AGENTS.md`: one channel's live stream.

## Gotchas

The media path has its own traps in `riffle.md`; these are the ones that live in
this layer.

- A consumer unregisters only on a closed channel: "full" is not "closed"
  (`AGENTS.md` rule 2). The lane registers lossy, so a full queue drops the
  individual message instead of disconnecting, and `MAX_CONSUMER_LAG` only bites
  a strict consumer.
- Never block the reactor. Every send on the media path is `try_send` or an
  `AsyncSender`; a blocking send parks the only thread, so the task that would
  free the slot can never be polled.
- Debug mode resolves every login to the mock playlist, so an unknown channel
  cannot be exercised offline.

## Open questions

- Per-stream slot reservation.
- Per-stream spans for tracing (the scheduler/media spans are currently
  stream-agnostic).
