//! Index v4: the on-disk record format. `index.hexpat` mirrors it.

use std::io::Read;

use super::DiskOutputError;

pub(super) const INDEX_MAGIC: [u8; 8] = *b"OZENIKEN";
pub(super) const INDEX_VERSION: u32 = 4;
pub(super) const HEADER_LEN: usize = 8 + 4 + 8;

pub(super) const TAG_SEGMENT: u8 = 0;
pub(super) const TAG_INIT: u8 = 1;
pub(super) const TAG_DISCONTINUITY: u8 = 2;
pub(super) const TAG_RUN: u8 = 3;

pub(super) const SEGMENT_BODY: u32 = 1 + 8 + 4 + 8 + 4 + 8 + 4 + 16 + 1;
pub(super) const INIT_BODY: u32 = 1 + 8 + 8 + 4 + 16;
pub(super) const DISCONTINUITY_BODY: u32 = 2 + 2 + 2 + 2;
pub(super) const RUN_BODY: u32 = 4 + 8 + 8 + 8 + 8 + 4 + 1;

/// Offset of the `complete` byte inside a serialized `Run` record.
pub(super) const RUN_COMPLETE_OFFSET: u64 = 1 + 4 + 8 + 8 + 8 + 8 + 4;

pub(super) const SOURCE_LIVE: u8 = 0;
pub(super) const SOURCE_DVR: u8 = 1;

// Index v4 records

/// Sanity cap for a `Run` marker's reserved record area. A torn marker cannot
/// make the resume path reserve an unbounded index range.
pub(super) const MAX_RUN_RECORDS: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SegmentRecord {
    pub(super) source: u8,
    pub(super) sequence: u64,
    pub(super) elapsed_ms: u32,
    pub(super) pdt_ms: u64,
    pub(super) duration_ms: u32,
    pub(super) byte_offset: u64,
    pub(super) byte_length: u32,
    pub(super) hash: u128,
    pub(super) flags: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct InitRecord {
    pub(super) source: u8,
    pub(super) counter: u64,
    pub(super) byte_offset: u64,
    pub(super) byte_length: u32,
    pub(super) hash: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DiscontinuityRecord {
    pub(super) width: u16,
    pub(super) height: u16,
    pub(super) rate_num: u16,
    pub(super) rate_denom: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RunRecord {
    pub(super) run: u32,
    pub(super) dst_segs_offset: u64,
    pub(super) segs_len: u64,
    pub(super) source_segs_offset: u64,
    pub(super) source_index_offset: u64,
    pub(super) record_len: u32,
    /// 0 while the copy is pending, 1 when it is done (or was abandoned).
    pub(super) complete: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Record {
    Segment(SegmentRecord),
    Init(InitRecord),
    Discontinuity(DiscontinuityRecord),
    Run(RunRecord),
}

impl Record {
    pub(super) const fn tag(&self) -> u8 {
        match self {
            Self::Segment(_) => TAG_SEGMENT,
            Self::Init(_) => TAG_INIT,
            Self::Discontinuity(_) => TAG_DISCONTINUITY,
            Self::Run(_) => TAG_RUN,
        }
    }

    pub(super) const fn encoded_len(&self) -> u32 {
        match self {
            Self::Segment(_) => 1 + SEGMENT_BODY,
            Self::Init(_) => 1 + INIT_BODY,
            Self::Discontinuity(_) => 1 + DISCONTINUITY_BODY,
            Self::Run(_) => 1 + RUN_BODY,
        }
    }

    /// Bytes the record occupies in the index, including a `Run`'s reserved
    /// record area.
    pub(super) fn span(&self) -> u64 {
        match self {
            Self::Run(run) => u64::from(1 + RUN_BODY) + u64::from(run.record_len),
            other => u64::from(other.encoded_len()),
        }
    }

    pub(super) fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.tag());
        match self {
            Self::Segment(segment) => {
                out.push(segment.source);
                out.extend_from_slice(&segment.sequence.to_le_bytes());
                out.extend_from_slice(&segment.elapsed_ms.to_le_bytes());
                out.extend_from_slice(&segment.pdt_ms.to_le_bytes());
                out.extend_from_slice(&segment.duration_ms.to_le_bytes());
                out.extend_from_slice(&segment.byte_offset.to_le_bytes());
                out.extend_from_slice(&segment.byte_length.to_le_bytes());
                out.extend_from_slice(&segment.hash.to_le_bytes());
                out.push(segment.flags);
            }
            Self::Init(init) => {
                out.push(init.source);
                out.extend_from_slice(&init.counter.to_le_bytes());
                out.extend_from_slice(&init.byte_offset.to_le_bytes());
                out.extend_from_slice(&init.byte_length.to_le_bytes());
                out.extend_from_slice(&init.hash.to_le_bytes());
            }
            Self::Discontinuity(discontinuity) => {
                out.extend_from_slice(&discontinuity.width.to_le_bytes());
                out.extend_from_slice(&discontinuity.height.to_le_bytes());
                out.extend_from_slice(&discontinuity.rate_num.to_le_bytes());
                out.extend_from_slice(&discontinuity.rate_denom.to_le_bytes());
            }
            Self::Run(run) => {
                out.extend_from_slice(&run.run.to_le_bytes());
                out.extend_from_slice(&run.dst_segs_offset.to_le_bytes());
                out.extend_from_slice(&run.segs_len.to_le_bytes());
                out.extend_from_slice(&run.source_segs_offset.to_le_bytes());
                out.extend_from_slice(&run.source_index_offset.to_le_bytes());
                out.extend_from_slice(&run.record_len.to_le_bytes());
                out.push(run.complete);
            }
        }
    }

    pub(super) fn decode<R: Read>(tag: u8, reader: &mut R) -> std::io::Result<Self> {
        Ok(match tag {
            TAG_SEGMENT => Self::Segment(SegmentRecord {
                source: read_u8(reader)?,
                sequence: read_u64(reader)?,
                elapsed_ms: read_u32(reader)?,
                pdt_ms: read_u64(reader)?,
                duration_ms: read_u32(reader)?,
                byte_offset: read_u64(reader)?,
                byte_length: read_u32(reader)?,
                hash: read_u128(reader)?,
                flags: read_u8(reader)?,
            }),
            TAG_INIT => Self::Init(InitRecord {
                source: read_u8(reader)?,
                counter: read_u64(reader)?,
                byte_offset: read_u64(reader)?,
                byte_length: read_u32(reader)?,
                hash: read_u128(reader)?,
            }),
            TAG_DISCONTINUITY => Self::Discontinuity(DiscontinuityRecord {
                width: read_u16(reader)?,
                height: read_u16(reader)?,
                rate_num: read_u16(reader)?,
                rate_denom: read_u16(reader)?,
            }),
            TAG_RUN => Self::Run(RunRecord {
                run: read_u32(reader)?,
                dst_segs_offset: read_u64(reader)?,
                segs_len: read_u64(reader)?,
                source_segs_offset: read_u64(reader)?,
                source_index_offset: read_u64(reader)?,
                record_len: read_u32(reader)?,
                complete: read_u8(reader)?,
            }),
            tag => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown record tag {tag}"),
                ));
            }
        })
    }

    /// Hash-covered byte range, for resume integrity checks.
    pub(super) const fn hashed_range(&self) -> Option<(u64, u32, u128)> {
        match self {
            Self::Segment(segment) => {
                Some((segment.byte_offset, segment.byte_length, segment.hash))
            }
            Self::Init(init) => Some((init.byte_offset, init.byte_length, init.hash)),
            Self::Discontinuity(_) | Self::Run(_) => None,
        }
    }

    /// Rebase a run's record into the canonical file.
    pub(super) fn rebase(&mut self, delta: i64, init_counter: &mut u64) {
        match self {
            Self::Segment(segment) => {
                segment.byte_offset = add_signed(segment.byte_offset, delta);
            }
            Self::Init(init) => {
                init.byte_offset = add_signed(init.byte_offset, delta);
                init.counter = *init_counter;
                *init_counter += 1;
            }
            Self::Discontinuity(_) | Self::Run(_) => {}
        }
    }
}

pub(super) fn add_signed(value: u64, delta: i64) -> u64 {
    i64::try_from(value)
        .ok()
        .and_then(|value| value.checked_add(delta))
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or(value)
}

pub(super) fn read_u8<R: Read>(reader: &mut R) -> std::io::Result<u8> {
    let mut bytes = [0u8; 1];
    reader.read_exact(&mut bytes)?;
    Ok(bytes[0])
}

pub(super) fn read_u16<R: Read>(reader: &mut R) -> std::io::Result<u16> {
    let mut bytes = [0u8; 2];
    reader.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

pub(super) fn read_u32<R: Read>(reader: &mut R) -> std::io::Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

pub(super) fn read_u64<R: Read>(reader: &mut R) -> std::io::Result<u64> {
    let mut bytes = [0u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

pub(super) fn read_u128<R: Read>(reader: &mut R) -> std::io::Result<u128> {
    let mut bytes = [0u8; 16];
    reader.read_exact(&mut bytes)?;
    Ok(u128::from_le_bytes(bytes))
}

pub(super) const fn body_len(tag: u8) -> Option<u32> {
    match tag {
        TAG_SEGMENT => Some(SEGMENT_BODY),
        TAG_INIT => Some(INIT_BODY),
        TAG_DISCONTINUITY => Some(DISCONTINUITY_BODY),
        TAG_RUN => Some(RUN_BODY),
        _ => None,
    }
}

pub(super) fn encode_header(stream_id: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN);
    out.extend_from_slice(&INDEX_MAGIC);
    out.extend_from_slice(&INDEX_VERSION.to_le_bytes());
    out.extend_from_slice(&stream_id.to_le_bytes());
    out
}

pub(super) fn parse_header(bytes: &[u8]) -> Result<u64, DiskOutputError> {
    if bytes.len() < HEADER_LEN {
        return Err(DiskOutputError::InvalidHeader);
    }
    if bytes[..8] != INDEX_MAGIC {
        return Err(DiskOutputError::InvalidHeader);
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let stream_id = u64::from_le_bytes(bytes[12..20].try_into().unwrap());
    if version != INDEX_VERSION {
        return Err(DiskOutputError::InvalidHeader);
    }
    Ok(stream_id)
}

pub(super) struct ParsedRecord {
    pub(super) offset: u64,
    pub(super) record: Record,
    pub(super) span: u64,
}

/// Structurally parse the index. A truncated tail is not an error: the caller
/// truncates at the end of the last complete record.
pub(super) fn parse_records(bytes: &[u8]) -> Result<Vec<ParsedRecord>, DiskOutputError> {
    let mut records = Vec::new();
    let mut position = HEADER_LEN;
    while position < bytes.len() {
        let Some(&tag) = bytes.get(position) else {
            break;
        };
        let Some(body) = body_len(tag) else {
            break; // torn tail: no valid tag
        };
        let body = body as usize;
        if position + 1 + body > bytes.len() {
            break; // torn tail: partial body
        }
        let mut reader = &bytes[position + 1..position + 1 + body];
        let record = Record::decode(tag, &mut reader)
            .map_err(|error| DiskOutputError::Record(error.to_string()))?;
        let span = record.span();
        let fits = usize::try_from(span).is_ok_and(|span| span <= bytes.len() - position);
        if !fits {
            // A complete Run marker may be followed by a reserved record area
            // that a crash left missing; keep the marker so resume can redo it.
            if tag != TAG_RUN || position + 1 + body > bytes.len() {
                break;
            }
            let Record::Run(run) = &record else {
                break;
            };
            if usize::try_from(run.record_len).is_ok_and(|len| len > MAX_RUN_RECORDS) {
                break;
            }
        }
        records.push(ParsedRecord {
            offset: position as u64,
            record,
            span,
        });
        let Some(next) = usize::try_from(span)
            .ok()
            .and_then(|span| position.checked_add(span))
        else {
            break;
        };
        position = next;
    }
    Ok(records)
}
