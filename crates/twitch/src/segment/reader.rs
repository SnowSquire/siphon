use bytes::Buf;
use wait_on_address::AtomicWait;

use crate::constants::CHUNK_SIZE;
use crate::segment::{Chunk, SegmentStream};

use std::sync::atomic::Ordering;

use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct SegmentReader {
    pub(super) cursor: Arc<Chunk>, // pins the suffix from here onward; frees the prefix
    pub(super) position: usize,
    pub(super) handle: Arc<SegmentStream>,
}

impl SegmentReader {
    pub(super) fn new(cursor: Arc<Chunk>, handle: Arc<SegmentStream>) -> Self {
        Self {
            cursor,
            handle,
            position: 0,
        }
    }
    #[inline(always)]
    fn offset(&self) -> usize {
        debug_assert!(self.position >= self.cursor.base);
        self.position - self.cursor.base
    }

    // returns false when no more data, returns true when data
    pub fn wait_for_data(&self) -> bool {
        const SPIN: u32 = 100;
        const TIMEOUT_DUR: Duration = Duration::from_secs(1);
        loop {
            for _ in 0..SPIN {
                let closed = self.handle.closed.load(Ordering::Acquire);
                let published_len = self.handle.published_len.load(Ordering::Acquire);
                if published_len > self.position {
                    return true;
                };
                if closed {
                    return false;
                }

                std::hint::spin_loop();
            }

            self.handle
                .published_len
                .wait_timeout(self.position, TIMEOUT_DUR);
        }
    }
}

impl Buf for SegmentReader {
    fn remaining(&self) -> usize {
        self.handle
            .published_len
            .load(Ordering::Acquire)
            .checked_sub(self.position)
            .expect("reader is beyond the published frontier")
    }

    fn chunk(&self) -> &[u8] {
        let published = self.handle.published_len.load(Ordering::Acquire);
        debug_assert!(published >= self.position);

        let (cursor, offset) = match self.offset() {
            CHUNK_SIZE if published == self.position => return &[],
            CHUNK_SIZE => (
                self.cursor
                    .next
                    .get()
                    .expect("published data without a linked next chunk"),
                0,
            ),
            offset => {
                debug_assert!(offset < CHUNK_SIZE);
                (&self.cursor, offset)
            }
        };

        let end = (published - cursor.base).min(CHUNK_SIZE);

        debug_assert!(offset <= end);

        // SAFETY: the Acquire load synchronizes with the writer's Release
        // publication, so [0, end) is initialized. The chunk chain keeps
        // `cursor` alive for the returned borrow.
        unsafe { cursor.data.get().as_ref_unchecked()[offset..end].assume_init_ref() }
    }

    fn advance(&mut self, cnt: usize) {
        assert!(cnt <= self.remaining(), "advance past published frontier");
        self.position += cnt;
        while let Some(next) = self.cursor.next.get() {
            if self.position < next.base {
                break;
            }
            self.cursor = next.clone();
        }
    }
}
