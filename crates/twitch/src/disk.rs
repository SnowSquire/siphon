use std::io::Read;
use std::io::Write;
use std::num::NonZeroU16;
use std::rc::Rc;

use compio::buf::IoBuf;
use compio::fs::File;
use compio::fs::OpenOptions;
use compio::io::AsyncReadAtExt;
use compio::io::AsyncWriteAtExt;
use tracing::debug;
use tracing::error;
use xxhash_rust::xxh3::Xxh3;

use crate::scheduler::ConsumerRegistration;
use crate::scheduler::SegmentMessage;
use crate::spool::ConsumerWake;
use crate::spool::Reader;
use crate::spool::WakeSet;

const INDEX_MAGIC: [u8; 8] = *b"OZENIKEN";
const INDEX_VERSION: u64 = 3;

#[derive(Debug, thiserror::Error)]
#[allow(variant_size_differences)] // `io::Error` next to a unit variant
pub enum DiskOutputError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid index header")]
    InvalidHeader,
    #[error("failed to register disk output: {0}")]
    RegisterReader(#[from] kanal::SendError),
}

pub fn spawn_disk_output(
    stream_id: u64,
    wakes: &Rc<WakeSet>,
    add_output: &kanal::Sender<ConsumerRegistration>,
) {
    let (tx, rx) = kanal::unbounded();
    if let Err(error) = add_output.send(ConsumerRegistration { tx, backlog: true }) {
        error!(%error, "failed to register disk output");
        return;
    }

    let wake = ConsumerWake::new();
    wakes.register(&wake);

    compio::runtime::spawn(async move {
        if let Err(error) = run_disk_output(rx.to_async(), stream_id, &wake).await {
            error!(?error, "disk output stopped");
        }
    })
    .detach();
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
    fn internal<W: Write>(mut file: W, record: StreamRecord) -> std::io::Result<()> {
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

    internal(file, record).map_err(DiskOutputError::Io)
}

fn read_record<R: Read>(mut reader: R) -> std::io::Result<Option<StreamRecord>> {
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

        tag => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unknown stream record tag: {tag}"),
        )),
    }
}

fn read_u16<R: Read>(mut reader: R) -> std::io::Result<u16> {
    let mut bytes = [0u8; 2];
    reader.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32<R: Read>(mut reader: R) -> std::io::Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64<R: Read>(mut reader: R) -> std::io::Result<u64> {
    let mut bytes = [0u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_u128<R: Read>(mut reader: R) -> std::io::Result<u128> {
    let mut bytes = [0u8; 16];
    reader.read_exact(&mut bytes)?;
    Ok(u128::from_le_bytes(bytes))
}

fn read_bool<R: Read>(mut reader: R) -> std::io::Result<bool> {
    let mut byte = [0u8; 1];
    reader.read_exact(&mut byte)?;

    match byte[0] {
        0 => Ok(false),
        1 => Ok(true),
        value => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid boolean value: {value}"),
        )),
    }
}

fn write_stream_index_header<W: Write>(mut writer: W, stream_id: u64) -> std::io::Result<()> {
    writer.write_all(&INDEX_MAGIC)?;
    writer.write_all(&INDEX_VERSION.to_le_bytes())?;
    writer.write_all(&stream_id.to_le_bytes())?;
    Ok(())
}

fn read_stream_index_header<R: Read>(mut reader: R) -> std::io::Result<IndexHeader> {
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

async fn write_records(
    file: &mut File,
    position: &mut u64,
    scratch: &mut Vec<u8>,
    record: StreamRecord,
) -> Result<(), DiskOutputError> {
    // Records are at most 42 bytes; reusing the buffer keeps this allocation-free
    // after the first write.
    scratch.clear();
    write_record(&mut *scratch, record)?;
    let len = scratch.len() as u64;
    let result = file.write_all_at(std::mem::take(scratch), *position).await;
    *scratch = result.1;
    result.0?;
    *position += len;
    Ok(())
}

async fn write_stream(
    file: &mut File,
    position: &mut u64,
    hasher: &mut Xxh3,
    reader: &mut Reader,
    wake: &ConsumerWake,
) -> Result<u64, DiskOutputError> {
    let mut written = 0;
    while let Some(view) = reader.next_view(wake).await {
        let len = view.as_init().len() as u64;
        hasher.update(view.as_init());
        let result = file.write_all_at(view, *position).await;
        result.0?;
        *position += len;
        written += len;
    }
    Ok(written)
}

async fn run_disk_output(
    reader_rx: kanal::AsyncReceiver<SegmentMessage>,
    stream_id: u64,
    wake: &ConsumerWake,
) -> Result<(), DiskOutputError> {
    std::fs::create_dir_all("manifest")?;

    let segment_path = format!("manifest/{stream_id}.segs");
    let index_path = format!("manifest/{stream_id}.index");

    let mut is_fmp4 = false;

    let mut segment_file = OpenOptions::new()
        .write(true)
        .create(true)
        .open(&segment_path)
        .await?;
    let mut segment_pos = segment_file.metadata().await?.len();

    let mut index_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&index_path)
        .await?;
    let index_len = index_file.metadata().await?.len();
    let mut index_pos = index_len;

    let mut index_sequence = if index_len > 0 {
        let result = index_file.read_to_end_at(Vec::new(), 0).await;
        let buffer = result.1;
        result.0?;

        let mut cursor: &[u8] = &buffer;
        let header = read_stream_index_header(&mut cursor)?;
        if header.magic != INDEX_MAGIC
            || header.version != INDEX_VERSION
            || header.stream_id != stream_id
        {
            return Err(DiskOutputError::InvalidHeader);
        }

        let mut next_init_counter = 0;
        while let Some(record) = read_record(&mut cursor)? {
            match record {
                StreamRecord::Init { counter, .. } => {
                    if counter != next_init_counter {
                        return Err(DiskOutputError::InvalidHeader);
                    }
                    next_init_counter += 1;
                    is_fmp4 = true;
                }
                StreamRecord::FMp4(_) => is_fmp4 = true,
                StreamRecord::MpegTS(_) => is_fmp4 = false,
                StreamRecord::Discontinuity { .. } => (),
            }
        }
        next_init_counter
    } else {
        let mut header = Vec::new();
        write_stream_index_header(&mut header, stream_id)?;
        index_pos = header.len() as u64;
        index_file.write_all_at(header, 0).await.0?;
        0
    };

    let mut hasher = Xxh3::new();
    let mut record_buf = Vec::with_capacity(64);

    // TODO(rtsp): re-introduce discontinuity metadata (width/height/frame rate) from
    // the init segment / SPS parser. The index record type stays, so the on-disk
    // format (v3) is unchanged; see notes/es-rtp-rtsp.md "Dropped from the plan".

    while let Ok(message) = reader_rx.recv().await {
        match message {
            SegmentMessage::Segment(segment) => {
                debug!(sequence = segment.sequence_num, "writing segment");
                hasher.reset();
                let start = segment_pos;
                let mut reader = segment.reader;
                let length = write_stream(
                    &mut segment_file,
                    &mut segment_pos,
                    &mut hasher,
                    &mut reader,
                    wake,
                )
                .await?;

                let record = Segment {
                    hash: hasher.digest128(),
                    sequence_number: segment.sequence_num,
                    byte_offset: start,
                    byte_length: length.try_into().unwrap(),
                    timestamp_ms: segment.timestamp_ms.try_into().unwrap(),
                    muted: false,
                };

                if is_fmp4 {
                    write_records(
                        &mut index_file,
                        &mut index_pos,
                        &mut record_buf,
                        StreamRecord::FMp4(record),
                    )
                    .await?;
                } else {
                    write_records(
                        &mut index_file,
                        &mut index_pos,
                        &mut record_buf,
                        StreamRecord::MpegTS(record),
                    )
                    .await?;
                }
            }
            SegmentMessage::Prefetch { .. }
            | SegmentMessage::Discontinuity()
            | SegmentMessage::Seek(_) => {}
            SegmentMessage::Init { mut reader } => {
                // TODO(rtsp): this is where the init segment parser will pick up
                // codec configuration for the SDP.
                hasher.reset();
                is_fmp4 = true;
                let start = segment_pos;
                let length = write_stream(
                    &mut segment_file,
                    &mut segment_pos,
                    &mut hasher,
                    &mut reader,
                    wake,
                )
                .await?;

                write_records(
                    &mut index_file,
                    &mut index_pos,
                    &mut record_buf,
                    StreamRecord::Init {
                        counter: index_sequence,
                        hash: hasher.digest128(),
                        byte_length: length.try_into().unwrap(),
                        byte_offset: start,
                    },
                )
                .await?;

                index_sequence += 1;
            }
        }
    }

    Ok(())
}
