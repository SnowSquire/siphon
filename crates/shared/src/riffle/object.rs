//! The object header every carrier shares.
//!
//! A Riffle object is one access unit, one audio frame or one blob. On a QUIC
//! datagram the object is the frame, header first. On the pipe and the QUIC
//! control stream it is the payload of a class-1 frame (`riffle::codec`): a
//! `u32` length and a class byte in front of exactly these bytes, so the object
//! layer never learns which carrier it is on.
//!
//! `lane` is what a QUIC DATAGRAM frame has none of: the frame carries no stream
//! association, so the header has to name the lane. Control objects use lane 0;
//! a producer's lane number is the same one its `obj_id` range is seeded from.
//!
//! `obj_id` is monotonic per lane for the life of a connection and is never
//! reset, so a seek needs no epoch field and "drop if `obj_id <= last_delivered`"
//! survives one unchanged.
//!
//! A media object's payload opens with its own timing, because this header has
//! no timestamp field: `pts` then `dts`, two big-endian `i64`s in the track's
//! timescale (the one `StreamConfig` carries). Those 16 bytes are part of
//! `total_len` and fragment like every other payload byte, so the receiver
//! reads them from the reassembled object. `daemon::media::wire` writes them.

use std::fmt;

/// Bytes in the datagram form: class, lane, flags, id, total length, fragment
/// offset.
pub const DATAGRAM_HEADER: usize = 20;
/// Sanity ceiling for one object. A 1080p keyframe is around 1 MiB.
pub const MAX_OBJECT: u32 = 4 * 1024 * 1024;
/// The payload prefix a media object opens with: `pts`, then `dts`, each a
/// big-endian `i64`. See the module docs.
pub const TIMESTAMP_PREFIX: usize = 16;

/// Which policy an object is subject to.
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
    /// Which lane on the connection this object belongs to. Control is lane 0.
    pub lane: u16,
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
        let lane = u16::from_be_bytes([buf[1], buf[2]]);
        let flags = Flags::from_bits(buf[3]);
        let mut id = [0u8; 8];
        id.copy_from_slice(&buf[4..12]);
        let mut total = [0u8; 4];
        total.copy_from_slice(&buf[12..16]);
        let mut offset = [0u8; 4];
        offset.copy_from_slice(&buf[16..20]);
        let object = Self {
            class,
            lane,
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
        out[1..3].copy_from_slice(&self.lane.to_be_bytes());
        out[3] = self.flags.bits();
        out[4..12].copy_from_slice(&self.obj_id.to_be_bytes());
        out[12..16].copy_from_slice(&self.total_len.to_be_bytes());
        out[16..20].copy_from_slice(&self.frag_offset.to_be_bytes());
        out[DATAGRAM_HEADER..written].copy_from_slice(self.payload);
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
            lane: 3,
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
        assert_eq!(decoded.lane, 3);
        assert!(decoded.is_last());
        assert!(decoded.flags.contains(Flags::KEYFRAME));
        assert!(!decoded.flags.contains(Flags::LAST_FRAGMENT));
    }

    /// The datagram form is 20 bytes, so a full 1200-byte datagram carries 1180
    /// payload bytes and not one more.
    #[test]
    fn the_datagram_budget_holds_with_the_lane_field() {
        assert_eq!(DATAGRAM_HEADER, 20);
        let payload = [9u8; 1200 - DATAGRAM_HEADER];
        let object = Object {
            total_len: u32::try_from(payload.len()).unwrap(),
            frag_offset: 0,
            payload: &payload,
            ..sample()
        };
        let mut buf = [0u8; 1200];
        let written = object.encode_datagram(&mut buf).unwrap();
        assert_eq!(written, 1200, "the header and payload are the whole budget");
        assert_eq!(
            Object::decode_datagram(&buf[..written]).unwrap().lane,
            object.lane
        );
    }

    #[test]
    fn a_short_datagram_is_rejected() {
        assert_eq!(
            Object::decode_datagram(&[0u8; DATAGRAM_HEADER - 1]),
            Err(ObjectError::Short)
        );
    }

    #[test]
    fn unknown_class_and_bad_length_are_rejected() {
        let mut buf = [0u8; 32];
        buf[0] = 9;
        assert_eq!(Object::decode_datagram(&buf), Err(ObjectError::Class(9)));
        buf[0] = 0;
        buf[12..16].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(Object::decode_datagram(&buf), Err(ObjectError::TotalLen(0)));
        buf[12..16].copy_from_slice(&(MAX_OBJECT + 1).to_be_bytes());
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
