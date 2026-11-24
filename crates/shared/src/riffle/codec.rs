//! Length-prefixed `postcard` frames.
//!
//! One frame is a big-endian `u32` payload length followed by the payload.
//! The length is checked against [`MAX_FRAME`] before the payload is read or
//! allocated, so a bad peer can make the connection fail but never make the
//! process grow without bound.

use compio::io::AsyncRead;
use compio::io::AsyncWrite;
use compio::io::AsyncWriteExt as _;
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::riffle::protocol::MAX_FRAME;

/// Staging buffer for one socket read. RPC traffic is user actions and state
/// updates, never a hot loop, so this is about syscalls rather than throughput.
const READ_BUFFER: usize = 16 * 1024;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame is {0} bytes, over the {MAX_FRAME} byte cap")]
    Oversize(usize),
    #[error("peer closed the connection")]
    Eof,
    #[error("postcard: {0}")]
    Postcard(#[from] postcard::Error),
}

impl CodecError {
    /// Whether the connection could still be used. A framing error means the
    /// stream position is unknown, so anything but a clean EOF is terminal.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        !matches!(self, Self::Eof)
    }
}

/// Turns a message into one complete frame.
pub fn encode<T: Serialize>(message: &T, out: &mut Vec<u8>) -> Result<(), CodecError> {
    let payload = postcard::to_allocvec(message)?;
    if payload.len() > MAX_FRAME {
        return Err(CodecError::Oversize(payload.len()));
    }
    let length = u32::try_from(payload.len()).map_err(|_| CodecError::Oversize(payload.len()))?;
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(())
}

/// Decodes one payload.
pub fn decode<T: DeserializeOwned>(payload: &[u8]) -> Result<T, CodecError> {
    Ok(postcard::from_bytes(payload)?)
}

/// Incremental decoder over already-read bytes.
#[derive(Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    #[must_use]
    pub const fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Appends bytes from the socket.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The payload length of the buffered header, if four bytes arrived.
    fn frame_length(&self) -> Option<usize> {
        let header: [u8; 4] = self.buffer.get(..4)?.try_into().ok()?;
        usize::try_from(u32::from_be_bytes(header)).ok()
    }

    /// Whether a complete frame is buffered.
    #[must_use]
    pub fn has_frame(&self) -> bool {
        self.frame_length()
            .is_some_and(|length| self.buffer.len() >= 4 + length)
    }

    /// Pops one payload; `None` means more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, CodecError> {
        let Some(length) = self.frame_length() else {
            return Ok(None);
        };
        if length > MAX_FRAME {
            return Err(CodecError::Oversize(length));
        }
        if self.buffer.len() < 4 + length {
            return Ok(None);
        }
        let payload = self.buffer[4..4 + length].to_vec();
        self.buffer.drain(..4 + length);
        Ok(Some(payload))
    }

    /// Bytes buffered but not yet consumed, for diagnostics and tests.
    #[must_use]
    pub const fn buffered(&self) -> usize {
        self.buffer.len()
    }
}

/// Reads frames from a socket half.
pub struct FrameReader {
    decoder: FrameDecoder,
    spare: Vec<u8>,
}

impl Default for FrameReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameReader {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            decoder: FrameDecoder::new(),
            spare: Vec::new(),
        }
    }

    /// Reads one message, waiting for the rest of its frame as needed.
    pub async fn read<T: DeserializeOwned, R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
    ) -> Result<T, CodecError> {
        loop {
            if let Some(payload) = self.decoder.next_frame()? {
                return decode(&payload);
            }
            let mut chunk = std::mem::take(&mut self.spare);
            chunk.clear();
            // A completion read learns its capacity from the buffer.
            chunk.reserve(READ_BUFFER);
            let buffer = chunk;
            let result = reader.read(buffer).await;
            let read = result.0;
            self.spare = result.1;
            let read = read?;
            if read == 0 {
                return Err(CodecError::Eof);
            }
            self.decoder.push(&self.spare[..read]);
        }
    }
}

/// Writes one framed message.
pub async fn write<T: Serialize, W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &T,
) -> Result<(), CodecError> {
    let mut frame = Vec::new();
    encode(message, &mut frame)?;
    let result = writer.write_all(frame).await.0;
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::riffle::protocol::ClientMessage;
    use crate::riffle::protocol::MAX_AVATAR;
    use crate::riffle::protocol::Request;

    #[test]
    fn roundtrip_preserves_the_message() {
        let message = ClientMessage::Request {
            id: 7,
            request: Request::AddLogin {
                login: "alice".to_owned(),
            },
        };
        let mut frame = Vec::new();
        encode(&message, &mut frame).unwrap();
        assert_eq!(
            &frame[..4],
            &u32::try_from(frame.len() - 4).unwrap().to_be_bytes()
        );

        let mut decoder = FrameDecoder::new();
        decoder.push(&frame);
        let payload = decoder.next_frame().unwrap().expect("a complete frame");
        let message: ClientMessage = decode(&payload).unwrap();
        assert!(matches!(
            message,
            ClientMessage::Request { id: 7, request: Request::AddLogin { login } } if login == "alice"
        ));
    }

    #[test]
    fn decoder_waits_for_the_header_and_then_the_payload() {
        let message = ClientMessage::Request {
            id: 1,
            request: Request::GetSnapshot,
        };
        let mut frame = Vec::new();
        encode(&message, &mut frame).unwrap();

        let mut decoder = FrameDecoder::new();
        decoder.push(&frame[..2]);
        assert!(!decoder.has_frame());
        assert!(decoder.next_frame().unwrap().is_none());
        decoder.push(&frame[2..6]);
        assert!(!decoder.has_frame());
        assert!(decoder.next_frame().unwrap().is_none());
        decoder.push(&frame[6..]);
        assert!(decoder.has_frame());
        assert!(decoder.next_frame().unwrap().is_some());
        assert_eq!(decoder.buffered(), 0);
    }

    #[test]
    fn decoder_rejects_an_oversized_length_without_buffering() {
        let mut decoder = FrameDecoder::new();
        decoder.push(&(u32::try_from(MAX_FRAME).unwrap() + 1).to_be_bytes());
        assert!(matches!(decoder.next_frame(), Err(CodecError::Oversize(_))));
    }

    #[test]
    fn decode_rejects_a_truncated_payload() {
        let message = ClientMessage::Request {
            id: 1,
            request: Request::SetRecord {
                channel: 7,
                record: true,
            },
        };
        let mut frame = Vec::new();
        encode(&message, &mut frame).unwrap();
        let payload = &frame[4..frame.len() - 1];
        assert!(matches!(
            decode::<ClientMessage>(payload),
            Err(CodecError::Postcard(_))
        ));
    }

    /// A capped avatar has to fit one frame, or `GetAvatar` would take the
    /// connection down instead of failing the picture.
    #[test]
    fn the_largest_avatar_fits_in_one_frame() {
        let response = crate::riffle::protocol::Response::Avatar(vec![0; MAX_AVATAR]);
        let mut frame = Vec::new();
        encode(&response, &mut frame).expect("a capped avatar fits one frame");
    }

    #[test]
    fn two_frames_in_one_read_are_both_returned() {
        let mut frame = Vec::new();
        encode(
            &ClientMessage::Request {
                id: 1,
                request: Request::GetSnapshot,
            },
            &mut frame,
        )
        .unwrap();
        encode(
            &ClientMessage::Request {
                id: 2,
                request: Request::GetArena,
            },
            &mut frame,
        )
        .unwrap();

        let mut decoder = FrameDecoder::new();
        decoder.push(&frame);
        assert!(decoder.next_frame().unwrap().is_some());
        assert!(decoder.next_frame().unwrap().is_some());
        assert!(decoder.next_frame().unwrap().is_none());
    }
}
