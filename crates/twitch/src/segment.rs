use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

// TODO(compio): remove the `dead_code` allow once `writer`/`reader` are rebuilt on
// top of the arena (phase 3 of the port).
#[allow(dead_code)]
mod arena;
mod reader;
mod writer;

use wait_on_address::AtomicWait;

use crate::constants::CHUNK_SIZE;
pub use crate::segment::reader::Reader;
pub use crate::segment::writer::Writer;

#[derive(Debug)]
#[repr(align(64))]
pub struct Chunk {
    pub(super) base: usize,
    pub(super) next: OnceLock<Arc<Self>>,
    pub(super) data: UnsafeCell<[MaybeUninit<u8>; CHUNK_SIZE]>,
}

impl Chunk {
    pub fn new(base: usize) -> Arc<Self> {
        let mut chunk = Arc::<Self>::new_uninit();

        // SAFETY: `chunk` is fresh from `new_uninit`: strong count 1, weak
        // count 0, so `get_mut` returns `Some`.
        let uninit = unsafe { Arc::get_mut(&mut chunk).unwrap_unchecked() };
        let ptr = uninit.as_mut_ptr();

        // SAFETY: `ptr` is the exclusively-owned allocation; `addr_of_mut!`
        // forms a field pointer without creating a reference.
        let base_ptr = unsafe { std::ptr::addr_of_mut!((*ptr).base) };
        // SAFETY: writes `base` exactly once, before `assume_init`, with no
        // other aliases in existence.
        unsafe { base_ptr.write(base) }
        // SAFETY: same allocation and aliasing state as `base_ptr`.
        let next_ptr = unsafe { std::ptr::addr_of_mut!((*ptr).next) };
        // SAFETY: writes `next` exactly once, before `assume_init`.
        unsafe { next_ptr.write(OnceLock::new()) }
        // SAFETY: `base` and `next` are initialized. `data` is
        // `[MaybeUninit<u8>; CHUNK_SIZE]`, valid for any bit pattern, so it
        // needs no initialization before `assume_init`.
        unsafe { chunk.assume_init() }
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        let mut next = self.next.take();
        while let Some(chunk) = next.as_mut().and_then(Arc::get_mut) {
            next = chunk.next.take();
        }
    }
}

// SAFETY: only the single `Writer` writes `data`, and only in its
// exclusively-owned tail. Readers only read bytes below the published
// frontier, ordered by the `Release` store / `Acquire` load of `written_len`.
// `base` is immutable after init, so no mutable aliasing is possible.
unsafe impl Send for Chunk {}
// SAFETY: as for `Send`; shared access is read-only on published bytes, and
// the `next` link is set once before publication crosses the chunk boundary.
unsafe impl Sync for Chunk {}

// ---------------------------------------------------------------- handle --

#[derive(Debug)]
pub struct Stream {
    /// Packed frontier: low 63 bits are the published length, the top bit is
    /// `closed`. Monotonic; the writer `Release`-stores, readers `Acquire`-load.
    inner: AtomicU64,
}

impl Stream {
    const CLOSED_BIT: u64 = 1 << 63;
    const LEN_MASK: u64 = !Self::CLOSED_BIT;

    fn load(&self) -> (usize, bool) {
        let val = self.inner.load(Ordering::Acquire);
        (
            (val & Self::LEN_MASK) as usize,
            (val & Self::CLOSED_BIT) != 0,
        )
    }

    fn set(&self, len: usize, closed: bool) {
        let len = len as u64;
        debug_assert!(len <= Self::LEN_MASK, "len exceeds 63-bit maximum");
        self.inner.store(
            (len & Self::LEN_MASK) | (u64::from(closed) * Self::CLOSED_BIT),
            Ordering::Release,
        );
    }

    /// Blocks while the packed frontier still equals `value`; returns once a
    /// publish or close changes it.
    ///
    /// This function might also return spuriously,
    /// without a corresponding wake operation.
    fn wait(&self, value: usize) {
        let value = value.try_into().unwrap_or(u64::MAX);
        assert!(
            value <= Self::LEN_MASK,
            "wait value cannot exceed 63-bit maximum"
        );
        self.inner.wait(value);
    }

    pub fn new_stream() -> (Writer, Reader) {
        let handle = Arc::new(Self {
            inner: AtomicU64::new(0),
        });

        let head = Chunk::new(0);

        let writer = Writer::new(head.clone(), handle.clone());
        let reader = Reader::new(head, handle);

        (writer, reader)
    }
}
