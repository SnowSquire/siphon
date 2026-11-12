//! H.264 Annex B splitting, parameter-set tracking and RFC 6184 packetization.
//!
//! Ported from `mediacommon/pkg/codecs/h264` and `gortsplib/pkg/format/rtph264`
//! (both MIT). Packetization mode 1: single NAL, STAP-A and FU-A.

/// NAL unit type of an access unit delimiter.
const NAL_AUD: u8 = 9;
/// NAL unit type of a coded slice belonging to an IDR picture.
const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
/// Aggregation packets (RFC 6184 §5.7.1).
const NAL_STAP_A: u8 = 24;
/// Fragmentation Unit A (RFC 6184 §5.8).
const NAL_FU_A: u8 = 28;

/// The parameter sets needed to build an SDP and decode from a keyframe.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct H264Config {
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
}

impl H264Config {
    /// Record a parameter set found in the elementary stream.
    pub fn push(&mut self, nal: &[u8]) {
        let Some(&header) = nal.first() else {
            return;
        };
        match header & 0x1F {
            NAL_SPS => insert_unique(&mut self.sps, nal),
            NAL_PPS => insert_unique(&mut self.pps, nal),
            _ => {}
        }
    }

    /// Whether the SDP has everything it needs.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        !self.sps.is_empty() && !self.pps.is_empty()
    }

    /// `profile-level-id` value of the newest SPS.
    #[must_use]
    pub fn profile_level_id(&self) -> Option<[u8; 3]> {
        let sps = self.sps.last()?;
        (sps.len() >= 4).then(|| [sps[1], sps[2], sps[3]])
    }
}

fn insert_unique(sets: &mut Vec<Vec<u8>>, nal: &[u8]) {
    if !sets.iter().any(|existing| existing == nal) {
        sets.push(nal.to_vec());
    }
}

/// Split an Annex B byte stream into NAL unit ranges.
///
/// Mirrors mediaconvert's `AnnexB.Unmarshal`: an initial start code is required,
/// a single zero byte preceding a `00 00 01` delimiter is treated as part of a
/// four-byte start code, and everything else (including trailing
/// `cabac_zero_word` padding) stays in the NAL.
///
/// The ranges index the caller's buffer instead of copying each NAL out of it:
/// one access unit is ~13 NAL units, and copying them cost one more pass over
/// the whole elementary stream (plus an allocation per NAL).
#[must_use]
pub fn split_annex_b(buf: &[u8]) -> Option<Vec<(usize, usize)>> {
    let mut pos = if buf.len() >= 4 && buf[0..4] == [0, 0, 0, 1] {
        4
    } else if buf.len() >= 3 && buf[0..3] == [0, 0, 1] {
        3
    } else {
        return None;
    };

    if buf.len() == pos {
        return None;
    }

    let mut positions = Vec::new();
    while pos < buf.len() {
        match find_start_code(buf, pos) {
            None => {
                positions.push((pos, buf.len()));
                break;
            }
            Some(index) => {
                let end = if index > pos && buf[index - 1] == 0 {
                    index - 1
                } else {
                    index
                };
                if end > pos {
                    positions.push((pos, end));
                }
                pos = index + 3;
            }
        }
    }

    if positions.is_empty() {
        return None;
    }

    Some(positions)
}

/// Offset of the next `00 00 01` start code at or after `from`.
///
/// Every NAL boundary of the elementary stream is found by this scan, and H.264
/// slice data is dense with `00 00 03` emulation-prevention pairs, so a
/// byte-at-a-time search is the hottest loop in the demuxer. `memmem` searches
/// the three-byte delimiter with SIMD instead.
pub fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    let tail = data.get(from..)?;
    memchr::memmem::find(tail, &[0, 0, 1]).map(|offset| from + offset)
}

/// mediaconvert removes the access unit delimiter before packetization.
pub fn strip_aud(au: &[u8], nals: &mut Vec<(usize, usize)>) {
    let is_aud = nals
        .first()
        .is_some_and(|&(start, _)| au.get(start).is_some_and(|header| header & 0x1F == NAL_AUD));
    if is_aud {
        nals.remove(0);
    }
}

/// Whether an access unit (its NAL units) starts with an IDR picture.
#[must_use]
pub fn is_keyframe(au: &[u8], nals: &[(usize, usize)]) -> bool {
    nals.iter()
        .any(|&(start, _)| au.get(start).is_some_and(|header| header & 0x1F == NAL_IDR))
}

/// Packetize one access unit into RTP payloads (RFC 6184, packetization mode 1).
///
/// `emit` receives `(payload, last_packet_of_access_unit)`. The reference
/// implementation batches small NAL units into STAP-A packets and fragments a
/// large one into FU-A packets.
pub fn packetize(
    au: &[u8],
    nals: &[(usize, usize)],
    max_payload: usize,
    scratch: &mut Vec<u8>,
    mut emit: impl FnMut(&[u8], bool),
) {
    let mut batch: Vec<&[u8]> = Vec::with_capacity(4);
    // A STAP-A header byte plus (2-byte length + NALU) per member.
    let mut batch_len = 1usize;

    for &(start, end) in nals {
        let unit = &au[start..end];
        let addition = 2 + unit.len();
        if batch_len + addition <= max_payload {
            batch.push(unit);
            batch_len += addition;
        } else {
            if !batch.is_empty() {
                write_batch(&batch, false, max_payload, scratch, &mut emit);
            }
            batch.clear();
            batch.push(unit);
            batch_len = 1 + addition;
        }
    }

    if !batch.is_empty() {
        // The marker identifies the last packet of the access unit.
        write_batch(&batch, true, max_payload, scratch, &mut emit);
    }
}

fn write_batch(
    batch: &[&[u8]],
    marker: bool,
    max_payload: usize,
    scratch: &mut Vec<u8>,
    emit: &mut impl FnMut(&[u8], bool),
) {
    if batch.len() == 1 {
        let nalu = batch[0];
        if nalu.len() < max_payload {
            emit(nalu, marker);
        } else {
            write_fragmented(nalu, marker, max_payload, scratch, emit);
        }
        return;
    }

    // Single-time aggregation packet.
    scratch.clear();
    scratch.push(NAL_STAP_A);
    for nalu in batch {
        let len = u16::try_from(nalu.len()).unwrap_or(u16::MAX);
        scratch.extend_from_slice(&len.to_be_bytes());
        scratch.extend_from_slice(nalu);
    }
    emit(scratch, marker);
}

fn write_fragmented(
    nalu: &[u8],
    marker: bool,
    max_payload: usize,
    scratch: &mut Vec<u8>,
    emit: &mut impl FnMut(&[u8], bool),
) {
    let available = max_payload.saturating_sub(2).max(1);
    let nri = (nalu[0] >> 5) & 0x03;
    let fragment_type = nalu[0] & 0x1F;
    let payload = &nalu[1..];
    let mut offset = 0;
    let mut start = true;

    while offset < payload.len() {
        let end = (offset + available).min(payload.len());
        let last = end == payload.len();

        let mut fu_header = fragment_type;
        if start {
            fu_header |= 0x80;
        }
        if last {
            fu_header |= 0x40;
        }

        scratch.clear();
        // FU indicator: F and NRI from the original header, type 28.
        scratch.push((nri << 5) | NAL_FU_A);
        scratch.push(fu_header);
        scratch.extend_from_slice(&payload[offset..end]);
        emit(scratch, marker && last);

        start = false;
        offset = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The NAL ranges as byte slices, for readable assertions.
    fn units<'a>(au: &'a [u8], nals: &[(usize, usize)]) -> Vec<&'a [u8]> {
        nals.iter().map(|&(start, end)| &au[start..end]).collect()
    }

    #[test]
    fn splits_start_codes_like_mediaconvert() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 4, 5, 0,
        ];
        let nals = split_annex_b(&data).expect("annex b");
        assert_eq!(nals.len(), 3);
        assert_eq!(units(&data, &nals)[0], [0x67, 1, 2]);
        assert_eq!(units(&data, &nals)[1], [0x68, 3]);
        // The final NAL keeps its trailing zero: it can be cabac_zero_word.
        assert_eq!(units(&data, &nals)[2], [0x65, 4, 5, 0]);
    }

    #[test]
    fn requires_an_initial_start_code() {
        assert!(split_annex_b(&[0x67, 1, 2]).is_none());
        assert!(split_annex_b(&[0, 0, 1]).is_none());
    }

    #[test]
    fn removes_the_access_unit_delimiter() {
        let au = [0, 0, 1, 0x09, 0xF0, 0, 0, 1, 0x65, 1];
        let mut nals = split_annex_b(&au).expect("annex b");
        assert_eq!(nals.len(), 2);
        strip_aud(&au, &mut nals);
        assert_eq!(units(&au, &nals), vec![&[0x65, 1][..]]);
    }

    #[test]
    fn tracks_sps_and_pps() {
        let mut config = H264Config::default();
        config.push(&[0x67, 0x64, 0x00, 0x2A, 0xAC]);
        config.push(&[0x68, 0xEE, 0x3C]);
        config.push(&[0x68, 0xEE, 0x3C]); // duplicate is ignored
        assert!(config.is_ready());
        assert_eq!(config.sps.len(), 1);
        assert_eq!(config.pps.len(), 1);
        assert_eq!(config.profile_level_id(), Some([0x64, 0x00, 0x2A]));
    }

    #[test]
    fn detects_idr() {
        let key = [0x67, 1, 0x65, 2];
        let delta = [0x41, 2];
        assert!(is_keyframe(&key, &[(0, 2), (2, 4)]));
        assert!(!is_keyframe(&delta, &[(0, 2)]));
    }

    #[test]
    fn small_nals_are_aggregated() {
        let mut scratch = Vec::new();
        let mut packets = Vec::new();
        let au = [0x67, 1, 2, 0x68, 3];
        packetize(
            &au,
            &[(0, 3), (3, 5)],
            1200,
            &mut scratch,
            |payload, marker| {
                packets.push((payload.to_vec(), marker));
            },
        );
        assert_eq!(packets.len(), 1);
        assert!(packets[0].1);
        assert_eq!(packets[0].0, [24, 0, 3, 0x67, 1, 2, 0, 2, 0x68, 3]);
    }

    #[test]
    fn a_large_nal_is_fragmented() {
        let au = [0x65u8, 1, 2, 3, 4, 5, 6];
        let mut scratch = Vec::new();
        let mut packets = Vec::new();
        packetize(&au, &[(0, au.len())], 4, &mut scratch, |payload, marker| {
            packets.push((payload.to_vec(), marker));
        });
        assert_eq!(packets.len(), 3);
        assert_eq!(packets[0].0, [0x7C, 0x85, 1, 2]);
        assert_eq!(packets[1].0, [0x7C, 0x05, 3, 4]);
        assert_eq!(packets[2].0, [0x7C, 0x45, 5, 6]);
        assert!(packets[2].1);
    }

    #[test]
    fn only_the_last_batch_carries_the_marker() {
        let mut scratch = Vec::new();
        let mut packets = Vec::new();
        let au = [[0x41; 10], [0x65; 10]].concat();
        // Two 10-byte NALs do not fit in one 20-byte packet, so each is emitted
        // separately; the marker must only be on the last one.
        packetize(
            &au,
            &[(0, 10), (10, 20)],
            20,
            &mut scratch,
            |payload, marker| {
                packets.push((payload[0] & 0x1F, marker));
            },
        );
        assert_eq!(packets, [(1, false), (5, true)]);
    }
}
