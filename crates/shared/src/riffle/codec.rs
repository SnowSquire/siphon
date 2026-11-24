//! Length-prefixed frames with one byte of class.
//!
//! One frame is a big-endian `u32` payload length, a one-byte class, then the
//! payload:
//!
//! ```text
//! u32 payload_len | u8 class | payload
//! ```
//!
//! Class `0` is a control message: the `postcard` encoding, unchanged. Class `1`
//! is one media object in the datagram form of [`crate::riffle::object`], which
//! `Object::decode_datagram` reads as it is; the pipe and the QUIC control
//! stream use the same frame, so a media lane needs no second transport.
//!
//! The class byte is a framing change and not a message change, so
//! [`PROTOCOL_VERSION`](crate::riffle::protocol::PROTOCOL_VERSION) stays where
//! it is: both ends of a connection are one build, and a stale one is refused by
//! the handshake either way.
//!
//! Each class has its own ceiling — [`MAX_FRAME`] for control,
//! [`MAX_OBJECT`](crate::riffle::object::MAX_OBJECT) for media. The length is
//! checked against the widest ceiling as soon as it arrives, and against the
//! frame's own ceiling as soon as its class byte does, so a bad peer can make
//! the connection fail but never make the process grow past one object.

use compio::io::AsyncRead;
use compio::io::AsyncWrite;
use compio::io::AsyncWriteExt as _;
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::riffle::object::MAX_OBJECT;
use crate::riffle::protocol::MAX_FRAME;

/// Staging buffer for one socket read. RPC traffic is user actions and state
/// updates, never a hot loop, so this is about syscalls rather than throughput.
const READ_BUFFER: usize = 16 * 1024;

/// Bytes of framing in front of a payload: the `u32` length and the class.
pub const FRAME_PREFIX: usize = 5;

/// What a frame carries. This is the class byte, after the length prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameClass {
    /// One `postcard`-encoded message.
    Control = 0,
    /// One media object in the datagram form of [`crate::riffle::object`].
    Media = 1,
}

impl FrameClass {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::Control,
            1 => Self::Media,
            _ => return None,
        })
    }

    /// The largest payload this class may carry.
    #[must_use]
    pub const fn cap(self) -> usize {
        match self {
            Self::Control => MAX_FRAME,
            Self::Media => MAX_OBJECT as usize,
        }
    }
}

/// The widest payload any class accepts, for the check a peer's length gets
/// before its class byte has arrived.
const WIDEST_CAP: usize = if MAX_FRAME > MAX_OBJECT as usize {
    MAX_FRAME
} else {
    MAX_OBJECT as usize
};

impl std::fmt::Display for FrameClass {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Control => "control",
            Self::Media => "media",
        })
    }
}

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame payload is {len} bytes, over the {cap} byte cap of its class")]
    Oversize { len: usize, cap: usize },
    #[error("unknown frame class {0}")]
    UnknownClass(u8),
    #[error("expected a control frame, got a {0} frame")]
    UnexpectedClass(FrameClass),
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

/// Turns a message into one complete control frame.
pub fn encode<T: Serialize>(message: &T, out: &mut Vec<u8>) -> Result<(), CodecError> {
    let payload = postcard::to_allocvec(message)?;
    encode_payload(FrameClass::Control, &payload, out)
}

/// Turns one media object's datagram form into one complete media frame.
pub fn encode_media(datagram: &[u8], out: &mut Vec<u8>) -> Result<(), CodecError> {
    encode_payload(FrameClass::Media, datagram, out)
}

fn encode_payload(class: FrameClass, payload: &[u8], out: &mut Vec<u8>) -> Result<(), CodecError> {
    let cap = class.cap();
    if payload.len() > cap {
        return Err(CodecError::Oversize {
            len: payload.len(),
            cap,
        });
    }
    let length = u32::try_from(payload.len()).map_err(|_| CodecError::Oversize {
        len: payload.len(),
        cap,
    })?;
    out.reserve(FRAME_PREFIX + payload.len());
    out.extend_from_slice(&length.to_be_bytes());
    out.push(class.as_u8());
    out.extend_from_slice(payload);
    Ok(())
}

/// Decodes one control payload.
pub fn decode<T: DeserializeOwned>(payload: &[u8]) -> Result<T, CodecError> {
    Ok(postcard::from_bytes(payload)?)
}

/// One complete frame.
///
/// The payload is owned: a frame that a task hands on — a media object on its
/// way to the UI's assembler — must not borrow the decoder's buffer.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    class: FrameClass,
    payload: Vec<u8>,
}

impl Frame {
    #[must_use]
    pub const fn class(&self) -> FrameClass {
        self.class
    }

    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// The payload, handed over without a copy.
    #[must_use]
    pub fn into_payload(self) -> Vec<u8> {
        self.payload
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.payload.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }
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
    fn payload_len(&self) -> Option<usize> {
        let header: [u8; 4] = self.buffer.get(..4)?.try_into().ok()?;
        usize::try_from(u32::from_be_bytes(header)).ok()
    }

    /// Whether a complete frame is buffered.
    #[must_use]
    pub fn has_frame(&self) -> bool {
        self.payload_len().is_some_and(|len| {
            self.buffer.get(4).is_some() && self.buffer.len() >= FRAME_PREFIX + len
        })
    }

    /// Pops one frame; `None` means more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, CodecError> {
        let Some(len) = self.payload_len() else {
            return Ok(None);
        };
        // No class byte yet: the length is checked against the widest ceiling,
        // so waiting for it can never buffer more than one object.
        if len > WIDEST_CAP {
            return Err(CodecError::Oversize {
                len,
                cap: WIDEST_CAP,
            });
        }
        let Some(&class) = self.buffer.get(4) else {
            return Ok(None);
        };
        let class = FrameClass::from_u8(class).ok_or(CodecError::UnknownClass(class))?;
        let cap = class.cap();
        if len > cap {
            return Err(CodecError::Oversize { len, cap });
        }
        let total = FRAME_PREFIX + len;
        if self.buffer.len() < total {
            return Ok(None);
        }
        let mut payload = Vec::with_capacity(len);
        payload.extend_from_slice(&self.buffer[FRAME_PREFIX..total]);
        // The rest stays buffered with the capacity it already has.
        self.buffer.drain(..total);
        Ok(Some(Frame { class, payload }))
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

    /// Reads one frame of either class, waiting for the rest of it as needed.
    pub async fn read_frame<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
    ) -> Result<Frame, CodecError> {
        loop {
            if let Some(frame) = self.decoder.next_frame()? {
                return Ok(frame);
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

    /// Reads one control message, waiting for the rest of its frame as needed.
    ///
    /// A media frame here is an error: this is the control path, and a message
    /// the caller is waiting on never hides behind a picture.
    pub async fn read<T: DeserializeOwned, R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
    ) -> Result<T, CodecError> {
        let frame = self.read_frame(reader).await?;
        if frame.class() != FrameClass::Control {
            return Err(CodecError::UnexpectedClass(frame.class()));
        }
        decode(frame.payload())
    }
}

/// Writes one framed control message.
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
    use crate::riffle::object::DATAGRAM_HEADER;
    use crate::riffle::object::Object;
    use crate::riffle::protocol::ClientMessage;
    use crate::riffle::protocol::MAX_AVATAR;
    use crate::riffle::protocol::Request;

    fn control_message() -> ClientMessage {
        ClientMessage::Request {
            id: 7,
            request: Request::AddLogin {
                login: "alice".to_owned(),
            },
        }
    }

    /// One media object's datagram form, as the packer writes it.
    fn datagram(payload: &[u8]) -> Vec<u8> {
        let object = Object {
            class: crate::riffle::object::Class::Video,
            lane: 3,
            flags: crate::riffle::object::Flags::KEYFRAME,
            obj_id: 11,
            total_len: u32::try_from(payload.len()).unwrap(),
            frag_offset: 0,
            payload,
        };
        let mut buf = vec![0u8; DATAGRAM_HEADER + payload.len()];
        let written = object.encode_datagram(&mut buf).unwrap();
        buf.truncate(written);
        buf
    }

    #[test]
    fn a_control_frame_round_trips() {
        let message = control_message();
        let mut frame = Vec::new();
        encode(&message, &mut frame).unwrap();
        assert_eq!(frame[4], FrameClass::Control.as_u8());
        assert_eq!(
            &frame[..4],
            &u32::try_from(frame.len() - FRAME_PREFIX)
                .unwrap()
                .to_be_bytes()
        );

        let mut decoder = FrameDecoder::new();
        decoder.push(&frame);
        let frame = decoder.next_frame().unwrap().expect("a complete frame");
        assert_eq!(frame.class(), FrameClass::Control);
        let message: ClientMessage = decode(frame.payload()).unwrap();
        assert!(matches!(
            message,
            ClientMessage::Request { id: 7, request: Request::AddLogin { login } } if login == "alice"
        ));
    }

    #[test]
    fn a_media_frame_round_trips() {
        let object = datagram(&[0x5A; 300]);
        let mut frame = Vec::new();
        encode_media(&object, &mut frame).unwrap();
        assert_eq!(frame[4], FrameClass::Media.as_u8());
        assert_eq!(
            &frame[..4],
            &u32::try_from(frame.len() - FRAME_PREFIX)
                .unwrap()
                .to_be_bytes()
        );

        let mut decoder = FrameDecoder::new();
        decoder.push(&frame);
        let frame = decoder.next_frame().unwrap().expect("a complete frame");
        assert_eq!(frame.class(), FrameClass::Media);
        assert_eq!(frame.payload(), object.as_slice(), "the datagram is intact");
        let object = Object::decode_datagram(frame.payload()).expect("a valid object header");
        assert_eq!(object.lane, 3);
        assert_eq!(object.obj_id, 11);
        assert_eq!(object.payload.len(), 300);
    }

    /// Both classes may share one read: the class byte is what splits them.
    #[test]
    fn a_control_and_a_media_frame_in_one_read_are_both_returned() {
        let mut bytes = Vec::new();
        encode(&control_message(), &mut bytes).unwrap();
        encode_media(&datagram(&[1; 40]), &mut bytes).unwrap();

        let mut decoder = FrameDecoder::new();
        decoder.push(&bytes);
        assert_eq!(
            decoder
                .next_frame()
                .unwrap()
                .expect("the control frame")
                .class(),
            FrameClass::Control
        );
        assert_eq!(
            decoder
                .next_frame()
                .unwrap()
                .expect("the media frame")
                .class(),
            FrameClass::Media
        );
        assert!(decoder.next_frame().unwrap().is_none());
        assert_eq!(decoder.buffered(), 0);
    }

    /// The cap is checked from the class byte alone: the decoder must not wait
    /// for — let alone allocate — a payload it has already refused.
    #[test]
    fn an_over_cap_media_frame_is_rejected_before_the_payload_arrives() {
        let len = MAX_OBJECT as usize + 1;
        let mut decoder = FrameDecoder::new();
        decoder.push(&u32::try_from(len).unwrap().to_be_bytes());
        decoder.push(&[FrameClass::Media.as_u8()]);
        assert!(matches!(
            decoder.next_frame(),
            Err(CodecError::Oversize { len: at, cap }) if at == len && cap == MAX_OBJECT as usize
        ));
    }

    #[test]
    fn an_over_cap_control_frame_is_rejected_before_the_payload_arrives() {
        let len = MAX_FRAME + 1;
        let mut decoder = FrameDecoder::new();
        decoder.push(&u32::try_from(len).unwrap().to_be_bytes());
        decoder.push(&[FrameClass::Control.as_u8()]);
        assert!(matches!(
            decoder.next_frame(),
            Err(CodecError::Oversize { len: at, cap }) if at == len && cap == MAX_FRAME
        ));
    }

    /// A length past every class's ceiling is refused before the class byte,
    /// so a peer cannot grow the buffer by withholding it.
    #[test]
    fn a_length_past_every_cap_is_rejected_before_the_class_byte() {
        let len = MAX_OBJECT as usize + 1;
        let mut decoder = FrameDecoder::new();
        decoder.push(&u32::try_from(len).unwrap().to_be_bytes());
        assert!(matches!(
            decoder.next_frame(),
            Err(CodecError::Oversize { .. })
        ));
    }

    #[test]
    fn an_unknown_class_is_rejected() {
        let mut decoder = FrameDecoder::new();
        decoder.push(&1u32.to_be_bytes());
        decoder.push(&[9]);
        decoder.push(&[0]);
        assert!(matches!(
            decoder.next_frame(),
            Err(CodecError::UnknownClass(9))
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
        let payload = &frame[FRAME_PREFIX..frame.len() - 1];
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
}
