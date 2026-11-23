use std::sync::Arc;
use std::time::Duration;

use compio::io::AsyncRead;
use compio::io::AsyncWriteExt;
use compio::net::TcpStream;
use rustls::ClientConfig;
use rustls::client::UnbufferedClientConnection;
use rustls::pki_types::ServerName;
use rustls::unbuffered::ConnectionState;
use rustls::unbuffered::EncodeError;
use rustls::unbuffered::EncryptError;

use crate::constants::IO_TIMEOUT;
use crate::constants::READ_BUFFER;
use crate::constants::TLS_OUT_BUFFER as OUT_BUFFER;
use crate::constants::TLS_WRITE_CHUNK as WRITE_CHUNK;
use crate::http::error::Error;
use crate::http::timeout;
use crate::http::transport::Carry;

/// TLS over completion IO, driven through `rustls::unbuffered`.
///
/// rustls hands decrypted payloads back as `&[u8]` owned by its own buffer, so one
/// copy out is unavoidable; everything else — ciphertext buffering, record framing,
/// handshake transitions — happens in caller-owned buffers and read sizes are ours.
pub struct TlsConn {
    conn: UnbufferedClientConnection,
    raw: TcpStream,
    /// Ciphertext not yet consumed by `process_tls_records`.
    ct: Vec<u8>,
    /// Scratch for ciphertext reads.
    inbuf: Vec<u8>,
    /// Scratch for encoded handshake records and encrypted application data.
    out: Vec<u8>,
    /// Decrypted bytes waiting to be served.
    plain: Carry,
    closed: bool,
    /// Per-read deadline for ciphertext reads. `None` waits indefinitely, for a
    /// WebSocket whose peer may idle for a full keepalive interval; the TLS
    /// handshake always runs with `IO_TIMEOUT`.
    read_timeout: Option<Duration>,
    /// A TLS error was returned once. rustls has already queued its fatal
    /// alert and a second `process_tls_records` on the same connection trips
    /// its `sent_fatal_alert` assertion, so a dead connection answers every
    /// later call with `Closed` instead of touching the state machine again.
    failed: bool,
}

/// What one state-machine step produced.
enum Pump {
    Plaintext,
    Progress,
    Eof,
}

/// Deferred side effect of one state-machine step, handled once the borrow of the
/// connection has ended.
enum Action {
    None,
    Write(usize),
    Read,
    Eof,
}

impl TlsConn {
    pub async fn connect(
        raw: TcpStream,
        config: Arc<ClientConfig>,
        host: &str,
    ) -> Result<Self, Error> {
        let name = ServerName::try_from(host.to_owned())
            .map_err(|_| Error::Protocol("invalid server name"))?;
        let conn = UnbufferedClientConnection::new(config, name)?;
        let mut this = Self {
            conn,
            raw,
            ct: Vec::with_capacity(READ_BUFFER),
            inbuf: Vec::with_capacity(READ_BUFFER),
            out: vec![0; OUT_BUFFER],
            plain: Carry::default(),
            closed: false,
            read_timeout: Some(IO_TIMEOUT),
            failed: false,
        };
        this.handshake().await?;
        Ok(this)
    }

    async fn handshake(&mut self) -> Result<(), Error> {
        loop {
            let action;
            let discard;

            {
                let status = self.conn.process_tls_records(&mut self.ct);
                discard = status.discard;
                action = match status.state {
                    Err(error) => {
                        self.failed = true;
                        return Err(error.into());
                    }
                    Ok(ConnectionState::WriteTraffic(_)) => return Ok(()),
                    Ok(ConnectionState::EncodeTlsData(mut encode)) => {
                        Action::Write(encode.encode(&mut self.out).map_err(encode_error)?)
                    }
                    Ok(ConnectionState::TransmitTlsData(transmit)) => {
                        // The record was written when it was encoded.
                        transmit.done();
                        Action::None
                    }
                    Ok(ConnectionState::BlockedHandshake) => Action::Read,
                    Ok(_) => Action::None,
                };
            }

            if discard > 0 {
                self.ct.drain(..discard);
            }

            match action {
                Action::Write(len) => self.write_raw(len).await?,
                Action::Read => self.refill().await?,
                Action::None => {}
                Action::Eof => return Err(Error::Closed),
            }
        }
    }

    async fn write_raw(&mut self, len: usize) -> Result<(), Error> {
        let buf = self.out[..len].to_vec();
        timeout(IO_TIMEOUT, self.raw.write_all(buf)).await?.0?;
        Ok(())
    }

    pub const fn set_read_timeout(&mut self, limit: Option<Duration>) {
        self.read_timeout = limit;
    }

    async fn refill(&mut self) -> Result<(), Error> {
        let mut buf = std::mem::take(&mut self.inbuf);
        buf.clear();
        buf.reserve(READ_BUFFER);
        let read_timeout = self.read_timeout;
        let result = match read_timeout {
            Some(limit) => timeout(limit, self.raw.read(buf)).await?,
            None => self.raw.read(buf).await,
        };
        self.inbuf = result.1;
        let read = result.0?;
        if read == 0 {
            self.closed = true;
            return Err(Error::Closed);
        }
        self.ct.extend_from_slice(&self.inbuf[..read]);
        Ok(())
    }

    async fn pump(&mut self) -> Result<Pump, Error> {
        let plaintext_before = self.plain.data.len();
        let mut discard;
        let mut action = Action::None;

        {
            let status = self.conn.process_tls_records(&mut self.ct);
            discard = status.discard;
            match status.state {
                Err(error) => {
                    self.failed = true;
                    return Err(error.into());
                }
                Ok(ConnectionState::ReadTraffic(mut traffic)) => {
                    while let Some(record) = traffic.next_record() {
                        let record = record?;
                        self.plain.data.extend_from_slice(record.payload);
                        discard += record.discard;
                    }
                }
                Ok(ConnectionState::EncodeTlsData(mut encode)) => {
                    action = Action::Write(encode.encode(&mut self.out).map_err(encode_error)?);
                }
                Ok(ConnectionState::TransmitTlsData(transmit)) => transmit.done(),
                Ok(ConnectionState::WriteTraffic(_) | ConnectionState::BlockedHandshake) => {
                    action = Action::Read;
                }
                Ok(ConnectionState::PeerClosed | ConnectionState::Closed) => {
                    action = Action::Eof;
                }
                Ok(_) => {}
            }
        }

        if discard > 0 {
            self.ct.drain(..discard);
        }

        match action {
            Action::Eof => {
                self.closed = true;
                Ok(Pump::Eof)
            }
            Action::Write(len) => {
                self.write_raw(len).await?;
                Ok(Pump::Progress)
            }
            Action::Read => {
                if self.closed {
                    return Ok(Pump::Eof);
                }
                match self.refill().await {
                    Ok(()) => Ok(Pump::Progress),
                    Err(Error::Closed) if self.closed => Ok(Pump::Eof),
                    Err(error) => Err(error),
                }
            }
            Action::None => {
                if self.plain.data.len() > plaintext_before {
                    Ok(Pump::Plaintext)
                } else {
                    Ok(Pump::Progress)
                }
            }
        }
    }

    pub async fn read_into(&mut self, dst: &mut [u8]) -> Result<usize, Error> {
        if self.failed {
            return Err(Error::Closed);
        }
        loop {
            let served = self.plain.serve(dst);
            if served > 0 || dst.is_empty() {
                return Ok(served);
            }
            if self.closed {
                return Ok(0);
            }
            match self.pump().await? {
                Pump::Plaintext | Pump::Progress => {}
                Pump::Eof => return Ok(0),
            }
        }
    }

    pub async fn write_all(&mut self, mut data: &[u8]) -> Result<(), Error> {
        if self.failed {
            return Err(Error::Closed);
        }
        while !data.is_empty() {
            let mut discard;
            let mut wrote = 0;
            let mut pending = None;

            {
                let status = self.conn.process_tls_records(&mut self.ct);
                discard = status.discard;
                match status.state {
                    Err(error) => {
                        self.failed = true;
                        return Err(error.into());
                    }
                    Ok(ConnectionState::EncodeTlsData(mut encode)) => {
                        pending = Some(encode.encode(&mut self.out).map_err(encode_error)?);
                    }
                    Ok(ConnectionState::TransmitTlsData(transmit)) => transmit.done(),
                    Ok(ConnectionState::WriteTraffic(mut traffic)) => {
                        let take = data.len().min(WRITE_CHUNK);
                        let written = traffic
                            .encrypt(&data[..take], &mut self.out)
                            .map_err(encrypt_error)?;
                        pending = Some(written);
                        wrote = take;
                    }
                    Ok(ConnectionState::ReadTraffic(mut traffic)) => {
                        while let Some(record) = traffic.next_record() {
                            let record = record?;
                            self.plain.data.extend_from_slice(record.payload);
                            // The ciphertext of every decrypted record must leave
                            // `ct` here too. Pings and subscribe responses arrive
                            // while the client is writing, and leaving their bytes
                            // in `ct` makes the next read decrypt them again into
                            // garbage.
                            discard += record.discard;
                        }
                    }
                    Ok(ConnectionState::BlockedHandshake) => {
                        return Err(Error::Protocol("still handshaking"));
                    }
                    Ok(ConnectionState::PeerClosed | ConnectionState::Closed) => {
                        return Err(Error::Closed);
                    }
                    Ok(_) => {}
                }
            }

            if discard > 0 {
                self.ct.drain(..discard);
            }
            if let Some(len) = pending {
                self.write_raw(len).await?;
            }
            data = &data[wrote..];
        }
        Ok(())
    }

    pub const fn is_closed(&self) -> bool {
        self.closed || self.failed
    }
}

#[allow(clippy::needless_pass_by_value)]
const fn encode_error(error: EncodeError) -> Error {
    Error::Protocol(match error {
        EncodeError::InsufficientSize(_) => "TLS record buffer too small",
        EncodeError::AlreadyEncoded => "TLS record encoded twice",
    })
}

#[allow(clippy::needless_pass_by_value)]
const fn encrypt_error(error: EncryptError) -> Error {
    Error::Protocol(match error {
        EncryptError::InsufficientSize(_) => "TLS encrypt buffer too small",
        EncryptError::EncryptExhausted => "TLS encrypter exhausted",
    })
}
