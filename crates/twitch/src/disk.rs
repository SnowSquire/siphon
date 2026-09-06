use std::fs::OpenOptions;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::io::{self};
use std::num::NonZeroU16;
use std::thread::JoinHandle;
use std::thread::{self};
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use ffmpeg_next::format::context::StreamIo;
use ffmpeg_next::{self as ffmpeg};
use tracing::debug;
use tracing::debug_span;
use tracing::error;
use xxhash_rust::xxh3::Xxh3;

use crate::scheduler::SegmentMessage;
use crate::segment::SeekableReader;

const INDEX_MAGIC: [u8; 8] = *b"OZENIKEN";
const INDEX_VERSION: u64 = 3;

#[derive(Debug, thiserror::Error)]
pub enum DiskOutputError {
    #[error("failed to register disk output")]
    RegisterReader {
        #[source]
        source: kanal::SendError,
    },

    #[error("failed to create manifest directory")]
    CreateManifestDirectory {
        #[source]
        source: io::Error,
    },

    #[error("failed to create segment file at {path}")]
    CreateSegmentFile {
        path: String,
        #[source]
        source: io::Error,
    },

    #[error("failed to create index file at {path}")]
    CreateIndexFile {
        path: String,
        #[source]
        source: io::Error,
    },

    #[error("failed to write index file header")]
    WriteIndexHeader {
        #[source]
        source: io::Error,
    },

    #[error("failed to get segment file position")]
    GetSegmentPosition {
        #[source]
        source: io::Error,
    },

    #[error("failed to segment data")]
    WriteSegmentData {
        #[source]
        source: io::Error,
    },

    #[error("failed to flush segment file")]
    FlushSegmentFile {
        #[source]
        source: io::Error,
    },

    #[error("failed to write index")]
    WriteIndex {
        #[source]
        source: io::Error,
    },
    #[error("too lazy to gaf")]
    SomeIo(#[from] io::Error),
    #[error("Header is invalid")]
    InvalidHeader(),
    #[error("failed to flush index file")]
    FlushIndexFile {
        #[source]
        source: io::Error,
    },
}

pub fn spawn_disk_output(
    add_output: &kanal::Sender<kanal::Sender<SegmentMessage>>,
    stream_id: u64,
) -> JoinHandle<()> {
    let (outupt_rx, ouput_tx): (
        kanal::Sender<SegmentMessage>,
        kanal::Receiver<SegmentMessage>,
    ) = kanal::unbounded();

    add_output
        .send(outupt_rx)
        .map_err(|source| DiskOutputError::RegisterReader { source })
        .expect("fuuuuuuuuuu");

    thread::Builder::new()
        .name(format!("disk output stream id:{stream_id}"))
        .spawn(move || {
            if let Err(error) = run_disk_output(ouput_tx, stream_id) {
                error!(?error, "disk output stopped");
            }
        })
        .expect("failed to spawn disk output")
}

struct IndexHeader {
    magic: [u8; 8],
    version: u64,
    stream_id: u64,
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

fn write_record<W: Write>(file: W, record: StreamRecord) -> Result<(), DiskOutputError> {
    fn internal<W: Write>(mut file: W, record: StreamRecord) -> io::Result<()> {
        let _: () = match record {
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
                file.write_all(&rate_num.map_or(0, std::num::NonZero::get).to_le_bytes())?;
                // (9) +2 bytes
                file.write_all(&rate_denom.map_or(0, std::num::NonZero::get).to_le_bytes())?;
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
                file.write_all(&byte_offset.to_le_bytes())?;
                file.write_all(&byte_length.to_le_bytes())?;
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
        };
        Ok(())
    }

    internal(file, record).map_err(|err| DiskOutputError::WriteIndex { source: err })
}

fn read_record<R: Read>(mut reader: R) -> io::Result<Option<StreamRecord>> {
    let mut tag = [0u8; 1];
    match reader.read(&mut tag)? {
        0 => return Ok(None),
        1 => {}
        _ => unreachable!(),
    }

    match tag[0] {
        0 | 1 => {
            let hash = read_u128(&mut reader)?;
            let sequence_number = read_u64(&mut reader)?;
            let byte_offset = read_u64(&mut reader)?;
            let byte_length = read_u32(&mut reader)?;
            let timestamp_ms = read_u32(&mut reader)?;
            let muted = read_bool(&mut reader)?;

            let segment = Segment {
                hash,
                sequence_number,
                byte_offset,
                byte_length,
                timestamp_ms,
                muted,
            };

            if tag[0] == 0 {
                Ok(Some(StreamRecord::MpegTS(segment)))
            } else {
                Ok(Some(StreamRecord::FMp4(segment)))
            }
        }

        2 => {
            let width = read_u16(&mut reader)?;
            let height = read_u16(&mut reader)?;
            let rate_num = read_u16(&mut reader)?;
            let rate_denom = read_u16(&mut reader)?;

            Ok(Some(StreamRecord::Discontinuity {
                width,
                height,
                rate_num: NonZeroU16::new(rate_num),
                rate_denom: NonZeroU16::new(rate_denom),
            }))
        }

        3 => {
            let counter = read_u64(&mut reader)?;
            let hash = read_u128(&mut reader)?;
            let byte_offset = read_u64(&mut reader)?;
            let byte_length = read_u32(&mut reader)?;

            Ok(Some(StreamRecord::Init {
                counter,
                hash,
                byte_length,
                byte_offset,
            }))
        }

        tag => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown stream record tag: {tag}"),
        )),
    }
}

fn read_u16<R: Read>(mut reader: R) -> io::Result<u16> {
    let mut bytes = [0u8; 2];
    reader.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32<R: Read>(mut reader: R) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64<R: Read>(mut reader: R) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_u128<R: Read>(mut reader: R) -> io::Result<u128> {
    let mut bytes = [0u8; 16];
    reader.read_exact(&mut bytes)?;
    Ok(u128::from_le_bytes(bytes))
}

fn read_bool<R: Read>(mut reader: R) -> io::Result<bool> {
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

fn write_stream_index_header<W: Write>(mut writer: W, stream_id: u64) -> io::Result<()> {
    writer.write_all(&INDEX_MAGIC)?;
    writer.write_all(&INDEX_VERSION.to_le_bytes())?;
    writer.write_all(&stream_id.to_le_bytes())?;
    Ok(())
}

fn read_stream_index_header<R: Read>(mut reader: R) -> io::Result<IndexHeader> {
    let mut magic_buf = [0u8; 8];

    reader.read_exact(magic_buf.as_mut_slice())?;
    let version = read_u64(&mut reader)?;
    let stream_id = read_u64(reader)?;

    Ok(IndexHeader {
        magic: magic_buf,
        version,
        stream_id,
    })
}

pub fn run_disk_output(
    reader_tx: kanal::Receiver<SegmentMessage>,
    stream_id: u64,
) -> Result<(), DiskOutputError> {
    ffmpeg::init().unwrap();
    let file_timestamp = unix_timestamp_nanos();

    std::fs::create_dir_all("manifest")
        .map_err(|source| DiskOutputError::CreateManifestDirectory { source })?;

    let segment_path = format!("manifest/{stream_id}.segs");
    let index_path = format!("manifest/{stream_id}.index");

    let mut is_fmp4 = false;

    let mut segment_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&segment_path)
        .map_err(|source| DiskOutputError::CreateSegmentFile {
            path: segment_path.clone(),
            source,
        })?;

    let mut index_file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(&index_path)
        .map_err(|source| DiskOutputError::CreateIndexFile {
            path: index_path.clone(),
            source,
        })?;

    let mut index_sequence = if index_file.metadata().is_ok_and(|m| m.len() > 0) {
        index_file.seek(io::SeekFrom::Start(0))?;

        let header = read_stream_index_header(&index_file)?;
        (header.magic == INDEX_MAGIC
            && header.version == INDEX_VERSION
            && header.stream_id == stream_id)
            .ok_or_else(DiskOutputError::InvalidHeader)?;
        let mut next_init_counter = 0;

        while let Some(record) = read_record(&index_file)? {
            match record {
                StreamRecord::Init { counter, .. } => {
                    if counter != next_init_counter {
                        return Err(
                            DiskOutputError::InvalidHeader(/* or custom corrupt index error */),
                        );
                    }
                    next_init_counter += 1;
                    is_fmp4 = true; // Bonus: also fixes restoring `is_fmp4` state on restart!
                }
                StreamRecord::FMp4(_) => {
                    is_fmp4 = true;
                }
                StreamRecord::MpegTS(_) => {
                    is_fmp4 = false;
                }
                StreamRecord::Discontinuity { .. } => (),
            }
        }
        index_file.seek(io::SeekFrom::End(0))?;
        next_init_counter
    } else {
        write_stream_index_header(&mut index_file, stream_id)
            .map_err(|source| DiskOutputError::WriteIndexHeader { source })?;
        0
    };

    let mut probed_stream = false;
    let mut hasher = Xxh3::new();

    // this needs to be initialized from the stream header but i dont wanna do that right now

    for msg in reader_tx {
        debug!("message: {:#?}", msg);
        match msg {
            SegmentMessage::Segment(mut segment) => {
                hasher.reset();

                let segment_start = segment_file
                    .stream_position()
                    .map_err(|source| DiskOutputError::GetSegmentPosition { source })?;

                let _span = debug_span!("writing file", sequence = segment.sequence_num).entered();
                let ffmpeg_reader = if probed_stream {
                    None
                } else {
                    Some(segment.reader.clone())
                };

                while let Some(buf) = segment.reader.chunk() {
                    segment_file
                        .write_all(buf)
                        .map_err(|source| DiskOutputError::WriteSegmentData { source })?;
                    hasher.update(buf);
                }

                segment_file
                    .flush()
                    .map_err(|source| DiskOutputError::FlushSegmentFile { source })?;

                let segment_end = segment_file
                    .stream_position()
                    .map_err(|source| DiskOutputError::GetSegmentPosition { source })?;

                let segment_size = segment_end - segment_start;

                if let Some(reader) = ffmpeg_reader {
                    // TODO:  create a better seekable struct for the reader (put all the Arc<Chunk> in an Vec & wrap it with read + seek)
                    let input_ctx = ffmpeg::format::input_from_stream(
                        StreamIo::from_read_seek(SeekableReader::new(reader)).unwrap(),
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

                    write_record(
                        &mut index_file,
                        StreamRecord::Discontinuity {
                            width: width.try_into().unwrap(),
                            height: height.try_into().unwrap(),
                            rate_num: framerate
                                .and_then(|f| NonZeroU16::new(f.numerator().try_into().unwrap())),
                            rate_denom: framerate
                                .and_then(|f| NonZeroU16::new(f.denominator().try_into().unwrap())),
                        },
                    )?;

                    probed_stream = true;
                }
                let segment = Segment {
                    hash: hasher.digest128(),
                    sequence_number: segment.sequence_num,
                    byte_offset: segment_start,
                    byte_length: segment_size.try_into().unwrap(),
                    timestamp_ms: segment.timestamp_ms.try_into().unwrap(),
                    muted: false,
                };

                if is_fmp4 {
                    write_record(&mut index_file, StreamRecord::FMp4(segment))?;
                } else {
                    write_record(&mut index_file, StreamRecord::MpegTS(segment))?;
                }
                index_file
                    .flush()
                    .map_err(|source| DiskOutputError::FlushIndexFile { source })?;
            }
            SegmentMessage::Prefetch { .. } => {}
            SegmentMessage::Discontinuity() => probed_stream = false,
            SegmentMessage::Init { mut reader } => {
                // we should do the ffmpeg probing on these
                hasher.reset();
                is_fmp4 = true;
                let segment_start = segment_file
                    .stream_position()
                    .map_err(|source| DiskOutputError::GetSegmentPosition { source })?;

                while let Some(buf) = reader.chunk() {
                    segment_file
                        .write_all(buf)
                        .map_err(|source| DiskOutputError::WriteSegmentData { source })?;

                    hasher.update(buf);
                }

                segment_file
                    .flush()
                    .map_err(|source| DiskOutputError::FlushSegmentFile { source })?;

                let segment_end = segment_file
                    .stream_position()
                    .map_err(|source| DiskOutputError::GetSegmentPosition { source })?;

                let segment_size = segment_end - segment_start;

                write_record(
                    &mut index_file,
                    StreamRecord::Init {
                        counter: index_sequence,
                        hash: hasher.digest128(),
                        byte_length: segment_size.try_into().unwrap(),
                        byte_offset: segment_start,
                    },
                )?;

                index_file
                    .flush()
                    .map_err(|source| DiskOutputError::FlushIndexFile { source })?;

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
