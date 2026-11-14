# Twitch HLS transport: probe notes

Investigation of how Twitch actually delivers HLS segments, and what that implies
for the client stack (ureq today, compio/cyper later). The throwaway probe that
produced these numbers has been discarded; this document is the only artifact.

Probe stream: `sunnys`, 2026-11-07/08, `*.j.cloudfront.hls.ttvnw.net`, 2 s
segments, ~1.8 MB each.

## TL;DR

- Twitch's segment CDN is **HTTP/1.1 only**. No ALPN, no HTTP/2, no `alt-svc`,
  no HTTP/3.
- **CTE is the norm, not an edge case.** 100% of prefetch (live-tip) segments
  are `Transfer-Encoding: chunked`; catch-up segments are chunked ~80-90% of
  the time, with occasional `Content-Length`.
- Prefetch segments are **production-paced** (~2 s to complete) and trickle in
  ~10-19 KiB chunks. Catch-up segments arrive in 30-120 ms bursts, with chunks
  up to 256 KiB.
- Chunk sizes are extremely variable: **6 bytes to 262112 bytes**. Any decoder
  that assumes "chunk == one read" or "chunk >= some minimum" will stall.
- Plain `http` saves **one TLS handshake (~6 ms per connection)** and nothing
  else. TTFB and body time are identical. With connection pooling the
  steady-state gain of stripping TLS is ~0.
- `cyper`/hyper-style response bodies are **push** (`Stream<Bytes>`), which
  forces an extra payload copy. A hand-rolled pull reader avoids it.

## 1. Body API shape: push vs pull

### cyper

`cyper` exposes `Response::bytes_stream() -> impl Stream<Item = Result<Bytes>>`
(`cyper/src/response.rs`), plus `impl Stream for Response`. `Response::body` is
`pub(crate)` and there is no `into_body`/`into_parts`, so the underlying
`hyper::body::Incoming` is not reachable. This is **push**: the library hands
you owned `Bytes` and you copy out.

Even at the hyper layer, frames are `read_buf.split_to(len).freeze()`, i.e.
slices of hyper's own read buffer. There is no API to say "read the body into
this preallocated slice".

Resulting payload-sized userland writes for a TLS body:

| path | decrypt write | staging / frame copy | copy into your buffer |
|---|---|---|---|
| ureq + rustls (current) | 1 | - | 1 (CTE decoded straight into `dst`) |
| compio-tls, hand-rolled h1.1 | 1 | - | 1 (rustls plaintext into `dst`) |
| compio-tls via hyper/cyper | 1 | 1 (rustls -> hyper read_buf) | 1 (`Bytes` -> `dst`) |
| plaintext, read direct into `dst` | - | - | 0 (kernel copy only) |
| plaintext, staging + decode | - | 1 (kernel -> staging) | 1 (staging -> `dst`) |
| kTLS (Linux only) | in kernel | - | 0 (kernel decrypts into `dst`) |

So `cyper` is strictly worse than ureq for this workload, and a raw pull
reader is the only way to get back to one copy on TLS.

### compio

`compio`'s `AsyncRead` is completion-based and owns the buffer:

```rust
async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B>;
```

`IoBuf: 'static`, so buffers are either owned (`Vec<u8>`) or `&'static mut [u8]`.
On Windows this bottoms out at `WSARecv` with a `WSABUF` pointing at your memory
(`compio-driver/src/sys/op/socket/iocp.rs`); on Linux at `recv`/io_uring. The
OS never hands you a pointer to its own buffer for a TCP socket; you always
provide the destination, and one kernel->user copy is unavoidable.

### compio-tls

`compio_tls::TlsStream` only implements `futures_util::AsyncRead`
(`poll_read(&mut [u8])`), not compio's `AsyncRead`. It is fed a
`PollFd<Socket>`, whose `poll_read` is readiness poll + `recv` straight into the
caller's slice (`compio-runtime/src/fd/poll_fd/mod.rs`). Consequence: the HTTPS
path is borrowed slices, not `BufResult` ownership, so the `'static` dance only
applies to plain compio `TcpStream` reads.

## 2. rustls read sizing

`DeframerVecBuffer::prepare_read` (`rustls/src/msgs/deframer/buffers.rs`):

```rust
const READ_SIZE: usize = 4096;
let need_capacity = Ord::min(allow_max, self.used + READ_SIZE);
...
} else if self.used == 0 || self.buf.len() > allow_max {
    self.buf.resize(need_capacity, 0);
    self.buf.shrink_to(need_capacity);
}
```

`allow_max` is `MAX_WIRE_SIZE` (~16 KiB). When plaintext is consumed promptly,
`used` returns to 0 and the deframer shrinks back to a **4 KiB read slice**. So
in steady state rustls issues one socket `recv` per ~4 KiB, and there is no
public knob to change it.

Consequences:

- On TLS, byte-at-a-time reads of the chunk-size line cost **no extra
  syscalls**; they are memcpys out of rustls's decrypted buffer. Framing-aware
  reads are effectively free.
- On plaintext, there is no such buffer. A 1-byte read is a 1-byte `recv`.

## 3. Decoder design implications

Two regimes, and they want different points on the read-into-dst vs
staging-buffer trade:

- **Live prefetch (CTE, production-paced, ~2 s).** The socket has little queued
  at each wakeup. Syscall count is driven by arrivals, not read sizes. One read
  per arrival into a staging buffer, decode, copy payload to `dst`, publish.
  The copy runs while waiting for the next arrival.
- **Catch-up (finalized, bursty, 30-120 ms).** A big destination drains the
  whole socket queue in one `recv`. Here a framing-aware reader (parse the size
  line small, read payload direct into `dst`) saves a payload memmove at the
  cost of ~2 syscalls per CTE chunk. Crossover depends on the server's chunk
  size; measured chunks are often large (tens of KiB up to 256 KiB), which
  favours framing-aware.
- **TLS makes framing-aware strictly better.** rustls already staged the
  plaintext, so small framing reads are free; payload can go direct into `dst`
  with no extra syscall and no scratch buffer.
- Do **not** read plaintext into the Writer chunk and then compact in place.
  The compaction is a payload-sized memmove and the framing bytes temporarily
  occupy chunk space. Read the size line into a tiny buffer instead.
- `chunk_remaining` can be 6 bytes or 256 KiB, so the read loop must treat a
  short read as normal and never require a minimum.

### 3.1 Read size and strategy (measured)

The earlier crossover was a guess; these are measured on the dev machine
(release `copy_nonoverlapping`, single-threaded, MSVC target + `lld-link`):

| operation | measured |
|---|---|
| memcpy, 1-4 KiB | ~160 GB/s (L1-resident) |
| memcpy, 8-256 KiB | ~70-80 GB/s |
| memcpy, 1 MiB | ~67 GB/s |
| non-blocking `recv` with no data | **352 ns/call** |

(Measured on the GNU target first: mingw's `memcpy` is much weaker at large
sizes, 30 GB/s at 1 MiB, which made the cache cliff look worse than it is. The
MSVC numbers are what the real build uses.)

Chunk-size distribution from the probe logs, grouped per fetch:

| kind | fetches | chunks/fetch | MB/fetch | median chunk | mean chunk |
|---|---|---|---|---|---|
| prefetch | 50 | 101 | 1.46 | 13.9 KiB | 14.1 KiB |
| main | 36 | 26 | 1.60 | 40.8 KiB | 60.3 KiB |

Model: per chunk, framing-aware pays one extra read for the size line and saves
one copy of the chunk. Crossover is `C* = S * BW` (S = per-read cost, BW =
copy bandwidth):

| S | BW | C* |
|---|---|---|
| 0.35 us | 80 GB/s | 28 KiB |
| 0.35 us | 40 GB/s | 14 KiB |
| 1 us | 80 GB/s | 80 KiB |
| 2 us | 80 GB/s | 160 KiB |

Verdict:

- **TLS: always framing-aware.** The size-line read lands in rustls's decrypted
  buffer (a memcpy, not a syscall), so the "extra syscall" side of the trade
  disappears. Staging is a pure extra payload copy.
- **Plaintext: pick by C*.** Prefetch chunks (13.9 KiB median) sit below every
  C* except the most optimistic pair -> staging. Main chunks (40.8 KiB median,
  60.3 KiB mean) straddle the line: framing wins at the measured 0.35 us
  sync-`recv` cost, staging wins once the effective per-read cost approaches
  ~1 us (IOCP completion, or the `poll`+`recv` readiness pair).
- **Staging buffer size: `min(SO_RCVBUF, 256 KiB)`.** You cannot drain more than
  the socket queue per read, so a bigger buffer buys no syscalls. The MSVC CRT
  memcpy stays ~80 GB/s through 256 KiB (~67 GB/s at 1 MiB), so the cap is the
  socket queue, not the cache; on the default Windows queue that means 64-128 KiB.
- **Magnitude:** 20-100 us of CPU per 1.8 MB segment, i.e. ~0.01% of one core at
  one segment per 2 s. The choice is about not burning cycles, not about
  wall-clock latency.

## 4. The probe

A throwaway build replaced ureq for segment fetches with a hand-rolled blocking
HTTP/1.1 client (since removed):

- fresh TCP (+ rustls TLS) connection per fetch, `Connection: close`;
- `SEG_SCHEME=alternate|http|https` (default alternated per segment);
- ALPN probe: offer `h2, http/1.1` once per host, log the result, reconnect with
  `http/1.1` if the server picks h2;
- logs every CTE chunk (`target: "http_chunk"`: declared size, raw hex line,
  time since previous chunk), every payload read (`target: "http_read"` at
  debug), and a per-fetch summary + rolling stats;
- a hardcoded signed playlist URL so runs did not hit GQL/usher each time
  (removed with the rest of the probe).

Signed URLs expire in ~2 minutes, so manual probes have to be quick. To
re-create the probe, the useful pieces are: a raw h1.1 client over rustls with
ALPN set to `[h2, http/1.1]`, per-read timing, and a CTE parser that logs the
declared chunk size before reading it.

```sh
RUST_LOG=info ./target/debug/daemon -c sunnys
```

## 5. Results

### 5.1 Transfer-Encoding

| run | main chunked | main content-length | prefetch chunked | prefetch CL |
|---|---|---|---|---|
| 45 s, alternate | 11/15 | 4/15 | 15/15 | 0 |
| 30 s, alternate | 13/14 | 1/14 | 6/6 | 0 |
| 25 s, alternate | 12/14 | 2/14 | 6/6 | 0 |

Prefetch is always CTE. Catch-up is mostly CTE too, so a CL fast path exists but
is not the common case.

### 5.2 HTTP/2

Not supported on this host.

- Our rustls client offers `h2, http/1.1`; the server returns **no ALPN**
  (`ALPN probe host=... supported=false alpn="-"`).
- `openssl s_client -alpn h2` and `-alpn h2,http/1.1`: `No ALPN negotiated`.
- `curl --http2`: `http_version=1.1`.
- The playlist host (`use*.playlist.ttvnw.net`) also negotiates no ALPN.

### 5.3 HTTP/3

Not supported either.

- No `alt-svc` header on segment or playlist responses.
- `curl --http3-only` fails: `Could not connect to server`.
- `curl --http3` falls back to HTTP/1.1 (`ver=1.1`, with an extra ~100 ms
  failed QUIC attempt).
- Control on the same machine: `curl --http3-only https://cloudflare.com/` ->
  `ver=3`, and `--http2` -> `ver=2`. So the curl build and the network path do
  support QUIC; the CDN simply does not.

### 5.4 TLS vs plain HTTP

Two separate runs (`SEG_SCHEME=https`, then `SEG_SCHEME=http`), 24 fetches each,
same stream and host. Medians:

| | connect (main) | connect (prefetch) | ttfb (main) | total (main) | ttfb (prefetch) | total (prefetch) |
|---|---|---|---|---|---|---|
| http | **6.0 ms** | **4.9 ms** | 8.2 ms | 46.6 ms | 70.2 ms | 1992 ms |
| https | **11.8 ms** | **11.2 ms** | 8.2 ms | 45.4 ms | 62.9 ms | 1986 ms |

- The only consistent difference is connection setup: **TLS costs ~6 ms per
  connection**.
- TTFB and body time are identical once connected. There is no measurable
  per-byte TLS penalty at ~7 Mbps.
- One 1004 ms `connect_ms` outlier appeared in the http main run (transient
  connect/DNS stall); medians above ignore it.
- Caveat: the probe opens a fresh connection per fetch. The production client
  pools connections via `ureq::Agent` (`max_idle_connections_per_host(10)`), so
  the handshake is paid once per host, not per segment. In steady state,
  `httpify_url` saves ~0.
- Security note: the signed URL authenticates the URL, not the payload. Without
  TLS the TS bytes have no integrity protection against a path attacker.

### 5.5 Chunk sizes and pacing

From per-fetch summaries over ~90 segments:

| | prefetch | catch-up |
|---|---|---|
| payload per segment | ~1.8 MB | ~1.8 MB |
| chunks per segment | 95-218 | 3-73 |
| chunk avg | 9-19 KiB | 22-64 KiB |
| chunk max | 25-57 KiB | up to 262112 B |
| chunk min | **6 B** | **6 B** |
| total time | ~2 s | 30-120 ms |

Examples:

```
prefetch  chunks=95  chunk_min=39  chunk_max=49952  chunk_avg=19009  total_ms=1989
prefetch  chunks=193 chunk_min=6   chunk_max=26061  chunk_avg=9751   total_ms=2001
main      chunks=46  chunk_min=138 chunk_max=262112 chunk_avg=35891  total_ms=48
```

The recurring 6-byte chunks are real payload chunks, not framing artefacts.
Prefetch pacing tracks production: ~2 s of wall time per segment regardless of
scheme.

## 6. Connection pooling

Pool per channel, keyed by `(host, port)`. A warm request skips DNS/TCP/TLS and
starts at the request write. The skipped steps cost ~12 ms (TCP ~5 ms + TLS
~6 ms) and ~6 KB of copies.

Warm prefetch ledger: ~2 syscalls for the request, ~2-4 for headers, ~374-748
for the body (rustls still reads 4 KiB at a time), ~100-200 publish wakes,
~4.4 MB of copies. The pool removes ~10-17 setup syscalls, not the body work.

Sizing:

- A live prefetch occupies one connection for ~2 s, exactly the segment
  cadence. One connection is 100% busy in steady state.
- 2 covers the overlap when the next prefetch arrives before the previous body
  finishes, plus a retry or a simultaneous catch-up fetch.
- 3-4 give headroom for catch-up (2-4 parallel 50-120 ms fetches), ad breaks,
  discontinuities (`#EXT-X-MAP` on a new host), and retries.
- The manifest origin (`use*.playlist.ttvnw.net`) gets its own 1-connection
  pool; don't mix it with segment connections.

HTTP/1.1 means one request at a time per connection, so pool size == fetch
concurrency. There is no h2/h3 multiplexing to lean on.

Details:

- Checkout/return are userspace; no syscalls.
- After the CTE terminator (or `Content-Length` bytes) the connection is
  reusable. Track `last_used`; drop after ~30 s idle.
- A server may close an idle keep-alive connection; you find out on the next
  write/read. Discard and retry once on a fresh connection.
- Enable rustls session resumption so a replacement connection is 1-RTT/PSK.
- Do not pipeline requests on one connection.

Daemon shape: one reactor, one task tree per channel. Per channel: scheduler,
writer chain, disk file, connection pool, stats. Shared: `Arc<ClientConfig>`,
TLS session cache, DNS cache with TTL, staging buffer pool. Shutdown drains
pools and flushes readers.

## 7. Reinterpretation of the existing client

- `utils::httpify_url` rewrote every segment URL from `https://` to `http://`,
  and both `dispatch_main` and `dispatch_prefetch` used it. That was
  unconditional, not alternating.
- The measured benefit is one TLS handshake per connection. With ureq's
  connection pool that is amortised to roughly nothing over a stream.
- The bigger latency lever is connection reuse and (if it existed) multiplexing,
  neither of which this CDN offers over h2/h3.
- The CTE path is the hot path; the decoder should be written CTE-first and
  publish incrementally, with `Content-Length` as the secondary path.

## 8. Caveats

- Single stream, single POP, a few minutes of samples. Numbers are indicative,
  not benchmarks.
- Signed playlist/segment URLs expire (~2 min), so manual probes must be quick.
- The probe opens one connection per fetch; production pools. Do not read the
  `connect_ms` deltas as per-segment savings.
- `SEG_SCHEME=alternate` (the probe default) mixes live and finalized segments
  into the aggregate `http_*`/`https_*` stats; use separate runs for clean
  comparisons.
- Single downloader thread. Catch-up throughput is deliberately not measured;
  the probe can lag the live edge.
