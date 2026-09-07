# Invariants and soundness proof for the lock-free chunked stream

This document describes every invariant the `segment` module depends on for
memory safety and correctness, and argues that the current code upholds them.
It deliberately does not trust the inline `// SAFETY:` comments; each is
re-derived from the code and corrected where it is misleading.

Files covered: `segment.rs`, `segment/writer.rs`, `segment/reader.rs`, plus the
only writer (`downloader.rs::launch_download_ureq`) and the two readers
(`tcp.rs`, `disk.rs`).

---

## 1. Types

```rust
pub struct Chunk {                       // repr(align(64))
    base: usize,                          // absolute stream offset of byte 0 of this chunk
    next: OnceLock<Arc<Chunk>>,           // at most one successor
    data: UnsafeCell<[MaybeUninit<u8>; CHUNK_SIZE]>,
}

pub struct Stream { inner: AtomicU64 }    // packed (len, closed)

pub struct Writer { tail: Arc<Chunk>, handle: Arc<Stream>, written_len: usize }

pub struct Reader { cursor: Arc<Chunk>, position: usize, handle: Arc<Stream> }

pub struct SeekableReader { chunks: Vec<Arc<Chunk>>, length: usize, position: usize }
```

`Stream.inner` packs a monotonically increasing frontier:

```
bit 63            : closed
bits 0..63        : published length (LEN_MASK = !(1<<63))
```

`set(len, closed)` performs a `Release` store; `load()` performs an `Acquire`
load. `wait(value)` blocks until the packed value differs from `value` (or
wakes spuriously), using `wait_on_address::AtomicWait`.

`CHUNK_SIZE = 770_048 = LCM(16384, 188)`, so `CHUNK_SIZE` is a multiple of both
the 16 KiB download buffer and the 188 B TS packet, and chunk boundaries always
fall on TS packet boundaries.

---

## 2. Invariants

### 2.1 Ownership / aliasing

- **I1 (single writer).** There is exactly one `Writer` per `Stream`.
  `Writer` is not `Clone`; it is created once in `Stream::new_stream` and
  thereafter *moved* (never shared) through the download worker. Consequently
  only one thread may ever write `Chunk::data` for a given stream.

- **I2 (immutable base).** `Chunk::base` is written exactly once, during
  `Chunk::new`, before the `Arc<Chunk>` escapes to any other owner. It is never
  mutated afterwards.

- **I3 (single link).** `Chunk::next` is set at most once. It is stored via
  `OnceLock::set`, which is a `Release` store, and only the writer ever calls
  it. (The `Drop` impl calls `OnceLock::take`, which merely moves the value
  back out; it never re-`set`s.)

- **I4 (data ownership).** Bytes in `Chunk::data` are written only by the
  `Writer`, only in its current tail chunk, and only at offsets
  `>= written_len - tail.base`. Every other access to `data` is a read.

### 2.2 Layout

- **I5 (contiguous bases).** The chunks form a linked list starting at head
  (`base = 0`), where each successor satisfies
  `next.base = prev.base + CHUNK_SIZE`. Hence chunk *i* has `base = i * CHUNK_SIZE`.

- **I6 (tail span).** For the writer's tail chunk,
  `tail.base <= written_len <= tail.base + CHUNK_SIZE`. Equivalently
  `0 <= tail_offset() <= CHUNK_SIZE`, and `tail_offset() == CHUNK_SIZE` iff the
  tail is exactly full.

### 2.3 Initialization

- **I7 (frontier of initialization).** Every byte at absolute offset
  `< written_len` is initialized; every byte at absolute offset
  `>= written_len` (which can only exist in the tail) is uninitialized.

### 2.4 Publication / memory ordering

- **I8 (monotonic frontier).** `written_len` never decreases and `closed`
  never reverts to `false`, so the packed `AtomicU64` value is monotonically
  non-decreasing. This makes the `wait` primitive safe against ABA (no
  lost wakeups).

- **I9 (data publication).** The writer writes bytes, then advances
  `written_len` via `commit`, then calls `publish`, which does a `Release`
  store of `written_len` (`closed = false`). A reader's `Acquire` load of the
  frontier therefore synchronizes with the writer's writes: any reader that
  observes `published_len = P` may safely read all bytes in `[0, P)`.

- **I10 (link-before-publish).** A chunk's `next` is set *before* the writer
  ever publishes `written_len > chunk.base + CHUNK_SIZE`. Therefore a reader
  that observes `published_len > chunk.base + CHUNK_SIZE` is guaranteed to find
  `chunk.next` already initialized. This is the "invariant 3" the writer's
  `link_new_chunk` comment refers to.

### 2.5 Position / frontier consistency

- **I11 (reader stays behind).** For any `Reader`, `position <= published_len`
  at all times; `position` always identifies the first unread byte.

- **I12 (cursor bounds).** For any `Reader`, `cursor.base <= position <=
  cursor.base + CHUNK_SIZE`. When `position == cursor.base + CHUNK_SIZE`
  (`offset() == CHUNK_SIZE`), the cursor is exhausted and must be advanced via
  `next` before reading.

- **I13 (non-empty tiling).** Each `Reader::chunk()` call returns a non-empty
  slice, and consecutive returned slices are contiguous and non-overlapping,
  tiling the stream up to the published frontier.

- **I14 (closure).** `Writer::drop` stores `(written_len, closed = true)` and
  notifies. After that no byte is ever published again, and a reader's
  `wait_for_data` returns `None` (stream terminates).

- **I15 (SeekableReader snapshot).** `SeekableReader` holds the entire chunk
  list (`chunks[i].base == i * CHUNK_SIZE`) and a `length` equal to the final
  `written_len`. Bytes `[0, length)` are immutable by the time construction
  completes (the stream is closed), so `read`/`seek` never race the writer.

---

## 3. Soundness proof

### 3.1 `Chunk::new`

`Arc::new_uninit` yields an `Arc<MaybeUninit<Chunk>>` with strong count 1 and
weak count 0. `Arc::get_mut(...).unwrap_unchecked()` is sound because a freshly
created `Arc` has no other strong handles and no weak handles.

`addr_of_mut!` is used to obtain field pointers without forming references, so
`base` and `next` are initialized without touching `data`. `data` has type
`[MaybeUninit<u8>; CHUNK_SIZE]`; `MaybeUninit<u8>` is a value that is valid for
*every* bit pattern, so the `data` field is a valid value even though its inner
bytes were never written. Thus after writing `base` and `next`,
`assume_init()` is sound.

### 3.2 `unsafe impl Send` / `unsafe impl Sync` for `Chunk`

- *Send*: by I1 only one thread writes `data`; the tail is exclusively owned.
- *Sync*: concurrent access to `data` is only reads of published bytes
  (I7 + I9), `base` is immutable (I2), and `next` is an interior-mutable
  `OnceLock` synchronized by its own release/acquire semantics plus I10.

### 3.3 `Writer::writable_slice`

`UnsafeCell::get()` returns `*mut [MaybeUninit<u8>; CHUNK_SIZE]`;
`as_mut_unchecked()` yields `&mut [MaybeUninit<u8>; CHUNK_SIZE]`, and indexing
`[tail_offset()..]` selects the writable tail. This is sound because the
`Writer` is the sole owner of the tail (I1, I4) and `tail_offset() <=
CHUNK_SIZE` (I6). When the tail is full, `link_new_chunk` first links a fresh
chunk, so the returned slice is always non-empty and in-bounds.

### 3.4 `Writer::commit`

`unsafe` because it trusts the caller to have initialized the next `n` bytes of
the last `writable_slice`. The caller (`downloader.rs`) initializes exactly
`written` bytes via `Read::read` and passes `n = written`; the `assert!`
bounds `n` to the tail span (I6). Sound.

### 3.5 `Writer::link_new_chunk`

`debug_assert_eq!(written_len, tail.base + CHUNK_SIZE)` (I6). `Chunk::new`
creates a successor with `base + CHUNK_SIZE` (I5). `tail.next.set(...)` stores
the link *before* any later `commit`/`publish` can raise `written_len` past
this chunk, upholding I10. The `.expect` is safe by I3.

### 3.6 `Reader::chunk`

`wait_for_data` returns either `Some(P)` with `P > position`, or `None` once
closed. If `offset() == CHUNK_SIZE`, the reader follows `next`: by I10 the
observed `P > cursor.base + CHUNK_SIZE` implies `next` is set, so
`.next.get().expect(...)` is sound. `end = min(P - base, CHUNK_SIZE)` satisfies
`offset < end`, so the slice is non-empty (I13).

The `Acquire` load in `load()` synchronizes with the writer's `Release` store
in `publish` (I9), so bytes `[0, end)` are initialized (I7) and the chunk is
kept alive by `self.cursor`. Hence `data.get().as_ref_unchecked()` and
`slice[offset..end].assume_init_ref()` are sound.

### 3.7 `SeekableReader::new`

Drains a `Reader` to completion. `wait_for_data` returns monotonically larger
published lengths until closure (I8, I14). The inner `while` walks the chain
exactly as far as chunks are fully covered (`(vec.len()+1)*CHUNK_SIZE <=
position`), following `next` only when data has been published past the
boundary (I10 ⇒ link exists). The final `if offset() != 0` pushes the partially
filled tail. The result satisfies I15.

### 3.8 `SeekableReader::read` / `seek`

`chunk(self.position)` indexes `chunks[position / CHUNK_SIZE]`, valid because
of I5 and because `position < length`. `end = min(length - base, CHUNK_SIZE)`
is `> offset`, and bytes `[offset, end)` are initialized (I7, I15) and kept
alive by the `Arc`s in `self.chunks`, so `from_raw_parts` is sound. `seek`
bounds `new_pos <= length` before storing.

---

## 4. Issues found

### 4.1 Unsound `assume_init_mut` in the writer (`downloader.rs:104`) — real bug

```rust
// SAFETY: `writable_slice` yields exclusively-owned uninitialized tail
// bytes; turning them into `&mut [u8]` is valid for `read` to fill, ...
let dst = unsafe { writer.writable_slice().assume_init_mut() };
let written = reader.read(dst)?;
```

`writable_slice()` returns `&mut [MaybeUninit<u8>]` whose bytes are *not*
initialized (I7). `assume_init_mut()` requires the values to be initialized, so
this is UB even before `read` is called: it forms a `&mut [u8]` over
uninitialized memory, and it passes that reference to `Read::read`, whose
contract requires a buffer of valid `u8`s. The comment's claim that this is
"valid for `read` to fill" is incorrect.

Fix (stable since Rust 1.87) — read directly into the uninitialized buffer with
`Read::read_buf`:

```rust
use std::io::BorrowedBuf;

let mut buf = BorrowedBuf::from(writer.writable_slice());
reader.read_buf(buf.unfilled())?;
let written = buf.len();
if written == 0 { break; }
// SAFETY: `read_buf` initialized exactly the first `written` bytes.
unsafe { writer.commit(written) };
writer.publish();
```

(Alternatively, move the read loop into `Writer` so the module owns the
`unsafe` and `MaybeUninit` handling.)

### 4.2 Misleading comment: `segment.rs:59-60`

```rust
// SAFETY: bytes in `data` are written by the single writer before being
// published via Release on `next` / `written_len`, ...
```

Data visibility is synchronized **only** by the `Release`/`Acquire` pair on
`written_len` (the `Stream` atomic). `next` is a separate `OnceLock` whose
release publishes the *link*, not the bytes. The link still needs to precede
publication (I10), but it is not a publication edge for `data` bytes. Reword to:
"bytes in `data` are written by the single writer and published via the
`Release` store of `written_len` (acquired by readers); the `next` link is
published separately and always precedes `written_len` crossing the chunk
boundary."

### 4.3 Dead conversion / debug-only overflow guard: `Stream::set`

```rust
let len = len.try_into().unwrap_or(u64::MAX);
debug_assert!(len <= Self::LEN_MASK, ...);
```

`usize → u64` is infallible on every supported target, so `unwrap_or(u64::MAX)`
is dead code and the 63-bit overflow is only caught by the `debug_assert!` in
debug builds (silently masked by `& LEN_MASK` in release). This is unreachable
in practice (it would require an 8 EiB stream) and is not a soundness issue,
but the fallback is misleading dead code.

### 4.4 Note: `Reader` is `Clone`, not shared-state `Sync`

`Reader: Clone` copies `position` and clones the two `Arc`s; a clone is an
*independent* reader at the same position. `disk.rs` uses `segment.reader.clone()`
for FFmpeg probing while the original drains. This is sound because each clone
has its own `position`/`cursor`, and concurrent reads of already-published
bytes are data-race-free (I7, I9). It is *not* a way to share a single
read cursor across threads.

---

## 5. Summary

The `segment` module's own concurrency design is sound: single-writer ownership
(I1), a monotonic release/acquire frontier (I8/I9), and link-before-publish
(I10) collectively rule out data races and out-of-bounds reads. The one genuine
memory-safety defect is in the *caller* (`downloader.rs:104`), which violates
the `MaybeUninit` contract when converting the writer's uninitialized buffer
into `&mut [u8]`; that must be replaced with `read_buf`.
