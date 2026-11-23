# TLS client interface

The HTTP client is `rustls::unbuffered` plus our own driver, confined to
`crates/daemon/src/http/tls.rs` (`TlsConn`): no rustls type escapes that module,
and everything above it — decoder, pool, redirects, timeouts — is TLS-agnostic.
This note records what that choice costs and what would make us change it.

## TL;DR

- rustls's public I/O contract is `std::io::Read` + `std::io::Write`, a
  readiness-era *pull* interface; compio is completion-based (hand over a buffer,
  get it back filled). Every bridge between them costs buffers, copies and control
  over read sizing.
- The one thing that would make the mismatch disappear — decrypting in place into
  the caller's buffer — is an acknowledged upstream TODO, absent from every API
  including the low-level `unbuffered` one:

  ```rust
  // src/conn/unbuffered.rs:328-336
  pub struct ReadTraffic<'c, 'i, Data> {
      conn: &'c mut UnbufferedConnectionCommon<Data>,
      // for forwards compatibility; to support in-place decryption in the future
      _incoming_tls: &'i mut [u8],
  ```

- `unbuffered` removes the traits and one copy, and keeps read sizing ours. That
  is the whole win, and it is enough.
- Copies are **noise** (~26 µs per 1.8 MB segment per copy at 8 Mbps). The real
  costs are read sizing and interface coupling: rustls clamps reads to 4 KiB,
  and `wants_read()` conflates socket readiness with rustls's buffer state.

## What we need

1. **Completion-shaped I/O**: "here is a buffer, fill it", no readiness poll.
2. **Decrypt into the destination we choose** — the slot arena region that will be
   published to consumers.
3. **Caller-controlled read sizes**, so one 64 KiB read amortizes one IOCP
   submission (`C* = S × BW`, `S` ≈ 1 µs ⇒ reads want to be ≫ 4 KiB).
4. **Explicit, resumable handshake state machine**, `Pending`-able across awaits.
5. TLS 1.3, session resumption (reconnects and new CDN hosts), SNI, ALPN
   (`http/1.1` only — Twitch's CDN negotiates nothing), real certificate
   verification.
6. Windows + Linux, no heavyweight native build step.
7. Little or no unsafe of our own in a security-critical path.

rustls satisfies 5–7 and fails 1–4 structurally.

## `unbuffered`: what it gives, what it costs

```rust
UnbufferedClientConnection::process_tls_records(&mut self, incoming_tls: &mut [u8])
    -> UnbufferedStatus { discard, state: ConnectionState }

enum ConnectionState {
    EncodeTlsData(..), TransmitTlsData(..), BlockedHandshake,
    WriteTraffic(..), ReadTraffic(..), ReadEarlyData(..), PeerClosed, Closed,
}
```

We own the ciphertext buffer and the read sizes; deframing happens in place in our
slice (`DeframerSliceBuffer`), so no ciphertext copy; the handshake is an explicit
resumable state machine. Plaintext, however, still arrives as
`AppDataRecord { payload: &[u8] }` backed by `received_plaintext.pop() -> Vec<u8>`
— the same `Vec`, the same 16 KiB limit, the same in-place-decryption TODO. One
copy out is mandatory.

Costs, in detail (the whole bill):

- **No async wrapper anywhere.** quinn is the only reference driver in the wild.
- **`discard` protocol.** The returned count must be applied to the *front* of the
  ciphertext slice before the next call — a `copy_within` compaction on every state
  transition.
- **Record straddling.** The ciphertext buffer must hold whole records: a 16 KiB
  record can arrive split across TCP reads, and after `discard` a trailing partial
  record must survive. That means a carry buffer with `MAX_WIRE_SIZE` (~18 KiB)
  headroom, not a plain `Vec`.
- **Transitions have mandatory actions.** `TransmitTlsData` needs `done()`;
  `encode`/`encrypt` can fail with `InsufficientSize` and must be retried with a
  larger buffer; there is `EncryptError::EncryptExhausted`. Miss one and the machine
  spins in the same state.
- **Writes appear inside the read loop.** Post-handshake tickets and key updates
  surface as `EncodeTlsData`/`TransmitTlsData` at arbitrary points, so the read path
  must be able to write to the socket.
- **Reads appear inside the write path.** `process_tls_records` can return
  `ReadTraffic` during a `write_all`; queue that plaintext for the next read and
  fold each record's `discard` into the same drain, or the next read decrypts
  already-consumed ciphertext again.
- **Granularity.** One call advances roughly one record: a 1.8 MB segment is ~115
  iterations, all our loop, our bounds checks, our `close_notify` handling.

Net: ~200–250 lines of driver plus tests, in the path that matters most for
correctness. The crypto and certificate verification stay rustls's.

## Payload copy ledger

Payload-touching memcpys beyond the cipher itself (socket reads counted as
"kernel → chosen buffer"):

| path | copies | read sizes | notes |
|---|---|---|---|
| in-place (schannel `DecryptMessage`, kTLS) | **0** | ours | rustls has no equivalent |
| rustls `unbuffered` + our driver | 2 | ours (up to buffer size) | plaintext `Vec` → dst + decrypt-out-of-slice |
| rustls `ConnectionCommon`, completion reads | 2 | 4 KiB (rustls) | ct → deframer is the socket read itself |
| rustls `ConnectionCommon` + `AsyncStream` | 3 | 4 KiB (rustls) + compat buf | +1 for the compat buffer |
| rustls `ConnectionCommon` + `PollFd` | 2 | 4 KiB (rustls) | +1 readiness op per read |

At 8 Mbps / 1.8 MB segments a copy is ~26 µs, i.e. 0.001 % of a core; a whole
segment's I/O is 20–100 µs of CPU (`hls-transport-probe.md` §2.1). **Copies are
not the reason to leave rustls.**

## The buffered API's traps

Why we do not drive `ConnectionCommon` even though it would work:

- **rustls owns read sizing**: `READ_SIZE = 4096`, grown to `MAX_WIRE_SIZE` for
  handshake messages and shrunk back when drained. The caller cannot ask for a
  large read, which is the most valuable knob on IOCP/io_uring.
- **16 KiB plaintext limit** (`received_plaintext`), 64 KiB outbound
  (`sendable_tls`). `wants_read()` returns false once 16 KiB is buffered and
  undrained, so the pipeline is chunked at 16 KiB regardless of our destination.
- **`wants_read()` is not socket readiness.** It is "my plaintext buffer is empty
  and I am not mid-handshake-with-pending-output", and it goes false while
  outbound handshake data is queued, coupling read and write.
- **`Reader::read` returns `WouldBlock`** — a readiness idiom inside a read call.
  Post-handshake tickets make `wants_write()` true at arbitrary times, so a driver
  must keep draining or the connection stalls and the peer times out.
- **`read_buf` is nightly-only** (build.rs-gated), so on stable plaintext always
  lands in zeroed memory. The zeroed arena makes this irrelevant to us.
- **No scatter/gather**: contiguous `Read` only; no "framing header + slot tail".
- Borrowed plaintext (`fill_buf`, `into_first_chunk`) is tied to the connection and
  lives in rustls's `Vec`: fine for parsing a manifest, useless for publishing.

Wrappers (`compio-tls`, `tokio-rustls`, `futures-rustls`) each reimplement "flush
`sendable_tls`, loop while handshaking, respect `wants_read`" with their own
buffers, and none exposes `unbuffered`.

## Alternatives

| option | copies | socket model | read sizing | our code | verdict |
|---|---|---|---|---|---|
| compio-tls + `AsyncStream(64K)` | 3 | completion | ours, then rustls 4 KiB | ~0 | works; fallback if the driver stalls the port |
| compio-tls + `PollFd` | 2 | readiness | rustls 4 KiB | ~0 | no: readiness in the hot path, ~450–750 ops/segment |
| **rustls `unbuffered` + driver** | 2 | completion | ours | ~250 lines | **chosen** |
| schannel in place | 0 | completion | ours | ~350 lines unsafe FFI | the only 0-copy option today; rejected for portability |
| plaintext segments | 0 | completion | ours | ~0 | cheapest; trades payload integrity |
| kTLS | 0 | completion | ours | ~0 | Linux only |

- **Plaintext segments** (TLS for the control plane only) would make the hot path
  pure compio with zero rustls and zero copies, and shrink TLS to three startup
  requests. Cost: the signed URL authenticates the URL, not the bytes, so a path
  attacker can rewrite the TS stream; it also relies on Twitch continuing to serve
  plaintext segment URLs.
- **schannel** is the only in-place option: `DecryptMessage` works in the buffer we
  hand it, which maps 1:1 onto the arena, and the handshake is already a token
  state machine. Windows-only, ~350 lines of unsafe FFI, and record straddling at
  slot boundaries becomes ours.
- **BoringSSL/OpenSSL** still copy out of an internal buffer. **Hand-rolled TLS
  1.3** is not worth owning certificate validation for. **Waiting for upstream
  in-place decryption** would make `unbuffered` the 0-copy answer and retire this
  note.

## Decision

`rustls::unbuffered` + our driver, for four reasons:

1. It is the only rustls interface that never leaves compio's completion model:
   no `futures_io`, no compat buffer, no `Rw`, no readiness polling.
2. It drops `compio-tls`, `futures-rustls` and compio's `io-compat` feature from
   the graph; TLS becomes a transform on a byte stream we already own.
3. The driver is the enabling investment for 0 copies: when upstream lands in-place
   decryption, one call site changes and the arena is decrypted into directly.
4. Read sizing is already solved: 64 KiB reads, same as `AsyncStream(64K)`.

Testing (we must not debug handshakes against Twitch): a loopback harness running
rustls's buffered `ServerConnection` against our client, covering handshake + ALPN,
a small body, a body ≫ 16 KiB, a record split across socket writes, `close_notify`,
abrupt EOF, a post-handshake ticket, and an `InsufficientSize` retry.

## Triggers to reopen

1. TLS record processing shows up in a `perf`/Tracy trace on the reactor.
2. We go Windows-only and want arena-direct decryption (schannel).
3. rustls lands in-place decryption — then we upgrade rather than strip.
4. Payload integrity on the media path is judged not worth a third-party TLS stack
   plus a copy (plaintext segments).
5. `ring`/`aws-lc-rs` build friction becomes a problem.

## Receipts

`rustls 0.23.43` (lockfile), `ring` provider; `compio 0.19.x`.

- `msgs/deframer/buffers.rs` — `READ_SIZE = 4096`, resize/shrink.
- `msgs/message/mod.rs` — `MAX_PAYLOAD`, `MAX_WIRE_SIZE`.
- `common_state.rs` — plaintext 16 KiB, outbound 64 KiB, `wants_read()`.
- `conn.rs` — buffered API, `Reader::read` `WouldBlock`, `fill_buf`.
- `conn/unbuffered.rs` — state machine, `discard`, plaintext still a `Vec`,
  in-place TODO.
- `lib.rs` — `cfg(read_buf)` set by build.rs, nightly only; unbuffered module docs.

## Trust store: the eMudhra case (Oct 2026)

Status: **open question, no decision.** We keep `webpki-roots` for now.

Eight eMudhra intermediates had ECDSA P-384 keys whose scalars were small integers
(`4, 56, 66, …`), recoverable from the public certificates in under a second. Each
also existed cross-signed by Sectigo's `AAA Certificate Services`, 16 certificates
in total. Anyone with one of those keys could mint a certificate for **any**
domain, so the blast radius was every TLS endpoint reachable from a client still
trusting the chain — Twitch included. Browsers blocked them by public key through
pushed CRLSet/OneCRL within hours; OS trust stores took days; bundled-root clients
got it only with a rebuild and a user update.

Three response mechanisms, and that is the whole menu:

| mechanism | needs a binary update? | needs a restart? | caught this incident? |
|---|---|---|---|
| signed blocklist, matched by public key (CRLSet/OneCRL) | no | no | yes, ~2–5 h |
| OS/platform trust store | no | next verification | yes, 1–10 days late |
| bundled roots + app logic (**us**) | **yes** | yes | only after rebuild + update |

**A root-bundle bump is not sufficient.** Removing `emSign ECC Root CA - G3` does
not touch the Sectigo cross-signs, because `AAA Certificate Services` stays
trusted. rustls has no SPKI-distrust data channel, so the fix is code: a
`ServerCertVerifier` wrapper that rejects the 16 presented intermediates by exact
DER (they are public; no X.509 parsing needed), which still catches leaves minted
under a compromised key because the intermediate appears in the chain.

Options: `webpki-roots` + that DER blocklist (always a rebuild); the OS verifier
(no rebuild, but trusts user-installed roots and drifts per platform); a narrowed
or pinned trust set for Twitch hosts (immune to rogue CAs, needs backups and an
escape hatch); CRLs (a CDN outage kills the stream, and the fetch is itself
attacker-influenced); our own runtime blocklist (a security-critical distribution
channel). SCT validation is not available in rustls.

Concretely: with `--auth-token`, a forged chain over `id.twitch.tv` or
`gql.twitch.tv` means token theft; without it, only content authenticity. The 30 s
idle pool re-verifies on every new handshake, so a trust-store change takes effect
on the next connection without a restart.

Open: bundle vs platform verifier; whether to pin; pin CA intermediates or leaf
SPKIs and how many backups; whether to run a runtime-updatable blocklist at all.
