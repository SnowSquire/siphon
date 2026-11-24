//! ADTS parsing and RFC 3640 (`MPEG4-GENERIC`, AAC-hbr) packetization.
//!
//! Ported from `mediacommon/pkg/codecs/mpeg4audio` and
//! `gortsplib/pkg/format/rtpmpeg4audio` (both MIT). The TS source carries AAC as
//! ADTS frames; the header is stripped and the implied
//! `AudioSpecificConfig` goes into the SDP.

/// ADTS sampling-frequency indexes. `0` marks the reserved entries.
const SAMPLE_RATES: [u32; 16] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350, 0, 0, 0,
];

/// One decoder configuration, as advertised in SDP and implied by every ADTS
/// header of the stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub channels: u8,
    /// `AudioSpecificConfig` bytes (`config=` in the SDP fmtp line).
    pub asc: Vec<u8>,
}

/// Header fields needed to strip an ADTS frame and describe it.
#[derive(Clone, Debug)]
pub struct AdtsFrame {
    /// Bytes of ADTS header (7 or 9 with CRC).
    pub header_len: usize,
    /// Raw access unit length.
    pub frame_len: usize,
    pub config: AudioConfig,
}

impl AdtsFrame {
    #[must_use]
    pub const fn total_len(&self) -> usize {
        self.header_len + self.frame_len
    }

    /// Raw AAC payload of `frame`, or `None` when the frame is truncated.
    #[must_use]
    pub fn payload<'a>(&self, frame: &'a [u8]) -> Option<&'a [u8]> {
        frame.get(self.header_len..self.total_len())
    }
}

/// Parse one ADTS frame header, following mediaconvert's `ADTSPackets.Unmarshal`.
///
/// Returns `None` when the sync word is missing, the fields are reserved, or the
/// frame is truncated.
#[must_use]
pub fn parse_adts(frame: &[u8]) -> Option<AdtsFrame> {
    // The reader insists on the full 7-byte header plus the frame-count byte.
    if frame.len() < 8 {
        return None;
    }

    let sync_word = (u16::from(frame[0]) << 4) | (u16::from(frame[1]) >> 4);
    if sync_word != 0xFFF {
        return None;
    }

    let protection_absent = frame[1] & 0x01;
    let header_len = if protection_absent == 1 { 7 } else { 9 };
    if frame.len() < header_len {
        return None;
    }

    // ADTS profile (2 bits) maps to object type = profile + 1.
    let object_type = (frame[2] >> 6) + 1;
    let frequency_index = (frame[2] >> 2) & 0x0F;
    if frequency_index > 12 {
        return None;
    }
    let sample_rate = SAMPLE_RATES[usize::from(frequency_index)];

    let channel_configuration = ((frame[2] & 0x01) << 2) | ((frame[3] >> 6) & 0x03);
    let channels = match channel_configuration {
        1..=6 => channel_configuration,
        7 => 8,
        // 0 means the layout comes from a PCE, which is not supported here.
        _ => return None,
    };

    let total_len = (usize::from(frame[3] & 0x03) << 11)
        | (usize::from(frame[4]) << 3)
        | (usize::from(frame[5]) >> 5);
    let frame_len = total_len.checked_sub(header_len)?;
    if frame_len == 0 || frame.len() - header_len < frame_len {
        return None;
    }

    // Multiple raw data blocks per ADTS frame are not supported.
    if frame[6] & 0x03 != 0 {
        return None;
    }

    // AudioSpecificConfig: 5 bits object type, 4 bits frequency index, 4 bits
    // channel configuration, then the GASpecificConfig flags (all zero).
    let asc = (u16::from(object_type) << 11)
        | (u16::from(frequency_index) << 7)
        | (u16::from(channel_configuration) << 3);

    Some(AdtsFrame {
        header_len,
        frame_len,
        config: AudioConfig {
            sample_rate,
            channels,
            asc: asc.to_be_bytes().to_vec(),
        },
    })
}

/// Parse an `AudioSpecificConfig`: the record `esds` carries in an fMP4 init
/// segment, where ADTS would carry the same fields in every frame header.
///
/// Only the head of the record is read: object type, sampling frequency index
/// and channel configuration. An explicit SBR/PS extension (object type 5 or
/// 29) is reported at the extension's output rate, which is what a decoder
/// produces; the underlying object type is not modeled, exactly as for the
/// ADTS path. Returns `None` for a reserved frequency index, a layout that
/// only a program config element describes, or a truncated record.
#[must_use]
pub fn parse_asc(asc: &[u8]) -> Option<AudioConfig> {
    let first = u16::from(*asc.first()?);
    let second = u16::from(*asc.get(1)?);

    let object_type = first >> 3;
    // An explicit SBR/PS extension leads with its own rate, which is what a
    // decoder outputs; otherwise the base frequency index is the rate.
    let frequency_index = if object_type == 5 || object_type == 29 {
        (u16::from(*asc.get(2)?) >> 4) & 0x0F
    } else {
        ((first & 0x07) << 1) | (second >> 7)
    };
    if frequency_index > 12 {
        return None;
    }
    let sample_rate = SAMPLE_RATES[usize::from(frequency_index)];

    let channel_configuration = (second >> 3) & 0x0F;
    let channels = match channel_configuration {
        1..=6 => channel_configuration,
        7 => 8,
        // 0 means the layout comes from a PCE, which is not supported here.
        _ => return None,
    };

    Some(AudioConfig {
        sample_rate,
        channels: u8::try_from(channels).ok()?,
        asc: asc.to_vec(),
    })
}

/// Packetize one raw AAC access unit with the AAC-hbr AU header.
///
/// This is `writeAggregated` for a single access unit: a 16-bit
/// AU-headers-length (one 16-bit AU header) followed by the 13-bit size and a
/// 3-bit index of zero.
#[must_use]
pub fn packetize(raw: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(raw.len() + 4);
    // AU-headers-length: the AU header itself, in bits.
    packet.extend_from_slice(&[0x00, 0x10]);
    let size = u16::try_from(raw.len() & 0x1FFF).unwrap_or(0x1FFF);
    packet.extend_from_slice(&(size << 3).to_be_bytes());
    packet.extend_from_slice(raw);
    packet
}

/// Lowercase hex for the SDP `config=` value.
#[must_use]
pub fn config_hex(asc: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(asc.len() * 2);
    for byte in asc {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_audio_specific_config() {
        let config = parse_asc(&[0x11, 0x90]).expect("asc");
        assert_eq!(config.sample_rate, 48_000);
        assert_eq!(config.channels, 2);
        assert_eq!(config.asc, [0x11, 0x90]);
    }

    /// Object type 5 is explicit SBR: the extension rate is the output rate.
    #[test]
    fn reads_the_extension_rate_of_an_sbr_record() {
        let config = parse_asc(&[0x2B, 0x90, 0x30]).expect("asc");
        assert_eq!(config.sample_rate, 48_000);
    }

    #[test]
    fn rejects_a_reserved_audio_specific_config() {
        assert!(parse_asc(&[]).is_none());
        // Frequency index 15 means the rate follows explicitly, which this does not read.
        assert!(parse_asc(&[0x17, 0x90]).is_none());
    }

    #[allow(clippy::cast_possible_truncation)] // the test header is 7 bits wide
    fn adts_header(
        profile: u8,
        frequency_index: u8,
        channel_configuration: u8,
        total_len: usize,
    ) -> [u8; 7] {
        let mut header = [0u8; 7];
        header[0] = 0xFF;
        header[1] = 0xF1;
        header[2] = (profile << 6) | (frequency_index << 2) | (channel_configuration >> 2);
        header[3] = ((channel_configuration & 0x03) << 6) | ((total_len >> 11) as u8 & 0x03);
        header[4] = (total_len >> 3) as u8;
        header[5] = ((total_len as u8 & 0x07) << 5) | 0x1F;
        header[6] = 0xFC;
        header
    }

    #[test]
    fn parses_lc_stereo_48k() {
        let mut frame = adts_header(1, 3, 2, 7 + 100).to_vec();
        frame.resize(7 + 100, 0);
        let parsed = parse_adts(&frame).expect("adts");
        assert_eq!(parsed.header_len, 7);
        assert_eq!(parsed.frame_len, 100);
        assert_eq!(parsed.config.sample_rate, 48_000);
        assert_eq!(parsed.config.channels, 2);
        assert_eq!(parsed.config.asc, [0x11, 0x90]);
    }

    #[test]
    fn strips_the_header() {
        let header = adts_header(1, 3, 2, 7 + 4);
        let mut frame = header.to_vec();
        frame.extend_from_slice(b"aac!");

        let parsed = parse_adts(&frame).expect("adts");
        assert_eq!(parsed.payload(&frame), Some(&b"aac!"[..]));
    }

    #[test]
    fn rejects_missing_sync() {
        assert!(parse_adts(&[0; 8]).is_none());
    }

    #[test]
    fn rejects_a_truncated_frame() {
        let header = adts_header(1, 3, 2, 7 + 100);
        // Header says 100 payload bytes, only one is present.
        assert!(parse_adts(&header).is_none());
    }

    #[test]
    fn packetizes_with_an_au_header() {
        let packet = packetize(&[1, 2, 3, 4, 5]);
        assert_eq!(&packet[..2], &[0x00, 0x10]);
        // 13-bit size 5 in the first 13 bits of the AU header.
        assert_eq!(&packet[2..4], &[0x00, 0x28]);
        assert_eq!(&packet[4..], &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn config_hex_is_lowercase() {
        assert_eq!(config_hex(&[0x11, 0x90]), "1190");
    }
}
