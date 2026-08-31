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
    muted: bool,
}

#[repr(C)]
struct StreamIndexHeader {
    magic: [u8; 8],
    version: u64,
}

#[repr(C, u8)]
enum StreamRecord {
    MpegTS(Segment) = 0,
    FMp4(Segment) = 1,
    Discontinuity {
        width: u16,
        height: u16,
        rate_num: u16,
        rate_denom: u16,
    } = 2,
    Init {
        counter: u64, // doesn't need to be a u64 but no harm
    } = 3,
}

fn write_record<W: Write>(file: &mut W, record: StreamRecord) -> io::Result<()> {
    match record {
        StreamRecord::Discontinuity {
            width,
            height,
            rate_num,
            rate_denom,
        } => {
            // 1 byte
            file.write_all(&[2])?;
            // (3) +2 bytes
            file.write_all(&width.to_le_bytes())?;
            // (5) +2 bytes
            file.write_all(&height.to_le_bytes())?;
            // (7) +2 bytes
            file.write_all(&rate_num.to_le_bytes())?;
            // (9) +2 bytes
            file.write_all(&rate_denom.to_le_bytes())?;
        }
        StreamRecord::Init { counter } => {
            // 1 byte
            file.write_all(&[3])?;
            // (5) + 8 bytes
            file.write_all(&counter.to_le_bytes())?;
        }
        StreamRecord::MpegTS(segment) => {
            // 1 byte
            file.write_all(&[0])?;
            // (9) +8 bytes
            file.write_all(&segment.sequence_number.to_le_bytes())?;
            // (17) +8 bytes
            file.write_all(&segment.byte_offset.to_le_bytes())?;
            // (21) +4 bytes
            file.write_all(&segment.byte_length.to_le_bytes())?;
            // (25) +4 bytes
            file.write_all(&segment.timestamp_ms.to_le_bytes())?;
            // (26) +1 bytes
            file.write_all(&u8::from(segment.muted).to_le_bytes())?;
        }
        StreamRecord::FMp4(segment) => {
            // 1 byte
            file.write_all(&[1])?;
            // (9) +8 bytes
            file.write_all(&segment.sequence_number.to_le_bytes())?;
            // (17) +8 bytes
            file.write_all(&segment.byte_offset.to_le_bytes())?;
            // (21) +4 bytes
            file.write_all(&segment.byte_length.to_le_bytes())?;
            // (25) +4 bytes
            file.write_all(&segment.timestamp_ms.to_le_bytes())?;
            // (26) +1 bytes
            file.write_all(&u8::from(segment.muted).to_le_bytes())?;
        }
    }
    Ok(())
}

fn read_record<R: Read>(reader: &mut R) -> io::Result<StreamRecord> {
    let mut tag = [0u8; 1];
    reader.read_exact(&mut tag)?;

    match tag[0] {
        0 | 1 => {
            let sequence_number = read_u64(reader)?;
            let byte_offset = read_u64(reader)?;
            let byte_length = read_u32(reader)?;
            let timestamp_ms = read_u32(reader)?;
            let muted = read_bool(reader)?;

            let segment = Segment {
                sequence_number,
                byte_offset,
                byte_length,
                timestamp_ms,
                muted,
            };

            if tag[0] == 0 {
                Ok(StreamRecord::MpegTS(segment))
            } else {
                Ok(StreamRecord::FMp4(segment))
            }
        }

        2 => {
            let width = read_u16(reader)?;
            let height = read_u16(reader)?;
            let rate_num = read_u16(reader)?;
            let rate_denom = read_u16(reader)?;

            Ok(StreamRecord::Discontinuity {
                width,
                height,
                rate_num,
                rate_denom,
            })
        }

        3 => {
            let counter = read_u64(reader)?;

            Ok(StreamRecord::Init { counter })
        }

        tag => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown stream record tag: {tag}"),
        )),
    }
}

fn read_u16<R: Read>(reader: &mut R) -> io::Result<u16> {
    let mut bytes = [0u8; 2];
    reader.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32<R: Read>(reader: &mut R) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64<R: Read>(reader: &mut R) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_bool<R: Read>(reader: &mut R) -> io::Result<bool> {
    let mut byte = [0u8; 1];
    reader.read_exact(&mut byte)?;

    match byte[0] {
        0 => Ok(false),
        1 => Ok(true),
        value => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid boolean value: {value}"),
        )),
    }
}

fn write_stream_index_header<W: Write>(file: &mut W) -> io::Result<()> {
    file.write_all(&INDEX_MAGIC)?;
    file.write_all(&INDEX_VERSION.to_le_bytes())?;
    Ok(())
}

pub fn run_mpegts_writer(
    reader_tx: kanal::Receiver<SegmentMessage>,
) -> Result<(), FileWriterError> {
    let file_timestamp = unix_timestamp_nanos();

    std::fs::create_dir_all("manifest")
        .map_err(|source| FileWriterError::CreateManifestDirectory { source })?;

    // this can technically be concatted fmp4 but idgaf
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

    write_stream_index_header(&mut index_file)
        .map_err(|source| FileWriterError::WriteBoundariesHeader { source })?;

    let mut probed_stream = false;
    for msg in reader_tx {
        match msg {
            SegmentMessage::Segment(mut segment) => {
                let segment_start = ts_file
                    .stream_position()
                    .map_err(|source| FileWriterError::GetTsFilePosition { source })?;

                let _span = debug_span!("writing file", sequence = segment.sequence_num).entered();

                loop {
                    let buf = segment.reader.chunk();

                    ts_file
                        .write_all(buf)
                        .map_err(|source| FileWriterError::WriteTsData { source })?;

                    segment.reader.advance(buf.len());

                    if !segment.reader.wait_for_data() {
                        break;
                    }
                }

                // PROBE STREAM FOR INFO HERE
                ts_file
                    .flush()
                    .map_err(|source| FileWriterError::FlushTsFile { source })?;

                let segment_end = ts_file
                    .stream_position()
                    .map_err(|source| FileWriterError::GetTsFilePosition { source })?;

                let segment_size = segment_end - segment_start;
                let completed_at = unix_timestamp_nanos();

                write_record(
                    &mut index_file,
                    StreamRecord::MpegTS(Segment {
                        sequence_number: segment.sequence_num,
                        byte_offset: segment_start,
                        byte_length: segment_size.try_into().unwrap(),
                        timestamp_ms: segment.timestamp_ms.try_into().unwrap(),
                        muted: false,
                    }),
                );

                index_file
                    .flush()
                    .map_err(|source| FileWriterError::FlushBoundariesFile { source })?;
            }
            SegmentMessage::Prefetch { .. } => {}
            SegmentMessage::Discontinuity {} => probed_stream = false,
            SegmentMessage::Init { .. } => {}
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
