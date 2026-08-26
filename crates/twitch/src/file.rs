use bytes::Buf;
use mimalloc::MiMalloc;

use clap::Parser;

use sonic_rs::{PointerTree, get, get_from_bytes, get_many, pointer};
use std::borrow::Cow;
use std::fmt::format;
use std::fs::File;
use std::hint::unreachable_unchecked;
use std::io::{self, IoSlice, Read, Seek, Write};
use std::mem::{MaybeUninit, align_of, size_of, transmute};
use std::mem::{discriminant, offset_of};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::ptr::addr_of;
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use std::{array, thread};
use tracing::subscriber::NoSubscriber;
use tracing::{Level, debug, debug_span, dispatcher, error, info, trace};
use tracing_subscriber::EnvFilter;
use ureq::ResponseExt;

use base64::engine::general_purpose::STANDARD;
use rand::{RngExt, distr::Alphanumeric};

use ureq::{Agent, http::header::AUTHORIZATION};

use crate::FileWriterError;
use crate::downloader::download_worker;
use crate::helpers::{fetch_master_manifest, select_track};
use crate::scheduler::SegmentMessage;

const INDEX_MAGIC: [u8; 8] = *b"OZENIKEN";
const INDEX_VERSION: u64 = 1;

#[repr(C)]
struct Segment {
    sequence_number: u64,
    byte_offset: u64,
    byte_length: u32,
    timestamp_ms: u32,
    flags: u8,
}

impl Segment {
    const MUTED: u8 = 1 << 0;
    const FMP4: u8 = 1 << 1;

    pub fn muted(&self) -> bool {
        self.flags & Self::MUTED != 0
    }

    pub fn set_muted(&mut self, value: bool) {
        self.set_flag(Self::MUTED, value);
    }

    pub fn fmp4(&self) -> bool {
        self.flags & Self::FMP4 != 0
    }

    pub fn set_fmp4(&mut self, value: bool) {
        self.set_flag(Self::FMP4, value);
    }

    fn set_flag(&mut self, flag: u8, value: bool) {
        if value {
            self.flags |= flag;
        } else {
            self.flags &= !flag;
        }
    }
}

#[repr(C)]
struct StreamIndexHeader {
    magic: [u8; 8],
    version: u64,
}

#[repr(C, u8)]
enum StreamRecord {
    Segment(Segment) = 0,
    Discontinuity {
        width: u16,
        height: u16,
        rate_num: u16,
        rate_denom: u16,
    } = 1,
    Init {
        counter: u64, // doesn't need to be a u64 but no harm
    } = 2,
}
#[repr(C)]
struct Init {
    counter: u64,
}
const FAST: bool = cfg!(target_endian = "little")
    && size_of::<StreamIndexHeader>() % align_of::<StreamRecord>() == 0
    && size_of::<StreamRecord>() % align_of::<StreamRecord>() == 0
    && size_of::<u8>() == 1
    && align_of::<u8>() == 1
    && size_of::<u16>() == 2
    && align_of::<u16>() == 2
    && size_of::<u32>() == 4
    && align_of::<u32>() == 4
    && size_of::<u64>() == 8
    && align_of::<u64>() == 8
    && size_of::<Segment>() == 32
    && align_of::<Segment>() == 8
    && offset_of!(Segment, sequence_number) == 0
    && offset_of!(Segment, byte_offset) == 8
    && offset_of!(Segment, byte_length) == 16
    && offset_of!(Segment, timestamp_ms) == 20
    && offset_of!(Segment, flags) == 24
    && size_of::<StreamRecord>() == 40
    && align_of::<StreamRecord>() == 8
    && offset_of!(StreamRecord, Segment.0) == 8
    && offset_of!(StreamRecord, Discontinuity.width) == 8
    && offset_of!(StreamRecord, Discontinuity.height) == 10
    && offset_of!(StreamRecord, Discontinuity.rate_num) == 12
    && offset_of!(StreamRecord, Discontinuity.rate_denom) == 14
    && offset_of!(StreamRecord, Init.counter) == 8;

const _: () = assert!(FAST);

fn write_record(file: &mut File, record: StreamRecord) -> io::Result<()> {
    match record {
        StreamRecord::Discontinuity {
            width,
            height,
            rate_num,
            rate_denom,
        } => {
            file.write_all(&[1, 0, 0, 0, 0, 0, 0, 0])?;
            file.write_all(&width.to_le_bytes())?;
            file.write_all(&height.to_le_bytes())?;
            file.write_all(&rate_num.to_le_bytes())?;
            file.write_all(&rate_denom.to_le_bytes())?;
            file.write_all(&[0u8; 24])?;
        }
        StreamRecord::Segment(segment) => {
            file.write_all(&[0, 0, 0, 0, 0, 0, 0, 0])?;
            file.write_all(&segment.sequence_number.to_le_bytes())?;
            file.write_all(&segment.byte_offset.to_le_bytes())?;
            file.write_all(&segment.byte_length.to_le_bytes())?;
            file.write_all(&segment.timestamp_ms.to_le_bytes())?;
            file.write_all(&segment.flags.to_le_bytes())?;
            file.write_all(&[0u8; 7])?;
        }

        StreamRecord::Init { counter } => {
            file.write_all(&[2, 0, 0, 0, 0, 0, 0, 0])?;
            file.write_all(&counter.to_le_bytes())?;
            file.write_all(&[0u8; 24])?;
        }
    }
    Ok(())
}

fn read_record_fast(file: &mut File) -> io::Result<StreamRecord> {
    // FAST guarantees this size and alignment.
    let mut record = MaybeUninit::<StreamRecord>::uninit();

    let bytes = unsafe {
        std::slice::from_raw_parts_mut(record.as_mut_ptr().cast::<u8>(), size_of::<StreamRecord>())
    };

    file.read_exact(bytes)?;

    // Do not assume_init until the discriminant has been validated. An
    // arbitrary discriminant would make the enum value invalid.
    match bytes[0] {
        0 | 1 | 2 => {
            // FAST also guarantees the target is little-endian and that the
            // serialized offsets match the Rust representation.
            Ok(unsafe { record.assume_init() })
        }
        _ => unreachable!("invalid stream record discriminant"),
    }
}

fn read_record(file: &mut File) -> io::Result<StreamRecord> {
    if FAST {
        return read_record_fast(file);
    }

    read_record_slow(file)
}

fn read_record_slow(file: &mut File) -> io::Result<StreamRecord> {
    let mut bytes = [0u8; 40];
    file.read_exact(&mut bytes)?;

    let tag = bytes[0];

    match tag {
        0 => {
            let segment = Segment {
                sequence_number: read_u64(&bytes, 8),
                byte_offset: read_u64(&bytes, 16),
                byte_length: read_u32(&bytes, 24),
                timestamp_ms: read_u32(&bytes, 28),
                flags: bytes[32],
            };

            Ok(StreamRecord::Segment(segment))
        }

        1 => Ok(StreamRecord::Discontinuity {
            width: read_u16(&bytes, 8),
            height: read_u16(&bytes, 10),
            rate_num: read_u16(&bytes, 12),
            rate_denom: read_u16(&bytes, 14),
        }),

        2 => Ok(StreamRecord::Init {
            counter: read_u64(&bytes, 8),
        }),

        _ => unreachable!("invalid stream record discriminant"),
    }
}

pub fn run_mpegts_writer(
    reader_tx: kanal::Receiver<SegmentMessage>,
) -> Result<(), FileWriterError> {
    let file_timestamp = unix_timestamp_nanos();

    std::fs::create_dir_all("manifest")
        .map_err(|source| FileWriterError::CreateManifestDirectory { source })?;

    let ts_path = format!("manifest/{file_timestamp}.ts");
    let index_path = format!("manifest/{file_timestamp}.index");

    let mut ts_file = File::create(&ts_path).map_err(|source| FileWriterError::CreateTsFile {
        path: ts_path.clone(),
        source,
    })?;

    let mut index_file =
        File::create(&index_path).map_err(|source| FileWriterError::CreateBoundariesFile {
            path: index_path.clone(),
            source,
        })?;

    let mut header = [0u8; 24];
    header[0..8].copy_from_slice(b"OZENIKEN");
    header[8..16].copy_from_slice(&1u64.to_le_bytes());
    header[16..24].copy_from_slice(&32u64.to_le_bytes());

    index_file
        .write_all(&header)
        .map_err(|source| FileWriterError::WriteBoundariesHeader { source })?;

    for msg in reader_tx {
        match msg {
            SegmentMessage::Segment {
                sequence_num,
                mut reader,
            } => {
                let segment_start = ts_file
                    .stream_position()
                    .map_err(|source| FileWriterError::GetTsFilePosition { source })?;

                let _span = debug_span!("writing file", sequence = sequence_num).entered();

                loop {
                    let buf = reader.chunk();

                    ts_file
                        .write_all(buf)
                        .map_err(|source| FileWriterError::WriteTsData { source })?;

                    reader.advance(buf.len());

                    if !reader.wait_for_data() {
                        break;
                    }
                }

                ts_file
                    .flush()
                    .map_err(|source| FileWriterError::FlushTsFile { source })?;

                let segment_end = ts_file
                    .stream_position()
                    .map_err(|source| FileWriterError::GetTsFilePosition { source })?;

                let segment_size = segment_end - segment_start;
                let completed_at = unix_timestamp_nanos();

                let mut boundary_record = [0u8; 32];
                boundary_record[0..8].copy_from_slice(&sequence_num.to_le_bytes());
                boundary_record[8..16].copy_from_slice(&completed_at.to_le_bytes());
                boundary_record[16..24].copy_from_slice(&segment_size.to_le_bytes());
                boundary_record[24..32].copy_from_slice(&segment_start.to_le_bytes());

                index_file
                    .write_all(&boundary_record)
                    .map_err(|source| FileWriterError::WriteBoundary { source })?;

                index_file
                    .flush()
                    .map_err(|source| FileWriterError::FlushBoundariesFile { source })?;
            }

            SegmentMessage::Init { reader: _ } => {}
        }
    }

    Ok(())
}

fn unix_timestamp_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before UNIX epoch")
        .as_nanos()
        .try_into()
        .expect("timestamp does not fit in u64")
}
