use bytes::Buf;
use wait_on_address::AtomicWait;

use crate::constants::CHUNK_SIZE;
use crate::stream::{Chunk, Stream};

use std::mem::MaybeUninit;
use std::sync::atomic::Ordering;

use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct SegmentReader {
    pub cursor: Arc<Chunk>, // pins the suffix from here onward; frees the prefix
    pub offset: usize,      // read offset inside cursor
    pub handle: Arc<Stream>,
}

impl SegmentReader {
    #[inline(always)]
    fn position(&self) -> usize {
        self.cursor.base + self.offset
    }

    pub fn closed(&self) -> bool {
        self.handle.is_closed()
    }

    pub fn wait_for_data(&self) -> bool {
        const SPIN: u32 = 100;
        let pos = self.position();
        loop {
            for _ in 0..SPIN {
                if self.handle.frontier() > pos {
                    return true;
                }
                if self.handle.is_closed() {
                    return false;
                }
                std::hint::spin_loop();
            }

            self.handle.published_len.wait(pos);
        }
    }
}

impl Buf for SegmentReader {
    fn remaining(&self) -> usize {
        self.handle.frontier() - self.position()
    }

    fn chunk(&self) -> &[u8] {
        let (cursor, offset) = match (self.offset, self.cursor.next.get()) {
            (CHUNK_SIZE, Some(next)) => (next, 0),
            _ => (&self.cursor, self.offset),
        };

        let end = (self.handle.frontier() - cursor.base).min(CHUNK_SIZE);
        // SAFETY: the Acquire on written_len synchronizes with the
        // writer's Release publish, so [0, published) is initialized.
        // The Arc chain from self.cursor keeps `cursor` alive for the
        // borrow's lifetime.
        debug_assert!(offset < end || self.remaining() == 0);
        unsafe { cursor.data.get().as_ref_unchecked()[offset..end].assume_init_ref() }
    }

    fn advance(&mut self, cnt: usize) {
        assert!(cnt <= self.remaining(), "advance past published frontier");
        self.offset += cnt;
        while self.offset >= CHUNK_SIZE {
            self.offset -= CHUNK_SIZE;
            match self.cursor.next.get() {
                Some(next) => self.cursor = next.clone(),
                None => break,
            }
        }
    }
}
