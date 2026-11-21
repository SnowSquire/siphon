//! Canonical open/create and the resume repair path.

use std::path::Path;
use std::path::PathBuf;

use compio::fs::File;
use compio::fs::OpenOptions;
use compio::io::AsyncReadAtExt;
use compio::io::AsyncWriteAtExt;
use tracing::info;
use tracing::warn;
use xxhash_rust::xxh3::xxh3_128;

use super::Canonical;
use super::DiskOutputError;
use super::RunData;
use super::index::HEADER_LEN;
use super::index::ParsedRecord;
use super::index::RUN_BODY;
use super::index::RUN_COMPLETE_OFFSET;
use super::index::Record;
use super::index::SOURCE_DVR;
use super::index::SOURCE_LIVE;
use super::index::body_len;
use super::index::encode_header;
use super::index::parse_header;
use super::index::parse_records;
use super::io::copy_between;
use super::io::read_exact_at;
use super::plan::Clock;
use super::plan::RunRecordMeta;
use super::plan::origin_of;

// Canonical open/create and resume repair

pub(super) struct Snapshot {
    pub(super) records: Vec<ParsedRecord>,
    pub(super) valid_end: u64,
    pub(super) segs_end: u64,
    pub(super) init_counter: u64,
    pub(super) active_init: Option<u128>,
    pub(super) next_run: u32,
    pub(super) live_origin: Option<i64>,
    pub(super) has_dvr: bool,
    pub(super) resume_abs: u64,
    pub(super) resume_clock: Option<Clock>,
}

pub(super) async fn read_canonical(
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
        let buffer = read_exact_at(
            &source_index,
            run.source_index_offset,
            usize::try_from(run.record_len).unwrap_or(usize::MAX),
        )
        .await;
        let Ok(buffer) = buffer else {
            let error = buffer.unwrap_err();
            warn!(run = run.run, %error, "disk: pending run index unreadable; abandoned");
            flip_complete(&mut index, marker_offset).await?;
            if let Record::Run(run) = &mut records[position].record {
                run.complete = 1;
            }
            continue;
        };

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
        if !keep_runs {
            let _ = compio::fs::remove_file(&segs_path).await;
            let _ = compio::fs::remove_file(&index_path).await;
        }
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

pub(super) async fn hash_range(file: &File, offset: u64, len: u32) -> Option<u128> {
    let buffer = read_exact_at(file, offset, usize::try_from(len).unwrap_or(usize::MAX))
        .await
        .ok()?;
    Some(xxh3_128(&buffer))
}

pub(super) async fn flip_complete(index: &mut File, marker_offset: u64) -> std::io::Result<()> {
    index
        .write_all_at(vec![1u8], marker_offset + RUN_COMPLETE_OFFSET)
        .await
        .0
}

pub(super) fn canonical_init_base(records: &[ParsedRecord], marker_offset: u64) -> u64 {
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

pub(super) fn scan_run_number(dir: &Path) -> u32 {
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

/// Create (or truncate) a `segs`+`index` pair and write the index header.
pub(super) async fn create_pair(
    segs_path: &Path,
    index_path: &Path,
    stream_id: u64,
) -> Result<(File, File), DiskOutputError> {
    let mut segs = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(segs_path)
        .await?;
    let mut index = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(index_path)
        .await?;
    index.write_all_at(encode_header(stream_id), 0).await.0?;
    // Touch `segs` so the file exists before the pair is ever a hole.
    segs.write_all_at(Vec::<u8>::new(), 0).await.0?;
    Ok((segs, index))
}

pub(super) async fn create_canonical(
    dir: &Path,
    stream_id: u64,
) -> Result<Canonical, DiskOutputError> {
    let (segs, index) = create_pair(&dir.join("segs"), &dir.join("index"), stream_id).await?;
    Ok(Canonical {
        segs,
        index,
        segs_end: 0,
        index_end: HEADER_LEN as u64,
        init_counter: 0,
        active_init: None,
    })
}

pub(super) async fn open_canonical(
    dir: &Path,
    snapshot: &Snapshot,
) -> Result<Canonical, DiskOutputError> {
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

pub(super) async fn rename_canonical_to_run(
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

pub(super) fn run_paths(dir: &Path, number: u32) -> (PathBuf, PathBuf) {
    (
        dir.join(format!("live.{number:03}.segs")),
        dir.join(format!("live.{number:03}.index")),
    )
}

pub(super) fn ms_u32(value: u64) -> u32 {
    if value > u64::from(u32::MAX) {
        u32::MAX
    } else {
        u32::try_from(value).unwrap_or(u32::MAX)
    }
}
