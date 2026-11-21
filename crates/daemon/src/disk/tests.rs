//! Unit tests for the index format, the hole arithmetic and the repair path.

use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use super::index::*;
use super::plan::*;
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
    let Record::Segment(copied) = Record::decode(tag, &mut reader).expect("copied record") else {
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

#[cfg(not(miri))]
#[test]
fn discontinuity_markers_are_written_when_the_archive_reaches_them() {
    use std::fs;

    let dir = std::env::temp_dir().join(format!(
        "twitch-disk-discontinuity-{}-{}",
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
            let mut marks = vec![
                DiscontinuityMark {
                    pdt_ms: 1_000,
                    elapsed_ms: 500,
                },
                DiscontinuityMark {
                    pdt_ms: 9_000,
                    elapsed_ms: 8_500,
                },
            ];

            // Before the first mark: nothing is placed.
            write_discontinuities(&mut canonical, &mut marks, 999, Clock::Pdt, None)
                .await
                .expect("nothing");
            assert_eq!(canonical.index_end, HEADER_LEN as u64);
            assert_eq!(marks.len(), 2);

            // Past the first: exactly one record, and the second stays pending.
            write_discontinuities(&mut canonical, &mut marks, 1_000, Clock::Pdt, None)
                .await
                .expect("first");
            assert_eq!(
                canonical.index_end,
                HEADER_LEN as u64 + u64::from(1 + DISCONTINUITY_BODY)
            );
            assert_eq!(marks.len(), 1);

            write_discontinuities(&mut canonical, &mut marks, u64::MAX, Clock::Pdt, None)
                .await
                .expect("second");
            assert!(marks.is_empty());

            let index = fs::read(dir.join("index")).expect("index");
            let mut position = HEADER_LEN;
            let mut tags = Vec::new();
            while position < index.len() {
                let tag = index[position];
                let body = body_len(tag).expect("tag");
                tags.push((tag, position));
                position += 1 + body as usize;
            }
            assert_eq!(tags.len(), 2);
            assert!(tags.iter().all(|(tag, _)| *tag == TAG_DISCONTINUITY));
            assert!(tags[0].1 < tags[1].1);
            assert_eq!(position, index.len());
        });

    fs::remove_dir_all(&dir).ok();
}
