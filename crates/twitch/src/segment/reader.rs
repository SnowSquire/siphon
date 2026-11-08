use std::sync::Arc;

use crate::constants::CHUNK_SIZE;
use crate::segment::Chunk;
use crate::segment::Stream;

#[derive(Clone, Debug)]
pub struct Reader {
    cursor: Arc<Chunk>,
    position: usize,
    handle: Arc<Stream>,
}

impl Reader {
    pub(super) const fn new(cursor: Arc<Chunk>, handle: Arc<Stream>) -> Self {
        Self {
            cursor,
            handle,
            position: 0,
        }
    }

    #[inline]
    fn offset(&self) -> usize {
        debug_assert!(self.position >= self.cursor.base);
        self.position - self.cursor.base
    }

    fn wait_for_data(&self) -> Option<usize> {
        const SPIN: u32 = 100;
        loop {
            for _ in 0..SPIN {
                let (published_len, closed) = self.handle.load();
                if published_len > self.position {
                    return Some(published_len);
                }
                if closed {
                    return None;
                }
                std::hint::spin_loop();
            }

            self.handle.wait(self.position);
        }
    }

    pub fn chunk(&mut self) -> Option<&[u8]> {
        // An exhausted cursor means `published` lies past its end, which the
        // writer only publishes after linking the next chunk, so the link
        // below always exists. Otherwise `wait_for_data` already blocked until
        // new data arrived or the stream closed.
        let (offset, published) = match (self.offset(), self.wait_for_data()?) {
            (CHUNK_SIZE, published) => {
                self.cursor = self
                    .cursor
                    .next
                    .get()
                    .expect("published data without a linked next chunk")
                    .clone();
                (0, published)
            }
            (offset, published) => {
                debug_assert!(offset < CHUNK_SIZE);
                (offset, published)
            }
        };

        let end = (published - self.cursor.base).min(CHUNK_SIZE);
        debug_assert!(offset < end);

        // SAFETY: the `Acquire` load in `wait_for_data` syncs with the writer's
        // `Release` publish, so `[0, end)` is initialized; `self.cursor` keeps
        // the chunk alive.
        let slice = unsafe { self.cursor.data.get().as_ref_unchecked() };
        // SAFETY: `offset < end`, so `[offset, end)` is inside the initialized
        // region above.
        let slice = unsafe { slice[offset..end].assume_init_ref() };

        // Implicit advance: consume the entire returned region.
        self.position = self.cursor.base + end;

        Some(slice)
    }
}
