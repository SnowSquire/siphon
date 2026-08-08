//! Lock-free SWMR streaming buffer: singly-linked rope of fixed chunks.
//!
//! Invariants that make the unsafe sound:
//!   1. Single writer, many readers.
//!   2. Writer writes bytes -> links `next` -> publishes via Release
//!      `fetch_add` on `written_len`. Readers Acquire-load `written_len`
//!      and only touch bytes below it.
//!   3. Therefore `written_len >= chunk.base + CHUNK_SIZE` PROVES
//!      `chunk.next` is set. Readers treat `next == None` strictly as
//!      "caught up to frontier", never as an error.
//!   4. Chunks are freed implicitly by Arc refcounts once the writer and
//!      ALL readers have advanced past them. Drop is iterative.

use std::cell::UnsafeCell;
use std::marker::PhantomPinned;
use std::mem::MaybeUninit;

use std::pin::Pin;
use std::ptr::{self, addr_of_mut};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use tracing::debug;

use crate::constants::CHUNK_SIZE;

use crate::stream_reader::SegmentReader;
use crate::stream_writer::SegmentWriter;

#[derive(Debug)]
#[repr(align(64))] // should probably be pinned
pub struct Chunk {
    pub base: usize, // global offset of data[0], immutable
    pub next: OnceLock<Arc<Chunk>>,
    pub data: UnsafeCell<[MaybeUninit<u8>; CHUNK_SIZE]>,
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

/// Shared coordination state. Deliberately holds NO reference to any chunk:
/// reclamation is driven purely by refcounts, so an anchor here would pin
/// the whole chain forever.
#[derive(Debug)]
pub struct Stream {
    // used for HLS segments
    pub written_len: AtomicUsize, // monotonic published frontier
    pub closed: AtomicBool,       // EOF: writer finished
}

impl Stream {
    /// Creates the stream: a writer and a reader prototype. Clone the
    /// prototype for each additional reader BEFORE writing starts
    /// (readers cannot join midstream).
    pub fn new() -> (SegmentWriter, SegmentReader) {
        let handle = Arc::new(Stream {
            written_len: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
        });

        let head = Chunk::new(0);

        let writer = SegmentWriter {
            tail: head.clone(),
            tail_offset: 0,

            handle: handle.clone(),
            closed: false,
        };
        let reader = SegmentReader {
            cursor: head,
            offset: 0,
            handle: handle,
        };

        (writer, reader)
    }
    #[inline(always)]
    pub fn frontier(&self) -> usize {
        self.written_len.load(Ordering::Acquire)
    }
    #[inline(always)]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}
