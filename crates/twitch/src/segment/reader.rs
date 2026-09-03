use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::{self};
use std::sync::Arc;

use crate::constants::CHUNK_SIZE;
use crate::segment::Chunk;
use crate::segment::SegmentStream;

#[derive(Clone, Debug)]
pub struct SegmentReader {
    cursor: Arc<Chunk>,
    position: usize,
    handle: Arc<SegmentStream>,
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

    fn wait_for_data(&self) -> Option<usize> {
        const SPIN: u32 = 100;
        loop {
            for _ in 0..SPIN {
                let (published_len, closed) = self.handle.load();
                if published_len > self.position {
                    return Some(published_len);
                };
                if closed {
                    return None;
                }
                std::hint::spin_loop();
            }

            self.handle.wait(self.position);
        }
    }

    pub fn chunk(&mut self) -> Option<&[u8]> {
        // If the current chunk is exhausted, follow the chain. If the
        // writer hasn't linked the next chunk yet, block until it
        // publishes more data or closes the stream.
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

        // SAFETY: the Acquire load synchronizes with the writer's Release
        // publication, so [0, end) is initialized. The chunk chain keeps
        // `cursor` alive for the returned borrow.
        let slice =
            unsafe { self.cursor.data.get().as_ref_unchecked()[offset..end].assume_init_ref() };

        // Implicit advance: consume the entire returned region.
        self.position = self.cursor.base + end;

        return Some(slice);
    }
}

pub struct SeekableSegmentReader {
    chunks: Vec<Arc<Chunk>>,
    length: usize,
    position: usize,
}

impl SeekableSegmentReader {
    fn chunk(&self, pos: usize) -> &Arc<Chunk> {
        &self.chunks[pos / CHUNK_SIZE]
    }

    pub fn new(mut reader: SegmentReader) -> Self {
        let mut vec = Vec::new();

        while let Some(size) = reader.wait_for_data() {
            reader.position = size;
            while (vec.len() + 1) * CHUNK_SIZE <= reader.position {
                let next = reader
                    .cursor
                    .next
                    .get()
                    .expect("published data without a linked next chunk")
                    .clone();
                vec.push(std::mem::replace(&mut reader.cursor, next));
            }
        }
        if reader.offset() != 0 {
            vec.push(reader.cursor);
        }

        Self {
            chunks: vec,
            length: reader.position,
            position: 0,
        }
    }
}

impl Read for SeekableSegmentReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.length {
            return Ok(0);
        }

        let chunk = self.chunk(self.position);
        let offset = self.position - chunk.base;
        // Data is initialized up to `end_pos`, so clamp the slice end.
        let end = (self.length - chunk.base).min(CHUNK_SIZE);

        // SAFETY: bytes in `chunk.base..chunk.base + end` are initialized
        // (guaranteed by the caller's invariant that data is initialized up
        // to `end_pos`), and `offset < end` since position < end_pos.
        let data: &[u8] = unsafe {
            let ptr = chunk.data.get() as *const u8;
            std::slice::from_raw_parts(ptr.add(offset), end - offset)
        };

        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        self.position += n;
        Ok(n)
    }
}

impl Seek for SeekableSegmentReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let end = self.length as i64;
        let new = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => end + n,
            SeekFrom::Current(n) => self.position as i64 + n,
        };

        if new < 0 || new > end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek position out of bounds",
            ));
        }

        self.position = new as usize;
        Ok(self.position as u64)
    }
}
