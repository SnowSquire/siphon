use std::mem::MaybeUninit;
use std::sync::Arc;

use wait_on_address::AtomicWait;

use crate::constants::CHUNK_SIZE;
use crate::segment::Chunk;
use crate::segment::Stream;

pub struct Writer {
    tail: Arc<Chunk>,
    handle: Arc<Stream>,
    written_len: usize,
}

impl Drop for Writer {
    fn drop(&mut self) {
        {
            self.handle.set(self.written_len, true);
            self.handle.inner.notify_all();
        };
    }
}

impl Writer {
    fn tail_offset(&self) -> usize {
        self.written_len - self.tail.base
    }

    pub(super) const fn new(tail: Arc<Chunk>, handle: Arc<Stream>) -> Self {
        Self {
            tail,
            handle,
            written_len: 0,
        }
    }

    /// Writable remainder of the tail chunk, linking a fresh chunk if the
    /// tail is full. Returns at most `CHUNK_SIZE` bytes per call.
    pub fn writable_slice(&mut self) -> &mut [MaybeUninit<u8>] {
        if self.tail_offset() == CHUNK_SIZE {
            self.link_new_chunk();
        }
        // SAFETY: single writer with exclusive unpublished tail access; readers stay below `written_len`.
        unsafe { self.tail.data.get().as_mut_unchecked()[self.tail_offset()..].as_mut() }
    }

    /// Advances `written_len` by `n` bytes.
    ///
    /// # Safety
    ///
    /// Caller must ensure the next `n` bytes from the last `writable_slice`
    /// were initialized and `n` does not exceed that slice's length.
    pub unsafe fn commit(&mut self, n: usize) {
        assert!(
            self.written_len + n <= self.tail.base + CHUNK_SIZE,
            "commit past slice"
        );

        self.written_len += n;
    }

    pub fn publish(&self) {
        self.handle.set(self.written_len, false);
        self.handle.inner.notify_all();
    }

    fn link_new_chunk(&mut self) {
        debug_assert_eq!(
            self.written_len,
            self.tail.base + CHUNK_SIZE,
            "you forgot to commit at some point"
        );

        let next = Chunk::new(self.tail.base + CHUNK_SIZE);
        // Link BEFORE publish (invariant 3): any reader that observes
        // written_len past this chunk is guaranteed to find this link.
        self.tail
            .next
            .set(next.clone())
            .expect("single writer links exactly once");

        self.tail = next;
    }
}
