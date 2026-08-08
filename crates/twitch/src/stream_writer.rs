use std::sync::{Arc, atomic::Ordering};

use tracing::debug;
use wait_on_address::AtomicWait;

use crate::{
    constants::CHUNK_SIZE,
    stream::{Chunk, Stream},
};

pub struct SegmentWriter {
    pub tail: Arc<Chunk>,
    pub tail_offset: usize, // bytes written into tail
    pub handle: Arc<Stream>,
    pub closed: bool,
}

impl SegmentWriter {
    /// Writable remainder of the tail chunk, linking a fresh chunk if the
    /// tail is full. Returns at most CHUNK_SIZE bytes per call.
    pub fn writable_slice(&mut self) -> &mut [u8] {
        if self.tail_offset == CHUNK_SIZE {
            self.link_new_chunk();
        }
        // SAFETY: single writer with exclusive access to unpublished tail
        // bytes; readers never read at/above `written_len`.
        let base = self.tail.data.get().cast::<u8>();
        unsafe {
            std::slice::from_raw_parts_mut(
                base.add(self.tail_offset),
                CHUNK_SIZE - self.tail_offset,
            )
        }
    }

    pub fn commit(&mut self, n: usize) {
        assert!(n <= CHUNK_SIZE - self.tail_offset, "commit past slice");
        self.tail_offset += n;

        // Just use 'n' directly since we publish every time
        self.handle.written_len.fetch_add(n, Ordering::Release);
        self.handle.written_len.notify_all();
    }

    fn link_new_chunk(&mut self) {
        debug_assert!(
            self.handle
                .written_len
                .load(Ordering::Acquire)
                .is_multiple_of(CHUNK_SIZE),
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
        self.tail_offset = 0;
    }

    /// Signal EOF and wake everyone. Idempotent.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.handle.closed.store(true, Ordering::Release);
        // Unconditional: sleepers may be waiting for len that never comes.
        self.handle.written_len.notify_all();
    }
}

impl Drop for SegmentWriter {
    fn drop(&mut self) {
        self.close(); // never leave readers parked forever
    }
}
