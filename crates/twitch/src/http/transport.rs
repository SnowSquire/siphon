use std::sync::Arc;

use compio::io::AsyncRead;
use compio::io::AsyncWriteExt;
use compio::net::TcpStream;
use rustls::ClientConfig;

use crate::constants::CONNECT_TIMEOUT;
use crate::constants::IO_TIMEOUT;
use crate::constants::READ_BUFFER;
use crate::http::error::Error;
use crate::http::pool::PoolKey;
use crate::http::timeout;
use crate::http::tls::TlsConn;

/// Bytes already received and not yet handed to the caller.
#[derive(Default)]
pub struct Carry {
    pub(crate) data: Vec<u8>,
    pub(crate) off: usize,
}

impl Carry {
    /// Copy the next `dst.len()` bytes out, if any are available.
    pub fn serve(&mut self, dst: &mut [u8]) -> usize {
        let available = self.data.len() - self.off;
        let take = available.min(dst.len());
        if take == 0 {
            return 0;
        }
        dst[..take].copy_from_slice(&self.data[self.off..self.off + take]);
        self.off += take;
        if self.off == self.data.len() {
            self.data.clear();
            self.off = 0;
        }
        take
    }

    pub const fn is_empty(&self) -> bool {
        self.off == self.data.len()
    }
}

pub struct TcpConn {
    stream: TcpStream,
    carry: Carry,
}

impl TcpConn {
    const fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            carry: Carry {
                data: Vec::new(),
                off: 0,
            },
        }
    }

    async fn read_into(&mut self, dst: &mut [u8]) -> Result<usize, Error> {
        let served = self.carry.serve(dst);
        if served > 0 || dst.is_empty() {
            return Ok(served);
        }

        let mut buf = std::mem::take(&mut self.carry.data);
        buf.clear();
        // A completion read learns its capacity from the buffer, and a fresh `Vec`
        // has none: reserve before handing it over.
        buf.reserve(READ_BUFFER);
        let result = timeout(IO_TIMEOUT, self.stream.read(buf)).await?;
        self.carry.data = result.1;
        let read = result.0?;
        self.carry.off = 0;
        if read == 0 {
            return Ok(0);
        }

        let served = self.carry.serve(dst);
        debug_assert!(served > 0, "read returned data but nothing was served");
        Ok(served)
    }

    async fn write_all(&mut self, data: &[u8]) -> Result<(), Error> {
        let buf = data.to_vec();
        timeout(IO_TIMEOUT, self.stream.write_all(buf)).await?.0?;
        Ok(())
    }
}

/// A pooled connection: plaintext TCP, or TLS driven by `rustls::unbuffered`.
pub enum Conn {
    Tcp(TcpConn),
    Tls(Box<TlsConn>),
}

impl Conn {
    /// Connect to `key`, upgrading to TLS when requested.
    ///
    /// # Errors
    ///
    /// Fails on DNS, TCP, or TLS handshake failure, or when the connect timeout
    /// expires.
    pub async fn connect(key: &PoolKey, config: &Arc<ClientConfig>) -> Result<Self, Error> {
        let addr = (key.host.as_str(), key.port);
        let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await??;
        stream.set_nodelay(true)?;

        if key.tls {
            Ok(Self::Tls(Box::new(
                TlsConn::connect(stream, Arc::clone(config), &key.host).await?,
            )))
        } else {
            Ok(Self::Tcp(TcpConn::new(stream)))
        }
    }

    /// Read up to `dst.len()` bytes. `Ok(0)` is EOF.
    pub async fn read_into(&mut self, dst: &mut [u8]) -> Result<usize, Error> {
        match self {
            Self::Tcp(conn) => conn.read_into(dst).await,
            Self::Tls(conn) => conn.read_into(dst).await,
        }
    }

    pub async fn write_all(&mut self, data: &[u8]) -> Result<(), Error> {
        match self {
            Self::Tcp(conn) => conn.write_all(data).await,
            Self::Tls(conn) => conn.write_all(data).await,
        }
    }

    pub const fn is_tls(&self) -> bool {
        matches!(self, Self::Tls(_))
    }
}
