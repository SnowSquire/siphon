use std::{
    mem::MaybeUninit,
    sync::{Arc, atomic::Ordering},
};

use wait_on_address::AtomicWait;

use crate::{
    constants::CHUNK_SIZE,
    segment::{Chunk, SegmentStream},
};

pub struct SegmentWriter {
    tail: Arc<Chunk>,
    handle: Arc<SegmentStream>,
    closed: bool,
    written_len: usize,
}

impl SegmentWriter {
    fn tail_offset(&self) -> usize {
        self.written_len - self.tail.base
    }
    pub(super) fn new(tail: Arc<Chunk>, handle: Arc<SegmentStream>) -> Self {
        Self {
            tail,
            // tail_offset: 0,
            handle,
            closed: false,
            written_len: 0,
        }
    }
    /// Writable remainder of the tail chunk, linking a fresh chunk if the
    /// tail is full. Returns at most CHUNK_SIZE bytes per call.
    pub fn writable_slice(&mut self) -> &mut [MaybeUninit<u8>] {
        if self.tail_offset() == CHUNK_SIZE {
            self.link_new_chunk();
        }
        // SAFETY: single writer with exclusive access to unpublished tail
        // bytes; readers never read at/above `written_len`.
        unsafe { self.tail.data.get().as_mut_unchecked()[self.tail_offset()..].as_mut() }
    }

    pub unsafe fn commit(&mut self, n: usize) {
        assert!(
            self.written_len + n <= self.tail.base + CHUNK_SIZE,
            "commit past slice"
        );

        self.written_len += n;
    }

    pub fn publish(&mut self) {
        self.handle
            .published_len
            .store(self.written_len, Ordering::Release);

        self.handle.published_len.notify_all();
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

    /// Signal EOF and wake everyone. Idempotent.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.handle.closed.store(true, Ordering::Release);
        // Unconditional: sleepers may be waiting for len that never comes.
        self.handle.published_len.notify_all();
    }
}

impl Drop for SegmentWriter {
    fn drop(&mut self) {
        self.close(); // never leave readers parked forever
    }
}
