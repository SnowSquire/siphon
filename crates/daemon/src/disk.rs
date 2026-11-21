//! Complete-broadcast disk recording.
//!
//! Two sources feed one canonical file:
//!
//! - the live playlist's consumer fan-out, recorded immediately into numbered
//!   *runs* (`live.NNN.{segs,index}`), split at every timestamp gap;
//! - the DVR playlist, which downloads the whole chunks the live playlist
//!   replaced with ads or never carried (the part before the client joined).
//!
//! The DVR owns the canonical `segs`/`index`: it writes every span a live run
//! cannot cover and reserves a *hole* where each run sits. A background copy
//! moves the run's uncovered middle into the hole; the DVR never waits for it.
//!
//! On-disk format: see `index.hexpat` and `notes/disk-complete-recording.md`.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use compio::buf::IoBuf;
use compio::fs::File;
use compio::fs::OpenOptions;
use compio::io::AsyncReadAtExt;
use compio::io::AsyncWriteAtExt;
use futures_util::future::Either;
use futures_util::future::select;
use futures_util::pin_mut;
use kanal::AsyncReceiver;
use kanal::AsyncSender;
use kanal::Receiver;
use kanal::Sender;
use tracing::debug;
use tracing::debug_span;
use tracing::error;
use tracing::info;
use tracing::warn;
use xxhash_rust::xxh3::Xxh3;
use xxhash_rust::xxh3::xxh3_128;

use crate::constants::DVR_SLOTS_MAX;
use crate::downloader::FetchOutcome;
use crate::downloader::ToDownloader;
use crate::http::Client;
use crate::metrics::DiskMetrics;
use crate::notify::Notifier;
use crate::rtsp::dvr::Dvr;
use crate::rtsp::dvr::DvrMap;
use crate::rtsp::dvr::DvrSegment;
use crate::rtsp::dvr::DvrTimeline;
use crate::scheduler::ConsumerRegistration;
use crate::scheduler::Segment;
use crate::scheduler::SegmentMessage;
use crate::spool::ArenaSet;
use crate::spool::ConsumerWake;
use crate::spool::Reader;
use crate::spool::SlotView;
use crate::spool::Spool;
use crate::spool::WakeSet;

const INDEX_MAGIC: [u8; 8] = *b"OZENIKEN";
const INDEX_VERSION: u32 = 4;
const HEADER_LEN: usize = 8 + 4 + 8;

const TAG_SEGMENT: u8 = 0;
const TAG_INIT: u8 = 1;
const TAG_DISCONTINUITY: u8 = 2;
const TAG_RUN: u8 = 3;

const SEGMENT_BODY: u32 = 1 + 8 + 4 + 8 + 4 + 8 + 4 + 16 + 1;
const INIT_BODY: u32 = 1 + 8 + 8 + 4 + 16;
const DISCONTINUITY_BODY: u32 = 2 + 2 + 2 + 2;
const RUN_BODY: u32 = 4 + 8 + 8 + 8 + 8 + 4 + 1;

/// Offset of the `complete` byte inside a serialized `Run` record.
const RUN_COMPLETE_OFFSET: u64 = 1 + 4 + 8 + 8 + 8 + 8 + 4;

const SOURCE_LIVE: u8 = 0;
const SOURCE_DVR: u8 = 1;

/// A jump larger than this between consecutive live records is a gap (an ad or
/// a reconnect), never normal timestamp jitter.
const GAP_TOLERANCE_MS: u64 = 250;
/// The broadcast's `pdt - elapsed` origin must agree within this tolerance, or
/// the two sources are not talking about the same content.
const ORIGIN_TOLERANCE_MS: i64 = 1_000;
/// How long discovery may take, and how long after the broadcast start it may
/// keep looking (a DVR playlist is not published immediately).
const DISCOVERY_WAIT: Duration = Duration::from_secs(15);
const DISCOVERY_AFTER_START: Duration = Duration::from_secs(30);
/// No DVR progress for this long means the recording cannot advance.
const DVR_STALL: Duration = Duration::from_secs(60);
/// Bytes read/written per copy block.
const COPY_BLOCK: usize = 256 * 1024;
/// Cap on an `EXT-X-MAP` / live init segment. Real inits are a few KiB; this is
/// only a guard against a wrong URL returning a whole segment or a playlist.
const MAX_INIT_BYTES: usize = 8 * 1024 * 1024;
/// Sanity cap for a `Run` marker's reserved record area. A torn marker cannot
/// make the resume path reserve an unbounded index range.
const MAX_RUN_RECORDS: usize = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[allow(variant_size_differences)]
pub enum DiskOutputError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid index header (want v{INDEX_VERSION})")]
    InvalidHeader,
    #[error("invalid index record: {0}")]
    Record(String),
    #[error("http: {0}")]
    Http(#[from] crate::http::Error),
}

impl DiskOutputError {
    /// A stale/incompatible index is the only error that exits the process; a
    /// full disk or a failed fetch stops the disk subsystem and keeps RTSP.
    const fn fatal(&self) -> bool {
        matches!(self, Self::InvalidHeader)
    }
}

enum DiskOutcome {
    Done,
    Fatal(String),
}

/// Options for one broadcast's disk output.
pub struct DiskOptions {
    pub output: PathBuf,
    pub stream_id: u64,
    pub keep_runs: bool,
    pub notifier: Option<Notifier>,
    pub dvr: Rc<Dvr>,
    /// Fetches `EXT-X-MAP` objects; the DVR media itself goes through the
    /// downloader queue.
    pub client: Rc<Client>,
    /// Read by `GetDisk`.
    pub metrics: Rc<DiskMetrics>,
}

/// Handle to the recording task. `wait` resolves when the disk subsystem stops;
/// `Some` carries a fatal error that must exit the process.
pub struct DiskOutput {
    stop: Sender<()>,
    done: Receiver<DiskOutcome>,
}

impl DiskOutput {
    pub fn stop(&self) {
        let _ = self.stop.try_send(());
    }

    pub async fn wait(&self) -> Option<String> {
        let done = self.done.clone().to_async();
        match done.recv().await {
            Ok(DiskOutcome::Fatal(error)) => Some(error),
            _ => None,
        }
    }
}

pub fn spawn_disk_output(
    options: DiskOptions,
    wakes: &Rc<WakeSet>,
    downloaders: &AsyncSender<ToDownloader>,
    add_output: &Sender<ConsumerRegistration>,
) -> DiskOutput {
    let (live_tx, live_rx) = kanal::unbounded();
    let (stop_tx, stop_rx) = kanal::bounded(1);
    let (done_tx, done_rx) = kanal::bounded(1);

    let registration = ConsumerRegistration {
        tx: live_tx,
        // The last ~36 s are the only part of the pre-join broadcast the live
        // source can still see, so record them before the DVR is known.
        backlog: true,
        lossy: false,
    };
    if let Err(error) = add_output.send(registration) {
        error!(%error, "failed to register disk output");
    }

    let live_wake = ConsumerWake::new();
    wakes.register(&live_wake);

    let downloaders = downloaders.clone();
    let metrics = Rc::clone(&options.metrics);
    compio::runtime::spawn(async move {
        metrics.active.set(true);
        let result = run_disk(options, live_rx, stop_rx, &downloaders, live_wake).await;
        metrics.active.set(false);
        if let Err(error) = &result {
            *metrics.last_error.borrow_mut() = Some(error.to_string());
        }
        match result {
            Ok(()) => {
                let _ = done_tx.send(DiskOutcome::Done);
            }
            Err(error) => {
                error!(?error, "disk output stopped");
                let outcome = if error.fatal() {
                    DiskOutcome::Fatal(error.to_string())
                } else {
                    DiskOutcome::Done
                };
                let _ = done_tx.send(outcome);
            }
        }
    })
    .detach();

    DiskOutput {
        stop: stop_tx,
        done: done_rx,
    }
}

// ---------------------------------------------------------------------------
// Index v4 records
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
struct SegmentRecord {
    source: u8,
    sequence: u64,
    elapsed_ms: u32,
    pdt_ms: u64,
    duration_ms: u32,
    byte_offset: u64,
    byte_length: u32,
    hash: u128,
    flags: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InitRecord {
    source: u8,
    counter: u64,
    byte_offset: u64,
    byte_length: u32,
    hash: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiscontinuityRecord {
    width: u16,
    height: u16,
    rate_num: u16,
    rate_denom: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RunRecord {
    run: u32,
    dst_segs_offset: u64,
    segs_len: u64,
    source_segs_offset: u64,
    source_index_offset: u64,
    record_len: u32,
    /// 0 while the copy is pending, 1 when it is done (or was abandoned).
    complete: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Record {
    Segment(SegmentRecord),
    Init(InitRecord),
    Discontinuity(DiscontinuityRecord),
    Run(RunRecord),
}

impl Record {
    const fn tag(&self) -> u8 {
        match self {
            Self::Segment(_) => TAG_SEGMENT,
            Self::Init(_) => TAG_INIT,
            Self::Discontinuity(_) => TAG_DISCONTINUITY,
            Self::Run(_) => TAG_RUN,
        }
    }

    const fn encoded_len(&self) -> u32 {
        match self {
            Self::Segment(_) => 1 + SEGMENT_BODY,
            Self::Init(_) => 1 + INIT_BODY,
            Self::Discontinuity(_) => 1 + DISCONTINUITY_BODY,
            Self::Run(_) => 1 + RUN_BODY,
        }
    }

    /// Bytes the record occupies in the index, including a `Run`'s reserved
    /// record area.
    fn span(&self) -> u64 {
        match self {
            Self::Run(run) => u64::from(1 + RUN_BODY) + u64::from(run.record_len),
            other => u64::from(other.encoded_len()),
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
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

    fn decode<R: Read>(tag: u8, reader: &mut R) -> std::io::Result<Self> {
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
    const fn hashed_range(&self) -> Option<(u64, u32, u128)> {
        match self {
            Self::Segment(segment) => {
                Some((segment.byte_offset, segment.byte_length, segment.hash))
            }
            Self::Init(init) => Some((init.byte_offset, init.byte_length, init.hash)),
            Self::Discontinuity(_) | Self::Run(_) => None,
        }
    }

    /// Rebase a run's record into the canonical file.
    fn rebase(&mut self, delta: i64, init_counter: &mut u64) {
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

fn add_signed(value: u64, delta: i64) -> u64 {
    i64::try_from(value)
        .ok()
        .and_then(|value| value.checked_add(delta))
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or(value)
}

fn read_u8<R: Read>(reader: &mut R) -> std::io::Result<u8> {
    let mut bytes = [0u8; 1];
    reader.read_exact(&mut bytes)?;
    Ok(bytes[0])
}

fn read_u16<R: Read>(reader: &mut R) -> std::io::Result<u16> {
    let mut bytes = [0u8; 2];
    reader.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32<R: Read>(reader: &mut R) -> std::io::Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64<R: Read>(reader: &mut R) -> std::io::Result<u64> {
    let mut bytes = [0u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_u128<R: Read>(reader: &mut R) -> std::io::Result<u128> {
    let mut bytes = [0u8; 16];
    reader.read_exact(&mut bytes)?;
    Ok(u128::from_le_bytes(bytes))
}

const fn body_len(tag: u8) -> Option<u32> {
    match tag {
        TAG_SEGMENT => Some(SEGMENT_BODY),
        TAG_INIT => Some(INIT_BODY),
        TAG_DISCONTINUITY => Some(DISCONTINUITY_BODY),
        TAG_RUN => Some(RUN_BODY),
        _ => None,
    }
}

fn encode_header(stream_id: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN);
    out.extend_from_slice(&INDEX_MAGIC);
    out.extend_from_slice(&INDEX_VERSION.to_le_bytes());
    out.extend_from_slice(&stream_id.to_le_bytes());
    out
}

fn parse_header(bytes: &[u8]) -> Result<u64, DiskOutputError> {
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

struct ParsedRecord {
    offset: u64,
    record: Record,
    span: u64,
}

/// Structurally parse the index. A truncated tail is not an error: the caller
/// truncates at the end of the last complete record.
fn parse_records(bytes: &[u8]) -> Result<Vec<ParsedRecord>, DiskOutputError> {
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

// ---------------------------------------------------------------------------
// Clock and hole arithmetic
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Clock {
    Pdt,
    Elapsed,
}

/// The `time` of a live record in the selected clock.
fn record_time(record: &SegmentRecord, clock: Clock, live_origin: Option<i64>) -> u64 {
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

fn add_signed_signed(value: u64, delta: i64) -> u64 {
    add_signed(value, delta)
}

fn record_duration(record: &SegmentRecord) -> u64 {
    u64::from(record.duration_ms)
}

fn origin_of(pdt: u64, elapsed: u64) -> Option<i64> {
    if pdt == 0 {
        None
    } else {
        Some(i64::try_from(pdt).ok()? - i64::try_from(elapsed).ok()?)
    }
}

/// Start of the DVR chunk containing `time`, i.e. where the DVR resumes after a
/// run so that the chunk crossing the run's end is written whole.
fn hole_end(
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

fn chunk_time_of(chunk: &DvrSegment, clock: Clock, dvr_origin: Option<i64>) -> u64 {
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
struct HolePlan {
    /// Byte offset of the first kept record inside the run's `segs`.
    source_segs_offset: u64,
    /// Bytes in the run's uncovered middle.
    segs_len: u64,
    /// Index offset of the first kept record inside the run's `index`.
    source_index_offset: u64,
    /// Bytes reserved for the rebased records.
    record_len: u32,
    /// Number of kept records.
    records: usize,
}

/// Pick the run records that the DVR's whole-chunk edges do not already cover.
/// Trimming is by the selected clock (PDT whenever both sides have it).
fn plan_hole(
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
struct RunRecordMeta {
    segment: SegmentRecord,
    record_offset: u64,
    record_len: u32,
}

// ---------------------------------------------------------------------------
// Disk state
// ---------------------------------------------------------------------------

/// What the live recorder is doing. Orthogonal to where the DVR cursor is: the
/// two together are the whole control state of the recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Live {
    /// Segments are still arriving and are recorded into runs.
    Recording,
    /// The DVR reached the run that is still recording: the main loop drains and
    /// closes it, then the DVR continues past it.
    Draining,
    /// Nothing more will come from the live playlist.
    Done,
}

/// A run the DVR cursor cannot pass: its hole is only sized once the run closes.
/// Holds the chunk that crosses the run's start, whose `pdt - start` is the
/// other side of the alignment check.
#[derive(Clone, Debug)]
struct Parked {
    run: u32,
    /// Time the DVR resumes at once the run's hole is reserved.
    hole_start: u64,
    crossing: DvrSegment,
}

/// Where the recording is in its life. Every combination the old flags allowed
/// but never meant (a handoff with no parked run, a canonical with no DVR) is
/// unrepresentable here.
#[derive(Clone, Debug)]
enum Phase {
    /// Discovery in progress: live segments go into runs and there is no
    /// canonical yet.
    Discovering,
    /// The canonical is the DVR file.
    Dvr { live: Live, parked: Option<Parked> },
    /// No DVR ever appeared: the canonical is the live stream directly.
    Promoted,
}

impl Phase {
    /// The DVR file owns the canonical and nothing is parked.
    const fn following() -> Self {
        Self::Dvr {
            live: Live::Recording,
            parked: None,
        }
    }

    const fn dvr(&self) -> bool {
        matches!(self, Self::Dvr { .. })
    }

    const fn promoted(&self) -> bool {
        matches!(self, Self::Promoted)
    }

    /// The live side's state; a non-DVR recording only ever records.
    const fn live(&self) -> Live {
        match self {
            Self::Dvr { live, .. } => *live,
            Self::Discovering | Self::Promoted => Live::Recording,
        }
    }

    const fn parked(&self) -> Option<&Parked> {
        match self {
            Self::Dvr { parked, .. } => parked.as_ref(),
            Self::Discovering | Self::Promoted => None,
        }
    }
}

type MapKey = (String, Option<(u64, u64)>);
type MapCache = HashMap<MapKey, (u128, Vec<u8>)>;

struct Canonical {
    segs: File,
    index: File,
    segs_end: u64,
    index_end: u64,
    init_counter: u64,
    /// Hash of the `Init` record currently in effect at the tail.
    active_init: Option<u128>,
}

struct RunData {
    number: u32,
    segs_path: PathBuf,
    index_path: PathBuf,
    records: Vec<RunRecordMeta>,
    segs_len: u64,
    index_len: u64,
    init_counter: u64,
    /// The run's leading `EXT-X-MAP`, if the live playlist had one. A map change
    /// closes the run, so a run has at most one init and it is always first.
    init: Option<InitRecord>,
    /// `Some` only for the run currently being recorded.
    files: Option<(File, File)>,
    first_segment: Option<SegmentRecord>,
    last_segment: Option<SegmentRecord>,
}

impl RunData {
    fn first_time(&self, clock: Clock, live_origin: Option<i64>) -> Option<u64> {
        self.first_segment
            .as_ref()
            .map(|record| record_time(record, clock, live_origin))
    }

    fn last_end(&self, clock: Clock, live_origin: Option<i64>) -> Option<u64> {
        self.last_segment.as_ref().map(|record| {
            record_time(record, clock, live_origin).saturating_add(record_duration(record))
        })
    }

    fn origin(&self) -> Option<i64> {
        self.first_segment
            .as_ref()
            .and_then(|record| origin_of(record.pdt_ms, u64::from(record.elapsed_ms)))
    }
}

struct InFlightChunk {
    chunk: DvrSegment,
    dst_offset: u64,
    written: u64,
    hasher: Xxh3,
}

struct CopyJob {
    run: u32,
    dst_segs: u64,
    dst_index: u64,
    segs_len: u64,
    records: Vec<u8>,
    written: u64,
    segs_path: PathBuf,
    index_path: PathBuf,
}

#[allow(clippy::struct_excessive_bools)] // a state machine, not flags of a data type
struct Disk {
    options: DiskOptions,
    dir: PathBuf,
    client: Rc<Client>,
    live_rx: Option<AsyncReceiver<SegmentMessage>>,
    events_rx: AsyncReceiver<Event>,
    events_tx: Sender<Event>,
    downloaders: AsyncSender<ToDownloader>,
    live_wake: Rc<ConsumerWake>,
    dvr: Rc<Dvr>,
    dvr_arena: Rc<ArenaSet>,
    dvr_wakes: Rc<WakeSet>,
    /// `(uri, byterange)` → `(init hash, bytes)`, so an unchanged map is fetched
    /// once and can still be inserted if the canonical's active map changes.
    map_cache: MapCache,

    /// The whole control state: what the canonical is, where the DVR cursor is
    /// and what the live recorder may still do.
    phase: Phase,
    canonical: Option<Canonical>,
    current_run: Option<RunData>,
    closed_runs: VecDeque<RunData>,
    next_run: u32,
    live_edge: Option<(u64, u64, bool)>, // (start, end, pdt domain)
    live_origin: Option<i64>,
    live_open: bool,

    clock: Option<Clock>,
    dvr_origin: Option<i64>,
    dvr_next_start_ms: u64,
    resume_abs: Option<u64>,
    resume_clock: Option<Clock>,
    in_flight: Option<InFlightChunk>,
    copies: Vec<CopyJob>,
    dvr_stall: u64,
    stall_armed: Option<u64>,
    dvr_seen_url: bool,

    stop: bool,
    finished: bool,
}

enum Event {
    ChunkPart(SlotView),
    ChunkDone(FetchOutcome),
    CopyPart { run: u32, data: Vec<u8> },
    CopyDone { run: u32 },
    CopyFailed { run: u32, error: String },
    DvrUpdate,
    DiscoveryDeadline,
    DvrStall(u64),
    Stop,
}

async fn run_disk(
    options: DiskOptions,
    live_rx: Receiver<SegmentMessage>,
    stop_rx: Receiver<()>,
    downloaders: &AsyncSender<ToDownloader>,
    live_wake: Rc<ConsumerWake>,
) -> Result<(), DiskOutputError> {
    let dir = options.output.join(options.stream_id.to_string());
    compio::fs::create_dir_all(&dir).await?;

    let (events_tx, events_rx) = kanal::unbounded();
    let dvr = Rc::clone(&options.dvr);
    let client = Rc::clone(&options.client);
    let disk = Disk {
        options,
        dir,
        client,
        live_rx: Some(live_rx.to_async()),
        events_rx: events_rx.to_async(),
        events_tx: events_tx.clone(),
        downloaders: downloaders.clone(),
        live_wake,
        dvr: Rc::clone(&dvr),
        dvr_arena: Rc::new(ArenaSet::new()),
        dvr_wakes: WakeSet::new(),
        map_cache: HashMap::new(),
        phase: Phase::Discovering,
        canonical: None,
        current_run: None,
        closed_runs: VecDeque::new(),
        next_run: 0,
        live_edge: None,
        live_origin: None,
        live_open: true,
        clock: None,
        dvr_origin: None,
        dvr_next_start_ms: 0,
        resume_abs: None,
        resume_clock: None,
        in_flight: None,
        copies: Vec::new(),
        dvr_stall: 0,
        stall_armed: None,
        dvr_seen_url: false,
        stop: false,
        finished: false,
    };

    spawn_stop_proxy(stop_rx, events_tx.clone());
    spawn_dvr_watcher(Rc::clone(&dvr), events_tx.clone());
    spawn_discovery_timer(&dvr, events_tx.clone());

    let mut disk = disk;
    disk.startup().await?;
    disk.run().await
}

fn spawn_stop_proxy(stop_rx: Receiver<()>, events: Sender<Event>) {
    compio::runtime::spawn(async move {
        let stop_rx = stop_rx.to_async();
        let _ = stop_rx.recv().await;
        let _ = events.send(Event::Stop);
    })
    .detach();
}

fn spawn_dvr_watcher(dvr: Rc<Dvr>, events: Sender<Event>) {
    compio::runtime::spawn(async move {
        let wake = ConsumerWake::new();
        dvr.subscribe(&wake);
        let mut seen = dvr.generation();
        loop {
            WaitGeneration {
                dvr: &dvr,
                wake: &wake,
                seen,
            }
            .await;
            seen = dvr.generation();
            if events.send(Event::DvrUpdate).is_err() {
                return;
            }
        }
    })
    .detach();
}

struct WaitGeneration<'a> {
    dvr: &'a Dvr,
    wake: &'a ConsumerWake,
    seen: u64,
}

impl Future for WaitGeneration<'_> {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.dvr.generation() == self.seen {
            self.wake.park(context);
        }
        if self.dvr.generation() == self.seen {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    }
}

fn spawn_discovery_timer(dvr: &Rc<Dvr>, events: Sender<Event>) {
    if dvr.url().is_some() {
        return;
    }
    let wait = discovery_wait(dvr);
    compio::runtime::spawn(async move {
        compio::time::sleep(wait).await;
        let _ = events.send(Event::DiscoveryDeadline);
    })
    .detach();
}

fn discovery_wait(dvr: &Dvr) -> Duration {
    let now = now_ms();
    let mut wait = DISCOVERY_WAIT;
    if let Some(start) = dvr.broadcast_start_ms() {
        let after_start = start
            .saturating_add(millis_u64(DISCOVERY_AFTER_START))
            .saturating_sub(now);
        wait = wait.max(Duration::from_millis(after_start));
    }
    wait
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn block_take(remaining: u64) -> usize {
    usize::try_from(remaining.min(COPY_BLOCK as u64)).unwrap_or(COPY_BLOCK)
}

/// Spawn a one-shot stall watchdog. The generation makes a later progress
/// invalidate it without keeping a timer handle around.
fn spawn_stall_timer(generation: u64, events: Sender<Event>) {
    compio::runtime::spawn(async move {
        compio::time::sleep(DVR_STALL).await;
        let _ = events.send(Event::DvrStall(generation));
    })
    .detach();
}

impl Disk {
    fn notify(&self, message: impl Into<String>) {
        if let Some(notifier) = &self.options.notifier {
            notifier.notify(message);
        }
    }

    fn notify_error(&self, message: impl Into<String>) {
        let message = message.into();
        warn!("{message}");
        self.notify(message);
    }

    /// Resume, or start from nothing: the index decides which of the note's
    /// five resume cases applies.
    async fn startup(&mut self) -> Result<(), DiskOutputError> {
        let has_url = self.dvr.url().is_some();
        let snapshot = read_canonical(
            &self.dir,
            self.options.stream_id,
            self.options.keep_runs,
        )
        .await?;

        let Some(snapshot) = snapshot else {
            self.next_run = scan_run_number(&self.dir);
            if has_url {
                // Discovery won the race with the disk task (debug, or a resume
                // whose URL is already known): start the canonical immediately.
                let canonical = create_canonical(&self.dir, self.options.stream_id).await?;
                self.enter_dvr(canonical);
            }
            debug!(
                phase = ?self.phase,
                next_run = self.next_run,
                "disk: recording started"
            );
            return Ok(());
        };

        self.next_run = snapshot.next_run;
        self.live_origin = snapshot.live_origin;
        if snapshot.has_dvr {
            // DVR records: the canonical is the DVR file. Without a URL the
            // discovery deadline decides; DVR -> live is not automatic.
            self.resume_abs = Some(snapshot.resume_abs);
            self.resume_clock = snapshot.resume_clock;
            let canonical = open_canonical(&self.dir, &snapshot).await?;
            self.enter_dvr(canonical);
            self.dvr_seen_url = has_url;
        } else if has_url {
            // A promoted live recording that now has a DVR: rename it to a run,
            // start a fresh canonical and prefill from the broadcast start.
            let run = rename_canonical_to_run(&self.dir, self.next_run, &snapshot).await?;
            self.closed_runs.push_back(run);
            self.next_run = self.next_run.saturating_add(1);
            let canonical = create_canonical(&self.dir, self.options.stream_id).await?;
            self.enter_dvr(canonical);
            self.dvr_next_start_ms = 0;
        } else {
            self.canonical = Some(open_canonical(&self.dir, &snapshot).await?);
            self.phase = Phase::Promoted;
            self.live_open = true;
        }

        debug!(
            phase = ?self.phase,
            next_run = self.next_run,
            "disk: recording started"
        );
        Ok(())
    }

    /// Take the DVR file as the canonical from now on.
    fn enter_dvr(&mut self, canonical: Canonical) {
        self.canonical = Some(canonical);
        self.phase = Phase::following();
        self.dvr_seen_url = true;
    }

    async fn run(&mut self) -> Result<(), DiskOutputError> {
        while !self.finished {
            if self.stop {
                self.finish().await?;
                break;
            }

            if self.live_open {
                let incoming = {
                    let live = self
                        .live_rx
                        .as_ref()
                        .expect("live receiver while recording")
                        .recv();
                    let events = self.events_rx.recv();
                    pin_mut!(live, events);
                    match select(live, events).await {
                        Either::Left((message, _)) => Incoming::Live(message),
                        Either::Right((event, _)) => Incoming::Event(event),
                    }
                };
                match incoming {
                    Incoming::Live(Ok(message)) => self.on_live(message).await?,
                    Incoming::Live(Err(_)) => {
                        self.live_open = false;
                        self.on_live_closed().await?;
                    }
                    Incoming::Event(Ok(event)) => self.on_event(event).await?,
                    Incoming::Event(Err(_)) => break,
                }
            } else {
                let event = {
                    let events = self.events_rx.recv();
                    pin_mut!(events);
                    events.await
                };
                match event {
                    Ok(event) => self.on_event(event).await?,
                    Err(_) => break,
                }
            }

            if self.phase.live() == Live::Draining {
                self.drain_live().await?;
            }

            self.pump().await?;
            self.check_done();
        }
        Ok(())
    }

    async fn on_event(&mut self, event: Event) -> Result<(), DiskOutputError> {
        match event {
            Event::ChunkPart(view) => self.on_chunk_part(view).await?,
            Event::ChunkDone(outcome) => self.on_chunk_done(outcome).await?,
            Event::CopyPart { run, data } => self.on_copy_part(run, data).await?,
            Event::CopyDone { run } => self.on_copy_done(run).await?,
            Event::CopyFailed { run, error } => {
                self.notify_error(format!("disk: run {run} copy failed: {error}"));
                return Err(DiskOutputError::Io(std::io::Error::other(error)));
            }
            Event::DvrUpdate => self.on_dvr_update().await?,
            Event::DiscoveryDeadline => self.on_discovery_deadline().await?,
            Event::DvrStall(generation) => {
                if generation == self.dvr_stall {
                    self.notify_error("disk: DVR made no progress; recording stopped");
                    return Err(DiskOutputError::Io(std::io::Error::other("DVR stalled")));
                }
            }
            Event::Stop => self.stop = true,
        }
        Ok(())
    }

    async fn on_dvr_update(&mut self) -> Result<(), DiskOutputError> {
        self.dvr_stall = self.dvr_stall.wrapping_add(1);
        self.stall_armed = None;
        if self.dvr.url().is_none() || self.dvr_seen_url {
            return Ok(());
        }
        self.dvr_seen_url = true;
        if matches!(self.phase, Phase::Discovering) {
            let canonical = create_canonical(&self.dir, self.options.stream_id).await?;
            self.enter_dvr(canonical);
            info!("disk: DVR playlist discovered, switching to the canonical DVR file");
        } else if self.phase.promoted() {
            self.notify(
                "disk: DVR playlist appeared after the live promotion; continuing live-only",
            );
        }
        Ok(())
    }

    async fn on_discovery_deadline(&mut self) -> Result<(), DiskOutputError> {
        if self.dvr.url().is_some() {
            return Ok(());
        }
        // A broadcast that started less than 30 s ago gets 30 s from its start.
        if let Some(start) = self.dvr.broadcast_start_ms() {
            let deadline = start.saturating_add(millis_u64(DISCOVERY_AFTER_START));
            if now_ms() < deadline {
                let wait = Duration::from_millis(deadline.saturating_sub(now_ms()));
                let events = self.events_tx.clone();
                compio::runtime::spawn(async move {
                    compio::time::sleep(wait).await;
                    let _ = events.send(Event::DiscoveryDeadline);
                })
                .detach();
                return Ok(());
            }
        }
        if matches!(self.phase, Phase::Discovering) {
            self.notify(
                "disk: no DVR playlist found; promoting the live recording to the canonical file",
            );
            self.promote().await?;
        } else if self.phase.dvr() {
            self.notify_error("disk: DVR playlist disappeared; recording stopped");
            return Err(DiskOutputError::Io(std::io::Error::other(
                "DVR disappeared",
            )));
        }
        Ok(())
    }

    async fn on_live(&mut self, message: SegmentMessage) -> Result<(), DiskOutputError> {
        if self.phase.live() != Live::Recording {
            return Ok(());
        }
        match message {
            SegmentMessage::Segment(segment) => {
                if self.phase.promoted() {
                    self.write_promoted(segment).await
                } else {
                    self.write_live(segment).await
                }
            }
            // fMP4 map. TS streams carry none. A map change closes the current
            // run, so every run is self-contained: its init is always leading.
            SegmentMessage::Init { mut reader } => {
                if self.phase.promoted() {
                    let bytes =
                        read_reader_bytes(&mut reader, &self.live_wake, MAX_INIT_BYTES).await?;
                    if let Some(canonical) = &mut self.canonical {
                        write_init_bytes(canonical, SOURCE_LIVE, &bytes).await?;
                    }
                    return Ok(());
                }
                if self
                    .current_run
                    .as_ref()
                    .is_some_and(|run| run.first_segment.is_some())
                {
                    self.close_current_run().await?;
                }
                if self.current_run.is_none() {
                    let number = self.next_run;
                    self.next_run = self.next_run.saturating_add(1);
                    self.open_run(number).await?;
                }
                if let Some(run) = &mut self.current_run
                    && let Some((segs, index)) = &mut run.files
                {
                    let mut counter = run.init_counter;
                    let init = write_init(
                        segs,
                        index,
                        &mut run.segs_len,
                        &mut run.index_len,
                        &mut counter,
                        SOURCE_LIVE,
                        &mut reader,
                        &self.live_wake,
                    )
                    .await?;
                    run.init_counter = counter;
                    run.init = Some(init);
                }
                Ok(())
            }
            SegmentMessage::Prefetch { .. }
            | SegmentMessage::Discontinuity()
            | SegmentMessage::Seek(_) => Ok(()),
        }
    }

    async fn write_promoted(&mut self, segment: Segment) -> Result<(), DiskOutputError> {
        debug!(
            sequence = segment.sequence_num,
            "disk: writing live segment"
        );
        let Some(canonical) = &mut self.canonical else {
            return Ok(());
        };
        let mut hasher = Xxh3::new();
        let start = canonical.segs_end;
        let mut reader = segment.reader;
        let written = write_segment_body(
            &mut canonical.segs,
            start,
            &mut reader,
            &mut hasher,
            &self.live_wake,
        )
        .await?;
        if written == 0 {
            return Ok(());
        }
        let record = SegmentRecord {
            source: SOURCE_LIVE,
            sequence: segment.sequence_num,
            elapsed_ms: ms_u32(segment.timestamp_ms),
            pdt_ms: segment.pdt_ms,
            duration_ms: ms_u32(segment.duration_ms),
            byte_offset: start,
            byte_length: u32::try_from(written).unwrap_or(u32::MAX),
            hash: hasher.digest128(),
            flags: 0,
        };
        canonical.segs_end += written;
        self.options
            .metrics
            .note_written(segment.timestamp_ms, written);
        write_record(
            &mut canonical.index,
            canonical.index_end,
            &Record::Segment(record),
        )
        .await?;
        canonical.index_end += u64::from(1 + SEGMENT_BODY);
        if let Some(origin) = origin_of(segment.pdt_ms, segment.timestamp_ms) {
            self.live_origin = Some(origin);
        }
        Ok(())
    }

    async fn write_live(&mut self, segment: Segment) -> Result<(), DiskOutputError> {
        let pdt = segment.pdt_ms;
        let elapsed = segment.timestamp_ms;
        let duration = segment.duration_ms;

        let gap = self
            .live_edge
            .is_some_and(|(last_start, last_end, pdt_domain)| {
                if pdt != 0 && pdt_domain {
                    pdt > last_end.saturating_add(GAP_TOLERANCE_MS)
                        || pdt.saturating_add(GAP_TOLERANCE_MS) < last_start
                } else {
                    elapsed > last_end.saturating_add(GAP_TOLERANCE_MS)
                        || elapsed.saturating_add(GAP_TOLERANCE_MS) < last_start
                }
            });

        if gap {
            debug!(
                previous = ?self.live_edge,
                sequence = segment.sequence_num,
                "disk: live timestamp gap, splitting the run"
            );
            // An empty run may have just been opened for a new map. Keep it: its
            // init is exactly what the first media after the gap needs.
            if self
                .current_run
                .as_ref()
                .is_some_and(|run| run.first_segment.is_some())
            {
                self.close_current_run().await?;
            }
        }

        if self.current_run.is_none() {
            let number = self.next_run;
            self.next_run = self.next_run.saturating_add(1);
            self.open_run(number).await?;
        }

        let Some(run) = &mut self.current_run else {
            return Ok(());
        };
        let Some((segs, index)) = &mut run.files else {
            return Ok(());
        };

        let mut hasher = Xxh3::new();
        let start = run.segs_len;
        let mut reader = segment.reader;
        let written =
            write_segment_body(segs, start, &mut reader, &mut hasher, &self.live_wake).await?;
        if written == 0 {
            // A failed download leaves no bytes. The next segment's timestamp
            // jump closes this run and the DVR fills the missing span.
            return Ok(());
        }

        let record = SegmentRecord {
            source: SOURCE_LIVE,
            sequence: segment.sequence_num,
            elapsed_ms: ms_u32(elapsed),
            pdt_ms: pdt,
            duration_ms: ms_u32(duration),
            byte_offset: start,
            byte_length: u32::try_from(written).unwrap_or(u32::MAX),
            hash: hasher.digest128(),
            flags: 0,
        };
        let record_offset = run.index_len;
        write_record(index, record_offset, &Record::Segment(record.clone())).await?;
        run.records.push(RunRecordMeta {
            segment: record.clone(),
            record_offset,
            record_len: 1 + SEGMENT_BODY,
        });
        run.segs_len += written;
        run.index_len += u64::from(1 + SEGMENT_BODY);
        self.options.metrics.note_written(elapsed, written);
        run.first_segment.get_or_insert_with(|| record.clone());
        run.last_segment = Some(record);

        if let Some(origin) = origin_of(pdt, elapsed) {
            self.live_origin = Some(origin);
        }
        self.live_edge = Some((
            if pdt != 0 { pdt } else { elapsed },
            if pdt != 0 { pdt } else { elapsed }.saturating_add(duration),
            pdt != 0,
        ));
        debug!(
            sequence = segment.sequence_num,
            bytes = written,
            "disk: wrote live segment"
        );
        Ok(())
    }

    async fn open_run(&mut self, number: u32) -> Result<(), DiskOutputError> {
        let (segs_path, index_path) = run_paths(&self.dir, number);
        let mut segs = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&segs_path)
            .await?;
        let mut index = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&index_path)
            .await?;
        let header = encode_header(self.options.stream_id);
        index.write_all_at(header, 0).await.0?;
        // Touch the segs file so it exists before the run is ever a hole.
        segs.write_all_at(Vec::<u8>::new(), 0).await.0?;

        self.options
            .metrics
            .runs
            .set(self.options.metrics.runs.get().saturating_add(1));
        debug!(run = number, "disk: opened live run");
        self.current_run = Some(RunData {
            number,
            segs_path,
            index_path,
            records: Vec::new(),
            segs_len: 0,
            index_len: HEADER_LEN as u64,
            init_counter: 0,
            init: None,
            files: Some((segs, index)),
            first_segment: None,
            last_segment: None,
        });
        Ok(())
    }

    async fn close_current_run(&mut self) -> Result<(), DiskOutputError> {
        let Some(mut run) = self.current_run.take() else {
            return Ok(());
        };
        if let Some((segs, index)) = run.files.take() {
            segs.sync_all().await?;
            index.sync_all().await?;
        }
        // An empty run has nothing to record (the DVR supplies everything).
        if run.first_segment.is_some() {
            debug!(
                run = run.number,
                bytes = run.segs_len,
                records = run.records.len(),
                "disk: closed live run"
            );
            self.closed_runs.push_back(run);
        } else {
            let _ = compio::fs::remove_file(&run.segs_path).await;
            let _ = compio::fs::remove_file(&run.index_path).await;
        }
        Ok(())
    }

    /// Park the DVR cursor at a run's trailing edge.
    fn park(&mut self, parked: Parked) {
        if let Phase::Dvr {
            parked: slot, ..
        } = &mut self.phase
        {
            *slot = Some(parked);
        }
    }

    const fn unpark(&mut self) -> Option<Parked> {
        match &mut self.phase {
            Phase::Dvr { parked, .. } => parked.take(),
            Phase::Discovering | Phase::Promoted => None,
        }
    }

    const fn set_live(&mut self, live: Live) {
        if let Phase::Dvr { live: slot, .. } = &mut self.phase {
            *slot = live;
        }
    }

    async fn drain_live(&mut self) -> Result<(), DiskOutputError> {
        self.set_live(Live::Done);
        // The DVR is authoritative from the live edge on. Anything still queued
        // is content the DVR chunk we just wrote (and the chunks that follow)
        // already supplies, so it is dropped rather than becoming one more run.
        if let Some(rx) = &self.live_rx {
            while matches!(rx.as_sync().try_recv(), Ok(Some(_))) {}
        }
        self.live_open = false;
        // Drop the receiver so the scheduler removes the disk consumer instead
        // of queueing segments nobody reads (which would pin arena slots).
        self.live_rx = None;
        self.close_current_run().await?;
        info!("disk: DVR reached the live edge, live recorder handed off");
        Ok(())
    }

    async fn on_live_closed(&mut self) -> Result<(), DiskOutputError> {
        self.close_current_run().await?;
        if self.phase.promoted() {
            self.finished = true;
        } else if matches!(self.phase, Phase::Discovering) {
            self.promote().await?;
            self.finished = true;
        } else {
            // Keep going until the DVR playlist says ENDLIST and every chunk has
            // been written.
            self.pump().await?;
        }
        Ok(())
    }

    /// No DVR: merge every live run into the canonical file. The runs may have
    /// been split at ads, so their records are concatenated with timestamp
    /// jumps; there is no hole to fill.
    async fn promote(&mut self) -> Result<(), DiskOutputError> {
        self.close_current_run().await?;
        let mut canonical = create_canonical(&self.dir, self.options.stream_id).await?;
        let runs: Vec<RunData> = self.closed_runs.drain(..).collect();

        for run in &runs {
            let source = OpenOptions::new().read(true).open(&run.segs_path).await?;
            let media_start = run.init.as_ref().map_or(0, |init| {
                init.byte_offset.saturating_add(u64::from(init.byte_length))
            });
            if let Some(init) = &run.init {
                let length = usize::try_from(init.byte_length).unwrap_or(usize::MAX);
                let result = source
                    .read_exact_at(vec![0u8; length], init.byte_offset)
                    .await;
                let bytes = result.1;
                result.0?;
                if xxh3_128(&bytes) != init.hash {
                    return Err(DiskOutputError::Record(format!(
                        "run {} init failed verification",
                        run.number
                    )));
                }
                write_init_bytes(&mut canonical, SOURCE_LIVE, &bytes).await?;
            }
            let dst_start = canonical.segs_end;
            let media_len = run.segs_len.saturating_sub(media_start);
            copy_between(
                &source,
                media_start,
                &mut canonical.segs,
                dst_start,
                media_len,
            )
            .await?;

            // The run index's byte offsets start at `media_start`; rebasing is
            // the append offset minus that base. A resumed promoted file has the
            // same shape (its init is at the front).
            let first_offset = run
                .records
                .first()
                .map_or(media_start, |meta| meta.segment.byte_offset);
            let delta = i64::try_from(dst_start).unwrap_or(i64::MAX)
                - i64::try_from(first_offset).unwrap_or(0);
            let mut buffer = Vec::new();
            for meta in &run.records {
                let mut record = Record::Segment(meta.segment.clone());
                record.rebase(delta, &mut canonical.init_counter);
                record.encode(&mut buffer);
            }
            canonical
                .index
                .write_all_at(std::mem::take(&mut buffer), canonical.index_end)
                .await
                .0?;
            canonical.index_end += run
                .records
                .iter()
                .map(|meta| u64::from(meta.record_len))
                .sum::<u64>();
            canonical.segs_end += media_len;
        }
        canonical.segs.sync_all().await?;
        canonical.index.sync_all().await?;
        for run in &runs {
            if !self.options.keep_runs {
                let _ = compio::fs::remove_file(&run.segs_path).await;
                let _ = compio::fs::remove_file(&run.index_path).await;
            }
        }
        self.canonical = Some(canonical);
        self.phase = Phase::Promoted;
        info!("disk: promoted the live recording to the canonical file");
        Ok(())
    }

    async fn on_chunk_part(&mut self, view: SlotView) -> Result<(), DiskOutputError> {
        let Some(in_flight) = &mut self.in_flight else {
            return Ok(());
        };
        let len = view.as_init().len() as u64;
        in_flight.hasher.update(view.as_init());
        let Some(canonical) = &mut self.canonical else {
            return Ok(());
        };
        canonical
            .segs
            .write_all_at(view, in_flight.dst_offset + in_flight.written)
            .await
            .0?;
        in_flight.written += len;
        Ok(())
    }

    async fn on_chunk_done(&mut self, outcome: FetchOutcome) -> Result<(), DiskOutputError> {
        let Some(in_flight) = self.in_flight.take() else {
            return Ok(());
        };
        let Some(canonical) = &mut self.canonical else {
            return Ok(());
        };

        if outcome == FetchOutcome::Failed {
            // The bytes before the failure are not a chunk. Drop them and leave a
            // timestamp jump; a failed early/edge chunk is exactly the case the
            // run/hole model expects.
            canonical.segs.set_len(in_flight.dst_offset).await?;
            self.notify_error(format!(
                "disk: DVR chunk {} failed; leaving a gap",
                in_flight.chunk.sequence
            ));
            self.dvr_stall = self.dvr_stall.wrapping_add(1);
            self.stall_armed = None;
            if let Some(parked) = self.unpark()
                && let Some(run) = self.take_run(parked.run)
            {
                // Without the crossing chunk there is no aligned hole; abandon
                // the run and let the DVR's later chunks supply what they can.
                self.abandon_run(run, in_flight.chunk.start_ms, in_flight.chunk.start_ms)
                    .await?;
            }
            self.dvr_next_start_ms = in_flight
                .chunk
                .start_ms
                .saturating_add(in_flight.chunk.duration_ms);
            return Ok(());
        }

        if in_flight.written == 0 {
            self.notify_error(format!(
                "disk: DVR chunk {} was empty; leaving a gap",
                in_flight.chunk.sequence
            ));
            self.dvr_next_start_ms = in_flight
                .chunk
                .start_ms
                .saturating_add(in_flight.chunk.duration_ms);
            return Ok(());
        }

        let record = SegmentRecord {
            source: SOURCE_DVR,
            sequence: in_flight.chunk.sequence,
            elapsed_ms: ms_u32(in_flight.chunk.start_ms),
            pdt_ms: in_flight.chunk.pdt_ms,
            duration_ms: ms_u32(in_flight.chunk.duration_ms),
            byte_offset: in_flight.dst_offset,
            byte_length: u32::try_from(in_flight.written).unwrap_or(u32::MAX),
            hash: in_flight.hasher.digest128(),
            flags: 0,
        };
        write_record(
            &mut canonical.index,
            canonical.index_end,
            &Record::Segment(record),
        )
        .await?;
        canonical.index_end += u64::from(1 + SEGMENT_BODY);
        canonical.segs_end = in_flight.dst_offset + in_flight.written;
        self.dvr_next_start_ms = in_flight
            .chunk
            .start_ms
            .saturating_add(in_flight.chunk.duration_ms);
        self.dvr_stall = self.dvr_stall.wrapping_add(1);
        self.stall_armed = None;
        debug!(
            sequence = in_flight.chunk.sequence,
            bytes = in_flight.written,
            "disk: wrote DVR chunk"
        );
        Ok(())
    }

    async fn on_copy_part(&mut self, run: u32, data: Vec<u8>) -> Result<(), DiskOutputError> {
        let Some(job) = self.copies.iter_mut().find(|job| job.run == run) else {
            return Ok(());
        };
        let Some(canonical) = &mut self.canonical else {
            return Ok(());
        };
        let len = data.len() as u64;
        canonical
            .segs
            .write_all_at(data, job.dst_segs + job.written)
            .await
            .0?;
        job.written += len;
        Ok(())
    }

    async fn on_copy_done(&mut self, run: u32) -> Result<(), DiskOutputError> {
        let Some(job) = self.copies.iter().position(|job| job.run == run) else {
            return Ok(());
        };
        let job = self.copies.remove(job);
        let Some(canonical) = &mut self.canonical else {
            return Ok(());
        };
        if job.written != job.segs_len {
            self.notify_error(format!(
                "disk: run {run} copy was short ({} of {} bytes)",
                job.written, job.segs_len
            ));
            return Err(DiskOutputError::Io(std::io::Error::other("short copy")));
        }

        canonical
            .index
            .write_all_at(job.records, job.dst_index + u64::from(1 + RUN_BODY))
            .await
            .0?;
        canonical.segs.sync_data().await?;
        canonical
            .index
            .write_all_at(vec![1u8], job.dst_index + RUN_COMPLETE_OFFSET)
            .await
            .0?;
        canonical.index.sync_data().await?;

        if !self.options.keep_runs {
            let _ = compio::fs::remove_file(&job.segs_path).await;
            let _ = compio::fs::remove_file(&job.index_path).await;
        }
        info!(run, bytes = job.segs_len, "disk: filled run hole");
        Ok(())
    }

    fn next_unprocessed_run(&self) -> Option<&RunData> {
        self.closed_runs.front().or(self.current_run.as_ref())
    }

    /// Drive the DVR: fetch the chunk the cursor points at, park when it reaches
    /// a run, and continue once that run's hole is reserved.
    async fn pump(&mut self) -> Result<(), DiskOutputError> {
        loop {
            if !self.phase.dvr() || self.stop || self.finished {
                return Ok(());
            }
            if self.in_flight.is_some() {
                return Ok(());
            }
            let Some(timeline) = self.dvr.snapshot() else {
                self.arm_stall();
                return Ok(());
            };
            self.ensure_clock(&timeline);

            // The cursor cannot pass a run until its hole is sized, and a hole
            // is only sized once the run closes.
            if let Some(number) = self.phase.parked().map(|parked| parked.run) {
                if self.run_is_closed(number) {
                    self.reserve_parked_hole(&timeline).await?;
                    continue;
                }
                if self.run_is_open(number) && self.phase.live() == Live::Recording {
                    // The DVR reached the run that is still recording: this is
                    // the handoff point. The main loop drains and closes it.
                    self.set_live(Live::Draining);
                }
                self.arm_stall();
                return Ok(());
            }

            if self.phase.live() == Live::Done {
                // A run created after the handoff is already covered by the DVR
                // (or by the chunk that will be published next); discard it.
                self.discard_closed_runs("post-handoff").await;
            } else if !self.live_open && self.next_unprocessed_run().is_none() {
                // The live source closed before any run existed: the DVR owns the
                // whole archive and there is nothing left to protect.
                self.set_live(Live::Done);
            }

            // The DVR has published everything it will ever have and the cursor
            // is past its last chunk: any remaining live run is past the end of
            // the broadcast and cannot be holed. Drop it rather than deadlock.
            if timeline.ended && !self.live_open && self.next_chunk(&timeline).is_none() {
                self.discard_closed_runs("past the DVR end").await;
                self.check_done();
                return Ok(());
            }

            // On resume the DVR continues from the last recorded chunk before the
            // new live run is considered.
            if self.resume_abs.is_some() {
                let Some(chunk) = self.next_chunk(&timeline) else {
                    self.arm_stall();
                    return Ok(());
                };
                self.start_fetch(chunk, false, &timeline).await?;
                return Ok(());
            }

            if self.phase.live() != Live::Done && self.next_unprocessed_run().is_none() {
                // No live segment yet: wait for the live start before prefilling.
                self.arm_stall();
                return Ok(());
            }

            let Some(chunk) = self.next_chunk(&timeline) else {
                if self.timeline_done(&timeline) {
                    self.check_done();
                } else {
                    self.arm_stall();
                }
                return Ok(());
            };

            if self.phase.live() == Live::Done {
                self.start_fetch(chunk, false, &timeline).await?;
                return Ok(());
            }

            let Some(run) = self.next_unprocessed_run() else {
                return Ok(());
            };
            let clock = self.clock.unwrap_or(Clock::Elapsed);
            let Some(run_start) = run.first_time(clock, self.live_origin) else {
                return Ok(());
            };
            let run_number = run.number;
            let chunk_start = chunk_time_of(&chunk, clock, self.dvr_origin);
            let chunk_end = chunk_start.saturating_add(chunk.duration_ms);

            if chunk_end <= run_start {
                // Pre-fill or the ad gap before the run: the whole chunk is
                // content the live path never had.
                self.start_fetch(chunk, false, &timeline).await?;
                return Ok(());
            }
            if chunk_start <= run_start {
                // This chunk crosses the run's start. Write it whole; the run's
                // leading records are trimmed and supplied by it.
                self.start_fetch(chunk, true, &timeline).await?;
                return Ok(());
            }

            // The DVR starts after the run (retention window): the run has no
            // recoverable leading content. The first available chunk is the
            // hole's start.
            self.park(Parked {
                run: run_number,
                hole_start: chunk_start,
                crossing: chunk.clone(),
            });
            if self.run_is_closed(run_number) {
                self.reserve_parked_hole(&timeline).await?;
                continue;
            }
            if self.run_is_open(run_number) && self.phase.live() != Live::Done {
                self.set_live(Live::Draining);
            }
            return Ok(());
        }
    }

    /// The parked run has closed: reserve its hole and let the DVR move on.
    async fn reserve_parked_hole(
        &mut self,
        timeline: &DvrTimeline,
    ) -> Result<(), DiskOutputError> {
        let Some(parked) = self.unpark() else {
            return Ok(());
        };
        let Some(run) = self.take_run(parked.run) else {
            debug!(run = parked.run, "disk: parked run is gone");
            return Ok(());
        };
        self.process_hole(run, &parked, timeline).await
    }

    async fn discard_closed_runs(&mut self, why: &str) {
        while let Some(run) = self.closed_runs.pop_front() {
            debug!(run = run.number, why, "disk: discarding live run");
            self.delete_run(&run).await;
        }
    }

    fn arm_stall(&mut self) {
        if self.stall_armed == Some(self.dvr_stall) {
            return;
        }
        if !self.dvr_seen_url {
            return;
        }
        self.stall_armed = Some(self.dvr_stall);
        spawn_stall_timer(self.dvr_stall, self.events_tx.clone());
    }

    fn next_chunk(&self, timeline: &DvrTimeline) -> Option<DvrSegment> {
        if let Some(resume) = self.resume_abs {
            // Match by absolute time on resume, which survives re-chunking.
            let clock = self.resume_clock.or(self.clock).unwrap_or(Clock::Elapsed);
            return timeline
                .segments
                .iter()
                .find(|chunk| chunk_time_of(chunk, clock, self.dvr_origin) >= resume)
                .cloned();
        }
        let index = timeline
            .segments
            .partition_point(|chunk| chunk.start_ms < self.dvr_next_start_ms);
        timeline.segments.get(index).cloned()
    }

    async fn start_fetch(
        &mut self,
        chunk: DvrSegment,
        crossing: bool,
        timeline: &DvrTimeline,
    ) -> Result<(), DiskOutputError> {
        self.ensure_map(&chunk, timeline).await?;
        let Some(canonical) = &self.canonical else {
            return Ok(());
        };
        if self.resume_abs.is_some() {
            // Only apply the resume cursor once.
            self.resume_abs = None;
            self.clock = self.resume_clock.or(self.clock);
            self.dvr_next_start_ms = chunk.start_ms;
        }
        if crossing {
            let clock = self.clock.unwrap_or(Clock::Elapsed);
            let hole_start =
                chunk_time_of(&chunk, clock, self.dvr_origin).saturating_add(chunk.duration_ms);
            let Some(run) = self.next_unprocessed_run().map(|run| run.number) else {
                return Ok(());
            };
            // Direct field write: `canonical` is borrowed for `dst_offset` below.
            if let Phase::Dvr {
                parked: slot, ..
            } = &mut self.phase
            {
                *slot = Some(Parked {
                    run,
                    hole_start,
                    crossing: chunk.clone(),
                });
            }
        }
        let dst_offset = canonical.segs_end;
        self.in_flight = Some(InFlightChunk {
            chunk: chunk.clone(),
            dst_offset,
            written: 0,
            hasher: Xxh3::new(),
        });
        spawn_fetch(
            chunk,
            Rc::clone(&self.dvr_arena),
            Rc::clone(&self.dvr_wakes),
            self.downloaders.clone(),
            self.events_tx.clone(),
        );
        self.dvr_stall = self.dvr_stall.wrapping_add(1);
        self.stall_armed = None;
        Ok(())
    }

    /// Make the chunk's `EXT-X-MAP` the active map, inserting it only when its
    /// hash differs. This runs before `dst_offset` is taken, so the chunk (and
    /// everything after it) is naturally shifted by the init size.
    async fn ensure_map(
        &mut self,
        chunk: &DvrSegment,
        timeline: &DvrTimeline,
    ) -> Result<(), DiskOutputError> {
        let Some(index) = chunk.map else {
            return Ok(());
        };
        let Some(map) = timeline.maps.get(index) else {
            return Ok(());
        };
        let key = (map.uri.clone(), map.byterange);
        let (hash, bytes) = if let Some((hash, bytes)) = self.map_cache.get(&key) {
            (*hash, bytes.clone())
        } else {
            let bytes = fetch_map(&self.client, map).await?;
            let hash = xxh3_128(&bytes);
            self.map_cache.insert(key, (hash, bytes.clone()));
            (hash, bytes)
        };
        if self
            .canonical
            .as_ref()
            .is_some_and(|canonical| canonical.active_init == Some(hash))
        {
            return Ok(());
        }
        if let Some(canonical) = &mut self.canonical {
            write_init_bytes(canonical, SOURCE_DVR, &bytes).await?;
        }
        Ok(())
    }

    fn ensure_clock(&mut self, timeline: &DvrTimeline) {
        if self.clock.is_some() {
            return;
        }
        let dvr_origin = (0..timeline.segments.len()).find_map(|index| timeline.origin_ms(index));
        if let Some(clock) = self.resume_clock {
            self.clock = Some(clock);
            self.dvr_origin = dvr_origin;
            return;
        }
        let live_has_pdt = self
            .next_unprocessed_run()
            .and_then(|run| run.first_segment.as_ref())
            .is_some_and(|record| record.pdt_ms != 0);
        if dvr_origin.is_none() || !live_has_pdt {
            if self.next_unprocessed_run().is_none() {
                return;
            }
            self.clock = Some(Clock::Elapsed);
        } else {
            self.clock = Some(Clock::Pdt);
        }
        self.dvr_origin = dvr_origin;
    }

    /// Size the parked run's hole, reserve it and start the copy: the run's
    /// uncovered middle goes into the canonical in front of the DVR's next
    /// chunk, and its edges are trimmed because the DVR wrote those chunks whole.
    async fn process_hole(
        &mut self,
        run: RunData,
        parked: &Parked,
        timeline: &DvrTimeline,
    ) -> Result<(), DiskOutputError> {
        let clock = self.clock.unwrap_or(Clock::Elapsed);
        let Some(run_end) = run.last_end(clock, self.live_origin) else {
            self.abandon_run(run, parked.hole_start, parked.hole_start)
                .await?;
            self.advance_past(timeline, parked.hole_start, clock);
            return Ok(());
        };
        let end = hole_end(timeline, run_end, clock, self.dvr_origin).unwrap_or(parked.hole_start);
        let hole_end = end.max(parked.hole_start);
        let plan = plan_hole(
            &run.records,
            parked.hole_start,
            hole_end,
            clock,
            self.live_origin,
        );

        if self.origin_mismatch(&run, parked, timeline, clock, hole_end) {
            self.notify_error(format!(
                "disk: DVR/live alignment mismatch around run {}; leaving a zero-filled hole",
                run.number
            ));
            self.abandon_run(run, parked.hole_start, hole_end).await?;
            self.advance_past(timeline, hole_end, clock);
            return Ok(());
        }

        if plan.segs_len == 0 || plan.records == 0 {
            // The run lies entirely inside a DVR chunk; no copy is needed.
            self.write_zero_filled_marker(&run, &HolePlan::default()).await?;
            self.delete_run(&run).await;
            self.advance_past(timeline, hole_end, clock);
            return Ok(());
        }

        // The live map belongs before the live media, so it is inserted before
        // the Run marker (and therefore before the DVR resumes).
        self.insert_run_init(&run).await?;

        let Some(canonical) = &mut self.canonical else {
            return Ok(());
        };
        let dst_segs = canonical.segs_end;
        let dst_index = canonical.index_end;

        // Rebase and renumber the kept records before reserving the hole, so a
        // failure here cannot leave a half-reserved index.
        let mut rebased =
            Vec::with_capacity(usize::try_from(plan.record_len).unwrap_or(usize::MAX));
        let mut init_counter = canonical.init_counter;
        let delta = i64::try_from(dst_segs).unwrap_or(i64::MAX)
            - i64::try_from(plan.source_segs_offset).unwrap_or(0);
        let mut kept = 0usize;
        for meta in &run.records {
            let time = record_time(&meta.segment, clock, self.live_origin);
            let record_end = time.saturating_add(record_duration(&meta.segment));
            if time >= parked.hole_start && record_end <= hole_end {
                let mut record = Record::Segment(meta.segment.clone());
                record.rebase(delta, &mut init_counter);
                record.encode(&mut rebased);
                kept += 1;
            }
        }
        if kept != plan.records {
            // The plan counted the same records; a mismatch means the metadata
            // changed mid-flight, which is a bug, not a disk condition.
            self.notify_error("disk: inconsistent run plan");
            return Err(DiskOutputError::Io(std::io::Error::other(
                "inconsistent run plan",
            )));
        }

        let marker = Record::Run(RunRecord {
            run: run.number,
            dst_segs_offset: dst_segs,
            segs_len: plan.segs_len,
            source_segs_offset: plan.source_segs_offset,
            source_index_offset: plan.source_index_offset,
            record_len: plan.record_len,
            complete: 0,
        });
        write_record(&mut canonical.index, dst_index, &marker).await?;
        canonical.index_end += u64::from(1 + RUN_BODY) + u64::from(plan.record_len);
        canonical.segs_end += plan.segs_len;
        // The marker must be durable before the DVR writes anything past it.
        canonical.index.sync_data().await?;

        self.copies.push(CopyJob {
            run: run.number,
            dst_segs,
            dst_index,
            segs_len: plan.segs_len,
            records: rebased,
            written: 0,
            segs_path: run.segs_path.clone(),
            index_path: run.index_path.clone(),
        });
        spawn_copy(
            run.number,
            run.segs_path.clone(),
            plan.source_segs_offset,
            plan.segs_len,
            self.events_tx.clone(),
        );

        info!(
            run = run.number,
            segs_len = plan.segs_len,
            records = plan.records,
            "disk: reserved run hole"
        );
        self.advance_past(timeline, hole_end, clock);
        Ok(())
    }

    async fn insert_run_init(&mut self, run: &RunData) -> Result<(), DiskOutputError> {
        let Some(init) = &run.init else {
            return Ok(());
        };
        if self
            .canonical
            .as_ref()
            .is_some_and(|canonical| canonical.active_init == Some(init.hash))
        {
            return Ok(());
        }
        let Some(bytes) = read_run_init(run).await? else {
            return Ok(());
        };
        if let Some(canonical) = &mut self.canonical {
            write_init_bytes(canonical, SOURCE_LIVE, &bytes).await?;
        }
        Ok(())
    }

    async fn abandon_run(
        &mut self,
        run: RunData,
        hole_start: u64,
        hole_end: u64,
    ) -> Result<(), DiskOutputError> {
        let clock = self.clock.unwrap_or(Clock::Elapsed);
        let plan = plan_hole(&run.records, hole_start, hole_end, clock, self.live_origin);
        self.write_zero_filled_marker(&run, &plan).await?;
        self.delete_run(&run).await;
        Ok(())
    }

    /// Reserve a complete `Run` marker whose bytes are never written, so the
    /// span stays zero-filled: the run was covered by a DVR chunk, or its hole
    /// was abandoned. Either way it is final, not pending.
    async fn write_zero_filled_marker(
        &mut self,
        run: &RunData,
        plan: &HolePlan,
    ) -> Result<(), DiskOutputError> {
        let Some(canonical) = &mut self.canonical else {
            return Ok(());
        };
        let marker = Record::Run(RunRecord {
            run: run.number,
            dst_segs_offset: canonical.segs_end,
            segs_len: plan.segs_len,
            source_segs_offset: plan.source_segs_offset,
            source_index_offset: plan.source_index_offset,
            record_len: 0,
            complete: 1,
        });
        write_record(&mut canonical.index, canonical.index_end, &marker).await?;
        canonical.index_end += u64::from(1 + RUN_BODY);
        canonical.segs_end += plan.segs_len;
        canonical.index.sync_data().await?;
        Ok(())
    }

    fn origin_mismatch(
        &self,
        run: &RunData,
        parked: &Parked,
        timeline: &DvrTimeline,
        clock: Clock,
        hole_end_time: u64,
    ) -> bool {
        if clock != Clock::Pdt {
            return false;
        }
        let Some(live) = run.origin() else {
            return false;
        };
        let Some(start_dvr) = origin_of(parked.crossing.pdt_ms, parked.crossing.start_ms) else {
            return false;
        };
        if (live - start_dvr).abs() > ORIGIN_TOLERANCE_MS {
            return true;
        }
        let end_chunk = timeline
            .segments
            .iter()
            .rev()
            .find(|chunk| chunk_time_of(chunk, clock, self.dvr_origin) <= hole_end_time);
        end_chunk
            .and_then(|chunk| origin_of(chunk.pdt_ms, chunk.start_ms))
            .is_some_and(|end_dvr| (live - end_dvr).abs() > ORIGIN_TOLERANCE_MS)
    }

    fn advance_past(&mut self, timeline: &DvrTimeline, time: u64, clock: Clock) {
        if let Some(index) = timeline
            .segments
            .iter()
            .position(|chunk| chunk_time_of(chunk, clock, self.dvr_origin) >= time)
        {
            self.dvr_next_start_ms = timeline.segments[index].start_ms;
        } else {
            self.dvr_next_start_ms = timeline.end_ms;
        }
    }

    fn run_is_closed(&self, run: u32) -> bool {
        self.closed_runs
            .iter()
            .any(|candidate| candidate.number == run)
    }

    fn run_is_open(&self, run: u32) -> bool {
        self.current_run
            .as_ref()
            .is_some_and(|candidate| candidate.number == run)
    }

    fn take_run(&mut self, run: u32) -> Option<RunData> {
        if let Some(index) = self
            .closed_runs
            .iter()
            .position(|candidate| candidate.number == run)
        {
            return self.closed_runs.remove(index);
        }
        None
    }

    async fn delete_run(&self, run: &RunData) {
        if !self.options.keep_runs {
            let _ = compio::fs::remove_file(&run.segs_path).await;
            let _ = compio::fs::remove_file(&run.index_path).await;
        }
    }

    fn timeline_done(&self, timeline: &DvrTimeline) -> bool {
        timeline.ended && self.next_chunk(timeline).is_none()
    }

    fn check_done(&mut self) {
        if self.stop || self.finished {
            return;
        }
        if self.phase.promoted() {
            if !self.live_open {
                self.finished = true;
            }
            return;
        }
        if !self.phase.dvr() {
            return;
        }
        if !self.live_open
            && self.phase.live() != Live::Draining
            && self.phase.parked().is_none()
            && self.in_flight.is_none()
            && self.copies.is_empty()
            && self.closed_runs.is_empty()
            && self.current_run.is_none()
            && self
                .dvr
                .snapshot()
                .is_some_and(|timeline| self.timeline_done(&timeline))
        {
            self.finished = true;
        }
    }

    async fn finish(&mut self) -> Result<(), DiskOutputError> {
        if let Some(canonical) = &mut self.canonical {
            canonical.segs.sync_all().await?;
            canonical.index.sync_all().await?;
        }
        if matches!(self.phase, Phase::Discovering) {
            self.promote().await?;
        }
        Ok(())
    }
}

enum Incoming {
    Live(Result<SegmentMessage, kanal::ReceiveError>),
    Event(Result<Event, kanal::ReceiveError>),
}

// ---------------------------------------------------------------------------
// Live writes and copying
// ---------------------------------------------------------------------------

async fn write_segment_body(
    file: &mut File,
    position: u64,
    reader: &mut Reader,
    hasher: &mut Xxh3,
    wake: &ConsumerWake,
) -> Result<u64, DiskOutputError> {
    let mut written = 0u64;
    let mut offset = position;
    while let Some(view) = reader.next_view(wake).await {
        let len = view.as_init().len() as u64;
        hasher.update(view.as_init());
        file.write_all_at(view, offset).await.0?;
        offset += len;
        written += len;
    }
    Ok(written)
}

#[allow(clippy::too_many_arguments)] // two files plus one cursor pair
async fn write_init(
    segs: &mut File,
    index: &mut File,
    segs_end: &mut u64,
    index_end: &mut u64,
    init_counter: &mut u64,
    source: u8,
    reader: &mut Reader,
    wake: &ConsumerWake,
) -> Result<InitRecord, DiskOutputError> {
    let mut hasher = Xxh3::new();
    let start = *segs_end;
    let written = write_segment_body(segs, start, reader, &mut hasher, wake).await?;
    let record = InitRecord {
        source,
        counter: *init_counter,
        byte_offset: start,
        byte_length: u32::try_from(written).unwrap_or(u32::MAX),
        hash: hasher.digest128(),
    };
    write_record(index, *index_end, &Record::Init(record.clone())).await?;
    *segs_end += written;
    *index_end += u64::from(1 + INIT_BODY);
    *init_counter += 1;
    Ok(record)
}

/// Write an init into the canonical unless the same map is already active.
/// Returns whether bytes were written. The `segs` sync is what makes the init
/// durable before the DVR is allowed to write past it.
async fn write_init_bytes(
    canonical: &mut Canonical,
    source: u8,
    bytes: &[u8],
) -> Result<bool, DiskOutputError> {
    let hash = xxh3_128(bytes);
    if canonical.active_init == Some(hash) {
        return Ok(false);
    }
    let start = canonical.segs_end;
    let len = bytes.len() as u64;
    canonical.segs.write_all_at(bytes.to_vec(), start).await.0?;
    canonical.segs.sync_data().await?;
    let record = InitRecord {
        source,
        counter: canonical.init_counter,
        byte_offset: start,
        byte_length: u32::try_from(bytes.len()).unwrap_or(u32::MAX),
        hash,
    };
    write_record(
        &mut canonical.index,
        canonical.index_end,
        &Record::Init(record),
    )
    .await?;
    canonical.segs_end += len;
    canonical.index_end += u64::from(1 + INIT_BODY);
    canonical.init_counter += 1;
    canonical.active_init = Some(hash);
    // The init record must be durable before any media record that depends on
    // it, not just before the next fsync of the whole index.
    canonical.index.sync_data().await?;
    Ok(true)
}

async fn read_reader_bytes(
    reader: &mut Reader,
    wake: &ConsumerWake,
    limit: usize,
) -> Result<Vec<u8>, DiskOutputError> {
    let mut out = Vec::new();
    while let Some(view) = reader.next_view(wake).await {
        let bytes = view.as_init();
        if out.len().saturating_add(bytes.len()) > limit {
            return Err(DiskOutputError::Record("init segment is too large".into()));
        }
        out.extend_from_slice(bytes);
    }
    Ok(out)
}

/// Read and verify a run's leading init from its own `segs` file.
async fn read_run_init(run: &RunData) -> Result<Option<Vec<u8>>, DiskOutputError> {
    let Some(init) = &run.init else {
        return Ok(None);
    };
    let file = OpenOptions::new().read(true).open(&run.segs_path).await?;
    let length = usize::try_from(init.byte_length).unwrap_or(usize::MAX);
    let result = file
        .read_exact_at(vec![0u8; length], init.byte_offset)
        .await;
    let bytes = result.1;
    result.0?;
    if xxh3_128(&bytes) != init.hash {
        return Err(DiskOutputError::Record(format!(
            "run {} init failed verification",
            run.number
        )));
    }
    Ok(Some(bytes))
}

async fn fetch_map(client: &Client, map: &DvrMap) -> Result<Vec<u8>, DiskOutputError> {
    let range = map.byterange.map(|(offset, length)| {
        format!(
            "bytes={offset}-{}",
            offset.saturating_add(length).saturating_sub(1)
        )
    });
    let mut response = if let Some(range) = range.as_deref() {
        client.get_with(&map.uri, &[("Range", range)]).await?
    } else {
        client.get(&map.uri).await?
    };
    if !response.status.is_success() {
        return Err(DiskOutputError::Io(std::io::Error::other(format!(
            "map fetch returned {}",
            response.status
        ))));
    }
    let partial = response.status.as_u16() == 206;
    let body = response.body.read_to_end(MAX_INIT_BYTES).await?;
    match map.byterange {
        None => Ok(body),
        // A 206 already contains exactly the requested range.
        Some((_, length)) if partial => {
            if u64::try_from(body.len()).unwrap_or(u64::MAX) != length {
                return Err(DiskOutputError::Io(std::io::Error::other(
                    "map byte range returned the wrong length",
                )));
            }
            Ok(body)
        }
        // A server that ignored Range returned the whole object.
        Some((offset, length)) => {
            let start = usize::try_from(offset).map_err(|_| {
                DiskOutputError::Io(std::io::Error::other("map range offset is too large"))
            })?;
            let length = usize::try_from(length).map_err(|_| {
                DiskOutputError::Io(std::io::Error::other("map range length is too large"))
            })?;
            let end = start
                .checked_add(length)
                .ok_or_else(|| DiskOutputError::Io(std::io::Error::other("map range overflows")))?;
            body.get(start..end).map(<[u8]>::to_vec).ok_or_else(|| {
                DiskOutputError::Io(std::io::Error::other(
                    "map byte range is outside the object",
                ))
            })
        }
    }
}

async fn write_record(
    file: &mut File,
    position: u64,
    record: &Record,
) -> Result<(), DiskOutputError> {
    let mut buffer =
        Vec::with_capacity(usize::try_from(record.encoded_len()).unwrap_or(usize::MAX));
    record.encode(&mut buffer);
    file.write_all_at(buffer, position).await.0?;
    Ok(())
}

async fn copy_between(
    source: &File,
    source_offset: u64,
    destination: &mut File,
    destination_offset: u64,
    len: u64,
) -> Result<(), DiskOutputError> {
    let mut position = 0u64;
    while position < len {
        let take = block_take(len - position);
        let buffer = vec![0u8; take];
        let result = source.read_exact_at(buffer, source_offset + position).await;
        let buffer = result.1;
        result.0?;
        destination
            .write_all_at(buffer, destination_offset + position)
            .await
            .0?;
        position += take as u64;
    }
    Ok(())
}

fn spawn_copy(run: u32, segs_path: PathBuf, source_offset: u64, len: u64, events: Sender<Event>) {
    compio::runtime::spawn(async move {
        let file = OpenOptions::new().read(true).open(&segs_path).await;
        let Ok(file) = file else {
            let _ = events.send(Event::CopyFailed {
                run,
                error: "cannot open run segs".to_owned(),
            });
            return;
        };
        let mut position = 0u64;
        while position < len {
            let take = block_take(len - position);
            let buffer = vec![0u8; take];
            let result = file.read_exact_at(buffer, source_offset + position).await;
            let buffer = result.1;
            if let Err(error) = result.0 {
                let _ = events.send(Event::CopyFailed {
                    run,
                    error: error.to_string(),
                });
                return;
            }
            if events.send(Event::CopyPart { run, data: buffer }).is_err() {
                return;
            }
            position += take as u64;
        }
        let _ = events.send(Event::CopyDone { run });
    })
    .detach();
}

fn spawn_fetch(
    chunk: DvrSegment,
    arena: Rc<ArenaSet>,
    wakes: Rc<WakeSet>,
    downloaders: AsyncSender<ToDownloader>,
    events: Sender<Event>,
) {
    compio::runtime::spawn(async move {
        let (writer, mut reader) = match Spool::open_with_cap(&arena, &wakes, DVR_SLOTS_MAX) {
            Ok(pair) => pair,
            Err(error) => {
                warn!(%error, "disk: DVR spool allocation failed");
                let _ = events.send(Event::ChunkDone(FetchOutcome::Failed));
                return;
            }
        };
        let (done_tx, done_rx) = kanal::bounded(1);
        let span = debug_span!("dvr chunk", sequence = chunk.sequence);
        let send = downloaders
            .send(ToDownloader::FetchURL {
                url: chunk.url.into(),
                writer,
                span,
                done: Some(done_tx),
                stats: None,
            })
            .await;
        if send.is_err() {
            let _ = events.send(Event::ChunkDone(FetchOutcome::Failed));
            return;
        }

        let wake = ConsumerWake::new();
        wakes.register(&wake);
        while let Some(view) = reader.next_view(&wake).await {
            if events.send(Event::ChunkPart(view)).is_err() {
                return;
            }
        }
        // The downloader sends `done` before it drops the writer, so waiting
        // for it is not optional -- but it must not be a blocking `recv()`, which
        // parks the only reactor thread the downloader needs to make progress.
        let outcome = done_rx
            .to_async()
            .recv()
            .await
            .unwrap_or(FetchOutcome::Failed);
        let _ = events.send(Event::ChunkDone(outcome));
    })
    .detach();
}

// ---------------------------------------------------------------------------
// Canonical open/create and resume repair
// ---------------------------------------------------------------------------

struct Snapshot {
    records: Vec<ParsedRecord>,
    valid_end: u64,
    segs_end: u64,
    init_counter: u64,
    active_init: Option<u128>,
    next_run: u32,
    live_origin: Option<i64>,
    has_dvr: bool,
    resume_abs: u64,
    resume_clock: Option<Clock>,
}

async fn read_canonical(
    dir: &Path,
    stream_id: u64,
    keep_runs: bool,
) -> Result<Option<Snapshot>, DiskOutputError> {
    let segs_path = dir.join("segs");
    let index_path = dir.join("index");
    let mut segs = match OpenOptions::new()
        .read(true)
        .write(true)
        .open(&segs_path)
        .await
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut index = match OpenOptions::new()
        .read(true)
        .write(true)
        .open(&index_path)
        .await
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    let index_len = index.metadata().await?.len();
    if index_len == 0 {
        return Ok(None);
    }
    let result = index.read_to_end_at(Vec::new(), 0).await;
    let bytes = result.1;
    result.0?;

    let header_stream = parse_header(&bytes)?;
    if header_stream != stream_id {
        return Err(DiskOutputError::InvalidHeader);
    }
    let mut records = parse_records(&bytes)?;

    // Verify the last hash-bearing record and walk back while it is torn.
    let mut valid = records.len();
    while valid > 0 {
        let record = &records[valid - 1].record;
        let Some((offset, len, expected)) = record.hashed_range() else {
            valid -= 1;
            continue;
        };
        if hash_range(&segs, offset, len).await == Some(expected) {
            break;
        }
        valid -= 1;
    }
    records.truncate(valid);

    let mut valid_end = HEADER_LEN as u64;
    let mut segs_max = 0u64;
    let mut init_counter = 0u64;
    let mut next_run = 0u32;
    let mut has_dvr = false;
    let mut resume_pdt = 0u64;
    let mut resume_elapsed = 0u64;
    let mut live_first_pdt = 0u64;
    let mut live_first_elapsed = 0u64;
    for parsed in &records {
        valid_end = parsed.offset + parsed.span;
        match &parsed.record {
            Record::Segment(segment) => {
                segs_max = segs_max.max(
                    segment
                        .byte_offset
                        .saturating_add(u64::from(segment.byte_length)),
                );
                match segment.source {
                    SOURCE_DVR => {
                        has_dvr = true;
                        let end = if segment.pdt_ms != 0 {
                            segment
                                .pdt_ms
                                .saturating_add(u64::from(segment.duration_ms))
                        } else {
                            u64::from(segment.elapsed_ms)
                                .saturating_add(u64::from(segment.duration_ms))
                        };
                        if segment.pdt_ms != 0 {
                            resume_pdt = resume_pdt.max(end);
                        } else {
                            resume_elapsed = resume_elapsed.max(end);
                        }
                    }
                    SOURCE_LIVE => {
                        if live_first_pdt == 0
                            && let Some(pdt) = (segment.pdt_ms != 0).then_some(segment.pdt_ms)
                        {
                            live_first_pdt = pdt;
                            live_first_elapsed = segment.elapsed_ms.into();
                        }
                    }
                    _ => {}
                }
            }
            Record::Init(init) => {
                segs_max =
                    segs_max.max(init.byte_offset.saturating_add(u64::from(init.byte_length)));
                init_counter = init_counter.max(init.counter.saturating_add(1));
            }
            Record::Run(run) => {
                segs_max = segs_max.max(run.dst_segs_offset.saturating_add(run.segs_len));
                next_run = next_run.max(run.run.saturating_add(1));
            }
            Record::Discontinuity(_) => {}
        }
    }

    index.set_len(valid_end).await?;
    segs.set_len(segs_max).await?;

    // Redo pending copies. The run pair is kept until the marker is complete, so
    // the source is always still on disk.
    for position in 0..records.len() {
        let Record::Run(run) = records[position].record.clone() else {
            continue;
        };
        if run.complete != 0 {
            continue;
        }
        let marker_offset = records[position].offset;
        let (segs_path, index_path) = run_paths(dir, run.run);
        let source_exists = compio::fs::metadata(&segs_path).await.is_ok()
            && compio::fs::metadata(&index_path).await.is_ok();
        if !source_exists {
            // The pair is gone: abandon the hole, leaving zeros and no records.
            flip_complete(&mut index, marker_offset).await?;
            if let Record::Run(run) = &mut records[position].record {
                run.complete = 1;
            }
            warn!(run = run.run, "disk: pending run has no source; abandoned");
            continue;
        }

        let source_index = OpenOptions::new().read(true).open(&index_path).await?;
        let result = source_index
            .read_exact_at(
                vec![0u8; usize::try_from(run.record_len).unwrap_or(usize::MAX)],
                run.source_index_offset,
            )
            .await;
        let buffer = result.1;
        if let Err(error) = result.0 {
            warn!(run = run.run, %error, "disk: pending run index unreadable; abandoned");
            flip_complete(&mut index, marker_offset).await?;
            if let Record::Run(run) = &mut records[position].record {
                run.complete = 1;
            }
            continue;
        }

        let mut init_base = canonical_init_base(&records, marker_offset);
        let mut rebased = Vec::with_capacity(buffer.len());
        let mut cursor: &[u8] = &buffer;
        while !cursor.is_empty() {
            let tag = cursor[0];
            let Some(body) = body_len(tag) else {
                break;
            };
            let body = body as usize;
            if cursor.len() < 1 + body {
                break;
            }
            let mut reader = &cursor[1..=body];
            if let Ok(mut record) = Record::decode(tag, &mut reader) {
                record.rebase(
                    i64::try_from(run.dst_segs_offset).unwrap_or(i64::MAX)
                        - i64::try_from(run.source_segs_offset).unwrap_or(0),
                    &mut init_base,
                );
                record.encode(&mut rebased);
            }
            cursor = &cursor[1 + body..];
        }

        let source_segs = OpenOptions::new().read(true).open(&segs_path).await?;
        copy_between(
            &source_segs,
            run.source_segs_offset,
            &mut segs,
            run.dst_segs_offset,
            run.segs_len,
        )
        .await?;
        index
            .write_all_at(rebased, marker_offset + u64::from(1 + RUN_BODY))
            .await
            .0?;
        segs.sync_data().await?;
        flip_complete(&mut index, marker_offset).await?;
        index.sync_data().await?;
        let _ = compio::fs::remove_file(&segs_path).await;
        let _ = compio::fs::remove_file(&index_path).await;
        if let Record::Run(run) = &mut records[position].record {
            run.complete = 1;
        }
        info!(run = run.run, "disk: recovered pending run hole");
    }

    // A crash between flipping `complete` and deleting the pair leaves a
    // completed run behind. It is redundant now.
    if !keep_runs {
        for parsed in &records {
            if let Record::Run(run) = &parsed.record
                && run.complete != 0
            {
                let (segs_path, index_path) = run_paths(dir, run.run);
                let _ = compio::fs::remove_file(&segs_path).await;
                let _ = compio::fs::remove_file(&index_path).await;
            }
        }
    }

    let live_origin =
        (live_first_pdt != 0).then(|| origin_of(live_first_pdt, live_first_elapsed).unwrap_or(0));
    // Every `Init` is top-level (a run's leading map is inserted before its
    // marker), so the last one is the map in effect at the tail. Verify them all:
    // there are few, and a torn init must not be played as garbage.
    let mut active_init = None;
    for parsed in &records {
        if let Record::Init(init) = &parsed.record {
            if hash_range(&segs, init.byte_offset, init.byte_length).await != Some(init.hash) {
                return Err(DiskOutputError::Record(format!(
                    "init record at {} failed verification",
                    parsed.offset
                )));
            }
            active_init = Some(init.hash);
        }
    }
    let (resume_abs, resume_clock) = if resume_pdt != 0 {
        (resume_pdt, Some(Clock::Pdt))
    } else if resume_elapsed != 0 {
        (resume_elapsed, Some(Clock::Elapsed))
    } else {
        (0, None)
    };
    Ok(Some(Snapshot {
        records,
        valid_end,
        segs_end: segs_max,
        init_counter,
        active_init,
        next_run: next_run.max(scan_run_number(dir)),
        live_origin,
        has_dvr,
        resume_abs,
        resume_clock,
    }))
}

async fn hash_range(file: &File, offset: u64, len: u32) -> Option<u128> {
    let buffer = vec![0u8; usize::try_from(len).unwrap_or(usize::MAX)];
    let result = file.read_exact_at(buffer, offset).await;
    result.0.ok().map(|()| xxh3_128(&result.1))
}

async fn flip_complete(index: &mut File, marker_offset: u64) -> std::io::Result<()> {
    index
        .write_all_at(vec![1u8], marker_offset + RUN_COMPLETE_OFFSET)
        .await
        .0
}

fn canonical_init_base(records: &[ParsedRecord], marker_offset: u64) -> u64 {
    let mut base = 0u64;
    for parsed in records {
        if parsed.offset >= marker_offset {
            break;
        }
        if let Record::Init(init) = &parsed.record {
            base = base.max(init.counter.saturating_add(1));
        }
    }
    base
}

fn scan_run_number(dir: &Path) -> u32 {
    let mut highest = 0u32;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = name.strip_prefix("live.") else {
            continue;
        };
        let Some(number) = rest
            .strip_suffix(".segs")
            .or_else(|| rest.strip_suffix(".index"))
        else {
            continue;
        };
        if let Ok(number) = number.parse::<u32>() {
            highest = highest.max(number.saturating_add(1));
        }
    }
    highest
}

async fn create_canonical(dir: &Path, stream_id: u64) -> Result<Canonical, DiskOutputError> {
    let segs = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(dir.join("segs"))
        .await?;
    let mut index = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(dir.join("index"))
        .await?;
    index.write_all_at(encode_header(stream_id), 0).await.0?;
    Ok(Canonical {
        segs,
        index,
        segs_end: 0,
        index_end: HEADER_LEN as u64,
        init_counter: 0,
        active_init: None,
    })
}

async fn open_canonical(dir: &Path, snapshot: &Snapshot) -> Result<Canonical, DiskOutputError> {
    let segs = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join("segs"))
        .await?;
    let index = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join("index"))
        .await?;
    Ok(Canonical {
        segs,
        index,
        segs_end: snapshot.segs_end,
        index_end: snapshot.valid_end,
        init_counter: snapshot.init_counter,
        active_init: snapshot.active_init,
    })
}

async fn rename_canonical_to_run(
    dir: &Path,
    number: u32,
    snapshot: &Snapshot,
) -> Result<RunData, DiskOutputError> {
    let (segs_path, index_path) = run_paths(dir, number);
    compio::fs::rename(dir.join("segs"), &segs_path).await?;
    compio::fs::rename(dir.join("index"), &index_path).await?;

    let mut records = Vec::new();
    let mut first = None;
    let mut last = None;
    let mut init = None;
    let mut first_media_offset = None;
    for parsed in &snapshot.records {
        if let Record::Init(record) = &parsed.record
            && first_media_offset.is_none()
        {
            // The map in effect for the first live media. Later maps inside the
            // promoted file are not split retroactively; they stay as bytes.
            init = Some(record.clone());
        }
        if let Record::Segment(segment) = &parsed.record {
            if segment.source != SOURCE_LIVE {
                continue;
            }
            first_media_offset.get_or_insert(segment.byte_offset);
            records.push(RunRecordMeta {
                segment: segment.clone(),
                record_offset: parsed.offset,
                record_len: parsed.record.encoded_len(),
            });
            first.get_or_insert_with(|| segment.clone());
            last = Some(segment.clone());
        }
    }
    Ok(RunData {
        number,
        segs_path,
        index_path,
        records,
        segs_len: snapshot.segs_end,
        index_len: snapshot.valid_end,
        init_counter: snapshot.init_counter,
        init,
        files: None,
        first_segment: first,
        last_segment: last,
    })
}

fn run_paths(dir: &Path, number: u32) -> (PathBuf, PathBuf) {
    (
        dir.join(format!("live.{number:03}.segs")),
        dir.join(format!("live.{number:03}.index")),
    )
}

fn ms_u32(value: u64) -> u32 {
    if value > u64::from(u32::MAX) {
        u32::MAX
    } else {
        u32::try_from(value).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(
        source: u8,
        pdt: u64,
        elapsed: u64,
        duration: u64,
        offset: u64,
        len: u32,
    ) -> SegmentRecord {
        SegmentRecord {
            source,
            sequence: 0,
            elapsed_ms: ms_u32(elapsed),
            pdt_ms: pdt,
            duration_ms: ms_u32(duration),
            byte_offset: offset,
            byte_length: len,
            hash: 0,
            flags: 0,
        }
    }

    #[test]
    fn v4_records_round_trip() {
        let records = [
            Record::Segment(segment(SOURCE_LIVE, 1_794_119_852_191, 0, 2_000, 0, 1_000)),
            Record::Init(InitRecord {
                source: SOURCE_DVR,
                counter: 7,
                byte_offset: 10,
                byte_length: 20,
                hash: 42,
            }),
            Record::Discontinuity(DiscontinuityRecord {
                width: 1920,
                height: 1080,
                rate_num: 60,
                rate_denom: 1,
            }),
            Record::Run(RunRecord {
                run: 3,
                dst_segs_offset: 1,
                segs_len: 2,
                source_segs_offset: 3,
                source_index_offset: 4,
                record_len: 5,
                complete: 1,
            }),
        ];

        for record in records {
            let mut encoded = Vec::new();
            record.encode(&mut encoded);
            assert_eq!(
                u32::try_from(encoded.len()).expect("record length"),
                record.encoded_len()
            );
            let mut reader = &encoded[1..];
            let decoded = Record::decode(encoded[0], &mut reader).expect("decode");
            assert_eq!(decoded, record);
        }
    }

    #[test]
    fn header_round_trips_and_rejects_old_versions() {
        let header = encode_header(123);
        assert_eq!(parse_header(&header).expect("header"), 123);
        let mut old = header;
        old[8..12].copy_from_slice(&3u32.to_le_bytes());
        assert!(parse_header(&old).is_err());
    }

    #[test]
    fn prefill_and_hole_boundaries_follow_chunk_edges() {
        let timeline = DvrTimeline {
            segments: vec![
                DvrSegment {
                    start_ms: 0,
                    duration_ms: 10_000,
                    pdt_ms: 1_000_000,
                    sequence: 0,
                    url: "a".into(),
                    map: None,
                },
                DvrSegment {
                    start_ms: 10_000,
                    duration_ms: 10_000,
                    pdt_ms: 1_010_000,
                    sequence: 1,
                    url: "b".into(),
                    map: None,
                },
                DvrSegment {
                    start_ms: 20_000,
                    duration_ms: 10_000,
                    pdt_ms: 1_020_000,
                    sequence: 2,
                    url: "c".into(),
                    map: None,
                },
            ],
            maps: Vec::new(),
            end_ms: 30_000,
            ended: false,
        };
        // Mid-chunk end: resume at the chunk's start.
        assert_eq!(
            hole_end(&timeline, 1_015_000, Clock::Pdt, Some(1_000_000)),
            Some(1_010_000)
        );
        assert_eq!(
            hole_end(&timeline, 1_020_000, Clock::Pdt, Some(1_000_000)),
            Some(1_020_000)
        );
    }

    #[test]
    fn plan_trims_the_records_the_dvr_edges_supply() {
        let records: Vec<RunRecordMeta> = [
            (1_000_000, 0u64, 2_000u64, 100u32),
            (1_002_000, 100, 2_000, 100),
            (1_004_000, 200, 2_000, 100),
            (1_006_000, 300, 2_000, 100),
            (1_008_000, 400, 2_000, 100),
        ]
        .into_iter()
        .map(|(pdt, offset, duration, len)| RunRecordMeta {
            segment: segment(SOURCE_LIVE, pdt, pdt - 1_000_000, duration, offset, len),
            record_offset: HEADER_LEN as u64 + offset / 50,
            record_len: 1 + SEGMENT_BODY,
        })
        .collect();

        // DVR covers [0, 1_004_000) and [1_010_000, ...): keep only the records
        // fully inside the hole.
        let plan = plan_hole(&records, 1_004_000, 1_010_000, Clock::Pdt, Some(1_000_000));
        assert_eq!(plan.records, 3);
        // Records at 1_004_000, 1_006_000, 1_008_000.
        assert_eq!(plan.source_segs_offset, 200);
        assert_eq!(plan.segs_len, 300);
        assert_eq!(plan.record_len, 3 * (1 + SEGMENT_BODY));
    }

    #[test]
    fn plan_is_empty_when_the_run_fits_in_one_chunk() {
        let records = vec![RunRecordMeta {
            segment: segment(SOURCE_LIVE, 1_000_000, 0, 2_000, 0, 100),
            record_offset: HEADER_LEN as u64,
            record_len: 1 + SEGMENT_BODY,
        }];
        assert_eq!(
            plan_hole(&records, 1_010_000, 1_010_000, Clock::Pdt, None),
            HolePlan::default()
        );
    }

    #[test]
    fn rebase_offsets_and_renumbers_init_counters() {
        let mut init = Record::Init(InitRecord {
            source: SOURCE_LIVE,
            counter: 9,
            byte_offset: 100,
            byte_length: 10,
            hash: 0,
        });
        let mut counter = 3;
        init.rebase(50, &mut counter);
        assert_eq!(
            init,
            Record::Init(InitRecord {
                source: SOURCE_LIVE,
                counter: 3,
                byte_offset: 150,
                byte_length: 10,
                hash: 0,
            })
        );
        assert_eq!(counter, 4);

        let mut segment = Record::Segment(segment(SOURCE_LIVE, 0, 0, 2_000, 500, 10));
        segment.rebase(-100, &mut counter);
        let Record::Segment(segment) = segment else {
            unreachable!()
        };
        assert_eq!(segment.byte_offset, 400);
    }

    #[test]
    fn a_torn_tail_is_ignored_by_the_parser() {
        let mut bytes = encode_header(0);
        let record = Record::Segment(segment(SOURCE_DVR, 1, 0, 2_000, 0, 10));
        record.encode(&mut bytes);
        let complete = bytes.len();
        record.encode(&mut bytes);
        bytes.truncate(bytes.len() - 3);

        let parsed = parse_records(&bytes).expect("parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].span, u64::from(1 + SEGMENT_BODY));
        assert!(bytes.len() > complete);
    }

    #[test]
    fn a_run_marker_skips_its_reserved_area() {
        let mut bytes = encode_header(0);
        let run = Record::Run(RunRecord {
            run: 1,
            dst_segs_offset: 0,
            segs_len: 0,
            source_segs_offset: 0,
            source_index_offset: 0,
            record_len: 20,
            complete: 0,
        });
        run.encode(&mut bytes);
        bytes.extend_from_slice(&[0u8; 20]);
        let after = Record::Segment(segment(SOURCE_DVR, 2, 0, 2_000, 0, 5));
        after.encode(&mut bytes);

        let parsed = parse_records(&bytes).expect("parse");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].span, u64::from(1 + RUN_BODY) + 20);
        assert_eq!(parsed[1].offset, parsed[0].offset + parsed[0].span);
    }

    #[cfg(not(miri))]
    #[test]
    fn resume_redoes_a_pending_run_hole() {
        use std::fs;

        let dir = std::env::temp_dir().join(format!(
            "twitch-disk-resume-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("dir");

        let before = b"0123456789";
        let middle = b"abcdefghij";
        let after = b"KLMNOPQRST";

        let mut canonical_segs = Vec::new();
        canonical_segs.extend_from_slice(before);
        canonical_segs.extend_from_slice(&[0u8; 10]); // reserved hole
        canonical_segs.extend_from_slice(after);

        let mut canonical_index = encode_header(0);
        Record::Segment(SegmentRecord {
            source: SOURCE_DVR,
            sequence: 0,
            elapsed_ms: 0,
            pdt_ms: 1_000_000,
            duration_ms: 10_000,
            byte_offset: 0,
            byte_length: 10,
            hash: xxh3_128(before),
            flags: 0,
        })
        .encode(&mut canonical_index);
        Record::Run(RunRecord {
            run: 0,
            dst_segs_offset: 10,
            segs_len: 10,
            source_segs_offset: 0,
            source_index_offset: HEADER_LEN as u64,
            record_len: 1 + SEGMENT_BODY,
            complete: 0,
        })
        .encode(&mut canonical_index);
        canonical_index.extend_from_slice(&[0u8; (1 + SEGMENT_BODY) as usize]);
        Record::Segment(SegmentRecord {
            source: SOURCE_DVR,
            sequence: 1,
            elapsed_ms: 20_000,
            pdt_ms: 1_020_000,
            duration_ms: 10_000,
            byte_offset: 20,
            byte_length: 10,
            hash: xxh3_128(after),
            flags: 0,
        })
        .encode(&mut canonical_index);

        fs::write(dir.join("segs"), &canonical_segs).expect("segs");
        fs::write(dir.join("index"), &canonical_index).expect("index");

        let mut run_index = encode_header(0);
        Record::Segment(SegmentRecord {
            source: SOURCE_LIVE,
            sequence: 7,
            elapsed_ms: 10_000,
            pdt_ms: 1_010_000,
            duration_ms: 10_000,
            byte_offset: 0,
            byte_length: 10,
            hash: xxh3_128(middle),
            flags: 0,
        })
        .encode(&mut run_index);
        let (segs_path, index_path) = run_paths(&dir, 0);
        fs::write(&segs_path, middle).expect("run segs");
        fs::write(&index_path, &run_index).expect("run index");

        let snapshot = compio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(read_canonical(&dir, 0, false))
            .expect("canonical")
            .expect("snapshot");

        assert!(snapshot.has_dvr);
        let Record::Run(run) = &snapshot.records[1].record else {
            panic!("second record is the run marker");
        };
        assert_eq!(run.complete, 1);
        assert_eq!(snapshot.segs_end, 30);
        assert!(!segs_path.exists(), "the source pair is deleted");
        assert!(!index_path.exists(), "the source pair is deleted");

        let segs = fs::read(dir.join("segs")).expect("segs");
        assert_eq!(&segs[10..20], middle);
        // The copied record is rebased into the hole and readable.
        let index = fs::read(dir.join("index")).expect("index");
        let marker_offset = usize::try_from(snapshot.records[1].offset).expect("marker offset");
        let mut reader = &index[marker_offset + 1 + RUN_BODY as usize
            ..marker_offset + 1 + RUN_BODY as usize + (1 + SEGMENT_BODY) as usize];
        let tag = reader[0];
        reader = &reader[1..];
        let Record::Segment(copied) = Record::decode(tag, &mut reader).expect("copied record")
        else {
            panic!("copied segment");
        };
        assert_eq!(copied.byte_offset, 10);
        assert_eq!(copied.sequence, 7);
        assert_eq!(copied.hash, xxh3_128(middle));

        fs::remove_dir_all(&dir).ok();
    }

    #[cfg(not(miri))]
    #[test]
    fn init_insertion_is_hash_gated_and_shifts_offsets() {
        use std::fs;

        let dir = std::env::temp_dir().join(format!(
            "twitch-disk-init-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("dir");

        compio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(async {
                let mut canonical = create_canonical(&dir, 0).await.expect("canonical");
                let first = b"ftypA";
                let second = b"ftypBBBB";

                assert!(
                    write_init_bytes(&mut canonical, SOURCE_DVR, first)
                        .await
                        .expect("first init")
                );
                let segs_after_first = canonical.segs_end;
                let index_after_first = canonical.index_end;

                // The same map is already active: no bytes, no record.
                assert!(
                    !write_init_bytes(&mut canonical, SOURCE_DVR, first)
                        .await
                        .expect("duplicate init")
                );
                assert_eq!(canonical.segs_end, segs_after_first);
                assert_eq!(canonical.index_end, index_after_first);

                assert!(
                    write_init_bytes(&mut canonical, SOURCE_LIVE, second)
                        .await
                        .expect("second init")
                );
                assert_eq!(
                    canonical.segs_end,
                    segs_after_first + u64::try_from(second.len()).unwrap()
                );
                assert_eq!(
                    canonical.index_end,
                    index_after_first + u64::from(1 + INIT_BODY)
                );
                assert_eq!(canonical.active_init, Some(xxh3_128(second)));

                drop(canonical);
                let snapshot = read_canonical(&dir, 0, false)
                    .await
                    .expect("read")
                    .expect("snapshot");
                assert_eq!(snapshot.active_init, Some(xxh3_128(second)));
                let inits: Vec<_> = snapshot
                    .records
                    .iter()
                    .filter_map(|parsed| match &parsed.record {
                        Record::Init(init) => Some(init.clone()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(inits.len(), 2);
                assert_eq!(inits[0].counter, 0);
                assert_eq!(inits[1].counter, 1);
                assert_eq!(inits[1].byte_offset, segs_after_first);
                assert_eq!(inits[1].hash, xxh3_128(second));
            });

        fs::remove_dir_all(&dir).ok();
    }
}
