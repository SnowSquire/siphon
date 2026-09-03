use std::fs::OpenOptions;
use std::io::{self, Read, Seek, Write};
use std::num::NonZeroU16;
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Buf;
use ffmpeg_next as ffmpeg;
use ffmpeg_next::format::context::StreamIo;
use tracing::{debug, debug_span, error};
use xxhash_rust::xxh3::Xxh3;

use crate::scheduler::SegmentMessage;

const INDEX_MAGIC: [u8; 8] = *b"OZENIKEN";
const INDEX_VERSION: u64 = 2;

#[derive(Debug, thiserror::Error)]
pub enum DiskWriterError {
    #[error("failed to register MPEG-TS reader")]
    RegisterReader {
        #[source]
        source: kanal::SendError,
    },

    #[error("failed to spawn MPEG-TS writer thread")]
    SpawnThread {
        #[source]
        source: io::Error,
    },

    #[error("failed to create manifest directory")]
    CreateManifestDirectory {
        #[source]
        source: io::Error,
    },

    #[error("failed to create MPEG-TS file at {path}")]
    CreateTsFile {
        path: String,
        #[source]
        source: io::Error,
    },

    #[error("failed to create segment-boundary file at {path}")]
    CreateBoundariesFile {
        path: String,
        #[source]
        source: io::Error,
    },

    #[error("failed to write segment-boundary file header")]
    WriteBoundariesHeader {
        #[source]
        source: io::Error,
    },

    #[error("failed to get MPEG-TS file position")]
    GetTsFilePosition {
        #[source]
        source: io::Error,
    },

    #[error("failed to write MPEG-TS data")]
    WriteTsData {
        #[source]
        source: io::Error,
    },

    #[error("failed to flush MPEG-TS file")]
    FlushTsFile {
        #[source]
        source: io::Error,
    },

    #[error("failed to write segment boundary")]
    WriteBoundary {
        #[source]
        source: io::Error,
    },

    #[error("failed to flush segment-boundary file")]
    FlushBoundariesFile {
        #[source]
        source: io::Error,
    },
}

pub fn spawn_disk_writer(
    add_readers: &kanal::Sender<kanal::Sender<SegmentMessage>>,
    stream_id: u64,
) -> JoinHandle<()> {
    let (reader_rx, reader_tx): (
        kanal::Sender<SegmentMessage>,
        kanal::Receiver<SegmentMessage>,
    ) = kanal::unbounded();

    add_readers
        .send(reader_rx)
        .map_err(|source| DiskWriterError::RegisterReader { source })
        .expect("fuuuuuuuuuu");

    let handle = thread::Builder::new()
        .name(format!("disk writer stream id:{stream_id}"))
        .spawn(move || {
            if let Err(error) = run_disk_writer(reader_tx, stream_id) {
                error!(?error, "disk writer stopped");
            }
        })
        .expect("failed to spawn disk writer");

    handle
}

struct Segment {
    hash: u128,
    sequence_number: u64,
    byte_offset: u64,
    byte_length: u32,
    timestamp_ms: u32,
    muted: bool,
}

#[repr(C, u8)]
enum StreamRecord {
    MpegTS(Segment) = 0,
    FMp4(Segment) = 1,
    Discontinuity {
        width: u16,
        height: u16,
        rate_num: Option<NonZeroU16>,
        rate_denom: Option<NonZeroU16>,
    } = 2,
    Init {
        counter: u64, // doesn't need to be a u64 but no harm
        byte_offset: u64,
        byte_length: u32,
        hash: u128,
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
            file.write_all(&rate_num.map(|v| v.get()).unwrap_or(0).to_le_bytes())?;
            // (9) +2 bytes
            file.write_all(&rate_denom.map(|v| v.get()).unwrap_or(0).to_le_bytes())?;
        }
        StreamRecord::Init {
            counter,
            hash,
            byte_length,
            byte_offset,
        } => {
            // 1 byte
            file.write_all(&[3])?;
            // (5) + 8 bytes
            file.write_all(&counter.to_le_bytes())?;
            file.write_all(&hash.to_le_bytes())?;
            file.write_all(&byte_length.to_le_bytes())?;
            file.write_all(&byte_offset.to_le_bytes())?;
        }
        StreamRecord::MpegTS(segment) => {
            // 1 byte
            file.write_all(&[0])?;
            // (9) +8 bytes
            file.write_all(&segment.hash.to_le_bytes())?;
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
            file.write_all(&segment.hash.to_le_bytes())?;
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
            let hash = read_u128(reader)?;
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
                hash,
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
                rate_num: NonZeroU16::new(rate_num),
                rate_denom: NonZeroU16::new(rate_denom),
            })
        }

        3 => {
            let counter = read_u64(reader)?;
            let hash = read_u128(reader)?;
            let byte_offset = read_u64(reader)?;
            let byte_length = read_u32(reader)?;

            Ok(StreamRecord::Init {
                counter,
                hash,
                byte_length,
                byte_offset,
            })
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

fn read_u128<R: Read>(reader: &mut R) -> io::Result<u128> {
    let mut bytes = [0u8; 16];
    reader.read_exact(&mut bytes)?;
    Ok(u128::from_le_bytes(bytes))
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

fn write_stream_index_header<W: Write>(file: &mut W, stream_id: u64) -> io::Result<()> {
    file.write_all(&INDEX_MAGIC)?;
    file.write_all(&INDEX_VERSION.to_le_bytes())?;
    file.write_all(&stream_id.to_le_bytes())?;
    Ok(())
}

pub fn run_disk_writer(
    reader_tx: kanal::Receiver<SegmentMessage>,
    stream_id: u64,
) -> Result<(), DiskWriterError> {
    ffmpeg::init().unwrap();
    let file_timestamp = unix_timestamp_nanos();

    std::fs::create_dir_all("manifest")
        .map_err(|source| DiskWriterError::CreateManifestDirectory { source })?;

    let segment_path = format!("manifest/{stream_id}.segs");
    let index_path = format!("manifest/{stream_id}.index");

    let mut segment_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&segment_path)
        .map_err(|source| DiskWriterError::CreateTsFile {
            path: segment_path.clone(),
            source,
        })?;

    let mut index_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&index_path)
        .map_err(|source| DiskWriterError::CreateBoundariesFile {
            path: index_path.clone(),
            source,
        })?;

    if !std::fs::metadata(&index_path)
        .map(|m| m.len() > 0)
        .unwrap_or(false)
    {
        write_stream_index_header(&mut index_file, stream_id)
            .map_err(|source| DiskWriterError::WriteBoundariesHeader { source })?;
    }

    let mut probed_stream = false;
    let mut hasher = Xxh3::new();

    // this needs to be initialized from the stream header but i dont wanna do that right now
    let mut index_sequence = 0;

    for msg in reader_tx {
        debug!("message: {:#?}", msg);
        match msg {
            SegmentMessage::Segment(mut segment) => {
                hasher.reset();

                let segment_start = segment_file
                    .stream_position()
                    .map_err(|source| DiskWriterError::GetTsFilePosition { source })?;

                let _span = debug_span!("writing file", sequence = segment.sequence_num).entered();
                let ffmpeg_reader = if !probed_stream {
                    Some(segment.reader.clone().reader())
                } else {
                    None
                };
                loop {
                    let buf = segment.reader.chunk();

                    segment_file
                        .write_all(buf)
                        .map_err(|source| DiskWriterError::WriteTsData { source })?;
                    hasher.update(buf);
                    segment.reader.advance(buf.len());

                    if !segment.reader.wait_for_data() {
                        break;
                    }
                }

                segment_file
                    .flush()
                    .map_err(|source| DiskWriterError::FlushTsFile { source })?;

                let segment_end = segment_file
                    .stream_position()
                    .map_err(|source| DiskWriterError::GetTsFilePosition { source })?;

                let segment_size = segment_end - segment_start;

                if let Some(reader) = ffmpeg_reader {
                    let input_ctx = ffmpeg::format::input_from_stream(
                        StreamIo::from_read(reader).unwrap(),
                        None,
                        None,
                    )
                    .unwrap();

                    let video_stream = input_ctx
                        .streams()
                        .best(ffmpeg::media::Type::Video)
                        .unwrap();

                    let decoder =
                        ffmpeg::codec::context::Context::from_parameters(video_stream.parameters())
                            .unwrap();
                    let video_decoder = decoder.decoder().video().unwrap();

                    let width = video_decoder.width();
                    let height = video_decoder.height();
                    let framerate = video_decoder
                        .frame_rate()
                        .into_iter()
                        .chain([video_stream.avg_frame_rate(), video_stream.rate()])
                        .find(|r| r.numerator() > 0 && r.denominator() > 0);

                    _ = write_record(
                        &mut index_file,
                        StreamRecord::Discontinuity {
                            width: width as u16,
                            height: height as u16,
                            rate_num: framerate.and_then(|f| NonZeroU16::new(f.numerator() as u16)),
                            rate_denom: framerate
                                .and_then(|f| NonZeroU16::new(f.denominator() as u16)),
                        },
                    );

                    probed_stream = true
                }

                _ = write_record(
                    &mut index_file,
                    StreamRecord::MpegTS(Segment {
                        hash: hasher.digest128(),
                        sequence_number: segment.sequence_num,
                        byte_offset: segment_start,
                        byte_length: segment_size.try_into().unwrap(),
                        timestamp_ms: segment.timestamp_ms.try_into().unwrap(),
                        muted: false,
                    }),
                );

                index_file
                    .flush()
                    .map_err(|source| DiskWriterError::FlushBoundariesFile { source })?;
            }
            SegmentMessage::Prefetch { .. } => {}
            SegmentMessage::Discontinuity {} => probed_stream = false,
            SegmentMessage::Init { mut reader } => {
                hasher.reset();
                let segment_start = segment_file
                    .stream_position()
                    .map_err(|source| DiskWriterError::GetTsFilePosition { source })?;
                loop {
                    let buf = reader.chunk();

                    segment_file
                        .write_all(buf)
                        .map_err(|source| DiskWriterError::WriteTsData { source })?;

                    hasher.update(buf);
                    reader.advance(buf.len());
                    if !reader.wait_for_data() {
                        break;
                    }
                }

                segment_file
                    .flush()
                    .map_err(|source| DiskWriterError::FlushTsFile { source })?;

                let segment_end = segment_file
                    .stream_position()
                    .map_err(|source| DiskWriterError::GetTsFilePosition { source })?;

                let segment_size = segment_end - segment_start;

                _ = write_record(
                    &mut index_file,
                    StreamRecord::Init {
                        counter: index_sequence,
                        hash: hasher.digest128(),
                        byte_length: segment_size.try_into().unwrap(),
                        byte_offset: segment_size,
                    },
                );

                index_file
                    .flush()
                    .map_err(|source| DiskWriterError::FlushBoundariesFile { source })?;

                index_sequence += 1;
                probed_stream = true;
            }
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
