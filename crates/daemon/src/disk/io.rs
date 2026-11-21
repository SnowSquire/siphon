//! Writers, copies and the `EXT-X-MAP` fetch.

use std::path::PathBuf;
use std::rc::Rc;

use compio::buf::IoBuf;
use compio::fs::File;
use compio::fs::OpenOptions;
use compio::io::AsyncReadAtExt;
use compio::io::AsyncWriteAtExt;
use kanal::AsyncSender;
use kanal::Sender;
use tracing::debug;
use tracing::debug_span;
use tracing::warn;
use xxhash_rust::xxh3::Xxh3;
use xxhash_rust::xxh3::xxh3_128;

use super::Canonical;
use super::DiskOutputError;
use super::Event;
use super::MAX_INIT_BYTES;
use super::RunData;
use super::block_take;
use super::index::DISCONTINUITY_BODY;
use super::index::DiscontinuityRecord;
use super::index::INIT_BODY;
use super::index::InitRecord;
use super::index::Record;
use super::index::SEGMENT_BODY;
use super::index::SOURCE_LIVE;
use super::index::SegmentRecord;
use super::plan::Clock;
use super::plan::DiscontinuityMark;
use super::resume::ms_u32;
use crate::constants::DVR_SLOTS_MAX;
use crate::downloader::FetchOutcome;
use crate::downloader::ToDownloader;
use crate::http::Client;
use crate::metrics::DiskMetrics;
use crate::rtsp::dvr::DvrMap;
use crate::rtsp::dvr::DvrSegment;
use crate::scheduler::Segment;
use crate::spool::ArenaSet;
use crate::spool::ConsumerWake;
use crate::spool::Reader;
use crate::spool::Spool;
use crate::spool::WakeSet;

// Live writes and copying

/// The smallest possible media fragment: a `moof` box header plus the `mdat`
/// header is 24 bytes (a TS packet is 188). Twitch closes a live playlist with a
/// `#EXTINF:1.000` entry whose body is a single 8-byte `skip` box carrying no
/// media at all, and it counts that second in its own `TOTAL-SECS` - the DVR does
/// not. Writing it would put a second in the index that no source can fill.
const MIN_FRAGMENT_BYTES: u64 = 24;

/// Append one live segment's bytes and its `Segment` record at the cursor.
/// Returns `None` when the segment is a failed download or an empty placeholder;
/// the next timestamp gap then closes the run and the DVR fills the span.
pub(super) async fn append_live_segment(
    segs: &mut File,
    index: &mut File,
    segs_end: &mut u64,
    index_end: &mut u64,
    segment: Segment,
    wake: &ConsumerWake,
    metrics: &DiskMetrics,
) -> Result<Option<SegmentRecord>, DiskOutputError> {
    let mut hasher = Xxh3::new();
    let start = *segs_end;
    let mut reader = segment.reader;
    let written = write_segment_body(segs, start, &mut reader, &mut hasher, wake).await?;
    if written < MIN_FRAGMENT_BYTES {
        // The bytes were written past `segs_end` and are simply not indexed, so
        // the next segment overwrites them.
        debug!(bytes = written, "disk: fragment carries no media, skipping");
        return Ok(None);
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
    *segs_end += written;
    metrics.note_written(segment.timestamp_ms, written);
    write_record(index, *index_end, &Record::Segment(record.clone())).await?;
    *index_end += u64::from(1 + SEGMENT_BODY);
    Ok(Some(record))
}

pub(super) async fn write_segment_body(
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
pub(super) async fn write_init(
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
pub(super) async fn write_init_bytes(
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

pub(super) async fn read_reader_bytes(
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
pub(super) async fn read_run_init(run: &RunData) -> Result<Option<Vec<u8>>, DiskOutputError> {
    let Some(init) = &run.init else {
        return Ok(None);
    };
    let file = OpenOptions::new().read(true).open(&run.segs_path).await?;
    let bytes = read_exact_at(
        &file,
        init.byte_offset,
        usize::try_from(init.byte_length).unwrap_or(usize::MAX),
    )
    .await?;
    if xxh3_128(&bytes) != init.hash {
        return Err(DiskOutputError::Record(format!(
            "run {} init failed verification",
            run.number
        )));
    }
    Ok(Some(bytes))
}

pub(super) async fn fetch_map(client: &Client, map: &DvrMap) -> Result<Vec<u8>, DiskOutputError> {
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

/// Write a `Discontinuity` record for every mark the archive has reached.
/// The record's position in the index is the information: everything before it
/// belongs to the old timestamp domain. Its geometry fields are reserved (zero)
/// until a demuxer has a reason to fill them.
pub(super) async fn write_discontinuities(
    canonical: &mut Canonical,
    marks: &mut Vec<DiscontinuityMark>,
    upto: u64,
    clock: Clock,
    live_origin: Option<i64>,
) -> Result<(), DiskOutputError> {
    let mut placed = 0;
    while marks
        .first()
        .is_some_and(|mark| mark.time(clock, live_origin) <= upto)
    {
        write_record(
            &mut canonical.index,
            canonical.index_end,
            &Record::Discontinuity(DiscontinuityRecord {
                width: 0,
                height: 0,
                rate_num: 0,
                rate_denom: 0,
            }),
        )
        .await?;
        canonical.index_end += u64::from(1 + DISCONTINUITY_BODY);
        marks.remove(0);
        placed += 1;
    }
    if placed > 0 {
        canonical.index.sync_data().await?;
    }
    Ok(())
}

pub(super) async fn write_record(
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

/// Read exactly `len` bytes at `offset`.
pub(super) async fn read_exact_at(
    file: &File,
    offset: u64,
    len: usize,
) -> std::io::Result<Vec<u8>> {
    let result = file.read_exact_at(vec![0u8; len], offset).await;
    result.0.map(|()| result.1)
}

/// A file region read in fixed blocks. The block size and the cursor live here
/// so every copy implementation shares them.
pub(super) struct Blocks<'a> {
    pub(super) file: &'a File,
    pub(super) offset: u64,
    pub(super) remaining: u64,
}

impl<'a> Blocks<'a> {
    const fn new(file: &'a File, offset: u64, len: u64) -> Self {
        Self {
            file,
            offset,
            remaining: len,
        }
    }

    async fn next(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let take = block_take(self.remaining);
        let buffer = read_exact_at(self.file, self.offset, take).await?;
        self.offset += take as u64;
        self.remaining -= take as u64;
        Ok(Some(buffer))
    }
}

pub(super) async fn copy_between(
    source: &File,
    source_offset: u64,
    destination: &mut File,
    destination_offset: u64,
    len: u64,
) -> Result<(), DiskOutputError> {
    let mut blocks = Blocks::new(source, source_offset, len);
    let mut position = 0u64;
    while let Some(buffer) = blocks.next().await? {
        let take = buffer.len() as u64;
        destination
            .write_all_at(buffer, destination_offset + position)
            .await
            .0?;
        position += take;
    }
    Ok(())
}

pub(super) fn spawn_copy(
    run: u32,
    segs_path: PathBuf,
    source_offset: u64,
    len: u64,
    events: Sender<Event>,
) {
    compio::runtime::spawn(async move {
        let file = OpenOptions::new().read(true).open(&segs_path).await;
        let Ok(file) = file else {
            let _ = events.send(Event::CopyFailed {
                run,
                error: "cannot open run segs".to_owned(),
            });
            return;
        };
        let mut blocks = Blocks::new(&file, source_offset, len);
        loop {
            let buffer = match blocks.next().await {
                Ok(Some(buffer)) => buffer,
                Ok(None) => break,
                Err(error) => {
                    let _ = events.send(Event::CopyFailed {
                        run,
                        error: error.to_string(),
                    });
                    return;
                }
            };
            if events.send(Event::CopyPart { run, data: buffer }).is_err() {
                return;
            }
        }
        let _ = events.send(Event::CopyDone { run });
    })
    .detach();
}

pub(super) fn spawn_fetch(
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
