# RTSP daemon mode

Status: plan only, nothing implemented. Prerequisite: the slot-arena rework
(dynamic arenas) lands first, as its own commit.

## Goal

One long-lived RTSP server; the channel is chosen by the request URI. Nothing
remote is fetched at startup: no GQL, no variant resolution, no DVR discovery,
no scheduler. Startup is: tracing, `Client`, one `ArenaSet`, the downloader
workers, the listeners.

## CLI

`twitch-hls-client2 daemon [--bind <addr>] [--max-streams N]`, with the current
behaviour moved under an explicit `play` subcommand. `-c/--channel` and
`-q/--quality` belong to `play` and are not valid for `daemon`.

## URI

`rtsp://host/<channel>/<quality>/`

- Path segments, never query parameters: ffmpeg joins a relative
  `a=control:trackID=0` by string append (`libavformat/rtsp.c:615`), so a query
  would swallow the track fragment (`...?quality=x/trackID=0`).
- `<channel>` is the Twitch login, lowercased.
- `<quality>` is a `STABLE-VARIANT-ID` (`chunked`, `720p60`, `audio_only`, ...).
  Missing means the first variant, i.e. today's `-q` behaviour.
- `a=control:trackID=0` stays relative; `Content-Base` is the request URI plus a
  trailing slash.

## Resolution and lifetime

`Registry` keyed by `(channel, quality)`.

1. DESCRIBE resolves the key. The channel entry does one GQL variant fetch per
   login and caches the title plus the variant list.
2. The stream entry picks the variant by id and builds the per-variant
   pipeline: `Hub`, `WakeSet`, consumer-registration channel, `Dvr::spawn`,
   `spawn_media`, scheduler task.
3. `Session` holds `Option<Rc<Stream>>`; the accept loop hands the registry down
   instead of a single `Rc<Hub>`.
4. **Last viewer disconnects: drop the stream immediately** (decided). That
   needs a `stop: Cell<bool>` checked by the scheduler loop and by `play_vod`,
   plus dropping the registration sender so the scheduler's consumer loop ends
   and the spools release.
5. Whether the channel entry (title + cached variants) is dropped too, or kept
   until the daemon exits, is still open.

## Sharing

- Global: `Client`, one `ArenaSet` (shared by every stream, already sized as a
  pool), the downloader queue, the RTSP listeners.
- Per channel: login, title, variant list.
- Per stream (one variant): scheduler, `Dvr`, `Hub`, `WakeSet`, media engine,
  consumer-registration channel, stop flag.

`Dvr` is per variant, not per channel: it stores the chosen `live_variant` and
resolves that variant's archive media playlist.

## Capacity

- `--max-streams`, defaulting to the renamed `MAX_STREAMS`. When full and no
  stream is eligible for an immediate drop, answer DESCRIBE with 503; do not
  evict a stream that someone is watching.
- The slot budget is global. A stalled segment on one stream can consume slots
  another stream needs (`SLOT_BUDGET` is per spool, not per stream), so a
  per-stream reservation is an open question.

## Naming

`Hub` becomes `Stream` (one variant's RTSP presentation); `MAX_BROADCASTS`
becomes `MAX_STREAMS`. "Broadcast" keeps its meaning from `AGENTS.md`: one
channel's live stream.

## Open questions

- Raw TS output: settled — `tcp.rs` was removed and RTSP is the only sink, so
  the daemon has nothing to decide here. The RTSP listener binds 7417.
- Per-stream slot reservation.
- Does the channel entry expire (stale title / variant list)?
- Per-stream spans for tracing.

## Prerequisite: arena rework

The arena currently cannot express "grow when needed": `MAX_ARENAS` is
compile-time, `alloc()` scans `[Option<Rc<Arena>>; MAX_ARENAS]`, and an emptied
arena is dropped at once, so a pool at the boundary re-allocates a zeroed 6 MB
arena per slot. The rework (linked list of arenas, monotonic ids, one spare empty
arena) is being designed separately.
