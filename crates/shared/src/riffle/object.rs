//! The object header every carrier shares.
//!
//! A Riffle object is one access unit, one audio frame or one blob. On a QUIC
//! datagram the header is 18 bytes; on a stream or the local pipe it is the same
//! header behind a `u32` length, so the object layer never learns which carrier
//! it is on.
//!
//! `obj_id` is monotonic for the life of a connection and is never reset, so a
//! seek needs no epoch field and "drop if `obj_id <= last_delivered`" survives
//! one unchanged.
//!
//! A media object's payload opens with its own timing, because this header has
//! no timestamp field: `pts` then `dts`, two big-endian `i64`s in the track's
//! timescale (the one `StreamConfig` carries). Those 16 bytes are part of
//! `total_len` and fragment like every other payload byte, so the receiver
//! reads them from the reassembled object. `daemon::media::wire` writes them.

use std::fmt;

/// Bytes in the datagram form: class, flags, id, total length, fragment offset.
pub const DATAGRAM_HEADER: usize = 18;
/// Bytes in the stream and pipe form, including the `u32` length prefix.
pub const STREAM_HEADER: usize = DATAGRAM_HEADER + 4;
/// Sanity ceiling for one object. A 1080p keyframe is around 1 MiB.
pub const MAX_OBJECT: u32 = 4 * 1024 * 1024;

/// Which policy an object is subject to. Doubles as the datagram lane tag,
/// because a QUIC DATAGRAM frame carries no stream association.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Class {
    Control = 0,
    Video = 1,
    Audio = 2,
    Blob = 3,
}

impl Class {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::Control,
            1 => Self::Video,
            2 => Self::Audio,
            3 => Self::Blob,
            _ => return None,
        })
    }
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Control => "control",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Blob => "blob",
        })
    }
}

/// Per-object policy bits.
///
/// `SUPERSEDE` means a receiver may discard this object once it has delivered a
/// newer one, and a sender may stop shipping it. `KEYFRAME` is the same fact
/// `AV_PKT_FLAG_KEY` carries: an IRAP, including an HEVC CRA.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags(u8);

impl Flags {
    pub const NONE: Self = Self(0);
    pub const SUPERSEDE: Self = Self(1);
    pub const KEYFRAME: Self = Self(1 << 1);
    pub const LAST_FRAGMENT: Self = Self(1 << 2);
    const KNOWN: u8 = Self::SUPERSEDE.0 | Self::KEYFRAME.0 | Self::LAST_FRAGMENT.0;

    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Keeps the known bits and drops the rest, so a newer sender cannot make an
    /// older receiver misinterpret a flag.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & Self::KNOWN)
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// A decoded header plus a borrowed payload.
///
/// For [`Class::Video`] and [`Class::Audio`] the payload begins with the
/// 16-byte timestamp prefix described in the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Object<'a> {
    pub class: Class,
    pub flags: Flags,
    pub obj_id: u64,
    pub total_len: u32,
    pub frag_offset: u32,
    pub payload: &'a [u8],
}

/// Why a buffer is not a well-formed object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectError {
    /// Shorter than the header for its form.
    Short,
    /// A class byte outside the known set.
    Class(u8),
    /// `total_len` is zero or past [`MAX_OBJECT`].
    TotalLen(u32),
    /// The fragment runs past `total_len`.
    Fragment { offset: u32, len: u32, total: u32 },
    /// The output buffer is too small to encode into.
    NoSpace,
}

impl fmt::Display for ObjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Short => f.write_str("object shorter than its header"),
            Self::Class(value) => write!(f, "unknown object class {value}"),
            Self::TotalLen(value) => write!(f, "object length {value} out of range"),
            Self::Fragment { offset, len, total } => {
                write!(f, "fragment {offset}+{len} past object length {total}")
            }
            Self::NoSpace => f.write_str("output buffer too small"),
        }
    }
}

impl std::error::Error for ObjectError {}

impl<'a> Object<'a> {
    /// One past the last byte this fragment carries.
    #[must_use]
    pub fn end(&self) -> u64 {
        u64::from(self.frag_offset) + self.payload.len() as u64
    }

    /// Whether this fragment completes the object.
    #[must_use]
    pub fn is_last(&self) -> bool {
        self.end() == u64::from(self.total_len)
    }

    /// Parses the datagram form: header, then payload to the end of the slice.
    pub fn decode_datagram(buf: &'a [u8]) -> Result<Self, ObjectError> {
        if buf.len() < DATAGRAM_HEADER {
            return Err(ObjectError::Short);
        }
        let class = Class::from_u8(buf[0]).ok_or(ObjectError::Class(buf[0]))?;
        let flags = Flags::from_bits(buf[1]);
        let mut id = [0u8; 8];
        id.copy_from_slice(&buf[2..10]);
        let mut total = [0u8; 4];
        total.copy_from_slice(&buf[10..14]);
        let mut offset = [0u8; 4];
        offset.copy_from_slice(&buf[14..18]);
        let object = Self {
            class,
            flags,
            obj_id: u64::from_be_bytes(id),
            total_len: u32::from_be_bytes(total),
            frag_offset: u32::from_be_bytes(offset),
            payload: &buf[DATAGRAM_HEADER..],
        };
        object.validate()?;
        Ok(object)
    }

    /// Writes the datagram form into `out`, returning the bytes written.
    pub fn encode_datagram(&self, out: &mut [u8]) -> Result<usize, ObjectError> {
        self.validate()?;
        let written = DATAGRAM_HEADER + self.payload.len();
        if out.len() < written {
            return Err(ObjectError::NoSpace);
        }
        out[0] = self.class.as_u8();
        out[1] = self.flags.bits();
        out[2..10].copy_from_slice(&self.obj_id.to_be_bytes());
        out[10..14].copy_from_slice(&self.total_len.to_be_bytes());
        out[14..18].copy_from_slice(&self.frag_offset.to_be_bytes());
        out[DATAGRAM_HEADER..written].copy_from_slice(self.payload);
        Ok(written)
    }

    /// Parses the stream and pipe form: `u32` length, then the datagram form.
    pub fn decode_frame(buf: &'a [u8]) -> Result<Self, ObjectError> {
        if buf.len() < STREAM_HEADER {
            return Err(ObjectError::Short);
        }
        let mut len = [0u8; 4];
        len.copy_from_slice(&buf[..4]);
        let len = u32::from_be_bytes(len) as usize;
        let end = 4usize.checked_add(len).ok_or(ObjectError::Short)?;
        if buf.len() < end {
            return Err(ObjectError::Short);
        }
        Self::decode_datagram(&buf[4..end])
    }

    /// Writes the stream and pipe form into `out`, returning the bytes written.
    pub fn encode_frame(&self, out: &mut [u8]) -> Result<usize, ObjectError> {
        self.validate()?;
        let body = DATAGRAM_HEADER + self.payload.len();
        let written = 4 + body;
        if out.len() < written {
            return Err(ObjectError::NoSpace);
        }
        let body = u32::try_from(body).map_err(|_| ObjectError::NoSpace)?;
        out[..4].copy_from_slice(&body.to_be_bytes());
        self.encode_datagram(&mut out[4..written])?;
        Ok(written)
    }

    fn validate(&self) -> Result<(), ObjectError> {
        if self.total_len == 0 || self.total_len > MAX_OBJECT {
            return Err(ObjectError::TotalLen(self.total_len));
        }
        if self.end() > u64::from(self.total_len) {
            return Err(ObjectError::Fragment {
                offset: self.frag_offset,
                len: u32::try_from(self.payload.len()).unwrap_or(u32::MAX),
                total: self.total_len,
            });
        }
        Ok(())
    }
}

/// Number of fragments an object of `total_len` needs at `payload` bytes each.
///
/// The last fragment is whatever is left, so this is a ceiling division, and a
/// zero-length object still needs one fragment. A zero `payload` is treated as
/// one byte: the caller is sizing a header, and this must not panic.
#[must_use]
pub fn fragment_count(total_len: u32, payload: usize) -> u32 {
    let payload = u64::try_from(payload.max(1)).unwrap_or(u64::MAX);
    let total = u64::from(total_len.max(1));
    u32::try_from(total.div_ceil(payload)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Object<'static> {
        Object {
            class: Class::Video,
            flags: Flags::SUPERSEDE.union(Flags::KEYFRAME),
            obj_id: 0x0102_0304_0506_0708,
            total_len: 1500,
            frag_offset: 1200,
            payload: &[7u8; 300],
        }
    }

    #[test]
    fn datagram_roundtrip() {
        let object = sample();
        let mut buf = [0u8; 512];
        let written = object.encode_datagram(&mut buf).unwrap();
        assert_eq!(written, DATAGRAM_HEADER + 300);
        let decoded = Object::decode_datagram(&buf[..written]).unwrap();
        assert_eq!(decoded, object);
        assert!(decoded.is_last());
        assert!(decoded.flags.contains(Flags::KEYFRAME));
        assert!(!decoded.flags.contains(Flags::LAST_FRAGMENT));
    }

    #[test]
    fn stream_roundtrip_and_length_prefix() {
        let object = sample();
        let mut buf = [0u8; 512];
        let written = object.encode_frame(&mut buf).unwrap();
        assert_eq!(written, STREAM_HEADER + 300);
        assert_eq!(&buf[..4], &318u32.to_be_bytes());
        assert_eq!(Object::decode_frame(&buf[..written]).unwrap(), object);
    }

    #[test]
    fn short_buffers_are_rejected() {
        assert_eq!(Object::decode_datagram(&[0u8; 17]), Err(ObjectError::Short));
        assert_eq!(
            Object::decode_frame(&[0u8; STREAM_HEADER - 1]),
            Err(ObjectError::Short)
        );
        // A length prefix that runs past the slice is short, not a panic.
        let mut buf = [0u8; 32];
        buf[..4].copy_from_slice(&1000u32.to_be_bytes());
        assert_eq!(Object::decode_frame(&buf), Err(ObjectError::Short));
    }

    #[test]
    fn unknown_class_and_bad_length_are_rejected() {
        let mut buf = [0u8; 32];
        buf[0] = 9;
        assert_eq!(Object::decode_datagram(&buf), Err(ObjectError::Class(9)));
        buf[0] = 0;
        buf[10..14].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(Object::decode_datagram(&buf), Err(ObjectError::TotalLen(0)));
        buf[10..14].copy_from_slice(&(MAX_OBJECT + 1).to_be_bytes());
        assert_eq!(
            Object::decode_datagram(&buf),
            Err(ObjectError::TotalLen(MAX_OBJECT + 1))
        );
    }

    #[test]
    fn fragment_past_total_is_rejected() {
        let object = Object {
            total_len: 100,
            frag_offset: 50,
            payload: &[0u8; 100],
            ..sample()
        };
        assert_eq!(
            object.encode_datagram(&mut [0u8; 256]),
            Err(ObjectError::Fragment {
                offset: 50,
                len: 100,
                total: 100
            })
        );
    }

    #[test]
    fn unknown_flag_bits_are_dropped() {
        assert_eq!(Flags::from_bits(0xff).bits(), 0b111);
    }

    #[test]
    fn fragment_counts() {
        assert_eq!(fragment_count(3000, 1000), 3);
        assert_eq!(fragment_count(3001, 1000), 4);
        assert_eq!(fragment_count(1, 1000), 1);
        assert_eq!(fragment_count(0, 1000), 1);
    }
}
