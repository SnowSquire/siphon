use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::{self};
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

pub struct SeekableReader {
    chunks: Vec<Arc<Chunk>>,
    length: usize,
    position: usize,
}

impl SeekableReader {
    fn chunk(&self, pos: usize) -> &Arc<Chunk> {
        &self.chunks[pos / CHUNK_SIZE]
    }

    pub fn new(mut reader: Reader) -> Self {
        let mut vec = Vec::new();

        // Drain the stream: each observed length advances `position`, and every
        // fully covered chunk is pushed in order, so `chunks[i].base` is
        // `i * CHUNK_SIZE`.
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
        // Keep the partially filled tail; at an exact chunk boundary there is
        // nothing left to keep.
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

impl Read for SeekableReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.length {
            return Ok(0);
        }

        let chunk = self.chunk(self.position);
        let offset = self.position - chunk.base;
        // Bytes below `length` are initialized (the stream is closed), so clamp
        // the slice end to them.
        let end = (self.length - chunk.base).min(CHUNK_SIZE);

        // SAFETY: the chunk is kept alive by `self.chunks`, and
        // `offset < end <= CHUNK_SIZE`, so `ptr` is in bounds.
        let ptr = unsafe { (chunk.data.get() as *const u8).add(offset) };

        // SAFETY: `[offset, end)` lies below `length`, hence initialized and
        // immutable; `from_raw_parts` with length `end - offset` is valid.
        let data: &[u8] = unsafe { std::slice::from_raw_parts(ptr, end - offset) };

        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        self.position += n;
        Ok(n)
    }
}

impl Seek for SeekableReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let current = self.position as u64; // Infallible: usize always fits in u64
        let end = self.length as u64; // Infallible: usize always fits in u64

        let new_pos = match pos {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(n) => current.checked_add_signed(n),
            SeekFrom::End(n) => end.checked_add_signed(n),
        };

        // Handles underflow (< 0) or overflow (> u64::MAX)
        let new_pos = new_pos.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid seek to a negative or overflowing position",
            )
        })?;

        // Guard bounds (if your reader doesn't support seeking past EOF)
        if new_pos > end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek position out of bounds",
            ));
        }

        // Safely downcast u64 back to usize (only relevant on 32-bit/16-bit targets)
        self.position = usize::try_from(new_pos).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek position exceeds target architecture address space",
            )
        })?;

        Ok(new_pos)
    }
}
