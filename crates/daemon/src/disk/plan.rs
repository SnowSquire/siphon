//! The clock the two sources are compared in, and the hole arithmetic.

use super::index::SegmentRecord;
use super::index::add_signed;
use crate::rtsp::dvr::DvrSegment;
use crate::rtsp::dvr::DvrTimeline;

// Clock and hole arithmetic

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Clock {
    Pdt,
    Elapsed,
}

/// The `time` of a live record in the selected clock.
pub(super) fn record_time(record: &SegmentRecord, clock: Clock, live_origin: Option<i64>) -> u64 {
    match clock {
        Clock::Pdt => {
            if record.pdt_ms != 0 {
                record.pdt_ms
            } else if let Some(origin) = live_origin {
                add_signed_signed(record.elapsed_ms.into(), origin)
            } else {
                record.elapsed_ms.into()
            }
        }
        Clock::Elapsed => record.elapsed_ms.into(),
    }
}

pub(super) fn add_signed_signed(value: u64, delta: i64) -> u64 {
    add_signed(value, delta)
}

pub(super) fn record_duration(record: &SegmentRecord) -> u64 {
    u64::from(record.duration_ms)
}

pub(super) fn origin_of(pdt: u64, elapsed: u64) -> Option<i64> {
    if pdt == 0 {
        None
    } else {
        Some(i64::try_from(pdt).ok()? - i64::try_from(elapsed).ok()?)
    }
}

/// Start of the DVR chunk containing `time`, i.e. where the DVR resumes after a
/// run so that the chunk crossing the run's end is written whole.
pub(super) fn hole_end(
    timeline: &DvrTimeline,
    time: u64,
    clock: Clock,
    dvr_origin: Option<i64>,
) -> Option<u64> {
    let chunk_time = |chunk: &DvrSegment| chunk_time_of(chunk, clock, dvr_origin);
    let index = timeline
        .segments
        .partition_point(|chunk| chunk_time(chunk) <= time);
    if index == 0 {
        timeline.segments.first().map(chunk_time)
    } else {
        Some(chunk_time(&timeline.segments[index - 1]))
    }
}

pub(super) fn chunk_time_of(chunk: &DvrSegment, clock: Clock, dvr_origin: Option<i64>) -> u64 {
    match clock {
        Clock::Pdt => {
            if chunk.pdt_ms != 0 {
                chunk.pdt_ms
            } else if let Some(origin) = dvr_origin {
                add_signed(chunk.start_ms, origin)
            } else {
                chunk.start_ms
            }
        }
        Clock::Elapsed => chunk.start_ms,
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct HolePlan {
    /// Byte offset of the first kept record inside the run's `segs`.
    pub(super) source_segs_offset: u64,
    /// Bytes in the run's uncovered middle.
    pub(super) segs_len: u64,
    /// Index offset of the first kept record inside the run's `index`.
    pub(super) source_index_offset: u64,
    /// Bytes reserved for the rebased records.
    pub(super) record_len: u32,
    /// Number of kept records.
    pub(super) records: usize,
}

/// Pick the run records that the DVR's whole-chunk edges do not already cover.
/// Trimming is by the selected clock (PDT whenever both sides have it).
pub(super) fn plan_hole(
    records: &[RunRecordMeta],
    hole_start: u64,
    hole_end: u64,
    clock: Clock,
    live_origin: Option<i64>,
) -> HolePlan {
    if hole_end <= hole_start {
        return HolePlan::default();
    }

    let mut first = None;
    let mut last = None;
    let mut segs_len = 0u64;
    let mut record_len = 0u32;
    for (index, meta) in records.iter().enumerate() {
        let time = record_time(&meta.segment, clock, live_origin);
        let end = time.saturating_add(record_duration(&meta.segment));
        if time >= hole_start && end <= hole_end {
            first.get_or_insert(index);
            last = Some(index);
            segs_len += u64::from(meta.segment.byte_length);
            record_len += meta.record_len;
        }
    }

    let (Some(first), Some(last)) = (first, last) else {
        return HolePlan::default();
    };

    debug_assert!(first <= last);
    let source_segs_offset = records[first].segment.byte_offset;
    let source_index_offset = records[first].record_offset;
    HolePlan {
        source_segs_offset,
        segs_len,
        source_index_offset,
        record_len,
        records: last - first + 1,
    }
}

/// A live record plus where it sits in the run's pair.
pub(super) struct RunRecordMeta {
    pub(super) segment: SegmentRecord,
    pub(super) record_offset: u64,
    pub(super) record_len: u32,
}

/// A `#EXT-X-DISCONTINUITY` from the live playlist, not yet placed in the
/// canonical. Both clocks are kept so it can be ordered against a DVR chunk or a
/// live record whichever clock the recording settled on.
#[derive(Clone, Copy, Debug)]
pub(super) struct DiscontinuityMark {
    pub(super) pdt_ms: u64,
    pub(super) elapsed_ms: u64,
}

impl DiscontinuityMark {
    pub(super) fn time(self, clock: Clock, live_origin: Option<i64>) -> u64 {
        match clock {
            Clock::Pdt => {
                if self.pdt_ms != 0 {
                    self.pdt_ms
                } else {
                    live_origin.map_or(self.elapsed_ms, |origin| {
                        add_signed(self.elapsed_ms, origin)
                    })
                }
            }
            Clock::Elapsed => self.elapsed_ms,
        }
    }
}
