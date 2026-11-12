//! MPEG-TS demuxer for the Twitch H.264 + AAC source.
//!
//! Everything the RTSP output needs is in the elementary stream: PAT/PMT give
//! the PIDs, PES headers carry the 90 kHz PTS/DTS, H.264 arrives as Annex B and
//! AAC as ADTS. The demuxer is fed the raw bytes of each HLS segment and emits
//! one [`Sample`] per PES access unit.

use crate::rtsp::aac;
use crate::rtsp::aac::AudioConfig;
use crate::rtsp::h264;
use crate::rtsp::h264::H264Config;

const TS_PACKET: usize = 188;
const TS_SYNC: u8 = 0x47;
const PID_PAT: u16 = 0x0000;
const STREAM_TYPE_H264: u8 = 0x1B;
const STREAM_TYPE_AAC: u8 = 0x0F;
/// PTS/DTS wrap at 2^33.
const PTS_WRAP: i64 = 1 << 33;

/// One demuxed access unit.
#[derive(Debug)]
pub enum Sample {
    Video(VideoSample),
    Audio(AudioSample),
}

#[derive(Debug)]
pub struct VideoSample {
    /// Presentation timestamp in 90 kHz units.
    pub pts: i64,
    /// Decode timestamp in 90 kHz units (equals `pts` when no reordering).
    pub dts: i64,
    /// The access unit's Annex B bytes (access unit delimiter included).
    ///
    /// One buffer with [`VideoSample::nals`] ranges into it, instead of a `Vec`
    /// per NAL: copying the units out cost a second pass over the whole
    /// elementary stream and thirteen allocations per access unit.
    pub data: Vec<u8>,
    /// NAL units of the access unit, as ranges into [`VideoSample::data`].
    pub nals: Vec<(usize, usize)>,
    pub keyframe: bool,
}

#[derive(Debug)]
pub struct AudioSample {
    /// Presentation timestamp of the first frame, in 90 kHz units.
    pub pts: i64,
    /// Raw AAC frame without the ADTS header.
    pub frame: Vec<u8>,
}

/// Raw PES bytes for one packet, header included.
///
/// The header is parsed only when the packet is flushed: a PES header can be
/// split across two TS packets (astits handles the same case).
#[derive(Default)]
struct PesBuffer {
    active: bool,
    data: Vec<u8>,
}

impl PesBuffer {
    fn start(&mut self, data: &[u8]) {
        self.active = true;
        self.data.clear();
        self.data.extend_from_slice(data);
    }

    fn append(&mut self, data: &[u8]) {
        self.data.extend_from_slice(data);
    }

    /// Take the completed PES payload as `(pts, dts, payload_start, bytes)`.
    ///
    /// Handing the buffer out instead of draining the header off it saves a
    /// memmove of every payload byte, and [`PesBuffer::recycle`] puts the
    /// allocation back so a stream allocates once instead of once per PES.
    fn flush(&mut self) -> Option<(i64, i64, usize, Vec<u8>)> {
        if !self.active {
            return None;
        }
        self.active = false;

        let mut data = std::mem::take(&mut self.data);
        let (pts, dts, header_end, declared) = parse_pes_header(&data)?;

        // PES_packet_length counts everything after the length field, so the
        // whole packet ends at `6 + declared`; zero means unbounded (video).
        if declared != 0 {
            data.truncate((6 + declared).min(data.len()));
        }
        if data.len() <= header_end {
            self.recycle(data);
            return None;
        }

        Some((pts, dts, header_end, data))
    }

    /// Return a flushed buffer so the next PES reuses its allocation.
    fn recycle(&mut self, mut data: Vec<u8>) {
        data.clear();
        self.data = data;
    }

    fn reset(&mut self) {
        self.active = false;
        self.data.clear();
    }
}

pub struct TsDemuxer {
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    audio_pid: Option<u16>,
    video: PesBuffer,
    audio: PesBuffer,
    /// Video access unit under assembly.
    ///
    /// Twitch's muxer does not align PES boundaries with access-unit boundaries:
    /// a PES can begin with the tail of the previous access unit, followed by
    /// the next access unit's AUD. The elementary stream is therefore a
    /// continuous Annex B byte stream, split at access-unit delimiters.
    video_au: Vec<u8>,
    video_au_active: bool,
    video_au_pts: i64,
    video_au_dts: i64,
    /// Bytes of a TS packet split across two feed calls.
    carry: Vec<u8>,
    video_config: H264Config,
    audio_config: Option<AudioConfig>,
}

impl Default for TsDemuxer {
    fn default() -> Self {
        Self::new()
    }
}

impl TsDemuxer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pmt_pid: None,
            video_pid: None,
            audio_pid: None,
            video: PesBuffer::default(),
            audio: PesBuffer::default(),
            video_au: Vec::new(),
            video_au_active: false,
            video_au_pts: 0,
            video_au_dts: 0,
            carry: Vec::with_capacity(TS_PACKET),
            video_config: H264Config::default(),
            audio_config: None,
        }
    }

    /// H.264 parameter sets seen so far.
    #[must_use]
    pub const fn video_config(&self) -> &H264Config {
        &self.video_config
    }

    /// AAC configuration implied by the first ADTS header.
    #[must_use]
    pub const fn audio_config(&self) -> Option<&AudioConfig> {
        self.audio_config.as_ref()
    }

    /// Forget codec-specific state after a discontinuity. The PMT is kept: the
    /// PIDs of one broadcast do not change across a dropout.
    pub fn reset(&mut self) {
        self.video.reset();
        self.audio.reset();
        self.video_au.clear();
        self.video_au_active = false;
        self.carry.clear();
    }

    /// Consume segment bytes, appending completed samples to `out`.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Sample>) {
        let mut data = bytes;

        if !self.carry.is_empty() {
            let needed = TS_PACKET - self.carry.len();
            let take = needed.min(data.len());
            self.carry.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.carry.len() == TS_PACKET {
                let mut packet = [0u8; TS_PACKET];
                packet.copy_from_slice(&self.carry);
                self.carry.clear();
                self.packet(&packet, out);
            }
        }

        while data.len() >= TS_PACKET {
            self.packet(&data[..TS_PACKET], out);
            data = &data[TS_PACKET..];
        }

        self.carry.extend_from_slice(data);
    }

    /// Flush the PES packets and the access unit left open at the end of a
    /// segment. HLS segment boundaries are access-unit boundaries, so the last
    /// AU is complete here.
    pub fn finish_segment(&mut self, out: &mut Vec<Sample>) {
        self.flush_video_pes(out);
        self.flush_audio(out);
        self.flush_video_au(out);
    }

    fn packet(&mut self, packet: &[u8], out: &mut Vec<Sample>) {
        debug_assert_eq!(packet.len(), TS_PACKET);
        if packet[0] != TS_SYNC {
            return;
        }

        let pid = (u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2]);
        let payload_unit_start = packet[1] & 0x40 != 0;
        let adaptation = (packet[3] >> 4) & 0x03;
        let mut offset = 4;

        if adaptation & 0x02 != 0 {
            let length = usize::from(packet[offset]);
            offset += 1 + length;
            if offset >= TS_PACKET {
                return;
            }
        }
        if adaptation & 0x01 == 0 {
            return;
        }
        let payload = &packet[offset..];

        if pid == PID_PAT {
            self.parse_pat(payload);
            return;
        }
        if Some(pid) == self.pmt_pid {
            self.parse_pmt(payload);
            return;
        }

        let kind = if Some(pid) == self.video_pid {
            Kind::Video
        } else if Some(pid) == self.audio_pid {
            Kind::Audio
        } else {
            return;
        };

        if payload_unit_start {
            match kind {
                Kind::Video => self.flush_video_pes(out),
                Kind::Audio => self.flush_audio(out),
            }
            match kind {
                Kind::Video => self.video.start(payload),
                Kind::Audio => self.audio.start(payload),
            }
        } else {
            let buffer = match kind {
                Kind::Video => &mut self.video,
                Kind::Audio => &mut self.audio,
            };
            if buffer.active {
                buffer.append(payload);
            }
        }
    }

    fn parse_pat(&mut self, payload: &[u8]) {
        let Some(section) = psi_section(payload) else {
            return;
        };
        if section.first() != Some(&0x00) {
            return;
        }
        // transport_stream_id, version/current, section_number, last_section.
        let Some(body) = section.get(8..) else {
            return;
        };

        // program_number (2) + network/program PID (2) per entry.
        for entry in body.as_chunks::<4>().0 {
            let program = u16::from_be_bytes([entry[0], entry[1]]);
            if program != 0 {
                self.pmt_pid = Some((u16::from(entry[2] & 0x1F) << 8) | u16::from(entry[3]));
                return;
            }
        }
    }

    fn parse_pmt(&mut self, payload: &[u8]) {
        let Some(section) = psi_section(payload) else {
            return;
        };
        if section.first() != Some(&0x02) || section.len() < 12 {
            return;
        }

        let program_info_length = (usize::from(section[10] & 0x0F) << 8) | usize::from(section[11]);
        let Some(streams) = section.get(12 + program_info_length..) else {
            return;
        };

        let mut offset = 0;
        while offset + 5 <= streams.len() {
            let stream_type = streams[offset];
            let pid = (u16::from(streams[offset + 1] & 0x1F) << 8) | u16::from(streams[offset + 2]);
            let info_length =
                (usize::from(streams[offset + 3] & 0x0F) << 8) | usize::from(streams[offset + 4]);

            match stream_type {
                STREAM_TYPE_H264 if self.video_pid != Some(pid) => {
                    self.video_pid = Some(pid);
                    self.video.reset();
                }
                STREAM_TYPE_AAC if self.audio_pid != Some(pid) => {
                    self.audio_pid = Some(pid);
                    self.audio.reset();
                }
                _ => {}
            }

            offset += 5 + info_length;
        }
    }

    fn flush_video_pes(&mut self, out: &mut Vec<Sample>) {
        let Some((pts, dts, header_end, data)) = self.video.flush() else {
            return;
        };
        let payload = &data[header_end..];

        if let Some(offset) = find_aud(payload) {
            // Bytes before the AUD finish the previous access unit.
            if self.video_au_active {
                self.video_au.extend_from_slice(&payload[..offset]);
                self.flush_video_au(out);
            }
            self.video_au.clear();
            self.video_au.extend_from_slice(&payload[offset..]);
            self.video_au_pts = pts;
            self.video_au_dts = dts;
            self.video_au_active = true;
        } else {
            if !self.video_au_active {
                self.video_au_pts = pts;
                self.video_au_dts = dts;
                self.video_au_active = true;
            }
            self.video_au.extend_from_slice(payload);
        }

        self.video.recycle(data);
    }

    fn flush_video_au(&mut self, out: &mut Vec<Sample>) {
        if !self.video_au_active {
            return;
        }
        self.video_au_active = false;

        let data = std::mem::take(&mut self.video_au);
        let Some(mut nals) = h264::split_annex_b(&data) else {
            return;
        };

        let keyframe = h264::is_keyframe(&data, &nals);
        for &(start, end) in &nals {
            self.video_config.push(&data[start..end]);
        }

        // mediaconvert drops the access unit delimiter before packetization.
        h264::strip_aud(&data, &mut nals);
        if nals.is_empty() {
            return;
        }

        out.push(Sample::Video(VideoSample {
            pts: self.video_au_pts,
            dts: self.video_au_dts,
            data,
            nals,
            keyframe,
        }));
    }

    fn flush_audio(&mut self, out: &mut Vec<Sample>) {
        let Some((pts, _, header_end, data)) = self.audio.flush() else {
            return;
        };
        let payload = &data[header_end..];

        let mut offset = 0;
        let mut index = 0i64;
        while offset + 7 <= payload.len() {
            let Some(frame) = aac::parse_adts(&payload[offset..]) else {
                break;
            };
            let Some(raw) = frame.payload(&payload[offset..]) else {
                break;
            };

            if self.audio_config.as_ref() != Some(&frame.config) {
                self.audio_config = Some(frame.config.clone());
            }

            // One AAC frame is 1024 samples; ADTS input always uses short frames.
            let ticks = 1024 * 90_000 / i64::from(frame.config.sample_rate);
            out.push(Sample::Audio(AudioSample {
                pts: pts + index * ticks,
                frame: raw.to_vec(),
            }));

            index += 1;
            offset += frame.total_len();
        }

        self.audio.recycle(data);
    }
}

enum Kind {
    Video,
    Audio,
}

/// Pointer field plus a complete PSI section, with the CRC trimmed off.
fn psi_section(payload: &[u8]) -> Option<&[u8]> {
    let pointer = usize::from(*payload.first()?);
    let section = payload.get(1 + pointer..)?;
    if section.len() < 3 {
        return None;
    }
    let length = (usize::from(section[1] & 0x0F) << 8) | usize::from(section[2]);
    let end = 3 + length;
    if end < 8 {
        return None;
    }
    // Drop the 4-byte CRC.
    section.get(..end - 4)
}

/// Offset of the next access unit delimiter NAL in `data`.
///
/// A four-byte start code's leading zero is included in the boundary so the
/// previous access unit keeps its (harmless) `cabac_zero_word`.
fn find_aud(data: &[u8]) -> Option<usize> {
    let mut from = 0;
    while let Some(at) = h264::find_start_code(data, from) {
        if data.get(at + 3).is_some_and(|&header| header & 0x1F == 9) {
            return Some(if at > 0 && data[at - 1] == 0 {
                at - 1
            } else {
                at
            });
        }
        from = at + 3;
    }
    None
}

/// Parse a PES header, returning `(pts, dts, header_end, declared_length)`.
fn parse_pes_header(data: &[u8]) -> Option<(i64, i64, usize, usize)> {
    if data.len() < 9 || data[0] != 0 || data[1] != 0 || data[2] != 1 {
        return None;
    }

    let declared = (usize::from(data[4]) << 8) | usize::from(data[5]);
    let flags = data[7];
    let header_end = 9 + usize::from(data[8]);

    // A PES without a PTS cannot be timed, so it is dropped rather than guessed.
    if flags & 0x80 == 0 {
        return None;
    }

    let pts = unwrap_pts(parse_timestamp(data.get(9..14)?));
    let dts = if flags & 0x40 != 0 {
        unwrap_pts(parse_timestamp(data.get(14..19)?))
    } else {
        pts
    };

    Some((pts, dts, header_end, declared))
}

fn parse_timestamp(bytes: &[u8]) -> i64 {
    (i64::from(bytes[0] & 0x0E) << 29)
        | (i64::from(bytes[1]) << 22)
        | (i64::from(bytes[2] & 0xFE) << 14)
        | (i64::from(bytes[3]) << 7)
        | (i64::from(bytes[4] & 0xFE) >> 1)
}

/// Keep the PTS/DTS in `0..PTS_WRAP` so arithmetic never observes the wrap
/// marker bits.
const fn unwrap_pts(pts: i64) -> i64 {
    pts.rem_euclid(PTS_WRAP)
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
mod tests {
    use super::*;

    /// The NAL units of a sample as byte slices.
    fn nals_of(sample: &VideoSample) -> Vec<&[u8]> {
        sample
            .nals
            .iter()
            .map(|&(start, end)| &sample.data[start..end])
            .collect()
    }

    fn packet(pid: u16, payload_unit_start: bool, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0xFFu8; TS_PACKET];
        packet[0] = TS_SYNC;
        packet[1] = (if payload_unit_start { 0x40 } else { 0 }) | ((pid >> 8) as u8 & 0x1F);
        packet[2] = pid as u8;
        packet[3] = 0x10; // payload only
        packet[4..4 + payload.len()].copy_from_slice(payload);
        packet
    }

    fn pat_section() -> Vec<u8> {
        let mut section = vec![0u8; 17];
        section[0] = 0; // pointer field
        section[1] = 0x00; // table_id
        section[2] = 0xB0; // section_syntax_indicator + length high
        section[3] = 0x0D; // 13 bytes after the length field
        section[9] = 0x00;
        section[10] = 0x01; // program 1
        section[11] = 0xF0;
        section[12] = 0x00; // PMT PID 0x1000
        section
    }

    fn pmt_section() -> Vec<u8> {
        let mut section = vec![0u8; 27];
        section[0] = 0; // pointer field
        section[1] = 0x02; // table_id
        section[2] = 0xB0;
        section[3] = 0x17; // 23 bytes after the length field
        section[4] = 0x00;
        section[5] = 0x01; // program number
        section[9] = 0xE0;
        section[10] = 0x00; // PCR PID
        section[11] = 0xF0;
        section[12] = 0x00; // program info length
        section[13] = STREAM_TYPE_H264;
        section[14] = 0xE1;
        section[15] = 0x01; // video PID 0x101
        section[16] = 0xF0;
        section[17] = 0x00;
        section[18] = STREAM_TYPE_AAC;
        section[19] = 0xE1;
        section[20] = 0x00; // audio PID 0x100
        section[21] = 0xF0;
        section[22] = 0x00;
        section
    }

    #[test]
    fn parses_pat_and_pmt() {
        let mut demuxer = TsDemuxer::new();
        demuxer.packet(&packet(PID_PAT, true, &pat_section()), &mut Vec::new());
        let pmt_pid = demuxer.pmt_pid.expect("pmt pid");
        demuxer.packet(&packet(pmt_pid, true, &pmt_section()), &mut Vec::new());

        assert_eq!(demuxer.video_pid, Some(0x101));
        assert_eq!(demuxer.audio_pid, Some(0x100));
    }

    fn encode_pts(pts: i64) -> [u8; 5] {
        [
            0x21 | ((pts >> 29) as u8 & 0x0E),
            (pts >> 22) as u8,
            ((pts >> 14) as u8 & 0xFE) | 0x01,
            (pts >> 7) as u8,
            ((pts << 1) as u8 & 0xFE) | 0x01,
        ]
    }

    fn video_pes(pts: i64, payload: &[u8]) -> Vec<u8> {
        let mut pes = vec![0, 0, 1, 0xE0, 0, 0, 0x80, 0x80, 5];
        // The declared length must cover the three flag/header bytes, the PTS
        // and the payload, so the padded TS packet is trimmed back to the PES.
        let declared = 3 + 5 + payload.len();
        pes[4] = (declared >> 8) as u8;
        pes[5] = declared as u8;
        pes.extend_from_slice(&encode_pts(pts));
        pes.extend_from_slice(payload);
        pes
    }

    fn video_pid(demuxer: &mut TsDemuxer) -> u16 {
        demuxer.packet(&packet(PID_PAT, true, &pat_section()), &mut Vec::new());
        let pmt_pid = demuxer.pmt_pid.expect("pmt pid");
        demuxer.packet(&packet(pmt_pid, true, &pmt_section()), &mut Vec::new());
        demuxer.video_pid.expect("video pid")
    }

    /// Twitch's muxer does not align PES boundaries with access units: a PES
    /// can start with the tail of the previous AU, before the next AU's AUD.
    #[test]
    fn keeps_a_tail_that_precedes_the_next_access_unit() {
        let mut demuxer = TsDemuxer::new();
        let pid = video_pid(&mut demuxer);

        let mut first = vec![0, 0, 0, 1, 0x09, 0xF0]; // AUD
        first.extend_from_slice(&[0, 0, 0, 1, 0x65, 1, 2]); // IDR

        let mut second = vec![0xAA, 0xBB]; // tail of the previous AU
        second.extend_from_slice(&[0, 0, 0, 1, 0x09, 0xF0]); // next AUD
        second.extend_from_slice(&[0, 0, 0, 1, 0x41, 3, 4]); // P slice

        let mut samples = Vec::new();
        demuxer.packet(&packet(pid, true, &video_pes(90_000, &first)), &mut samples);
        demuxer.packet(
            &packet(pid, true, &video_pes(91_000, &second)),
            &mut samples,
        );
        demuxer.finish_segment(&mut samples);

        assert_eq!(samples.len(), 2);
        let Sample::Video(a) = &samples[0] else {
            panic!("first sample is not video");
        };
        let Sample::Video(b) = &samples[1] else {
            panic!("second sample is not video");
        };
        assert_eq!(a.pts, 90_000);
        assert_eq!(nals_of(a), [&[0x65, 1, 2, 0xAA, 0xBB][..]]);
        assert_eq!(b.pts, 91_000);
        assert_eq!(nals_of(b), [&[0x41, 3, 4][..]]);
    }

    #[test]
    fn decodes_a_pes_timestamp() {
        let pts = 0x0001_F000_i64;
        let mut data = vec![0u8; 14];
        data[0..3].copy_from_slice(&[0, 0, 1]);
        data[3] = 0xE0;
        data[7] = 0x80;
        data[8] = 5;
        data[9] = 0x21 | ((pts >> 29) as u8 & 0x0E);
        data[10] = (pts >> 22) as u8;
        data[11] = ((pts >> 14) as u8 & 0xFE) | 0x01;
        data[12] = (pts >> 7) as u8;
        data[13] = ((pts << 1) as u8 & 0xFE) | 0x01;

        let (parsed, dts, header_end, declared) = parse_pes_header(&data).expect("pes");
        assert_eq!(parsed, pts);
        assert_eq!(dts, pts);
        assert_eq!(header_end, 14);
        assert_eq!(declared, 0);
    }
}
