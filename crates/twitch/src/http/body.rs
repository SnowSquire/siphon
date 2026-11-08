use std::cell::RefCell;
use std::rc::Rc;

use crate::constants::MAX_CHUNK_LINE as MAX_LINE;
use crate::http::error::Error;
use crate::http::pool::Pool;
use crate::http::pool::PoolKey;
use crate::http::transport::Conn;

/// How the response body is delimited.
#[derive(Clone, Copy, Debug)]
pub enum Mode {
    Length(u64),
    Chunked,
    Eof,
}

#[derive(Clone, Copy, Debug)]
enum State {
    Length(u64),
    Chunked(Chunk),
    Eof,
    Done,
}

#[derive(Clone, Copy, Debug)]
enum Chunk {
    Size,
    Data(u64),
    CrLf,
    Trailer,
}

/// Where a finished connection goes back to.
pub struct Lease {
    pub pool: Rc<RefCell<Pool>>,
    pub key: PoolKey,
}

/// Streaming response body.
///
/// Content-Length bodies read straight into the caller's destination;
/// `Transfer-Encoding: chunked` reads the size line into a tiny buffer and then the
/// payload straight into the destination, so payload bytes are copied at most once
/// (only when a previous read over-delivered into the carry).
pub struct Body {
    conn: Option<Conn>,
    state: State,
    carry: Vec<u8>,
    off: usize,
    lease: Option<Lease>,
    reusable: bool,
}

impl Body {
    pub const fn new(
        conn: Conn,
        mode: Mode,
        carry: Vec<u8>,
        lease: Option<Lease>,
        reusable: bool,
    ) -> Self {
        let state = match mode {
            Mode::Length(0) => State::Done,
            Mode::Length(len) => State::Length(len),
            Mode::Chunked => State::Chunked(Chunk::Size),
            Mode::Eof => State::Eof,
        };
        Self {
            conn: Some(conn),
            state,
            carry,
            off: 0,
            lease,
            reusable,
        }
    }

    pub const fn is_done(&self) -> bool {
        matches!(self.state, State::Done)
    }

    /// Read up to `out.len()` body bytes. `Ok(0)` means the body ended.
    pub async fn read(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        if out.is_empty() {
            return Ok(0);
        }

        let mut state = self.state;
        loop {
            match state {
                State::Done => {
                    self.state = State::Done;
                    return Ok(0);
                }
                State::Eof => {
                    let read = self.fill(out).await?;
                    self.state = state;
                    if read == 0 {
                        self.state = State::Done;
                    }
                    return Ok(read);
                }
                State::Length(remaining) => {
                    if remaining == 0 {
                        state = State::Done;
                        continue;
                    }
                    let want = out
                        .len()
                        .min(usize::try_from(remaining).unwrap_or(usize::MAX));
                    let read = self.fill(&mut out[..want]).await?;
                    if read == 0 {
                        return Err(Error::Closed);
                    }
                    self.state = State::Length(remaining - read as u64);
                    return Ok(read);
                }
                State::Chunked(chunk) => match chunk {
                    Chunk::Size => {
                        let line = self.read_line().await?;
                        let size = parse_size(&line)?;
                        state = if size == 0 {
                            State::Chunked(Chunk::Trailer)
                        } else {
                            State::Chunked(Chunk::Data(size))
                        };
                    }
                    Chunk::Data(remaining) => {
                        if remaining == 0 {
                            state = State::Chunked(Chunk::CrLf);
                            continue;
                        }
                        let want = out
                            .len()
                            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
                        let read = self.fill(&mut out[..want]).await?;
                        if read == 0 {
                            return Err(Error::Closed);
                        }
                        self.state = State::Chunked(Chunk::Data(remaining - read as u64));
                        return Ok(read);
                    }
                    Chunk::CrLf => {
                        let mut crlf = [0u8; 2];
                        self.read_exact(&mut crlf).await?;
                        if &crlf != b"\r\n" {
                            return Err(Error::Protocol("bad chunk terminator"));
                        }
                        state = State::Chunked(Chunk::Size);
                    }
                    Chunk::Trailer => {
                        loop {
                            if self.read_line().await?.is_empty() {
                                break;
                            }
                        }
                        state = State::Done;
                    }
                },
            }
        }
    }

    /// Read the whole body, refusing to exceed `limit` bytes.
    pub async fn read_to_end(&mut self, limit: usize) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        let mut scratch = vec![0u8; 32 * 1024];
        loop {
            let read = self.read(&mut scratch).await?;
            if read == 0 {
                break;
            }
            if out.len() + read > limit {
                return Err(Error::Protocol("body exceeds limit"));
            }
            out.extend_from_slice(&scratch[..read]);
        }
        Ok(out)
    }

    /// Copy from the carry if it has bytes, otherwise read straight from the socket.
    async fn fill(&mut self, dst: &mut [u8]) -> Result<usize, Error> {
        let available = self.carry.len() - self.off;
        if available > 0 {
            let take = available.min(dst.len());
            dst[..take].copy_from_slice(&self.carry[self.off..self.off + take]);
            self.off += take;
            if self.off == self.carry.len() {
                self.carry.clear();
                self.off = 0;
            }
            return Ok(take);
        }

        let conn = self.conn.as_mut().ok_or(Error::Closed)?;
        conn.read_into(dst).await
    }

    async fn read_exact(&mut self, dst: &mut [u8]) -> Result<(), Error> {
        let mut filled = 0;
        while filled < dst.len() {
            let read = self.fill(&mut dst[filled..]).await?;
            if read == 0 {
                return Err(Error::Closed);
            }
            filled += read;
        }
        Ok(())
    }

    async fn read_line(&mut self) -> Result<Vec<u8>, Error> {
        loop {
            if let Some(position) = self.carry[self.off..]
                .iter()
                .position(|&byte| byte == b'\n')
            {
                let end = self.off + position;
                let mut line = self.carry[self.off..end].to_vec();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.off = end + 1;
                if self.off == self.carry.len() {
                    self.carry.clear();
                    self.off = 0;
                }
                return Ok(line);
            }

            if self.carry.len() - self.off > MAX_LINE {
                return Err(Error::Protocol("chunk line too long"));
            }
            self.refill().await?;
        }
    }

    async fn refill(&mut self) -> Result<(), Error> {
        if self.off == self.carry.len() {
            self.carry.clear();
            self.off = 0;
        }
        let mut scratch = [0u8; 4096];
        let conn = self.conn.as_mut().ok_or(Error::Closed)?;
        let read = conn.read_into(&mut scratch).await?;
        if read == 0 {
            return Err(Error::Closed);
        }
        self.carry.extend_from_slice(&scratch[..read]);
        Ok(())
    }
}

impl Drop for Body {
    fn drop(&mut self) {
        if !self.reusable || !self.is_done() {
            return;
        }
        let (Some(conn), Some(lease)) = (self.conn.take(), self.lease.take()) else {
            return;
        };
        lease.pool.borrow_mut().put(lease.key, conn);
    }
}

fn parse_size(line: &[u8]) -> Result<u64, Error> {
    let line = std::str::from_utf8(line).map_err(|_| Error::Protocol("chunk size is not utf8"))?;
    let line = line.split(';').next().unwrap_or_default().trim();
    u64::from_str_radix(line, 16).map_err(|_| Error::Protocol("invalid chunk size"))
}
