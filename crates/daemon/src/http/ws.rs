//! Minimal RFC 6455 WebSocket client on top of the HTTP transport.
//!
//! Only what Hermes needs: one text stream, client-side masking, ping/pong,
//! fragmentation and the close handshake. No extensions, no compression and
//! no binary payloads; Hermes speaks JSON text frames.

use crate::constants::IO_TIMEOUT;
use crate::http::Error;
use crate::http::pool::PoolKey;
use crate::http::read_head;
use crate::http::timeout;
use crate::http::tls_config;
use crate::http::transport::Conn;
use crate::sha1;

const GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const READ_BUFFER: usize = 16 * 1024;
/// One message may not exceed this. Hermes notifications are a few KiB.
const MAX_MESSAGE: usize = 4 * 1024 * 1024;
/// Bytes buffered while waiting for a frame header/payload.
const MAX_BUFFERED: usize = MAX_MESSAGE + 64;

const OP_CONTINUATION: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

/// One decoded frame, after fragmentation has been reassembled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(u16, String),
}

/// A connected WebSocket, read and written sequentially by one task.
pub struct WsStream {
    conn: Conn,
    buffered: Vec<u8>,
    fragments: Vec<u8>,
    fragment_opcode: Option<u8>,
    closed: bool,
}

impl WsStream {
    /// Opens `ws://` or `wss://` and performs the upgrade handshake.
    pub async fn connect(url: &str) -> Result<Self, Error> {
        let (key, path) = parse_url(url)?;
        let host = key.host.clone();
        let authority = if key.port == default_port(key.tls) {
            host.clone()
        } else {
            format!("{host}:{}", key.port)
        };

        let mut conn = Conn::connect(&key, &tls_config()).await?;

        let nonce: [u8; 16] = rand::random();
        let client_key = base64(&nonce);
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {client_key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        conn.write_all(request.as_bytes()).await?;

        let head = read_head(&mut conn).await?;
        if head.status != http::StatusCode::SWITCHING_PROTOCOLS {
            return Err(Error::Protocol("websocket upgrade refused"));
        }
        if !header_is(&head.headers, "upgrade", "websocket")
            || !header_contains(&head.headers, "connection", "upgrade")
        {
            return Err(Error::Protocol("websocket upgrade headers missing"));
        }
        let expected = base64(&sha1::digest(&[client_key.as_bytes(), GUID].concat()));
        let accept = head
            .headers
            .get("sec-websocket-accept")
            .and_then(|value| value.to_str().ok());
        if accept != Some(expected.as_str()) {
            return Err(Error::Protocol("websocket accept key mismatch"));
        }

        Ok(Self {
            conn,
            buffered: head.leftover,
            fragments: Vec::new(),
            fragment_opcode: None,
            closed: false,
        })
    }

    /// Reads the next text message. Control frames are handled here: pings are
    /// answered, a close frame ends the stream with `None`.
    pub async fn next_text(&mut self) -> Result<Option<String>, Error> {
        loop {
            match self.read_frame().await? {
                Frame::Text(text) => return Ok(Some(text)),
                Frame::Ping(payload) => self.send(OP_PONG, &payload).await?,
                Frame::Pong(_) => {}
                Frame::Binary(_) => {
                    return Err(Error::Protocol("unexpected binary websocket frame"));
                }
                Frame::Close(_, _) => {
                    if !self.closed {
                        let _ = self.send(OP_CLOSE, &1000u16.to_be_bytes()).await;
                    }
                    self.closed = true;
                    return Ok(None);
                }
            }
        }
    }

    /// Sends one text message.
    pub async fn send_text(&mut self, text: &str) -> Result<(), Error> {
        self.send(OP_TEXT, text.as_bytes()).await
    }

    /// Sends a close frame. Best effort: the caller is already tearing down.
    pub async fn close(&mut self, code: u16, reason: &str) -> Result<(), Error> {
        let mut payload = code.to_be_bytes().to_vec();
        payload.extend_from_slice(reason.as_bytes());
        let result = self.send(OP_CLOSE, &payload).await;
        self.closed = true;
        result
    }

    async fn send(&mut self, opcode: u8, payload: &[u8]) -> Result<(), Error> {
        let frame = encode_client_frame(opcode, payload, rand::random::<[u8; 4]>());
        timeout(IO_TIMEOUT, self.conn.write_all(&frame)).await??;
        Ok(())
    }

    /// Reads exactly `len` bytes, pulling from the connection as needed.
    async fn read_exact(&mut self, len: usize) -> Result<Vec<u8>, Error> {
        while self.buffered.len() < len {
            let mut chunk = vec![0u8; READ_BUFFER];
            let read = timeout(IO_TIMEOUT, self.conn.read_into(&mut chunk)).await??;
            if read == 0 {
                return Err(Error::Closed);
            }
            self.buffered.extend_from_slice(&chunk[..read]);
            if self.buffered.len() > MAX_BUFFERED {
                return Err(Error::Protocol("websocket frame too large"));
            }
        }
        Ok(self.buffered.drain(..len).collect())
    }

    async fn read_frame(&mut self) -> Result<Frame, Error> {
        loop {
            let header = self.read_exact(2).await?;
            let first = header[0];
            let second = header[1];
            let fin = first & 0x80 != 0;
            if first & 0x70 != 0 {
                return Err(Error::Protocol("websocket reserved bits set"));
            }
            let opcode = first & 0x0F;
            if second & 0x80 != 0 {
                return Err(Error::Protocol("masked server frame"));
            }
            let mut length = u64::from(second & 0x7F);
            if length == 126 {
                let extended = self.read_exact(2).await?;
                length = u64::from(u16::from_be_bytes(
                    extended.try_into().map_err(|_| Error::Closed)?,
                ));
            } else if length == 127 {
                let extended = self.read_exact(8).await?;
                length = u64::from_be_bytes(extended.try_into().map_err(|_| Error::Closed)?);
            }
            let length = usize::try_from(length)
                .map_err(|_| Error::Protocol("websocket frame too large"))?;
            if length > MAX_MESSAGE {
                return Err(Error::Protocol("websocket frame too large"));
            }
            let is_control = opcode >= OP_CLOSE;
            if is_control && (!fin || length > 125) {
                return Err(Error::Protocol("invalid websocket control frame"));
            }
            let payload = self.read_exact(length).await?;

            match opcode {
                OP_TEXT | OP_BINARY if fin => return complete_message(opcode, payload),
                OP_TEXT | OP_BINARY => {
                    if self.fragment_opcode.is_some() {
                        return Err(Error::Protocol("interleaved websocket fragments"));
                    }
                    self.fragment_opcode = Some(opcode);
                    self.fragments = payload;
                }
                OP_CONTINUATION => {
                    let Some(_) = self.fragment_opcode else {
                        return Err(Error::Protocol("unexpected websocket continuation"));
                    };
                    if self.fragments.len() + payload.len() > MAX_MESSAGE {
                        return Err(Error::Protocol("websocket message too large"));
                    }
                    self.fragments.extend_from_slice(&payload);
                    if fin {
                        let opcode = self
                            .fragment_opcode
                            .take()
                            .expect("checked above before extending");
                        let payload = std::mem::take(&mut self.fragments);
                        return complete_message(opcode, payload);
                    }
                }
                OP_CLOSE => {
                    if payload.is_empty() {
                        return Ok(Frame::Close(1005, String::new()));
                    }
                    if payload.len() < 2 {
                        return Err(Error::Protocol("invalid websocket close frame"));
                    }
                    let code = u16::from_be_bytes([payload[0], payload[1]]);
                    let reason = String::from_utf8(payload[2..].to_vec())
                        .map_err(|_| Error::Protocol("invalid websocket close reason"))?;
                    return Ok(Frame::Close(code, reason));
                }
                OP_PING => return Ok(Frame::Ping(payload)),
                OP_PONG => return Ok(Frame::Pong(payload)),
                _ => return Err(Error::Protocol("unknown websocket opcode")),
            }
        }
    }
}

fn complete_message(opcode: u8, payload: Vec<u8>) -> Result<Frame, Error> {
    match opcode {
        OP_TEXT => String::from_utf8(payload)
            .map(Frame::Text)
            .map_err(|_| Error::Protocol("invalid utf-8 in websocket text")),
        OP_BINARY => Ok(Frame::Binary(payload)),
        _ => Err(Error::Protocol("invalid websocket message opcode")),
    }
}

const fn default_port(tls: bool) -> u16 {
    if tls { 443 } else { 80 }
}

fn header_is(headers: &http::HeaderMap, name: &str, expected: &str) -> bool {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

fn header_contains(headers: &http::HeaderMap, name: &str, needle: &str) -> bool {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case(needle))
        })
}

/// Splits `ws[s]://host[:port]/path` into a connection key and request path.
fn parse_url(url: &str) -> Result<(PoolKey, String), Error> {
    let (tls, rest) = if let Some(rest) = url.strip_prefix("wss://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("ws://") {
        (false, rest)
    } else {
        return Err(Error::Scheme(url.to_owned()));
    };
    let (authority, path) = rest.split_once('/').map_or((rest, "/"), |(authority, _)| {
        (authority, &rest[authority.len()..])
    });
    if authority.is_empty() {
        return Err(Error::Url(url.to_owned()));
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, suffix)) = rest.split_once(']') else {
            return Err(Error::Url(url.to_owned()));
        };
        let port = suffix
            .strip_prefix(':')
            .map(str::parse::<u16>)
            .transpose()
            .map_err(|_| Error::Url(url.to_owned()))?
            .unwrap_or_else(|| default_port(tls));
        (host.to_owned(), port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        (
            host.to_owned(),
            port.parse::<u16>()
                .map_err(|_| Error::Url(url.to_owned()))?,
        )
    } else {
        (authority.to_owned(), default_port(tls))
    };
    Ok((PoolKey::new(tls, host, port), path.to_owned()))
}

/// Builds one masked client frame.
fn encode_client_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 14);
    frame.push(0x80 | opcode);
    let length = payload.len();
    if length < 126 {
        frame.push(0x80 | u8::try_from(length).expect("length < 126"));
    } else if let Ok(length) = u16::try_from(length) {
        frame.push(0x80 | 0x7E);
        frame.extend_from_slice(&length.to_be_bytes());
    } else {
        frame.push(0x80 | 0x7F);
        frame.extend_from_slice(&u64::try_from(length).expect("usize fits u64").to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4]),
    );
    frame
}

/// Standard base64 with padding, for the handshake key and accept digest.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(b2 & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected, "{input}");
        }
    }

    #[test]
    fn accept_key_matches_the_rfc_example() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let digest = sha1::digest(&[key.as_bytes(), GUID].concat());
        assert_eq!(base64(&digest), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn client_frames_are_masked_and_round_trip() {
        let frame = encode_client_frame(OP_TEXT, b"hello", [1, 2, 3, 4]);
        assert_eq!(frame[0], 0x81);
        assert_eq!(frame[1], 0x85);
        assert_eq!(&frame[2..6], &[1, 2, 3, 4]);
        let payload: Vec<u8> = frame[6..]
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ [1u8, 2, 3, 4][index % 4])
            .collect();
        assert_eq!(payload, b"hello");
    }

    fn server_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(payload.len() + 10);
        frame.push(if fin { 0x80 } else { 0 } | opcode);
        let length = payload.len();
        if length < 126 {
            frame.push(u8::try_from(length).unwrap());
        } else {
            frame.push(126);
            frame.extend_from_slice(&u16::try_from(length).unwrap().to_be_bytes());
        }
        frame.extend_from_slice(payload);
        frame
    }

    /// Reads exactly `len` bytes into a fresh owned buffer.
    async fn read_vec(stream: &mut compio::net::TcpStream, len: usize) -> Result<Vec<u8>, Error> {
        use compio::io::AsyncReadExt as _;
        let result = stream.read_exact(vec![0u8; len]).await;
        let buffer = result.1;
        result.0?;
        Ok(buffer)
    }

    /// Reads one masked client frame and returns `(opcode, payload)`.
    async fn read_client_frame(
        stream: &mut compio::net::TcpStream,
    ) -> Result<(u8, Vec<u8>), Error> {
        let header = read_vec(stream, 2).await?;
        let opcode = header[0] & 0x0F;
        assert_ne!(header[1] & 0x80, 0, "client frames are masked");
        let mut length = usize::from(header[1] & 0x7F);
        if length == 126 {
            let extended = read_vec(stream, 2).await?;
            length = usize::from(u16::from_be_bytes([extended[0], extended[1]]));
        }
        let mask = read_vec(stream, 4).await?;
        let mut payload = if length > 0 {
            read_vec(stream, length).await?
        } else {
            Vec::new()
        };
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
        Ok((opcode, payload))
    }

    async fn serve_handshake(stream: &mut compio::net::TcpStream) -> Result<(), Error> {
        use compio::io::AsyncRead;
        use compio::io::AsyncWriteExt as _;

        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let result = stream.read(vec![0u8; 1024]).await;
            let read = result.0?;
            let chunk = result.1;
            if read == 0 {
                return Err(Error::Closed);
            }
            request.extend_from_slice(&chunk[..read]);
        }
        let text = String::from_utf8_lossy(&request);
        let key = text
            .lines()
            .find_map(|line| line.strip_prefix("Sec-WebSocket-Key: "))
            .expect("client key")
            .trim()
            .to_owned();
        let accept = base64(&sha1::digest(&[key.as_bytes(), GUID].concat()));
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        );
        stream.write_all(response.into_bytes()).await.0?;
        Ok(())
    }

    #[test]
    fn handshake_reassembles_fragments_and_answers_pings() {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = async move {
                use compio::io::AsyncWriteExt as _;
                let (mut stream, _) = listener.accept().await.unwrap();
                serve_handshake(&mut stream).await.unwrap();
                // Fragmented text, then a ping, then more text, then a close.
                stream
                    .write_all(server_frame(false, OP_TEXT, b"he"))
                    .await
                    .0
                    .unwrap();
                stream
                    .write_all(server_frame(true, OP_CONTINUATION, b"llo"))
                    .await
                    .0
                    .unwrap();
                stream
                    .write_all(server_frame(true, OP_PING, b"p"))
                    .await
                    .0
                    .unwrap();
                stream
                    .write_all(server_frame(true, OP_TEXT, b"world"))
                    .await
                    .0
                    .unwrap();
                let mut close = 1000u16.to_be_bytes().to_vec();
                close.extend_from_slice(b"bye");
                stream
                    .write_all(server_frame(true, OP_CLOSE, &close))
                    .await
                    .0
                    .unwrap();

                let (pong, payload) = read_client_frame(&mut stream).await.unwrap();
                assert_eq!(pong, OP_PONG);
                assert_eq!(payload, b"p");
                let (closed, _) = read_client_frame(&mut stream).await.unwrap();
                assert_eq!(closed, OP_CLOSE);
            };
            let client = async {
                let mut ws = WsStream::connect(&format!("ws://{addr}/v1?x=1"))
                    .await
                    .unwrap();
                assert_eq!(ws.next_text().await.unwrap().as_deref(), Some("hello"));
                assert_eq!(ws.next_text().await.unwrap().as_deref(), Some("world"));
                assert_eq!(ws.next_text().await.unwrap(), None);
            };
            futures_util::join!(server, client);
        });
    }

    #[test]
    fn urls_split_into_key_and_path() {
        let (key, path) = parse_url("wss://hermes.twitch.tv/v1?clientId=x").unwrap();
        assert!(key.tls);
        assert_eq!(key.host, "hermes.twitch.tv");
        assert_eq!(key.port, 443);
        assert_eq!(path, "/v1?clientId=x");

        let (key, path) = parse_url("ws://127.0.0.1:8000/").unwrap();
        assert!(!key.tls);
        assert_eq!(key.port, 8000);
        assert_eq!(path, "/");
    }
}
