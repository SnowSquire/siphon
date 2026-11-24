# Twitch HLS transport: probe measurements

How Twitch's edge actually delivers HLS, and what that fixes in the client. Probe
stream `sunnys`, 2026-11-07/08, `*.j.cloudfront.hls.ttvnw.net`, 2 s segments,
~1.8 MB each. The probe itself was a throwaway blocking HTTP/1.1 client, long
gone; this document is the only artifact, so the numbers are kept and the
investigation narrative is not.

Client design that came out of it is in `tls-client-api.md`; the body decoder is
chunked-first and reads payload straight into the caller's destination.

## TL;DR

- Twitch's segment CDN is **HTTP/1.1 only**. No ALPN, no HTTP/2, no `alt-svc`,
  no HTTP/3.
- **CTE is the norm, not an edge case.** 100% of prefetch (live-tip) segments are
  `Transfer-Encoding: chunked`; catch-up segments are chunked ~80–90% of the time,
  with occasional `Content-Length`.
- Prefetch segments are **production-paced** (~2 s to complete) and trickle in
  ~10–19 KiB chunks. Catch-up segments arrive in 30–120 ms bursts, with chunks up
  to 256 KiB.
- Chunk sizes are extremely variable: **6 bytes to 262112 bytes**. Any decoder
  that assumes "chunk == one read" or "chunk ≥ some minimum" will stall.
- Plain `http` saves **one TLS handshake (~6 ms per connection)** and nothing
  else. TTFB and body time are identical; with connection pooling the
  steady-state gain is ~0.

## 1. Read sizing

rustls decides its own read size and offers no knob: `DeframerVecBuffer::prepare_read`
resizes to `min(allow_max, used + 4096)` and shrinks back to a **4 KiB read slice**
once plaintext is drained (`READ_SIZE = 4096`, `MAX_WIRE_SIZE` ~16 KiB). So in
steady state one socket `recv` per ~4 KiB, whatever the caller wants.

Consequences:

- On TLS, byte-at-a-time reads of the chunk-size line cost **no extra syscalls**:
  they are memcpys out of rustls's decrypted buffer. Framing-aware reads are
  effectively free.
- On plaintext there is no such buffer. A 1-byte read is a 1-byte `recv`.

## 2. Decoder design

Two regimes want different points on the read-into-destination vs staging trade:

- **Live prefetch (CTE, production-paced, ~2 s).** Little is queued at each
  wakeup, so syscall count is driven by arrivals, not read sizes. One read per
  arrival into a staging buffer, decode, copy payload to the destination, publish.
- **Catch-up (finalized, bursty, 30–120 ms).** A big destination drains the whole
  socket queue in one `recv`. A framing-aware reader (parse the size line small,
  read payload direct into the destination) saves a payload memmove at the cost of
  ~2 syscalls per CTE chunk.
- **TLS makes framing-aware strictly better.** rustls already staged the
  plaintext, so the size-line read is free and the payload goes direct with no
  extra syscall and no scratch buffer.
- Do **not** read plaintext into the writer chunk and then compact in place: the
  compaction is a payload-sized memmove and the framing bytes temporarily occupy
  chunk space. Read the size line into a tiny buffer instead.
- `chunk_remaining` can be 6 bytes or 256 KiB, so a short read is normal and the
  loop must never require a minimum.

### 2.1 Measured crossover

Release `copy_nonoverlapping`, single-threaded, MSVC target + `lld-link`:

| operation | measured |
|---|---|
| memcpy, 1–4 KiB | ~160 GB/s (L1-resident) |
| memcpy, 8–256 KiB | ~70–80 GB/s |
| memcpy, 1 MiB | ~67 GB/s |
| non-blocking `recv` with no data | **352 ns/call** |

Chunk-size distribution from the probe logs, grouped per fetch:

| kind | fetches | chunks/fetch | MB/fetch | median chunk | mean chunk |
|---|---|---|---|---|---|
| prefetch | 50 | 101 | 1.46 | 13.9 KiB | 14.1 KiB |
| main | 36 | 26 | 1.60 | 40.8 KiB | 60.3 KiB |

Model: per chunk, framing-aware pays one extra read for the size line and saves one
copy of the chunk. Crossover is `C* = S * BW` (S = per-read cost, BW = copy
bandwidth):

| S | BW | C* |
|---|---|---|
| 0.35 µs | 80 GB/s | 28 KiB |
| 0.35 µs | 40 GB/s | 14 KiB |
| 1 µs | 80 GB/s | 80 KiB |
| 2 µs | 80 GB/s | 160 KiB |

Verdict:

- **TLS: always framing-aware.** The size-line read lands in rustls's decrypted
  buffer (a memcpy, not a syscall), so the "extra syscall" side of the trade
  disappears; staging is a pure extra payload copy.
- **Plaintext: pick by C\*.** Prefetch chunks (13.9 KiB median) sit below every
  C\* except the most optimistic pair → staging. Main chunks (40.8 KiB median,
  60.3 KiB mean) straddle the line: framing wins at the measured 0.35 µs sync
  `recv` cost, staging wins once the effective per-read cost approaches ~1 µs
  (IOCP completion, or the `poll`+`recv` pair).
- **Staging buffer size: `min(SO_RCVBUF, 256 KiB)`.** You cannot drain more than
  the socket queue per read, so a bigger buffer buys no syscalls. The CRT memcpy
  stays ~80 GB/s through 256 KiB, so the cap is the socket queue, not the cache.
- **Magnitude:** 20–100 µs of CPU per 1.8 MB segment, ~0.01% of one core at one
  segment per 2 s. The choice is about not burning cycles, not latency.

## 3. Results

### 3.1 Transfer-Encoding

| run | main chunked | main content-length | prefetch chunked | prefetch CL |
|---|---|---|---|---|
| 45 s, alternate | 11/15 | 4/15 | 15/15 | 0 |
| 30 s, alternate | 13/14 | 1/14 | 6/6 | 0 |
| 25 s, alternate | 12/14 | 2/14 | 6/6 | 0 |

Prefetch is always CTE. Catch-up is mostly CTE too, so a `Content-Length` fast
path exists but is not the common case.

### 3.2 HTTP/2

Not supported on this host.

- Our rustls client offers `h2, http/1.1`; the server returns **no ALPN**.
- `openssl s_client -alpn h2` and `-alpn h2,http/1.1`: `No ALPN negotiated`.
- `curl --http2`: `http_version=1.1`.
- The playlist host (`use*.playlist.ttvnw.net`) also negotiates no ALPN.

### 3.3 HTTP/3

Not supported either.

- No `alt-svc` header on segment or playlist responses.
- `curl --http3-only` fails: `Could not connect to server`.
- `curl --http3` falls back to HTTP/1.1, with an extra ~100 ms failed QUIC attempt.
- Control on the same machine: `curl --http3-only https://cloudflare.com/` → `ver=3`.
  The curl build and network path support QUIC; the CDN does not.

### 3.4 TLS vs plain HTTP

Two separate runs, 24 fetches each, same stream and host. Medians:

| | connect (main) | connect (prefetch) | ttfb (main) | total (main) | ttfb (prefetch) | total (prefetch) |
|---|---|---|---|---|---|---|
| http | **6.0 ms** | **4.9 ms** | 8.2 ms | 46.6 ms | 70.2 ms | 1992 ms |
| https | **11.8 ms** | **11.2 ms** | 8.2 ms | 45.4 ms | 62.9 ms | 1986 ms |

- The only consistent difference is connection setup: **TLS costs ~6 ms per
  connection**.
- TTFB and body time are identical once connected. No measurable per-byte TLS
  penalty at ~7 Mbps.
- The probe opened a fresh connection per fetch; production pools, so the
  handshake is paid once per host, not per segment.
- Security note: the signed URL authenticates the URL, not the payload. Without
  TLS the TS bytes have no integrity protection against a path attacker.

### 3.5 Chunk sizes and pacing

From per-fetch summaries over ~90 segments:

| | prefetch | catch-up |
|---|---|---|
| payload per segment | ~1.8 MB | ~1.8 MB |
| chunks per segment | 95–218 | 3–73 |
| chunk avg | 9–19 KiB | 22–64 KiB |
| chunk max | 25–57 KiB | up to 262112 B |
| chunk min | **6 B** | **6 B** |
| total time | ~2 s | 30–120 ms |

```
prefetch  chunks=95  chunk_min=39  chunk_max=49952  chunk_avg=19009  total_ms=1989
prefetch  chunks=193 chunk_min=6   chunk_max=26061  chunk_avg=9751   total_ms=2001
main      chunks=46  chunk_min=138 chunk_max=262112 chunk_avg=35891  total_ms=48
```

The recurring 6-byte chunks are real payload chunks, not framing artifacts.
Prefetch pacing tracks production: ~2 s of wall time per segment regardless of
scheme.

## 4. Connection pooling

Pool per channel, keyed by `(host, port)`. A warm request skips DNS/TCP/TLS and
starts at the request write; the skipped steps cost ~12 ms (TCP ~5 ms + TLS
~6 ms) and ~6 KB of copies. The pool removes ~10–17 setup syscalls, not the body
work: a warm prefetch is still ~374–748 body reads (rustls's 4 KiB clamp),
~100–200 publish wakes and ~4.4 MB of copies.

Sizing:

- A live prefetch occupies one connection for ~2 s, exactly the segment cadence:
  one connection is 100% busy in steady state.
- 2 covers the overlap when the next prefetch arrives before the previous body
  finishes, plus a retry or a simultaneous catch-up fetch.
- 3–4 give headroom for catch-up (2–4 parallel 50–120 ms fetches), ad breaks,
  discontinuities (`#EXT-X-MAP` on a new host) and retries.
- The manifest origin (`use*.playlist.ttvnw.net`) gets its own 1-connection pool;
  don't mix it with segment connections.

HTTP/1.1 means one request at a time per connection, so pool size == fetch
concurrency; there is no h2/h3 multiplexing to lean on.

Details:

- Checkout/return are userspace; no syscalls.
- After the CTE terminator (or `Content-Length` bytes) the connection is reusable.
  Track `last_used`; drop after ~30 s idle.
- A server may close an idle keep-alive connection; you find out on the next
  write/read. Discard and retry once on a fresh connection.
- Enable rustls session resumption so a replacement connection is 1-RTT/PSK.
- Do not pipeline requests on one connection.

Daemon shape: one reactor, one task tree per channel. Per channel: scheduler,
writer chain, disk file, connection pool, stats. Shared: `Arc<ClientConfig>`, TLS
session cache, DNS cache with TTL, staging buffer pool. Shutdown drains pools and
flushes readers.

## 5. Caveats

- Single stream, single POP, a few minutes of samples. Numbers are indicative,
  not benchmarks.
- Signed playlist/segment URLs expire (~2 min), so manual probes must be quick.
- `connect_ms` deltas are per-connection, not per-segment savings.
- Aggregate `http_*`/`https_*` stats mix live and finalized segments unless the
  runs are separated.
- Single downloader thread; catch-up throughput was deliberately not measured.
