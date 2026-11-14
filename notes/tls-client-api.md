# TLS client interface: what rustls costs us, and the alternatives

Status: decision input, not implemented. §7 records the chosen interface
(`rustls::unbuffered` + our own driver); this document records why the default
rustls API is the wrong shape for a completion runtime and what would make us
strip rustls entirely.

Related: `notes/hls-transport-probe.md` (copy/syscall measurements),
`notes/es-rtp-rtsp.md` (slot arena, `ChunkBuf`/`SlotView`, no blocking consumers).

## TL;DR

rustls is a sans-IO library whose public I/O contract is `std::io::Read` +
`std::io::Write` — a *readiness-era* pull interface. compio is completion-based
(push a buffer into the kernel/ring, get it back filled). Those two models do not
meet, so every async runtime inserts a bridge, and every bridge costs buffers,
copies, and control over read sizing. The one thing that would make the mismatch
disappear — in-place decryption into the caller's buffer — is an acknowledged
upstream TODO that does not exist in any API, including the low-level
`unbuffered` one:

```rust
// src/conn/unbuffered.rs:328-336
pub struct ReadTraffic<'c, 'i, Data> {
    conn: &'c mut UnbufferedConnectionCommon<Data>,
    // for forwards compatibility; to support in-place decryption in the future
    _incoming_tls: &'i mut [u8],
    // owner of the latest chunk obtained in `next_record`, as borrowed by
    // `AppDataRecord`
    chunk: Option<Vec<u8>>,
}

// src/conn/unbuffered.rs:349-350
// TODO deprecate in favor of `Iterator` implementation, which requires in-place decryption
pub fn next_record(&mut self) -> Option<Result<AppDataRecord<'_>, Error>> {
```

So: plaintext always lands in a `Vec<u8>` owned by rustls and must be copied out.
The best currently available is 2 payload memcpys per byte; the ideal is 0.

The throughput cost of those copies is **noise** (~26 µs per 1.8 MB segment per
copy at 8 Mbps). The real costs are: (a) rustls decides our read sizes, and
syscalls are 20× more expensive than the copies they would save; (b) three
internal buffers per connection we do not size or see; (c) a state machine and
backpressure signal we must poll correctly; (d) ~4 wrapper crates in the
ecosystem, none of which expose the one API that is completion-friendly.

## 1. What we need from a TLS client

Derived from the client design, not from TLS in general:

1. **Completion-shaped I/O.** "Here is a buffer, fill it" / "here is ciphertext
   in place, give me plaintext". No `Read`/`Write`, no readiness poll.
2. **Decrypt into the destination we choose** — the slot arena region that will
   be published to consumers. Zero payload copies.
3. **Caller-controlled read sizes**, so one 64 KiB read can amortize one IOCP
   submission (`C* = S × BW`, `S` ≈ 1 µs ⇒ reads want to be ≫ 4 KiB).
4. **Explicit, resumable handshake state machine** (`Pending`-able across awaits
   without an internal blocking loop).
5. TLS 1.3, session resumption (reconnects after idle-pool expiry and new CDN
   hosts), SNI, ALPN (offer `http/1.1` only — Twitch's CDN negotiates nothing),
   webpki/system roots, real certificate verification.
6. Windows + Linux, no heavyweight native build step (we just deleted
   `ffmpeg-next` for exactly that reason).
7. Little or no unsafe of our own in a security-critical path.

rustls satisfies 5, 6 (mostly — see §2.11), and 7. It fails 1–4 structurally.

## 2. What rustls actually provides

Three interfaces, all of which buffer, none of which decrypt in place.

### 2.1 `ConnectionCommon` — the default, used by everything

`src/conn.rs`:

| method | shape |
|---|---|
| `read_tls(&mut dyn Read)` (760) | pull, contiguous, `read()` into rustls's deframer buffer |
| `write_tls(&mut dyn io::Write)` (788) | push, contiguous |
| `process_new_packets()` (437) | drive the state machine after a read |
| `reader() -> Reader<'_>` (558) | drain decrypted plaintext (`read`, `fill_buf`, `into_first_chunk`) |
| `writer() -> Writer<'_>` (570) | queue outbound plaintext |
| `complete_io(&mut T: Read + Write)` (602) | **blocking** loop over the above, `WouldBlock`-aware |
| `wants_read()` / `wants_write()` | readiness *by rustls's own buffer state*, not the socket |

Buffers it owns, per connection:

- **Deframer**: `DeframerVecBuffer::prepare_read`
  (`src/msgs/deframer/buffers.rs:214-250`) — `READ_SIZE = 4096`, `Vec` resized to
  `min(allow_max, used + 4096)` on every read, grown to `MAX_WIRE_SIZE`
  (= `16_384 + 2048 + 5`, `src/msgs/message/mod.rs:258-262`) for big handshake
  messages, and shrunk back to 4 KiB when drained. **We cannot ask for a larger
  read**: the read target is rustls's buffer, sized by rustls.
- **Plaintext**: `received_plaintext`, a `ChunkVecBuffer` with
  `DEFAULT_RECEIVED_PLAINTEXT_LIMIT = 16 * 1024`
  (`src/common_state.rs:87, 1064`). `wants_read()` returns false once 16 KiB is
  buffered and undrained (`src/common_state.rs:674-684`) — this is the
  backpressure mechanism, and it means the plaintext pipeline is chunked at
  16 KiB no matter how big our destination is.
- **Outbound**: `sendable_tls` with `DEFAULT_BUFFER_LIMIT = 64 * 1024`
  (`src/common_state.rs:88, 1065`).

Consequences that bite in our code:

- `wants_read()` is **not** "the socket is readable". It is "my plaintext buffer
  is empty and I am not mid-handshake-with-pending-output". A driver that reads
  on socket readiness and only drains afterwards will stall behind the 16 KiB
  limit. It also goes false during the handshake while `sendable_tls` is
  non-empty, so read and write are coupled.
- `Reader::read` returns `ErrorKind::WouldBlock` when there is nothing decrypted
  (`src/conn.rs:240-275`) — a readiness idiom inside a "read" call.
- Post-handshake traffic (TLS 1.3 session tickets, KeyUpdate) makes
  `wants_write()` true at arbitrary times, long after the handshake. A
  hand-rolled completion driver must keep draining `sendable_tls` or the
  connection eventually stalls or the peer times out. Easy to get wrong, hard to
  notice.
- `read_buf` (the only way to avoid zeroing the plaintext buffer *and*, for us,
  the only way to write plaintext into an uninit destination) is **nightly
  only**: `build.rs` sets `cfg(read_buf)` only on nightly
  (`src/lib.rs:373-383`); on stable, everything is `&mut [u8]` over zeroed memory.
- No scatter/gather: `read_tls` takes `Read`, so we cannot hand it "framing
  header buffer + slot tail" the way `readv`/`WSASend` could.
- `set_buffer_limit(Option<usize>)` must be called after construction
  (`src/conn.rs:519`), and the defaults above are the policy unless we change
  them.

`Reader::fill_buf` / `into_first_chunk` do hand back a `&[u8]` pointing at
rustls's chunk (no copy out), but the borrow is tied to the connection and the
bytes live in rustls's `Vec`: useful for parsing a manifest in place, useless
for publishing into the arena.

### 2.2 `unbuffered` — the completion-shaped API that almost works

`rustls::unbuffered` (`src/lib.rs:504-541`, `src/conn/unbuffered.rs`) exists and
is public, not feature-gated. It is genuinely better shaped:

```rust
UnbufferedClientConnection::process_tls_records(&mut self, incoming_tls: &mut [u8])
    -> UnbufferedStatus { discard, state: ConnectionState }

enum ConnectionState {
    EncodeTlsData(..), TransmitTlsData(..), BlockedHandshake,
    WriteTraffic(..), ReadTraffic(..), ReadEarlyData(..), PeerClosed, Closed,
}
```

- No `Read`/`Write`; **we** own the ciphertext buffer and the read sizes.
- Deframing happens **in place** in our slice (`DeframerSliceBuffer::new(incoming_tls)`),
  so no ciphertext copy.
- Handshake is an explicit, resumable state machine — exactly what an async
  driver wants.
- `WriteTraffic::encrypt(app_data, out) -> usize` pushes plaintext into a buffer
  we provide.

And then it hands us plaintext as `AppDataRecord { discard, payload: &[u8] }`
backed by `received_plaintext.pop() -> Vec<u8>` — the same `Vec`, the same
16 KiB limit, and the same in-place-decryption TODO. So it removes 1 of the 2
copies and all of the `Read`/`Write` awkwardness, but not the fundamental one.

Costs, in detail (this is the whole bill):

- **No async wrapper anywhere.** No compio/tokio/futures integration exists, and
  the example the docs point at is **gone from the repo**
  (`src/lib.rs:533` links `examples/src/bin/unbuffered-client.rs`, which is not in
  `main`). quinn is the only reference implementation in the wild.
- **`discard` protocol.** `UnbufferedStatus::discard` (plus
  `AppDataRecord::discard`) must be applied to the *front* of the ciphertext
  slice before the next call. That is a `copy_within` compaction step in our read
  buffer on every state transition.
- **Record straddling.** Our ciphertext buffer must hold whole records; a 16 KiB
  record can arrive split across TCP reads, and after `discard` a trailing
  partial record must survive. That means a carry/ring buffer with
  `MAX_WIRE_SIZE` (18 437 B) headroom, not a plain `Vec`.
- **Transitions have mandatory actions.** `TransmitTlsData` requires
  `done()` to clear `wants_write`; `EncodeTlsData::encode` and
  `WriteTraffic::encrypt` can fail with `InsufficientSize` and must be retried
  with a larger buffer; `EncryptError::EncryptExhausted` exists. Miss an action
  and the machine spins in the same state.
- **Writes appear inside the read loop.** Post-handshake tickets and key updates
  surface as `EncodeTlsData`/`TransmitTlsData` at arbitrary points, so the read
  path must be able to write to the socket. Correctness trap, not just plumbing.
- **Granularity.** One call advances roughly one record, so a 1.8 MB segment is
  ~115 iterations. Fine, but it is our loop, our bounds checks, our EOF and
  `close_notify` handling.

Net: ~200–250 lines of driver plus tests, in the path that matters most for
correctness. The *crypto* and certificate verification stay rustls's.

### 2.3 The bridges

| bridge | underlying model | extra buffers/copies | read sizing |
|---|---|---|---|
| `compio-tls` + `compio_io::compat::AsyncStream<TcpStream>` | completion underneath, futures `poll_read` on top, internal `Rw` + buffer pool (`compio-io/src/compat/async_stream.rs`) | +1 payload copy (kernel→compat buf, compat→deframer) | ours at first, then rustls clamps to 4 KiB |
| `compio-tls` + `PollFd<TcpStream>` | readiness (`poll_read` = poll + `recv`, `compio-runtime/src/fd/poll_fd/mod.rs`) | none | rustls clamps to 4 KiB → ~450–750 poll+recv per segment |
| hand-driven `read_tls`/`write_tls` over `TcpStream::read(Vec)` | completion | ct buf→deframer copy | ours, but rustls still copies in |
| `unbuffered` + our own driver | completion, no traits | none | fully ours |

Each wrapper reimplements "flush `sendable_tls`, loop while handshaking, respect
`wants_read`" — tokio-rustls, futures-rustls and compio-tls each carry their own
copy of that logic, with their own buffers. None of them expose `unbuffered`.
Choosing `compio-tls` means the hot path trusts a fourth-party wrapper around the
buffered API.

## 3. Payload copy ledger

Counting payload-touching memcpys beyond the cipher itself (socket reads are
counted as "kernel → chosen buffer"):

| path | copies | read sizes | notes |
|---|---|---|---|
| in-place (schannel `DecryptMessage`, kTLS) | **0** | ours | rustls has no equivalent |
| rustls `unbuffered` + our driver | 2 | ours (up to buffer size) | pt `Vec` → dst + decrypt-out-of-slice |
| rustls `ConnectionCommon`, completion reads | 2 | 4 KiB (rustls) | ct → deframer is the socket read itself |
| rustls `ConnectionCommon` + `AsyncStream` | 3 | 4 KiB (rustls) + compat buf | +1 for the compat buffer |
| rustls `ConnectionCommon` + `PollFd` | 2 | 4 KiB (rustls) | +1 readiness op per read |

At 8 Mbps / 1.8 MB segments: ~26 µs per copy per segment, i.e. 0.001 % of a
core; a whole segment's I/O is documented as 20–100 µs of CPU in
`hls-transport-probe.md` §3.1. **Copies are not the reason to leave rustls.**
Syscall count and interface coupling are: with `AsyncStream(64 KiB)` we do
~30–100 socket reads per segment; rustls's 4 KiB clamp through a readiness
bridge would be ~450–750.

## 4. The complaints, condensed

1. **No completion-based API** in the core crate, in any version, behind any
   feature. The `Read`/`Write` contract is the only door.
2. **No in-place decryption**, even in the "unbuffered" API — upstream says so in
   the source (§TL;DR). Plaintext is always a rustls-owned `Vec`; copy-out is
   mandatory.
3. **rustls owns read sizing**: 4 KiB `READ_SIZE`, hidden resize to
   `MAX_WIRE_SIZE`, hidden shrink. The caller cannot request a large read, which
   is the single most valuable knob on IOCP/io_uring.
4. **`wants_read()` conflates socket readiness with rustls's buffer state**, and
   flips false at 16 KiB of undrained plaintext or while handshake output is
   pending. Read/write are coupled through it.
5. **16 KiB plaintext backpressure limit by default**, 64 KiB outbound. Hidden
   buffer policy, settable only after construction.
6. **`Read`/`Write` are blocking-shaped**, `complete_io` loops, `Reader::read`
   returns `WouldBlock`. Every async user rewrites the same driver.
7. **`read_buf` is nightly-only** (build.rs-gated), so on stable we cannot even
   write plaintext into uninitialized memory. The zeroed-arena design makes this
   irrelevant for us — but it is symptomatic.
8. **No scatter/gather**: contiguous `Read` only.
9. **Borrowed plaintext is useless to a publish pipeline** (`fill_buf` /
   `into_first_chunk` borrow rustls's `Vec`, tied to the connection).
10. **Ecosystem duplication**: 4+ wrapper crates, duplicated async drivers, none
    exposing the good API. Picking one means inheriting its buffer choices.
11. **Not actually dependency-free** (not a deciding factor): this project pins
    `ring` (`Cargo.lock`; ureq sets rustls `default-features = false` +
    `logging,std,tls12`), which compiles C and perlasm assembly and needs a C
    toolchain (and nasm on Windows x86_64). The default rustls provider is
    `aws-lc-rs`. Recorded for completeness only — `ffmpeg-next` was removed
    because it was too much machinery for what we use, not on principle, and a C
    crypto backend is not in that category.
12. **Version churn**: `Connection`/`ClientConnection`/`ConnectionCommon`,
    provider plumbing (`CryptoProvider`, `builder_with_provider`), and the
    0.22→0.23 rename pain. Minor, recurring.

## 5. Alternatives

### 5.1 schannel (`windows-sys`): the only in-place option we can actually use

`DecryptMessage` decrypts **in place** in the buffer you hand it, and
`EncryptMessage` does the same for writes. That maps 1:1 onto the arena: read a
record into the slot region, decrypt in place, publish. 0 payload copies, and
the handshake is already an explicit token-push/pull state machine
(`InitializeSecurityContext` with `SEC_I_CONTINUE_NEEDED`),
which is naturally completion-friendly. Certificate verification comes from the
OS/CTL (no webpki/roots crate), ALPN is available but unnecessary (the CDN
negotiates nothing), TLS 1.3 on Win11/Server 2022+.

Costs: Windows-only (drops Linux/macOS), ~300–400 lines of unsafe FFI and
`SecBuffer` management, `SEC_E_INCOMPLETE_MESSAGE` handling (i.e. record
straddling) is on us, and correctness testing of the security path becomes ours.
Record straddling matters for the arena: a TLS record is ≤ 16 KiB + 5, so reading
directly into a slot needs a 2-record carry region at slot boundaries.

### 5.2 Keep TLS for the control plane, plaintext for segments

Revert `httpify_url`'s deletion: segments over `http://`, everything else over
TLS. The hot path becomes pure compio completion I/O with zero rustls and zero
copies, and the client's TLS usage shrinks to 3 requests at startup where
rustls's interface costs nothing.

Cost: no payload integrity on the media path — the signed URL authenticates the
URL, not the bytes, so a path attacker can rewrite the TS stream (flagged in
`hls-transport-probe.md` §5.4). Also relies on Twitch continuing to serve
plaintext segment URLs.

### 5.3 kTLS (Linux only)

Kernel decrypts into userspace buffers in place, 0 copies. Not available on
Windows, which is the platform we ship. Recorded only for completeness.

### 5.4 BoringSSL / OpenSSL

`SSL_read` still copies out of an internal buffer; you add CMake/nasm/ABI
problems and a much larger dependency. No improvement on the actual complaint.

### 5.5 Hand-rolled TLS 1.3

No. Writing the record layer is one thing; certificate validation, ECH
resistance, downgrade protections, and constant-time discipline are another.

### 5.6 Wait for upstream in-place decryption

Tracked by the TODO quoted at the top of this document. If it lands, the
`unbuffered` API becomes the 0-copy, completion-shaped answer and this document
mostly expires.

## 6. Decision matrix

| option | payload copies | socket model | read sizing | our code | platform | verdict |
|---|---|---|---|---|---|---|
| compio-tls + `AsyncStream(64K)` | 3 | completion | ours, then rustls 4 KiB | ~0 | all | works; fallback if the driver stalls the port |
| compio-tls + `PollFd` | 2 | readiness | rustls 4 KiB | ~0 | all | no: readiness in the hot path, ~450–750 ops/segment |
| rustls `unbuffered` + driver | 2 | completion | ours | ~200–250 lines, ours to own | all | **chosen**: only completion-native rustls, no futures-io, 1 copy less, upgrades to 0 if upstream lands in-place decrypt |
| schannel in place | 0 | completion | ours | ~350 lines unsafe FFI | Windows | the only 0-copy option today; rejected for portability/FFI cost |
| plaintext segments | 0 | completion | ours | ~0 | all | cheapest; trades payload integrity |
| kTLS | 0 | completion | ours | ~0 | Linux | unavailable here |

## 7. What we do now

**`rustls::unbuffered` + our own driver.** Reasons, in order:

1. It is the only rustls interface that never leaves compio's completion model:
   no `futures_io`, no compat buffer, no `Rw`, no readiness polling anywhere in
   the client.
2. It drops `compio-tls`, `futures-rustls` and the `io-compat` feature from the
   dependency graph; TLS becomes a transform on a byte stream we already own.
3. The driver we write is the enabling investment for 0 copies: when upstream
   lands in-place decryption, one call site changes and the arena is decrypted
   into directly.
4. The cost that is *not* paid: read sizing. `unbuffered` lets us read 64 KiB at
   a time, same as `AsyncStream(64K)`, so the syscall count is already solved.

Shape, confined to `crates/daemon/src/http/tls.rs`:

```rust
// public surface of the transport module — no rustls types outside tls.rs
enum Conn { Tcp(TcpStream), Tls(TlsConn) }

struct TlsConn {
    conn: UnbufferedClientConnection,
    ct: Box<[u8]>,      // ciphertext carry buffer, >= 64 KiB, record-aligned tail
    ct_used: usize,
    out: Box<[u8]>,     // encrypt scratch: plaintext + record overhead
}

impl Conn {
    async fn read_into(&mut self, dst: &mut [u8]) -> io::Result<usize>;  // decrypted payload
    async fn write_all(&mut self, buf: &[u8]) -> io::Result<()>;
}
fn client_config() -> Arc<rustls::ClientConfig>;    // ALPN http/1.1, ring, session cache
```

Driver obligations are enumerated in §2.2. Everything above `http/tls.rs`
(decoder, pool, redirects, timeouts) is TLS-agnostic, so falling back to
`compio-tls` + `AsyncStream`, or switching to schannel/plaintext later, touches
one file.

Above the transport, `http/mod.rs` is a small pool keyed by `(tls, host, port)`
(4 idle connections per host, 8 total, 30 s idle); redirects are followed for
GET/HEAD; the body decoder is chunked-first (Twitch's segment CDN is mostly
`Transfer-Encoding: chunked`) and reads payload straight into the caller's
buffer, so a body is copied at most once.

Testing (we must not debug handshakes against Twitch):

- loopback harness: a `std::net::TcpListener` thread running rustls's *buffered*
  `ServerConnection` (the boring, well-tested side) against our unbuffered
  client;
- cases: handshake + ALPN negotiation, one small body, a body ≫ 16 KiB (record
  loop), a record split across two socket writes (carry buffer), server
  `close_notify`, abrupt TCP EOF, `refresh_traffic_keys` / post-handshake
  ticket, and an `InsufficientSize` retry path;
- then the real endpoints (GQL/usher) via the normal `-c <channel>` run.

## 8. Triggers that would make us strip rustls

Any one of these is enough to reopen the decision:

1. TLS record processing shows up in a `perf`/Tracy trace on the reactor (the
   remaining 2 copies, after our driver already removed the third).
2. We go Windows-only (tray-first product) and want arena-direct decryption.
3. rustls upstream lands in-place decryption — then we *upgrade* rather than
   strip.
4. Security review concludes payload integrity on the media path is not worth a
   third-party TLS stack *and* an extra copy (→ §5.2).
5. `ring`/`aws-lc-rs` build friction becomes a problem (→ §5.2 or platform TLS).

## 9. Receipts

Versions: `rustls 0.23.43` (lockfile), `ring` provider; `compio 0.19.x`.
(With §7, `compio-tls`, `futures-rustls` and compio's `io-compat` feature are not
needed.)

- `rustls/src/msgs/deframer/buffers.rs:214-250` — `READ_SIZE = 4096`,
  `prepare_read` resize/shrink.
- `rustls/src/msgs/message/mod.rs:258-262` — `MAX_PAYLOAD = 16_384 + 2048`,
  `MAX_WIRE_SIZE`.
- `rustls/src/common_state.rs:87, 88, 1064, 1065` — plaintext 16 KiB limit,
  outbound 64 KiB limit.
- `rustls/src/common_state.rs:674-684` — `wants_read()` semantics.
- `rustls/src/conn.rs:54, 64, 87, 112, 519, 558, 570, 602, 760, 788` — buffered
  API surface.
- `rustls/src/conn.rs:240-300` — `Reader::read` `WouldBlock`, `fill_buf`,
  `into_first_chunk`.
- `rustls/src/conn/unbuffered.rs:16-30, 182-200, 328-360, 424-433` — unbuffered
  state machine, `discard` protocol, plaintext still a `Vec`, in-place TODO.
- `rustls/src/lib.rs:373-383` — `cfg(read_buf)` set by build.rs, nightly only.
- `rustls/src/lib.rs:504-541` — unbuffered module docs ("does not internally
  buffer TLS nor plaintext data" — true for ciphertext, not for plaintext).
  Note `src/lib.rs:533` links `examples/src/bin/unbuffered-client.rs`, which no
  longer exists in `rustls` `main`; quinn is the only reference driver.
- `compio-runtime/src/fd/poll_fd/mod.rs` — `PollFd::poll_read` = readiness +
  `recv`; futures impls.
- `compio-io/src/compat/async_stream.rs` — `AsyncStream::with_capacity`,
  `poll_read` copies out of an internal buffer.

## 10. Trust store and revocation: the eMudhra case (Oct 2026)

Status: **open question, no decision.** This section records the incident, what
it would have cost us, and the options. It does not change the port: we keep
`webpki-roots` for now.

### 10.1 What happened

Eight **intermediate** CA certificates issued by eMudhra (India) had ECDSA P-384
private keys whose scalars were small integers — `4, 56, 66, 115, 155, 156, 230,
237`. Recovering them from the public certificates takes under a second (try
`k·G` for `k = 1..256`). Six were TLS intermediates, two code-signing. Each also
existed as a second certificate with the **same key**, cross-signed by Sectigo's
`AAA Certificate Services`, for 16 affected certificates in total.

| time (UTC) | event |
|---|---|
| Oct 17, 16:27 | keys recovered (`paranoid_crypto`) |
| Oct 17, 18:05 | reported to Apple, Chrome, Microsoft, Mozilla, eMudhra |
| Oct 17, 19:45 | Chrome CRLSet block live, matched by **public key** (covers Sectigo cross-signs) |
| Oct 17, 22:59 | Firefox OneCRL block live |
| Oct 18, 09:24–09:38 | eMudhra revokes all eight, reason "CA Compromise" |
| Oct 19 | Apple block; Sectigo revokes the eight cross-signs (10:35) |
| Oct 20 | Mozilla emergency-distrusts `emSign ECC Root CA - G3`; Microsoft block |
| Oct 26 | out-of-band Windows update removes seven affected certs from the trust list |
| Oct 27 | Chrome root-store removal reaches all devices |

Anyone holding one of those keys can mint a certificate for **any** domain, so
the affected set is not "eMudhra customers" — it is every TLS endpoint reachable
from a client that still trusts the chain, Twitch included. ~30 800 unexpired
precertificates chained to the affected root.

### 10.2 There are exactly three response mechanisms

| mechanism | examples | needs a binary update? | needs a restart? | caught this incident? |
|---|---|---|---|---|
| signed blocklist pushed out of band, matched by public key | Chrome CRLSet, Firefox OneCRL | no | no | yes, ~2–5 h |
| OS/platform trust store | Windows `disallowed` store, Apple | no | next verification | yes, 1–10 days late |
| bundled roots + app logic | Go, Node, Java, distros, `certifi`, **us** | **yes** | yes (new process) | only after a rebuild + user update |

The browser column is the exception, not the norm: non-browser clients get
patched releases. Mozilla's handler, on this incident: many clients do not
consume revocation data and require software updates.

### 10.3 A root-bundle bump is not sufficient (the cross-sign trap)

Removing `emSign ECC Root CA - G3` from `webpki-roots` fixes the eMudhra-signed
path but **not** the Sectigo cross-signed path: those eight keys are also
certificates under `AAA Certificate Services`, which stays trusted. rustls has no
CRLSet-style "distrust this intermediate by SPKI" data channel, so the fix is
code — a `ServerCertVerifier` wrapper that rejects presented `intermediates` by
exact DER (the 16 certs are public; no X.509 parsing needed, and blocking the
intermediate still catches leaves minted *under* the compromised key, because
that intermediate appears in the chain). A wrapper is a source change, i.e. a new
binary.

### 10.4 Options

| option | rebuild on CA incident | other costs |
|---|---|---|
| `webpki-roots` only (current) | yes, always | also needs the DER blocklist above; window = release + user update lag |
| `rustls-platform-verifier` (OS store) | no | trusts enterprise/user-installed roots (MITM proxies, malware); platform drift; still late by OS update cadence; Windows-only behaviour differs from Linux |
| narrowed root set / CA pin for `*.ttvnw.net`, `*.twitch.tv` | only when Twitch rotates CA | a rogue CA — trusted by everyone — cannot impersonate our endpoints at all; needs backup pins + escape hatch, or the app hard-fails on rotation |
| CRLs (`WebPkiServerVerifier…with_crls`) | no, but we must fetch/refresh | fail-closed = a CDN's CRL endpoint outage kills the stream; fail-open = attacker blocks the fetch. Deployment channel is also attacker-influenced |
| our own signed blocklist fetched at runtime | no | we now run a security-critical distribution channel; adds a network dependency and a second trust root |
| SCT/CT validation | n/a | rustls has no SCT validation; CT monitoring of `twitch.tv` is Twitch's control, not the client's |

### 10.5 Why this is structural, not a rustls problem

Any of ~150 roots — plus every intermediate they have signed — can mint a valid
certificate for any domain, and there is no mechanism that reliably removes that
ability from already-shipped clients. Delegation is transitive and unconstrained;
revocation is advisory in practice; the one enforcement channel that works
(pushed, key-matched blocklists) is a browser-infrastructure privilege we do not
have. Pinning narrows the blast radius to the pins' issuers (and inherits the
bootstrap problem: who signed the pin). Worth keeping in mind when deciding how
much machinery to build here — the ceiling on any in-app mitigation is lower than
it looks.

### 10.6 What this means for us, concretely

- Blast radius is not video tampering: `--auth-token` is sent to
  `id.twitch.tv` (OAuth validate) and `gql.twitch.tv` (`PlaybackAccessToken`), so
  a forged chain there means token theft. Without `--auth-token`, the impact is
  content authenticity only.
- Our 30 s idle pool re-verifies on every new handshake, so a trust-store change
  takes effect on the next connection without a restart. A long-lived connection
  never re-verifies — inherent to TLS.
- Whatever we choose, the operation must be written down: bump
  `webpki-roots` **and** add DER blocklist entries, ship, sign, hope users
  update. Worth measuring how long our own update tail is before deciding.

### 10.7 Open questions

1. Bundle roots (rebuild) vs platform verifier (no rebuild, trust-store baggage)?
2. Add a narrow/pinned trust set for Twitch's hosts (protects against rogue CAs
   outright) or accept the class?
3. If pinned: pin CA intermediates or leaf SPKIs; how many backups; what is the
   escape hatch when Twitch rotates?
4. Do we ever want a runtime-updatable blocklist, with the extra trust root that
   implies?

### 10.8 Sources

- Cloudflare, "8 eMudhra certificates are compromised" (2026-10-20)
- Google Security Blog, eMudhra interim report; Chrome Root Program CRLSet
- Mozilla dev-security-policy bug (Dennis Jackson), 2026-10-17/20
- Kudelski Security, "eMudhra Intermediate CA Private Key Compromise" (key table,
  revocation timestamps, Sectigo cross-signs)
- Scott Helme, key recovery write-up ("one of their private keys is the number 4")
- The Register, CSO Online, Ars Technica coverage of the timeline
