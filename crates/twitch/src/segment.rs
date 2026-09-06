use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

mod reader;
mod writer;

use wait_on_address::AtomicWait;

use crate::constants::CHUNK_SIZE;
pub use crate::segment::reader::Reader;
pub use crate::segment::reader::SeekableReader;
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

        let uninit = unsafe { Arc::get_mut(&mut chunk).unwrap_unchecked() };
        let ptr = uninit.as_mut_ptr();

        unsafe { (&raw mut (*ptr).base).write(base) }
        unsafe { (&raw mut (*ptr).next).write(OnceLock::new()) }
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

// SAFETY: bytes in `data` are written by the single writer before being
// published via Release on `next` / `written_len`, and only read after a
// corresponding Acquire. No mutable aliasing by construction.
unsafe impl Send for Chunk {}
unsafe impl Sync for Chunk {}

// ---------------------------------------------------------------- handle --

#[derive(Debug)]
pub struct Stream {
    inner: AtomicU64, // monotonic published frontier
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
        let len = len.try_into().unwrap_or(u64::MAX);
        debug_assert!(len <= Self::LEN_MASK, "len exceeds 63-bit maximum");
        self.inner.store(
            (len & Self::LEN_MASK) | (u64::from(closed) * Self::CLOSED_BIT),
            Ordering::Release,
        );
    }

    /// If the value is `value`, wait until woken up.
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
