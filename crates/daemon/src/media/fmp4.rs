//! fMP4 (fragmented ISO-BMFF) demuxer for Twitch's H.264 and H.265 sources.
//!
//! Twitch delivers H.264 over MPEG-TS (see [`crate::media::ts`]) but H.265 only
//! over fMP4, so this is the second input side of the Riffle media lane. An init
//! segment (`ftyp` + `moov`) describes the tracks; every media segment is a run
//! of `moof`/`mdat` pairs whose `traf` sample tables point into the `mdat`.
//!
//! The output is shaped like the TS demuxer's: a [`Sample`] per access unit, a
//! [`SampleStore`] holding the segment's bytes in one flat buffer plus a flat
//! list of NAL ranges, and Annex B for video — the length-prefixed NAL units of
//! the sample tables are rewritten with 4-byte start codes here, so nothing
//! downstream sees `avcC`/`hvcC` framing.
//!
//! Two things differ. Timing is per track: PES timestamps are already 90 kHz,
//! but fMP4 times are counts in each track's own `mdhd` timescale, so
//! [`Sample::dts`], [`Sample::pts`] and [`Sample::duration`] are in that
//! timescale and [`Track::timescale`] converts them. And there is no carry and
//! no `finish_segment`: a sample's bytes are contiguous in `mdat`, so a segment
//! is fed whole and nothing spans a call.

use siphon_shared::riffle::AudioConfig;
use siphon_shared::riffle::VideoCodec;
use siphon_shared::riffle::VideoConfig;

use crate::media::aac;

/// Bytes a visual sample entry spends before its child boxes: the 8 bytes after
/// the header, the 16 of the pre-defined fields, the 2+2 dimensions, the three
/// resolutions and reserved words, the frame count, the compressor name and the
/// depth/pre-defined pair.
const VISUAL_ENTRY: usize = 78;
/// The same for an audio sample entry: reserved/data-reference, the eight
/// reserved bytes, the channel count, sample size, pre-defined, reserved and the
/// 16.16 sample rate.
const AUDIO_ENTRY: usize = 28;

/// `trun` flags.
const TRUN_DATA_OFFSET: u32 = 0x0001;
const TRUN_FIRST_FLAGS: u32 = 0x0004;
const TRUN_DURATION: u32 = 0x0100;
const TRUN_SIZE: u32 = 0x0200;
const TRUN_SAMPLE_FLAGS: u32 = 0x0400;
const TRUN_COMPOSITION: u32 = 0x0800;

/// `tfhd` flags.
const TFHD_BASE_OFFSET: u32 = 0x0000_0001;
const TFHD_SAMPLE_DESCRIPTION: u32 = 0x0000_0002;
const TFHD_DEFAULT_DURATION: u32 = 0x0000_0008;
const TFHD_DEFAULT_SIZE: u32 = 0x0000_0010;
const TFHD_DEFAULT_FLAGS: u32 = 0x0000_0020;
const TFHD_DEFAULT_BASE_IS_MOOF: u32 = 0x0002_0000;

/// Bit 16 of a sample flags word: `sample_is_non_sync_sample`.
const SAMPLE_NON_SYNC: u32 = 0x0001_0000;

/// A decode time above this is not a stream. Capping it keeps the running
/// clock, the composition offsets and the presentation time from overflowing on
/// a `tfdt` that lies.
const MAX_TIME: i64 = 1 << 62;

/// Why a segment or an init segment could not be demuxed.
///
/// Both come off the network, so every field read is checked and every failure
/// is one of these rather than a panic.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A box header, its body, a length prefix or a sample range overruns the
    /// buffer it came in, or a declared size contradicts the bytes present.
    #[error("truncated {0}")]
    Truncated(&'static str),
    /// A box the structure requires is absent.
    #[error("missing {0}")]
    Missing(&'static str),
    /// The bytes are readable but carry something this demuxer does not handle.
    #[error("unsupported {0}")]
    Unsupported(&'static str),
}

/// A track's codec record, as the Riffle `StreamConfig` wants it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrackConfig {
    /// Video: the raw `avcC`/`hvcC` record — bit for bit what `extradata` means
    /// — plus the dimensions and the track timescale.
    Video(VideoConfig),
    /// Audio: the `esds` `AudioSpecificConfig` plus the sample rate and layout.
    Audio(AudioConfig),
}

/// One track of the init segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    /// `tkhd` track ID, which is what `tfhd` names the track by.
    id: u32,
    /// Bytes per NAL length prefix, from `avcC`/`hvcC`. Audio tracks carry the
    /// default and no audio path reads it.
    nal_length_size: u8,
    config: TrackConfig,
}

impl Track {
    #[must_use]
    pub const fn id(&self) -> u32 {
        self.id
    }

    /// The track's media timescale: `dts`, `pts` and `duration` are counts of
    /// these units per second.
    #[must_use]
    pub const fn timescale(&self) -> u32 {
        match &self.config {
            TrackConfig::Video(config) => config.timescale,
            TrackConfig::Audio(config) => config.timescale,
        }
    }

    /// The codec of a video track, `None` for audio.
    #[must_use]
    pub const fn codec(&self) -> Option<VideoCodec> {
        match &self.config {
            TrackConfig::Video(config) => Some(config.codec),
            TrackConfig::Audio(_) => None,
        }
    }

    #[must_use]
    pub const fn video(&self) -> Option<&VideoConfig> {
        match &self.config {
            TrackConfig::Video(config) => Some(config),
            TrackConfig::Audio(_) => None,
        }
    }

    #[must_use]
    pub const fn audio(&self) -> Option<&AudioConfig> {
        match &self.config {
            TrackConfig::Video(_) => None,
            TrackConfig::Audio(config) => Some(config),
        }
    }

    /// Bytes per length prefix in the media segments, as `avcC`/`hvcC` declares
    /// it (`lengthSizeMinusOne`); 4 is the default, and it is what an audio
    /// track carries.
    #[must_use]
    pub const fn nal_length_size(&self) -> u8 {
        self.nal_length_size
    }

    /// The per-sample facts the demuxer needs, copied out so the segment loop
    /// holds no borrow of the init segment while it writes to the store.
    const fn params(&self) -> TrackParams {
        TrackParams {
            track: self.id,
            video: match &self.config {
                TrackConfig::Video(config) => Some(VideoParams {
                    codec: config.codec,
                    nal_length_size: self.nal_length_size,
                }),
                TrackConfig::Audio(_) => None,
            },
        }
    }
}

/// `trex` defaults for a track: what a `tfhd`/`trun` that omits a field falls
/// back to. A zero field means "not set".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SampleDefaults {
    duration: Option<u32>,
    size: Option<u32>,
    flags: Option<u32>,
}

/// An init segment: what the tracks are, and the defaults their fragments may
/// rely on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Init {
    tracks: Vec<Track>,
    defaults: Vec<SampleDefaults>,
}

impl Init {
    /// Parse an init segment (`ftyp` + `moov`, or just `moov`).
    ///
    /// Every box length is checked against the bytes present, so a truncated or
    /// lying init segment is an [`Error`] and never an over-read or an
    /// allocation sized by a declared length.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        let mut tracks = Vec::new();
        let mut defaults = Vec::new();
        let mut found = false;

        for iso in Boxes::new(bytes, 0) {
            let iso = iso?;
            if iso.kind != *b"moov" {
                continue;
            }
            found = true;
            for child in iso.children() {
                let child = child?;
                match &child.kind {
                    b"trak" => tracks.push(parse_trak(&child)?),
                    b"mvex" => defaults = parse_mvex(&child)?,
                    _ => {}
                }
            }
        }

        if !found {
            return Err(Error::Missing("moov"));
        }
        if tracks.is_empty() {
            return Err(Error::Missing("trak"));
        }

        // `trex` is keyed by track ID; line the defaults up by index so the
        // demuxer can look one up by track index in the same step.
        let defaults = tracks
            .iter()
            .map(|track| {
                defaults
                    .iter()
                    .find(|(id, _)| *id == track.id)
                    .map_or_else(SampleDefaults::default, |(_, defaults)| *defaults)
            })
            .collect();

        Ok(Self { tracks, defaults })
    }

    #[must_use]
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    #[must_use]
    pub fn video(&self) -> Option<&Track> {
        self.tracks.iter().find(|track| track.video().is_some())
    }

    #[must_use]
    pub fn audio(&self) -> Option<&Track> {
        self.tracks.iter().find(|track| track.audio().is_some())
    }
}

/// One demuxed sample: a byte range in a [`SampleStore`] plus its timing.
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    track: u32,
    dts: i64,
    pts: i64,
    duration: i64,
    video: bool,
    keyframe: bool,
    sync: bool,
    data_start: usize,
    data_end: usize,
    nals_start: usize,
    nals_end: usize,
}

impl Sample {
    /// The `tkhd`/`tfhd` track ID this sample belongs to.
    #[must_use]
    pub const fn track(self) -> u32 {
        self.track
    }

    /// Decode timestamp, in the track's timescale.
    #[must_use]
    pub const fn dts(self) -> i64 {
        self.dts
    }

    /// Presentation timestamp, in the track's timescale: `dts` plus the
    /// composition offset `trun` gave the sample.
    #[must_use]
    pub const fn pts(self) -> i64 {
        self.pts
    }

    /// The composition offset this sample was given, in the track's timescale.
    #[must_use]
    pub const fn composition_offset(self) -> i64 {
        self.pts - self.dts
    }

    /// Sample duration, in the track's timescale.
    #[must_use]
    pub const fn duration(self) -> i64 {
        self.duration
    }

    #[must_use]
    pub const fn is_video(self) -> bool {
        self.video
    }

    /// Whether the elementary stream says this access unit is a random access
    /// point: an IDR NAL unit for H.264, any IRAP (16–23: IDR, CRA, BLA) for
    /// H.265. This is the single keyframe fact the rest of the code uses and
    /// `AV_PKT_FLAG_KEY` means, so IDR versus CRA is deliberately not modeled.
    /// Always false for audio.
    #[must_use]
    pub const fn keyframe(self) -> bool {
        self.keyframe
    }

    /// Whether the container calls this a sync sample (`trun` sample flags or
    /// `sdtp`; true when the fragment says nothing). [`Sample::keyframe`] is
    /// the authoritative answer, read from the bitstream.
    #[must_use]
    pub const fn sync(self) -> bool {
        self.sync
    }
}

/// One segment's samples in flat buffers.
///
/// `data` holds access units back to back as Annex B (video) or raw AAC frames
/// (audio), `nals` every access unit's NAL ranges relative to that sample's
/// bytes, and `samples` indexes both. Unlike the TS store this needs no
/// per-stream buffers: an fMP4 sample is never assembled across two boxes, so
/// appending in fragment order is enough.
///
/// This is one growing allocation per segment, reused, instead of one `Vec` per
/// access unit, one `Vec` per NAL list and one `Vec` per AAC frame.
#[derive(Debug, Default)]
pub struct SampleStore {
    data: Vec<u8>,
    nals: Vec<(usize, usize)>,
    samples: Vec<Sample>,
}

impl SampleStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops the segment's samples and keeps the capacity for the next one.
    pub fn clear(&mut self) {
        self.data.clear();
        self.nals.clear();
        self.samples.clear();
    }

    #[must_use]
    pub fn samples(&self) -> &[Sample] {
        &self.samples
    }

    /// The sample's bytes: an Annex B access unit, or one raw AAC frame.
    #[must_use]
    pub fn data(&self, sample: Sample) -> &[u8] {
        &self.data[sample.data_start..sample.data_end]
    }

    /// The sample's NAL units as ranges into [`SampleStore::data`], empty for
    /// audio.
    #[must_use]
    pub fn nals(&self, sample: Sample) -> &[(usize, usize)] {
        &self.nals[sample.nals_start..sample.nals_end]
    }

    /// The buffer lengths a failed segment is rolled back to.
    const fn mark(&self) -> Mark {
        Mark {
            data: self.data.len(),
            nals: self.nals.len(),
            samples: self.samples.len(),
        }
    }

    fn rollback(&mut self, mark: Mark) {
        self.data.truncate(mark.data);
        self.nals.truncate(mark.nals);
        self.samples.truncate(mark.samples);
    }
}

#[derive(Clone, Copy)]
struct Mark {
    data: usize,
    nals: usize,
    samples: usize,
}

/// The fMP4 demuxer: an init segment's tracks plus the clock that continues
/// across segments.
pub struct Fmp4Demuxer {
    init: Init,
    /// Running decode time per track (indexed like [`Init::tracks`]), used when
    /// a `traf` omits `tfdt`.
    next_dts: Vec<i64>,
}

impl Fmp4Demuxer {
    #[must_use]
    pub fn new(init: Init) -> Self {
        Self {
            next_dts: vec![0; init.tracks.len()],
            init,
        }
    }

    /// The media timescale of a track, by `tkhd` track ID. `None` for a track
    /// the init segment does not declare, which is a malformed fragment rather
    /// than a track.
    #[must_use]
    pub fn timescale(&self, track: u32) -> Option<u32> {
        self.init
            .tracks
            .iter()
            .find(|declared| declared.id == track)
            .map(Track::timescale)
    }

    /// Demux one complete media segment, appending its samples to `store`.
    ///
    /// The segment is fed whole: there is no carry between calls and no
    /// `finish_segment`, because a sample's bytes are contiguous in `mdat`. On
    /// error the store is rolled back, so a bad segment leaves nothing
    /// half-demuxed; the caller discards the segment either way.
    pub fn feed(&mut self, bytes: &[u8], store: &mut SampleStore) -> Result<(), Error> {
        let mark = store.mark();
        let result = self.segment(bytes, store);
        if result.is_err() {
            store.rollback(mark);
        }
        result
    }

    fn segment(&mut self, bytes: &[u8], store: &mut SampleStore) -> Result<(), Error> {
        for iso in Boxes::new(bytes, 0) {
            let iso = iso?;
            if iso.kind == *b"moof" {
                self.moof(&iso, bytes, store)?;
            }
        }
        Ok(())
    }

    fn moof(
        &mut self,
        moof: &IsoBox<'_>,
        bytes: &[u8],
        store: &mut SampleStore,
    ) -> Result<(), Error> {
        // `mfhd`'s sequence number is deliberately ignored: fragments are
        // ordered by their position in the segment, not across segments.
        //
        // Without `tfhd`'s base offset and without `default-base-is-moof`, the
        // spec falls back to the end of the previous `traf`'s data.
        let mut data_end = moof.start;
        for child in moof.children() {
            let child = child?;
            if child.kind == *b"traf" {
                data_end = self.traf(&child, moof.start, data_end, bytes, store)?;
            }
        }
        Ok(())
    }

    fn traf(
        &mut self,
        traf: &IsoBox<'_>,
        moof_start: usize,
        previous_end: usize,
        bytes: &[u8],
        store: &mut SampleStore,
    ) -> Result<usize, Error> {
        let mut header = None;
        let mut decode_time = None;
        let mut dependencies = None;
        for child in traf.children() {
            let child = child?;
            match &child.kind {
                b"tfhd" => header = Some(Tfhd::parse(child.body)?),
                b"tfdt" => decode_time = Some(parse_tfdt(child.body)?),
                b"sdtp" => dependencies = Some(Dependencies::new(child.body)),
                _ => {}
            }
        }

        let header = header.ok_or(Error::Missing("tfhd"))?;
        let index = self
            .init
            .tracks
            .iter()
            .position(|track| track.id == header.track_id)
            .ok_or(Error::Unsupported("track outside the init segment"))?;
        let params = self.init.tracks[index].params();
        let defaults = self.init.defaults[index];

        let base = if let Some(offset) = header.base_data_offset {
            usize::try_from(offset).map_err(|_| Error::Truncated("tfhd base_data_offset"))?
        } else if header.default_base_is_moof {
            moof_start
        } else {
            previous_end
        };

        let fragment = Fragment {
            base,
            params,
            header,
            defaults,
            bytes,
        };
        let mut dts = decode_time.unwrap_or(self.next_dts[index]);
        let mut data_end = base;
        for child in traf.children() {
            let child = child?;
            if child.kind == *b"trun" {
                data_end = fragment.trun(child.body, &mut dts, &mut dependencies, store)?;
            }
        }
        self.next_dts[index] = dts;
        Ok(data_end)
    }
}

/// One track's sample tables within one `traf`.
#[derive(Clone, Copy)]
struct Fragment<'a> {
    base: usize,
    params: TrackParams,
    header: Tfhd,
    defaults: SampleDefaults,
    bytes: &'a [u8],
}

impl Fragment<'_> {
    /// Parse one `trun`, emitting its samples and advancing `dts`.
    ///
    /// Returns the end offset of the run's data, which is the implicit base of
    /// a following `traf`.
    fn trun(
        &self,
        body: &[u8],
        dts: &mut i64,
        dependencies: &mut Option<Dependencies<'_>>,
        store: &mut SampleStore,
    ) -> Result<usize, Error> {
        let mut table = Reader::new(body);
        let version = table.u8()?;
        let flags = table.u24()?;
        if version > 1 {
            return Err(Error::Unsupported("trun version"));
        }
        let count = table.u32()?;
        let data_offset = if flags & TRUN_DATA_OFFSET != 0 {
            Some(i64::from(table.i32()?))
        } else {
            None
        };
        let first_flags = if flags & TRUN_FIRST_FLAGS != 0 {
            Some(table.u32()?)
        } else {
            None
        };

        let base = i64::try_from(self.base).map_err(|_| Error::Truncated("trun data offset"))?;
        let mut offset = base + data_offset.unwrap_or(0);

        // A sample is at least one byte of `mdat`, so a count above the
        // segment's length is a lie — and it keeps a `trun` that carries no
        // per-sample fields from spinning without ever touching the buffer.
        if count as usize > self.bytes.len() {
            return Err(Error::Truncated("trun sample count"));
        }

        for index in 0..count {
            let duration = if flags & TRUN_DURATION != 0 {
                Some(table.u32()?)
            } else {
                None
            };
            let size = if flags & TRUN_SIZE != 0 {
                Some(table.u32()?)
            } else {
                None
            };
            let own_flags = if flags & TRUN_SAMPLE_FLAGS != 0 {
                Some(table.u32()?)
            } else {
                None
            };
            let composition = if flags & TRUN_COMPOSITION != 0 {
                if version == 1 {
                    i64::from(table.i32()?)
                } else {
                    i64::from(table.u32()?)
                }
            } else {
                0
            };

            let duration = duration
                .or(self.header.default_duration)
                .or(self.defaults.duration)
                .ok_or(Error::Missing("sample duration"))?;
            let size = size
                .or(self.header.default_size)
                .or(self.defaults.size)
                .ok_or(Error::Missing("sample size"))?;

            // `first_sample_flags` overrides the first sample only, which is how
            // a muxer marks a keyframe followed by deltas without listing flags
            // for every sample.
            let flags = if index == 0 {
                first_flags.or(own_flags)
            } else {
                own_flags
            }
            .or(self.header.default_flags)
            .or(self.defaults.flags);

            // Always consume an `sdtp` byte, even when the `trun` answered, so
            // the table stays aligned for a later `trun` that does not.
            let dependency = match dependencies {
                Some(dependencies) => dependencies.advance()?,
                None => true,
            };
            let sync = flags.map_or(dependency, |flags| flags & SAMPLE_NON_SYNC == 0);

            let end = offset
                .checked_add(i64::from(size))
                .ok_or(Error::Truncated("sample range"))?;
            let start = usize::try_from(offset).map_err(|_| Error::Truncated("sample offset"))?;
            let stop = usize::try_from(end).map_err(|_| Error::Truncated("sample range"))?;
            let data = self
                .bytes
                .get(start..stop)
                .ok_or(Error::Truncated("sample range"))?;

            emit(
                data,
                store,
                self.params,
                Timing {
                    dts: *dts,
                    composition,
                    duration: i64::from(duration),
                },
                sync,
            )?;

            *dts += i64::from(duration);
            offset = end;
        }

        usize::try_from(offset).map_err(|_| Error::Truncated("trun data offset"))
    }
}

/// One sample's timing, in the track's timescale.
#[derive(Clone, Copy)]
struct Timing {
    dts: i64,
    composition: i64,
    duration: i64,
}

/// Copy one sample into `store`, converting video to Annex B.
fn emit(
    data: &[u8],
    store: &mut SampleStore,
    params: TrackParams,
    timing: Timing,
    sync: bool,
) -> Result<(), Error> {
    let data_start = store.data.len();
    let nals_start = store.nals.len();

    let keyframe = if let Some(video) = params.video {
        annex_b(data, video, &mut store.data, &mut store.nals)?
    } else {
        store.data.extend_from_slice(data);
        false
    };

    // An access unit with no bytes is not worth a `Sample`; a zero-length AAC
    // frame is the same. Dropping it is a buffer rollback, not a copy.
    if store.data.len() == data_start {
        store.nals.truncate(nals_start);
        return Ok(());
    }

    store.samples.push(Sample {
        track: params.track,
        dts: timing.dts,
        pts: timing.dts + timing.composition,
        duration: timing.duration,
        video: params.video.is_some(),
        keyframe,
        sync,
        data_start,
        data_end: store.data.len(),
        nals_start,
        nals_end: store.nals.len(),
    });
    Ok(())
}

/// Rewrite one sample's length-prefixed NAL units as Annex B, appending the NAL
/// ranges (relative to the sample's bytes) to `nals`.
///
/// Returns whether the sample carries a random access point.
fn annex_b(
    sample: &[u8],
    video: VideoParams,
    data: &mut Vec<u8>,
    nals: &mut Vec<(usize, usize)>,
) -> Result<bool, Error> {
    let prefix_len = usize::from(video.nal_length_size);
    let base = data.len();
    let mut pos = 0;
    let mut keyframe = false;

    while pos < sample.len() {
        let prefix = sample
            .get(pos..pos + prefix_len)
            .ok_or(Error::Truncated("NAL length prefix"))?;
        let mut len = 0usize;
        for &byte in prefix {
            len = (len << 8) | usize::from(byte);
        }
        pos += prefix_len;
        if len == 0 {
            continue;
        }

        let end = pos.checked_add(len).ok_or(Error::Truncated("NAL unit"))?;
        let nal = sample.get(pos..end).ok_or(Error::Truncated("NAL unit"))?;
        data.extend_from_slice(&[0, 0, 0, 1]);
        let start = data.len();
        data.extend_from_slice(nal);
        nals.push((start - base, data.len() - base));
        keyframe |= is_keyframe(video.codec, nal);
        pos = end;
    }

    Ok(keyframe)
}

/// Whether a NAL unit starts a random access point.
fn is_keyframe(codec: VideoCodec, nal: &[u8]) -> bool {
    let Some(&header) = nal.first() else {
        return false;
    };
    match codec {
        VideoCodec::H264 => header & 0x1F == 5,
        // HEVC NAL unit type is bits 1–6 of the two-byte header; 16–23 is the
        // IRAP range (IDR_W_RADL through RSV_IRAP_VCL23), which covers IDR, CRA
        // and BLA.
        VideoCodec::H265 => (16..=23).contains(&((header >> 1) & 0x3F)),
        VideoCodec::Av1 => false,
    }
}

/// What the demuxer needs to know about a track, copied out of its `Track`.
#[derive(Clone, Copy)]
struct TrackParams {
    track: u32,
    video: Option<VideoParams>,
}

#[derive(Clone, Copy)]
struct VideoParams {
    codec: VideoCodec,
    nal_length_size: u8,
}

/// `sdtp`: one dependency byte per sample of the `traf`, in table order.
struct Dependencies<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Dependencies<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// Whether the next sample is a sync sample.
    ///
    /// `sdtp` has no explicit sync bit: `sample_depends_on == 2` (an I picture)
    /// is the only value that describes a random access point.
    fn advance(&mut self) -> Result<bool, Error> {
        let byte = *self.bytes.get(self.pos).ok_or(Error::Truncated("sdtp"))?;
        self.pos += 1;
        Ok((byte >> 4) & 0x03 == 2)
    }
}

/// The fields of a `tfhd` the sample tables can fall back to.
#[derive(Clone, Copy)]
struct Tfhd {
    track_id: u32,
    base_data_offset: Option<u64>,
    default_duration: Option<u32>,
    default_size: Option<u32>,
    default_flags: Option<u32>,
    default_base_is_moof: bool,
}

impl Tfhd {
    fn parse(body: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(body);
        let version = reader.u8()?;
        let flags = reader.u24()?;
        if version != 0 {
            return Err(Error::Unsupported("tfhd version"));
        }

        let track_id = reader.u32()?;
        let base_data_offset = if flags & TFHD_BASE_OFFSET != 0 {
            Some(reader.u64()?)
        } else {
            None
        };
        if flags & TFHD_SAMPLE_DESCRIPTION != 0 {
            reader.skip(4)?;
        }
        let default_duration = read_default(&mut reader, flags, TFHD_DEFAULT_DURATION)?;
        let default_size = read_default(&mut reader, flags, TFHD_DEFAULT_SIZE)?;
        let default_flags = read_default(&mut reader, flags, TFHD_DEFAULT_FLAGS)?;

        Ok(Self {
            track_id,
            base_data_offset,
            default_duration,
            default_size,
            default_flags,
            default_base_is_moof: flags & TFHD_DEFAULT_BASE_IS_MOOF != 0,
        })
    }
}

fn read_default(reader: &mut Reader<'_>, flags: u32, flag: u32) -> Result<Option<u32>, Error> {
    if flags & flag == 0 {
        Ok(None)
    } else {
        Ok(Some(reader.u32()?))
    }
}

/// `tfdt`: the decode time the fragment's first sample starts at.
fn parse_tfdt(body: &[u8]) -> Result<i64, Error> {
    let mut reader = Reader::new(body);
    let version = reader.u8()?;
    reader.skip(3)?;
    let time = match version {
        0 => i64::from(reader.u32()?),
        1 => i64::try_from(reader.u64()?)
            .map_err(|_| Error::Unsupported("tfdt base media decode time"))?,
        _ => return Err(Error::Unsupported("tfdt version")),
    };
    if time > MAX_TIME {
        return Err(Error::Unsupported("tfdt base media decode time"));
    }
    Ok(time)
}

/// `mvex` → `trex` entries as `(track ID, defaults)`.
fn parse_mvex(mvex: &IsoBox<'_>) -> Result<Vec<(u32, SampleDefaults)>, Error> {
    let mut defaults = Vec::new();
    for child in mvex.children() {
        let child = child?;
        if child.kind != *b"trex" {
            continue;
        }
        let mut reader = Reader::new(child.body);
        let version = reader.u8()?;
        reader.skip(3)?;
        if version != 0 {
            return Err(Error::Unsupported("trex version"));
        }
        let track_id = reader.u32()?;
        reader.skip(4)?; // default_sample_description_index
        let duration = reader.u32()?;
        let size = reader.u32()?;
        let flags = reader.u32()?;
        // A zero entry is "not set" in a `trex`, so it must not shadow a
        // `tfhd`/`trun` value.
        defaults.push((
            track_id,
            SampleDefaults {
                duration: (duration != 0).then_some(duration),
                size: (size != 0).then_some(size),
                flags: (flags != 0).then_some(flags),
            },
        ));
    }
    Ok(defaults)
}

/// One `trak` of the init segment: ID, timescale, handler and sample entry.
fn parse_trak(trak: &IsoBox<'_>) -> Result<Track, Error> {
    let id = parse_tkhd(child(trak, *b"tkhd")?.ok_or(Error::Missing("tkhd"))?.body)?;
    let mdia = child(trak, *b"mdia")?.ok_or(Error::Missing("mdia"))?;

    let timescale = parse_mdhd(child(&mdia, *b"mdhd")?.ok_or(Error::Missing("mdhd"))?.body)?;
    let handler = parse_hdlr(child(&mdia, *b"hdlr")?.ok_or(Error::Missing("hdlr"))?.body)?;
    let minf = child(&mdia, *b"minf")?.ok_or(Error::Missing("minf"))?;
    let stbl = child(&minf, *b"stbl")?.ok_or(Error::Missing("stbl"))?;
    let stsd = child(&stbl, *b"stsd")?.ok_or(Error::Missing("stsd"))?;

    parse_stsd(id, timescale, handler, &stsd)
}

fn parse_tkhd(body: &[u8]) -> Result<u32, Error> {
    let mut reader = Reader::new(body);
    let version = reader.u8()?;
    reader.skip(3)?;
    // creation_time and modification_time, then the track ID.
    if version == 1 {
        reader.skip(16)?;
    } else {
        reader.skip(8)?;
    }
    reader.u32()
}

fn parse_mdhd(body: &[u8]) -> Result<u32, Error> {
    let mut reader = Reader::new(body);
    let version = reader.u8()?;
    reader.skip(3)?;
    if version == 1 {
        reader.skip(16)?;
    } else {
        reader.skip(8)?;
    }
    reader.u32()
}

fn parse_hdlr(body: &[u8]) -> Result<[u8; 4], Error> {
    let mut reader = Reader::new(body);
    reader.skip(8)?; // version/flags, pre_defined
    let handler = reader.take(4)?;
    Ok([handler[0], handler[1], handler[2], handler[3]])
}

/// Turn the first `stsd` entry into a track.
///
/// Twitch writes one sample entry per track, so only the first is read; a track
/// with a second description is not something the sources produce.
fn parse_stsd(
    id: u32,
    timescale: u32,
    handler: [u8; 4],
    stsd: &IsoBox<'_>,
) -> Result<Track, Error> {
    let mut reader = Reader::new(stsd.body);
    reader.skip(4)?; // version + flags
    let entries = reader.u32()?;
    if entries == 0 {
        return Err(Error::Missing("stsd sample entry"));
    }
    let base = stsd.start + stsd.header + reader.pos;
    let entry = Boxes::new(reader.rest(), base)
        .next()
        .ok_or(Error::Missing("stsd sample entry"))??;

    match &handler {
        b"vide" => parse_video_entry(id, timescale, &entry),
        b"soun" => parse_audio_entry(id, timescale, &entry),
        _ => Err(Error::Unsupported("handler type")),
    }
}

fn parse_video_entry(id: u32, timescale: u32, entry: &IsoBox<'_>) -> Result<Track, Error> {
    let (codec, record) = match &entry.kind {
        b"avc1" | b"avc3" => (VideoCodec::H264, *b"avcC"),
        b"hvc1" | b"hev1" => (VideoCodec::H265, *b"hvcC"),
        _ => return Err(Error::Unsupported("video sample entry")),
    };

    let mut reader = Reader::new(entry.body);
    reader.skip(24)?; // reserved/data reference, pre-defined fields
    let width = u32::from(reader.u16()?);
    let height = u32::from(reader.u16()?);

    let children = entry
        .body
        .get(VISUAL_ENTRY..)
        .ok_or(Error::Truncated("sample entry"))?;
    let base = entry.start + entry.header + VISUAL_ENTRY;
    let config = child_at(children, base, record)?.ok_or(Error::Missing("codec record"))?;

    // `lengthSizeMinusOne` is the low two bits of the byte after the fixed head
    // of the record: the 5th byte of an `avcC`, the 22nd of an `hvcC`.
    let nal_length_size = match codec {
        VideoCodec::H265 => length_size(config.body, 21)?,
        _ => length_size(config.body, 4)?,
    };

    Ok(Track {
        id,
        nal_length_size,
        config: TrackConfig::Video(VideoConfig {
            codec,
            extradata: config.body.to_vec(),
            timescale,
            width,
            height,
        }),
    })
}

fn parse_audio_entry(id: u32, timescale: u32, entry: &IsoBox<'_>) -> Result<Track, Error> {
    if entry.kind != *b"mp4a" {
        return Err(Error::Unsupported("audio sample entry"));
    }

    let children = entry
        .body
        .get(AUDIO_ENTRY..)
        .ok_or(Error::Truncated("sample entry"))?;
    let base = entry.start + entry.header + AUDIO_ENTRY;
    let esds = child_at(children, base, *b"esds")?.ok_or(Error::Missing("esds"))?;

    // The fMP4 source carries the AudioSpecificConfig out of band, so the
    // rate and layout come from the record rather than from every frame.
    let asc = audio_specific_config(esds.body)?;
    let config = aac::parse_asc(asc).ok_or(Error::Unsupported("AudioSpecificConfig"))?;

    Ok(Track {
        id,
        nal_length_size: 4,
        config: TrackConfig::Audio(AudioConfig {
            extradata: asc.to_vec(),
            timescale,
            sample_rate: config.sample_rate,
            channels: config.channels,
        }),
    })
}

/// `lengthSizeMinusOne` of an `avcC`/`hvcC`, at `at`.
fn length_size(record: &[u8], at: usize) -> Result<u8, Error> {
    let byte = *record.get(at).ok_or(Error::Truncated("codec record"))?;
    Ok((byte & 0x03) + 1)
}

/// The `esds` `DecoderSpecificInfo`, which is the `AudioSpecificConfig`.
fn audio_specific_config(body: &[u8]) -> Result<&[u8], Error> {
    let mut reader = Reader::new(body);
    reader.skip(4)?; // version + flags
    find_descriptor(reader.rest(), 0x05)
}

/// Walk MPEG-4 descriptors for `wanted`, descending into the ES and decoder
/// config descriptors after skipping their fixed fields.
fn find_descriptor(mut buf: &[u8], wanted: u8) -> Result<&[u8], Error> {
    while !buf.is_empty() {
        let (&tag, rest) = buf
            .split_first()
            .ok_or(Error::Truncated("esds descriptor"))?;

        let mut length = 0usize;
        let mut used = 0;
        loop {
            let byte = *rest.get(used).ok_or(Error::Truncated("esds descriptor"))?;
            used += 1;
            length = (length << 7) | usize::from(byte & 0x7F);
            if byte & 0x80 == 0 {
                break;
            }
            if used == 4 {
                return Err(Error::Truncated("esds descriptor length"));
            }
        }

        let end = used
            .checked_add(length)
            .ok_or(Error::Truncated("esds descriptor"))?;
        let payload = rest
            .get(used..end)
            .ok_or(Error::Truncated("esds descriptor"))?;
        buf = rest.get(end..).unwrap_or_default();

        if tag == wanted {
            return Ok(payload);
        }

        let nested = match tag {
            // ES_Descriptor: ES_ID, then a flags byte and its optional fields.
            0x03 => {
                let mut reader = Reader::new(payload);
                reader.skip(2)?;
                let flags = reader.u8()?;
                if flags & 0x80 != 0 {
                    reader.skip(2)?; // dependsOn_ES_ID
                }
                if flags & 0x40 != 0 {
                    let len = reader.u8()?;
                    reader.skip(usize::from(len))?; // URL
                }
                if flags & 0x20 != 0 {
                    reader.skip(2)?; // OCR_ES_Id
                }
                reader.rest()
            }
            // DecoderConfigDescriptor: object type, stream type, buffer size,
            // max bitrate, average bitrate.
            0x04 => payload.get(13..).unwrap_or_default(),
            _ => &[],
        };

        if !nested.is_empty()
            && let Ok(found) = find_descriptor(nested, wanted)
        {
            return Ok(found);
        }
    }

    Err(Error::Missing("esds AudioSpecificConfig"))
}

/// The first child box of `parent` with type `kind`.
fn child<'a>(parent: &IsoBox<'a>, kind: [u8; 4]) -> Result<Option<IsoBox<'a>>, Error> {
    child_at(parent.body, parent.start + parent.header, kind)
}

/// The first box of type `kind` in `body`, whose first byte is at absolute
/// offset `base`.
fn child_at(body: &[u8], base: usize, kind: [u8; 4]) -> Result<Option<IsoBox<'_>>, Error> {
    for child in Boxes::new(body, base) {
        let child = child?;
        if child.kind == kind {
            return Ok(Some(child));
        }
    }
    Ok(None)
}

/// One ISO-BMFF box: its type, where its header starts in the fed buffer, and
/// its payload.
#[derive(Clone, Copy)]
struct IsoBox<'a> {
    kind: [u8; 4],
    start: usize,
    header: usize,
    body: &'a [u8],
}

impl<'a> IsoBox<'a> {
    const fn children(&self) -> Boxes<'a> {
        Boxes::new(self.body, self.start + self.header)
    }
}

/// The boxes of `buf`, whose first byte sits at absolute offset `base`.
///
/// Sizes are checked against the bytes actually present: a declared length that
/// overruns its parent is an error, and it never sizes an allocation.
struct Boxes<'a> {
    buf: &'a [u8],
    base: usize,
    pos: usize,
}

impl<'a> Boxes<'a> {
    const fn new(buf: &'a [u8], base: usize) -> Self {
        Self { buf, base, pos: 0 }
    }
}

impl<'a> Iterator for Boxes<'a> {
    type Item = Result<IsoBox<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.buf.get(self.pos..)?;
        if rest.is_empty() {
            return None;
        }
        match parse_box(rest, self.base + self.pos) {
            Ok((iso, size)) => {
                self.pos += size;
                Some(Ok(iso))
            }
            Err(error) => {
                self.pos = self.buf.len();
                Some(Err(error))
            }
        }
    }
}

/// Parse the box at the head of `bytes`, whose first byte sits at absolute
/// offset `at`. Returns the box and the bytes it occupies.
fn parse_box(bytes: &[u8], at: usize) -> Result<(IsoBox<'_>, usize), Error> {
    if bytes.len() < 8 {
        return Err(Error::Truncated("box header"));
    }
    let declared = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let kind = [bytes[4], bytes[5], bytes[6], bytes[7]];

    let (size, header) = match declared {
        // Zero means "to the end of the parent".
        0 => (bytes.len(), 8),
        // One means a 64-bit size follows the type.
        1 => {
            let large = bytes.get(8..16).ok_or(Error::Truncated("box size"))?;
            let size = u64::from_be_bytes([
                large[0], large[1], large[2], large[3], large[4], large[5], large[6], large[7],
            ]);
            (
                usize::try_from(size).map_err(|_| Error::Truncated("box size"))?,
                16,
            )
        }
        size => (size as usize, 8),
    };

    if size < header || size > bytes.len() {
        return Err(Error::Truncated("box"));
    }

    Ok((
        IsoBox {
            kind,
            start: at,
            header,
            body: &bytes[header..size],
        },
        size,
    ))
}

/// A bounds-checked cursor over one box body.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], Error> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(Error::Truncated("box body"))?;
        let bytes = self
            .buf
            .get(self.pos..end)
            .ok_or(Error::Truncated("box body"))?;
        self.pos = end;
        Ok(bytes)
    }

    fn skip(&mut self, len: usize) -> Result<(), Error> {
        self.take(len).map(drop)
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, Error> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u24(&mut self) -> Result<u32, Error> {
        let bytes = self.take(3)?;
        Ok((u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]))
    }

    fn u32(&mut self) -> Result<u32, Error> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn i32(&mut self) -> Result<i32, Error> {
        let bytes = self.take(4)?;
        Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, Error> {
        let bytes = self.take(8)?;
        Ok(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::path::PathBuf;

    use super::*;

    /// The fixtures are gitignored, so a checkout without `captures/` skips.
    fn fixture(relative: &str) -> Option<PathBuf> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../captures")
            .join(relative);
        path.exists().then_some(path)
    }

    fn read(path: &Path) -> Vec<u8> {
        std::fs::read(path).expect("fixture reads")
    }

    /// A moof header claiming 16 KiB with nothing behind it, and an empty
    /// buffer, must both be errors rather than an over-read or a huge
    /// allocation. No fixture needed.
    #[test]
    fn a_lying_box_size_is_an_error() {
        let lying = [0, 0, 0x40, 0x00, b'm', b'o', b'o', b'f'];
        assert!(Init::parse(&lying).is_err());
        assert!(Init::parse(&[]).is_err());
    }

    #[test]
    fn demuxes_the_h264_fixture() {
        let Some(dir) = fixture("servermock-fmp4/landscape") else {
            println!("captures/ is absent; skipping the H.264 fMP4 test");
            return;
        };

        let init = Init::parse(&read(&dir.join("init.mp4"))).expect("init parses");
        assert_eq!(init.tracks().len(), 2);

        let video = init.video().expect("video track");
        let video_config = video.video().expect("video config");
        assert_eq!(video.codec(), Some(VideoCodec::H264));
        assert_eq!(video_config.codec, VideoCodec::H264);
        // 54 bytes is the `avcC` payload, which is what `extradata` means.
        assert_eq!(video_config.extradata.len(), 54);
        assert_eq!((video_config.width, video_config.height), (1920, 1080));
        assert_eq!(video.timescale(), 1_000_000);
        assert_eq!(video.nal_length_size(), 4);

        let audio = init.audio().expect("audio track");
        let audio_config = audio.audio().expect("audio config");
        assert_eq!(audio.codec(), None);
        assert_eq!(audio_config.sample_rate, 48_000);
        assert_eq!(audio_config.channels, 2);
        assert_eq!(audio_config.extradata, [0x11, 0x90]); // AAC-LC, 48 kHz, stereo
        assert_eq!(audio.timescale(), 48_000);

        let (video_id, audio_id) = (video.id(), audio.id());
        let mut demuxer = Fmp4Demuxer::new(init);
        let mut store = SampleStore::new();
        let mut video_samples = 0;
        let mut audio_samples = 0;
        let mut keyframes = 0;

        for index in 0..4 {
            let bytes = read(&dir.join(format!("live/{index:06}.m4s")));
            store.clear();
            demuxer.feed(&bytes, &mut store).expect("segment demuxes");

            let mut segment_video = 0;
            let mut segment_audio = 0;
            let mut segment_keyframes = 0;
            let mut bytes_out = 0;
            for sample in store.samples().iter().copied() {
                bytes_out += store.data(sample).len();
                if sample.is_video() {
                    segment_video += 1;
                    assert_eq!(sample.track(), video_id);
                    assert!(sample.duration() > 0);
                    assert!(sample.composition_offset() >= 0);
                    assert!(matches!(store.nals(sample), [_, ..]));
                    // Annex B, not the length prefixes the mdat holds.
                    assert_eq!(store.data(sample).get(..4), Some(&[0, 0, 0, 1][..]));
                    if sample.keyframe() {
                        segment_keyframes += 1;
                        assert!(sample.sync(), "an IDR cannot be a non-sync sample");
                    }
                } else {
                    segment_audio += 1;
                    assert_eq!(sample.track(), audio_id);
                    assert_eq!(sample.duration(), 1024);
                    assert!(matches!(store.nals(sample), []));
                }
            }
            assert!(segment_video > 0 && segment_audio > 0);
            assert!(bytes_out <= bytes.len(), "the segment cannot grow");
            println!(
                "landscape live/{index:06}.m4s: {} access units ({segment_video} video, \
                 {segment_audio} audio), {segment_keyframes} keyframes, {bytes_out} of {} bytes",
                segment_video + segment_audio,
                bytes.len()
            );

            if index == 0 {
                // The first fragment's `tfdt` and `trun` composition offset.
                let first = store
                    .samples()
                    .iter()
                    .copied()
                    .find(|sample| sample.is_video())
                    .expect("video sample");
                assert_eq!(first.dts(), 64_050_000);
                assert_eq!(first.composition_offset(), 50_000);
                assert_eq!(first.pts(), 64_100_000);
                assert_eq!(first.duration(), 16_000);
            }

            video_samples += segment_video;
            audio_samples += segment_audio;
            keyframes += segment_keyframes;
        }

        println!(
            "landscape totals: {video_samples} video access units, {audio_samples} AAC frames, \
             {keyframes} keyframes"
        );
        assert!(
            keyframes > 0,
            "a GOP has to start somewhere in four segments"
        );
    }

    #[test]
    fn demuxes_the_h265_fixture() {
        let Some(dir) =
            fixture("ozeniken-30s/ozeniken-portrait-dvr/tmp/ozeniken-portrait-dvr/0____")
        else {
            println!("captures/ is absent; skipping the H.265 fMP4 test");
            return;
        };

        let init = Init::parse(&read(&dir.join("_init.mp4"))).expect("init parses");
        let video = init.video().expect("video track");
        let config = video.video().expect("video config");
        assert_eq!(video.codec(), Some(VideoCodec::H265));
        // 131 bytes is the `hvcC` payload.
        assert_eq!(config.extradata.len(), 131);
        // `hvcC` configurationVersion, so the raw record is what was copied.
        assert_eq!(config.extradata.first(), Some(&1));
        assert_eq!((config.width, config.height), (1080, 1920));
        assert_eq!(video.timescale(), 1_000_000);
        assert_eq!(video.nal_length_size(), 4);
        assert_eq!(init.audio().map(Track::timescale), Some(48_000));

        let mut demuxer = Fmp4Demuxer::new(init);
        let mut store = SampleStore::new();
        let mut samples = 0;
        let mut irap = 0;

        for name in ["1795300590544.m4s", "1795300600544.m4s"] {
            let bytes = read(&dir.join(name));
            store.clear();
            demuxer.feed(&bytes, &mut store).expect("segment demuxes");

            let mut segment_irap = 0;
            let mut segment_video = 0;
            let mut segment_audio = 0;
            for sample in store.samples().iter().copied() {
                if !sample.is_video() {
                    segment_audio += 1;
                    continue;
                }
                samples += 1;
                segment_video += 1;
                assert!(sample.duration() > 0);
                let nals = store.nals(sample);
                assert!(matches!(nals, [_, ..]));

                // Re-derive the IRAP flag from the Annex B bytes, which also
                // proves the two-byte HEVC headers survived the rewrite.
                let data = store.data(sample);
                let is_irap = nals
                    .iter()
                    .any(|&(start, _)| (16..=23).contains(&((data[start] >> 1) & 0x3F)));
                assert_eq!(sample.keyframe(), is_irap);
                if is_irap {
                    segment_irap += 1;
                    assert!(sample.sync(), "an IRAP is a sync sample");
                }
            }
            println!(
                "ozeniken {name}: {segment_video} video access units, {segment_audio} AAC frames, \
                 {segment_irap} IRAPs"
            );
            irap += segment_irap;
        }

        println!("ozeniken totals: {samples} video access units, {irap} IRAPs");
        assert!(irap > 0, "an IRAP has to open the stream and every GOP");
    }

    #[test]
    fn a_truncated_segment_is_an_error() {
        let Some(dir) = fixture("servermock-fmp4/landscape") else {
            println!("captures/ is absent; skipping the truncated-segment test");
            return;
        };

        let init = Init::parse(&read(&dir.join("init.mp4"))).expect("init parses");
        let mut demuxer = Fmp4Demuxer::new(init);
        let mut store = SampleStore::new();
        let bytes = read(&dir.join("live/000000.m4s"));

        // Cut inside the first `mdat`: the `moof` before it is complete, so its
        // sample ranges point past the end of what is present.
        let truncated = bytes.get(..1024).expect("fixture is longer than 1 KiB");
        assert!(demuxer.feed(truncated, &mut store).is_err());
        assert!(
            store.samples().is_empty(),
            "a failed segment must leave nothing half-demuxed"
        );
    }
}
