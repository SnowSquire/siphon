# Architecture

Status: implemented. This is the detail behind `AGENTS.md`'s one-line module
list: the pipeline, the scheduler, the downloader, the daemon and the
listeners. The arena/spool, HTTP, RTSP and disk details have their own notes
(`slot-arena-rework.md`, `tls-client-api.md`, `es-rtp-rtsp.md` +
`rtsp-daemon.md`, `disk-complete-recording.md`).

## One thread

`#[compio::main]` starts a single-threaded completion reactor. Every component
is a task on that thread: there are no locks, no atomics and no `Send` bounds,
and `!Send` futures are intentional (`clippy::future_not_send` is allowed
crate-wide). The driver owns the threads (IOCP on Windows, io_uring on Linux).

## Pipeline

`play` and `daemon` build one `Stream` per variant:

```
manifest poll (scheduler) ──► downloader queue ──► spool ──► consumers
       │                                             ├─ RTSP media engine
       └─ variant resolution (GQL + usher)           └─ disk recorder
```

The scheduler polls the media playlist, parses Twitch's tags, dispatches
segment/prefetch/init fetches and fans the resulting spools out to registered
consumers. A consumer registers over a channel and owns a bounded
`Sender<SegmentMessage>`; the queue bound is the lag policy (a full queue can
drop the consumer or, for the lossy RTSP consumer, one message).

## `scheduler.rs`

- One manifest poll per iteration; the manifest is fetched through the shared
  `Client` and parsed by `playlist.rs`.
- `reload` applies events as it parses: media sequence, init, discontinuity,
  checkpoints, media, prefetch. `segment_tail` is the next undispatched
  sequence; ads consume a sequence number but are never fetched.
- Fetch URLs are shared as `Rc<str>` between the fetch message and the prefetch
  entry; a prefetched segment is dispatched from its spool instead of being
  fetched again.
- Consumer registration is also the life signal: when the last registration
  sender is gone and the channel is empty, the scheduler returns instead of
  waiting for a consumer that can never arrive.
- `variant.rs` resolves the master playlist through Twitch GQL + usher;
  `utils::query_string` percent-encodes the signed usher URL.

## `downloader.rs`

`constants::DOWNLOAD_TASKS` worker tasks pull `ToDownloader::FetchURL` from a
bounded queue and stream the body into the segment's spool. Retries back off
only while the spool is still empty: retrying after bytes were written would
append a second copy onto the same segment.

## `registry.rs`

The registry behind `daemon` mode; `rtsp-daemon.md` has the lifecycle, URI,
capacity and resolver. In short: one global `Client`, `ArenaSet` and downloader
queue; `(channel, quality)` maps to a `Stream`; DESCRIBE resolves the channel,
picks the variant and builds the pipeline; the last viewer leaving stops it;
`--max-streams` refuses rather than evicting.  With `--output` a recording keeps
its stream entry (and counts against the limit) after the last viewer leaves;
the registry releases it when the disk recorder finishes.

## `disk.rs`

`--output <dir>` records one complete, ad-free archive per broadcast:
`<dir>/<stream_id>/{segs,index}`.  Live segments are recorded immediately into
numbered runs, the DVR playlist fills the pre-join span and every ad gap, and
each run's uncovered middle is copied into a reserved hole; see
`disk-complete-recording.md` for the file format, the hole arithmetic and the
resume repair.  The DVR fetch and the copies run as their own tasks, so neither
source ever waits on the other; only the canonical assembly waits for a hole,
never for the network.

## Listeners

- `play` binds `127.0.0.1` and `::1` on the **same** port: v4 first (it resolves
  the port when `0` asks for an ephemeral one), then v6 on that number. Both go
  through `socket2` (`TcpListener::from_std`) rather than compio's binder, which
  sets `SO_REUSEADDR` unconditionally — on Windows that lets a second process
  share a port instead of failing, so the ephemeral fallback would never
  trigger. Unix still sets it, to rebind over `TIME_WAIT`. `AddrInUse` on
  either family moves the whole pair to an ephemeral port; a family that cannot
  bind at all (IPv6 switched off) is skipped.
- Never bind `[::]`: the wildcard is LAN-reachable, and loopback-only is the
  point.
- `daemon` binds exactly `--bind`, so the dual-family fallback is a `play`
  feature.
