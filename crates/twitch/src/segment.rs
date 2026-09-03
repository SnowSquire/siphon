use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, OnceLock};

mod reader;
mod writer;

use crate::constants::CHUNK_SIZE;
pub use crate::segment::reader::SegmentReader;
pub use crate::segment::writer::SegmentWriter;

#[derive(Debug)]
#[repr(align(64))]
pub struct Chunk {
    pub(super) base: usize,
    pub(super) next: OnceLock<Arc<Chunk>>,
    pub(super) data: UnsafeCell<[MaybeUninit<u8>; CHUNK_SIZE]>,
}

impl Chunk {
    pub fn new(base: usize) -> Arc<Self> {
        let mut chunk = Arc::<Self>::new_uninit();

        let uninit = unsafe { Arc::get_mut(&mut chunk).unwrap_unchecked() };
        let ptr = uninit.as_mut_ptr();
        unsafe {
            (&raw mut (*ptr).base).write(base);
            (&raw mut (*ptr).next).write(OnceLock::new());
            chunk.assume_init()
        }
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
pub struct SegmentStream {
    pub published_len: AtomicUsize, // monotonic published frontier
    pub closed: AtomicBool,         // EOF: writer finished
}

impl SegmentStream {
    pub fn new() -> (SegmentWriter, SegmentReader) {
        let handle = Arc::new(SegmentStream {
            published_len: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
        });

        let head = Chunk::new(0);

        let writer = SegmentWriter::new(head.clone(), handle.clone());
        let reader = SegmentReader::new(head, handle);

        (writer, reader)
    }
}
