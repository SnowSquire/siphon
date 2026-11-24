//! From a segment's bytes to the Riffle media objects.
//!
//! A [`SegmentSource`] demuxes one HLS segment with whichever container it is —
//! MPEG-TS through [`crate::media::ts`], fMP4 through [`crate::media::fmp4`] —
//! and yields access units in file order; a [`Packer`] turns each one into the
//! [`Object`]s the media lane ships. There is no socket, task or QUIC here: the
//! lane ([`crate::media::lane`]) owns the demuxer for its lane, the packer for
//! its lane, and the carrier.
//!
//! # The payload prefix
//!
//! The object header (`riffle::object`) has no timestamp field. It is 20 bytes
//! shared by control messages, blobs and media, and adding `pts`/`dts` to it
//! would make every blob pay for two `i64`s only the media classes use. So a
//! media object's payload opens with its timing instead: `pts` then `dts`,
//! each a big-endian `i64`, in **the track's own timescale** — 90 kHz for
//! MPEG-TS, the `mdhd` timescale for fMP4, which is the same number
//! `StreamConfig` carries for that track. That is [`TIMESTAMP_PREFIX`] bytes.
//!
//! The prefix is part of `total_len`, so it fragments like any other payload
//! byte and reassembly is the plain concatenation of the fragments in
//! `frag_offset` order. The first fragment always holds the whole prefix
//! because the budget is far above 16; [`timestamps`] reads it back.
//!
//! # The budget
//!
//! The fragment payload is not a constant: QUIC's [`Datagrams::max_size`] is the
//! application budget of one DATAGRAM frame on the current path, and it is
//! smaller over a loopback handshake than [`DATAGRAM_BUDGET`] assumes. A
//! [`Packer`] is therefore built from that number and re-sized when the path
//! changes; packing against a constant sends datagrams the carrier refuses.
//!
//! [`Datagrams::max_size`]: siphon_shared::riffle::quic::Datagrams::max_size
//!
//! # Inputs
//!
//! [`Container::detect`] says which demuxer a buffer needs: the MPEG-TS sync
//! byte, or an ISO-BMFF box type at offset 4. fMP4 is not self-describing — its
//! init segment carries the codec records and the sample-entry framing — so it
//! has to be handed to [`SegmentSource::new`]/[`SegmentSource::open`]
//! explicitly; a media segment without one is [`Error::MissingInit`], never
//! guessed. The segment itself is validated before it is demuxed, so a
//! truncated buffer is an [`Error`] rather than a silently partial yield.

use siphon_shared::riffle::fragment_count;
use siphon_shared::riffle::object::Class;
use siphon_shared::riffle::object::DATAGRAM_HEADER;
use siphon_shared::riffle::object::Flags;
use siphon_shared::riffle::object::MAX_OBJECT;
use siphon_shared::riffle::object::Object;
/// Two big-endian `i64`s: `pts`, then `dts`, in the track's timescale. Defined
/// with the header because both sides of the wire agree on it.
pub use siphon_shared::riffle::object::TIMESTAMP_PREFIX;

use crate::media::fmp4;
use crate::media::ts;

/// One candidate datagram, header and payload together.
///
/// QUIC's `max_datagram_frame_size` already accounts for the IP, UDP, QUIC and
/// AEAD overhead, so this number is the *application* datagram and nothing
/// above it is free. Sizing the payload at 1200 instead put 1218 bytes on the
/// wire, which cannot fit a 1280-byte path once QUIC frames it.
///
/// This is the **reference** budget: the real one is
/// [`siphon_shared::riffle::quic::Datagrams::max_size`], which is per path and moves with
/// PMTU discovery. [`Packer`] is always built from that.
pub const DATAGRAM_BUDGET: usize = 1200;

/// Payload bytes per datagram at [`DATAGRAM_BUDGET`]: the budget less the
/// header. The packer is driven by a carrier's budget, so this is the reference
/// figure the tests pack against, not something the media path may assume.
#[cfg_attr(not(test), allow(dead_code))]
pub const FRAGMENT_PAYLOAD: usize = DATAGRAM_BUDGET - DATAGRAM_HEADER;

/// The container a segment is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    /// MPEG-TS: 188-byte packets starting with `0x47`.
    Ts,
    /// Fragmented ISO-BMFF: boxes, and a media segment that needs an init
    /// segment first.
    Fmp4,
}

/// The box types a media segment or init segment starts with.
const BOX_TYPES: [&[u8]; 9] = [
    b"ftyp", b"styp", b"moov", b"moof", b"mdat", b"sidx", b"free", b"emsg", b"skip",
];

impl Container {
    /// Guess the container from the first bytes of a buffer.
    ///
    /// A box begins with a 32-bit size, so its first byte is `0x00` for every
    /// box smaller than 16 MiB; `0x47` is the MPEG-TS sync byte and cannot be
    /// the high byte of a real box. Otherwise the box type at offset 4 is
    /// matched against the types a Twitch segment starts with.
    #[must_use]
    pub fn detect(bytes: &[u8]) -> Option<Self> {
        if bytes.first() == Some(&ts::TS_SYNC) {
            return Some(Self::Ts);
        }
        let kind = bytes.get(4..8)?;
        BOX_TYPES.contains(&kind).then_some(Self::Fmp4)
    }
}

/// Which media lane an access unit belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Track {
    /// H.264/H.265 access units, Annex B.
    Video,
    /// Raw AAC frames, no ADTS.
    Audio,
}

impl Track {
    /// The object class this track's access units are shipped as.
    #[must_use]
    pub const fn class(self) -> Class {
        match self {
            Self::Video => Class::Video,
            Self::Audio => Class::Audio,
        }
    }
}

/// One access unit, borrowed from the segment's sample store.
#[derive(Clone, Copy, Debug)]
pub struct AccessUnit<'a> {
    /// Which lane it belongs to.
    pub track: Track,
    /// Presentation time, in the track's timescale.
    pub pts: i64,
    /// Decode time, in the track's timescale.
    pub dts: i64,
    /// Ticks of the track's own clock per second: 90 000 for MPEG-TS, the
    /// `mdhd` timescale for fMP4. Video and audio tracks may differ, which is
    /// why this rides on the access unit rather than being a constant.
    pub timescale: u32,
    /// A random access point, as the elementary stream says (an IDR for H.264,
    /// any IRAP for H.265).
    pub keyframe: bool,
    /// The bytes to ship: Annex B for video, one raw AAC frame for audio.
    pub data: &'a [u8],
}

impl AccessUnit<'_> {
    /// Presentation time on the broadcast's millisecond timeline.
    ///
    /// This is what the push rule and the supersede check compare: the lane's
    /// playhead is in milliseconds and the container's clock is not.
    /// A timestamp before zero (an fMP4 edit list can produce one) is `0`.
    #[must_use]
    pub fn pts_ms(&self) -> u64 {
        if self.timescale == 0 {
            return 0;
        }
        let ticks = i128::from(self.pts.max(0));
        let ms = ticks * 1000 / i128::from(self.timescale);
        u64::try_from(ms).unwrap_or(0)
    }
}

/// Why a segment could not be turned into access units, or an access unit into
/// objects.
///
/// Every variant comes from bytes off the network, so none of them panics.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// [`Container::detect`] matched neither container.
    #[cfg_attr(not(test), allow(dead_code))]
    #[error("unknown segment container")]
    UnknownContainer,
    /// An fMP4 media segment was handed over without its init segment.
    #[error("an fMP4 segment needs its init segment")]
    MissingInit,
    /// The buffer ends before the structure it declares.
    #[error("truncated {0}")]
    Truncated(&'static str),
    /// The access unit is larger than one object may be ([`MAX_OBJECT`]).
    #[error("access unit of {0} bytes is past the object ceiling")]
    TooLarge(usize),
    /// The fMP4 demuxer rejected the init or media segment.
    #[error(transparent)]
    Fmp4(#[from] fmp4::Error),
}

enum Inner {
    Ts {
        demuxer: ts::TsDemuxer,
        store: ts::SampleStore,
    },
    Fmp4 {
        demuxer: fmp4::Fmp4Demuxer,
        store: fmp4::SampleStore,
    },
}

/// One stream's demuxer, fed one segment at a time.
///
/// The demuxer carries state across segments (the MPEG-TS PMT and codec
/// records, the fMP4 running decode time), so a source lives for the stream,
/// not for a segment: [`SegmentSource::feed`] replaces the previous segment's
/// access units.
pub struct SegmentSource {
    inner: Inner,
}

impl SegmentSource {
    /// A source for `container`. `init` is the fMP4 init segment and is
    /// required for [`Container::Fmp4`]; MPEG-TS needs nothing fed first.
    ///
    /// # Errors
    ///
    /// [`Error::MissingInit`] when `container` is fMP4 and `init` is `None`,
    /// and [`Error::Fmp4`] when the init segment does not parse.
    pub fn new(container: Container, init: Option<&[u8]>) -> Result<Self, Error> {
        let inner = match container {
            Container::Ts => Inner::Ts {
                demuxer: ts::TsDemuxer::new(),
                store: ts::SampleStore::new(),
            },
            Container::Fmp4 => Inner::Fmp4 {
                demuxer: fmp4::Fmp4Demuxer::new(fmp4::Init::parse(
                    init.ok_or(Error::MissingInit)?,
                )?),
                store: fmp4::SampleStore::new(),
            },
        };
        Ok(Self { inner })
    }

    /// Detect the container from `segment`, build the source and feed it.
    ///
    /// This is [`SegmentSource::new`] plus [`SegmentSource::feed`], for a
    /// caller that has one complete segment in hand.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownContainer`], plus everything [`SegmentSource::new`] and
    /// [`SegmentSource::feed`] return.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn open(segment: &[u8], init: Option<&[u8]>) -> Result<Self, Error> {
        let container = Container::detect(segment).ok_or(Error::UnknownContainer)?;
        let mut source = Self::new(container, init)?;
        source.feed(segment)?;
        Ok(source)
    }

    /// The container this source was built for.
    #[must_use]
    pub const fn container(&self) -> Container {
        match self.inner {
            Inner::Ts { .. } => Container::Ts,
            Inner::Fmp4 { .. } => Container::Fmp4,
        }
    }

    /// Demux one whole segment, replacing the previous one's access units.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`] for an MPEG-TS segment that is empty or not a whole
    /// number of 188-byte packets — the demuxer resyncs by design, so it would
    /// otherwise drop the tail silently — and [`Error::Fmp4`] for a media
    /// segment the fMP4 demuxer rejects. On any error the source holds no
    /// access units: a segment is demuxed whole or not at all.
    pub fn feed(&mut self, segment: &[u8]) -> Result<(), Error> {
        match &mut self.inner {
            Inner::Ts { demuxer, store } => {
                // Start from an empty store: `finish_segment` closes everything
                // this feed opened, and the next segment is independent.
                demuxer.reset(store);
                if segment.is_empty() || !segment.len().is_multiple_of(ts::TS_PACKET) {
                    return Err(Error::Truncated("MPEG-TS segment"));
                }
                demuxer.feed(segment, store);
                demuxer.finish_segment(store);
            }
            Inner::Fmp4 { demuxer, store } => {
                store.clear();
                demuxer.feed(segment, store)?;
            }
        }
        Ok(())
    }

    /// The last video keyframe at or before `target_ms`, on the broadcast
    /// timeline where this segment starts at `segment_ms`; the segment's first
    /// keyframe when the target precedes them all, and `None` when the segment
    /// carries no video keyframe at all.
    ///
    /// This is the same mapping the lane producer uses — a track's container
    /// clock offset by the difference between the playlist's segment start and
    /// the segment's first access unit on that track — so a seek answer and the
    /// first access unit the producer sends for it agree exactly.
    ///
    /// A segment boundary is not a GOP boundary: Twitch's fMP4 segments carry
    /// several IRAPs, so the answer has to come from the access units and never
    /// from the playlist.
    #[must_use]
    pub fn seek_keyframe(&self, segment_ms: u64, target_ms: u64) -> Option<u64> {
        let mut anchor: Option<i64> = None;
        let mut first = None;
        let mut last_before = None;
        for unit in self.access_units() {
            if unit.track != Track::Video {
                continue;
            }
            let container_ms = i64::try_from(unit.pts_ms()).unwrap_or(0);
            let offset = *anchor
                .get_or_insert_with(|| i64::try_from(segment_ms).unwrap_or(0) - container_ms);
            if !unit.keyframe {
                continue;
            }
            let media = u64::try_from(container_ms.saturating_add(offset)).unwrap_or(0);
            if first.is_none() {
                first = Some(media);
            }
            if media <= target_ms {
                last_before = Some(media);
            }
        }
        last_before.or(first)
    }

    /// The access units of the last fed segment, in demux order.
    #[must_use]
    pub const fn access_units(&self) -> AccessUnits<'_> {
        let (samples, timescales) = match &self.inner {
            Inner::Ts { store, .. } => (Samples::Ts(store), Timescales::Ts),
            Inner::Fmp4 { demuxer, store } => (Samples::Fmp4(store), Timescales::Fmp4(demuxer)),
        };
        AccessUnits {
            samples,
            timescales,
            index: 0,
        }
    }
}

enum Samples<'a> {
    Ts(&'a ts::SampleStore),
    Fmp4(&'a fmp4::SampleStore),
}

/// The clock each sample is timed in.
enum Timescales<'a> {
    /// MPEG-TS PES timestamps are 90 kHz for every stream.
    Ts,
    Fmp4(&'a fmp4::Fmp4Demuxer),
}

impl Timescales<'_> {
    fn of(&self, track: u32) -> u32 {
        match self {
            Self::Ts => ts::TIMESCALE,
            Self::Fmp4(demuxer) => demuxer.timescale(track).unwrap_or(ts::TIMESCALE),
        }
    }
}

/// The access units of one fed segment.
pub struct AccessUnits<'a> {
    samples: Samples<'a>,
    timescales: Timescales<'a>,
    index: usize,
}

impl AccessUnits<'_> {
    fn remaining(&self) -> usize {
        let len = match &self.samples {
            Samples::Ts(store) => store.samples().len(),
            Samples::Fmp4(store) => store.samples().len(),
        };
        len.saturating_sub(self.index)
    }
}

impl<'a> Iterator for AccessUnits<'a> {
    type Item = AccessUnit<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match &self.samples {
            Samples::Ts(store) => {
                let sample = *store.samples().get(self.index)?;
                let timescale = self.timescales.of(0);
                self.index += 1;
                Some(AccessUnit {
                    track: track(sample.is_video()),
                    pts: sample.pts(),
                    dts: sample.dts(),
                    timescale,
                    keyframe: sample.keyframe(),
                    data: store.data(sample),
                })
            }
            Samples::Fmp4(store) => {
                let sample = *store.samples().get(self.index)?;
                let timescale = self.timescales.of(sample.track());
                self.index += 1;
                Some(AccessUnit {
                    track: track(sample.is_video()),
                    pts: sample.pts(),
                    dts: sample.dts(),
                    timescale,
                    keyframe: sample.keyframe(),
                    data: store.data(sample),
                })
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.remaining();
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for AccessUnits<'_> {}

const fn track(video: bool) -> Track {
    if video { Track::Video } else { Track::Audio }
}

/// The `pts` and `dts` at the front of a media object's payload.
///
/// `None` means fewer than [`TIMESTAMP_PREFIX`] bytes were passed, which only a
/// fragment other than the first can be.
// Nothing in the daemon reads a timestamp back — the receiver is the UI — so the
// test build is what keeps this honest.
#[cfg_attr(not(test), allow(dead_code))]
#[must_use]
pub fn timestamps(payload: &[u8]) -> Option<(i64, i64)> {
    let pts = payload.get(..8)?;
    let dts = payload.get(8..TIMESTAMP_PREFIX)?;
    Some((
        i64::from_be_bytes(pts.try_into().ok()?),
        i64::from_be_bytes(dts.try_into().ok()?),
    ))
}

/// One lane's object ids and fragment size.
///
/// `obj_id` is monotonic for the life of a Riffle connection and never reset
/// (`notes/riffle.md`, "One header"), so the packer that fills one lane owns
/// the counter. A connection with several lanes either shares one packer or
/// seeds the others with [`Packer::from_id`]/[`Packer::next_id`].
///
/// The fragment size comes from the transport, not from a constant: build the
/// packer with the carrier's [`siphon_shared::riffle::quic::Datagrams::max_size`] and
/// resize it when the path's budget moves.
#[derive(Debug)]
pub struct Packer {
    next_id: u64,
    /// The lane every object this packer produces names in the header.
    lane: u16,
    /// Payload bytes one fragment may carry.
    payload: usize,
}

impl Default for Packer {
    fn default() -> Self {
        Self::new(DATAGRAM_BUDGET)
    }
}

impl Packer {
    /// A packer for lane 0 whose first object is id 0 and whose datagrams may
    /// be `datagram_budget` bytes on the wire.
    #[must_use]
    pub const fn new(datagram_budget: usize) -> Self {
        Self::from_id(0, datagram_budget)
    }

    /// A packer that continues at `next_id`, still on lane 0. This is the
    /// control plane's packer and what a test that does not care about lanes
    /// builds.
    #[must_use]
    pub const fn from_id(next_id: u64, datagram_budget: usize) -> Self {
        Self::for_lane(0, next_id, datagram_budget)
    }

    /// A packer for one lane: every object it produces carries `lane`, and its
    /// ids start at `next_id` — normally `lane << 40`, so the ranges are
    /// disjoint.
    #[must_use]
    pub const fn for_lane(lane: u16, next_id: u64, datagram_budget: usize) -> Self {
        Self {
            next_id,
            lane,
            payload: payload_of(datagram_budget),
        }
    }

    /// The lane this packer stamps on its objects.
    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub const fn lane(&self) -> u16 {
        self.lane
    }

    /// The id the next object will carry.
    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub const fn next_id(&self) -> u64 {
        self.next_id
    }

    /// Payload bytes each fragment may carry right now.
    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub const fn fragment_payload(&self) -> usize {
        self.payload
    }

    /// Follow the carrier's budget: it moves with path-MTU discovery, so this
    /// is re-applied rather than read once for the life of the connection.
    pub const fn set_datagram_budget(&mut self, datagram_budget: usize) {
        self.payload = payload_of(datagram_budget);
    }

    /// Turn one access unit into its fragments, written into `scratch`.
    ///
    /// The payload — the timestamp prefix followed by the access unit — is
    /// copied once into the caller's reusable buffer, and the returned
    /// [`Fragments`] borrows it, so nothing is allocated per access unit once
    /// `scratch` has grown. Video sets [`Flags::KEYFRAME`] where the demuxer
    /// said so; the last fragment is marked [`Flags::LAST_FRAGMENT`] as well as
    /// being derivable from `total_len`.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] when the prefixed access unit does not fit
    /// [`MAX_OBJECT`]; no id is consumed in that case.
    pub fn pack<'a>(
        &mut self,
        unit: AccessUnit<'_>,
        scratch: &'a mut Vec<u8>,
    ) -> Result<Fragments<'a>, Error> {
        let total = TIMESTAMP_PREFIX
            .checked_add(unit.data.len())
            .ok_or(Error::TooLarge(unit.data.len()))?;
        let total_len = u32::try_from(total).map_err(|_| Error::TooLarge(total))?;
        if total_len > MAX_OBJECT {
            return Err(Error::TooLarge(total));
        }

        scratch.clear();
        scratch.reserve(total);
        scratch.extend_from_slice(&unit.pts.to_be_bytes());
        scratch.extend_from_slice(&unit.dts.to_be_bytes());
        scratch.extend_from_slice(unit.data);

        let obj_id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        Ok(Fragments {
            class: unit.track.class(),
            lane: self.lane,
            flags: if unit.track == Track::Video && unit.keyframe {
                Flags::KEYFRAME
            } else {
                Flags::NONE
            },
            obj_id,
            total_len,
            remaining: fragment_count(total_len, self.payload),
            payload_size: self.payload,
            frag_offset: 0,
            payload: scratch,
        })
    }
}

/// Payload bytes per fragment: the datagram budget less the object header.
///
/// A budget below the header leaves one byte, so a mis-sized carrier cannot
/// produce a zero-length fragment and divide by zero in
/// [`fragment_count`].
const fn payload_of(datagram_budget: usize) -> usize {
    match datagram_budget.checked_sub(DATAGRAM_HEADER) {
        Some(payload) if payload > 0 => payload,
        _ => 1,
    }
}

/// The fragments of one access unit, in wire order.
pub struct Fragments<'a> {
    class: Class,
    lane: u16,
    flags: Flags,
    obj_id: u64,
    total_len: u32,
    remaining: u32,
    /// Payload bytes per fragment, as the packer was built.
    payload_size: usize,
    frag_offset: u32,
    payload: &'a [u8],
}

impl<'a> Iterator for Fragments<'a> {
    type Item = Object<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        // `total_len` is past the prefix and inside `MAX_OBJECT`, so the
        // offset and the fragment length both fit their header fields.
        let start = usize::try_from(self.frag_offset).ok()?;
        let end = start
            .checked_add(self.payload_size)?
            .min(self.payload.len());
        let payload = self.payload.get(start..end)?;
        let len = u32::try_from(payload.len()).ok()?;

        let mut flags = self.flags;
        if end == self.payload.len() {
            flags = flags.union(Flags::LAST_FRAGMENT);
        }

        let object = Object {
            class: self.class,
            lane: self.lane,
            flags,
            obj_id: self.obj_id,
            total_len: self.total_len,
            frag_offset: self.frag_offset,
            payload,
        };
        self.frag_offset = self.frag_offset.saturating_add(len);
        self.remaining -= 1;
        Some(object)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = usize::try_from(self.remaining).unwrap_or(usize::MAX);
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for Fragments<'_> {}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::path::PathBuf;

    use siphon_shared::riffle::object::DATAGRAM_HEADER;

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

    #[derive(Default)]
    struct Report {
        access_units: usize,
        audio: usize,
        audio_fragments: usize,
        fragments: usize,
        keyframes: usize,
        /// Largest access unit in bytes, without the prefix.
        largest: usize,
        largest_fragments: u32,
    }

    /// Pack every access unit of a fed segment and verify each object end to
    /// end: class, flags, ordering, budget, datagram round trip and reassembly.
    fn pack_all(source: &SegmentSource, packer: &mut Packer, scratch: &mut Vec<u8>) -> Report {
        let mut report = Report::default();
        let mut expected = Vec::new();
        let mut reassembled = Vec::new();
        let mut datagram = [0u8; DATAGRAM_BUDGET];
        let mut last_au_id: Option<u64> = None;

        for unit in source.access_units() {
            report.access_units += 1;
            let keyframe = unit.track == Track::Video && unit.keyframe;
            report.keyframes += usize::from(keyframe);
            if unit.track == Track::Audio {
                report.audio += 1;
            }

            expected.clear();
            expected.extend_from_slice(&unit.pts.to_be_bytes());
            expected.extend_from_slice(&unit.dts.to_be_bytes());
            expected.extend_from_slice(unit.data);
            let total_len = u32::try_from(expected.len()).expect("an access unit fits an object");

            reassembled.clear();
            let mut emitted = 0u32;
            let mut au_id = None;
            for object in packer.pack(unit, scratch).expect("the access unit packs") {
                emitted += 1;
                // Every fragment of one access unit shares its object id, and
                // the next access unit's id is above it.
                if let Some(id) = au_id {
                    assert_eq!(object.obj_id, id, "one id per access unit");
                } else {
                    if let Some(previous) = last_au_id {
                        assert!(
                            object.obj_id > previous,
                            "obj_id must strictly increase across the segment"
                        );
                    }
                    au_id = Some(object.obj_id);
                }
                assert_eq!(object.class, unit.track.class());
                assert_eq!(
                    object.lane,
                    packer.lane(),
                    "every fragment names the packer's lane"
                );
                assert_eq!(
                    object.flags.contains(Flags::KEYFRAME),
                    keyframe,
                    "the keyframe bit is exactly what the demuxer said"
                );
                assert_eq!(object.total_len, total_len);
                assert_eq!(
                    usize::try_from(object.frag_offset).unwrap(),
                    reassembled.len(),
                    "fragments arrive in order and cover the object"
                );
                assert_ne!(object.payload.len(), 0, "a fragment carries bytes");
                assert!(
                    object.payload.len() <= packer.fragment_payload(),
                    "a fragment is {} bytes, past the budget",
                    object.payload.len()
                );
                assert_eq!(
                    object.flags.contains(Flags::LAST_FRAGMENT),
                    object.is_last(),
                    "the last fragment is the one that ends the object"
                );
                if object.frag_offset == 0 {
                    assert_eq!(
                        timestamps(object.payload),
                        Some((unit.pts, unit.dts)),
                        "fragment 0 carries the whole prefix"
                    );
                }

                let written = object
                    .encode_datagram(&mut datagram)
                    .expect("a fragment fits a datagram buffer");
                assert_eq!(written, DATAGRAM_HEADER + object.payload.len());
                assert!(
                    written <= DATAGRAM_BUDGET,
                    "a datagram is {written} bytes, past the {DATAGRAM_BUDGET} byte budget"
                );
                let decoded =
                    Object::decode_datagram(&datagram[..written]).expect("the datagram decodes");
                assert_eq!(decoded, object, "the datagram round trip is lossless");

                reassembled.extend_from_slice(object.payload);
                report.fragments += 1;
                if unit.track == Track::Audio {
                    report.audio_fragments += 1;
                }
            }
            last_au_id = au_id;
            assert!(
                last_au_id.is_some(),
                "an access unit is at least one fragment"
            );

            assert_eq!(
                emitted,
                fragment_count(total_len, packer.fragment_payload()),
                "the packer agrees with fragment_count exactly"
            );
            assert_eq!(
                reassembled, expected,
                "the fragments reassemble to the prefixed access unit"
            );
            assert_eq!(usize::try_from(total_len).unwrap(), reassembled.len());

            if unit.data.len() > report.largest {
                report.largest = unit.data.len();
                report.largest_fragments = emitted;
            }
        }

        report
    }

    fn print_report(label: &str, report: &Report) {
        println!(
            "{label}: {} access units ({} audio), {} fragments, {} keyframes, \
             largest AU {} B in {} fragments",
            report.access_units,
            report.audio,
            report.fragments,
            report.keyframes,
            report.largest,
            report.largest_fragments
        );
    }

    #[test]
    fn detects_the_container_from_the_bytes() {
        assert_eq!(Container::detect(&[0x47; 376]), Some(Container::Ts));
        assert_eq!(
            Container::detect(&[0, 0, 0, 16, b'm', b'o', b'o', b'f']),
            Some(Container::Fmp4)
        );
        assert_eq!(
            Container::detect(&[0, 0, 1, 0x88, b'e', b'm', b's', b'g']),
            Some(Container::Fmp4)
        );
        assert_eq!(Container::detect(&[]), None);
        assert_eq!(Container::detect(b"not a segment"), None);
    }

    #[test]
    fn an_access_unit_packs_with_its_timestamps() {
        let unit = AccessUnit {
            track: Track::Video,
            pts: 90_001,
            dts: 89_999,
            timescale: ts::TIMESCALE,
            keyframe: true,
            data: &[0, 0, 0, 1, 0x65, 1, 2, 3],
        };
        let mut packer = Packer::new(DATAGRAM_BUDGET);
        let mut scratch = Vec::new();

        let objects: Vec<_> = packer.pack(unit, &mut scratch).unwrap().collect();
        assert_eq!(objects.len(), 1);
        let object = objects[0];
        assert_eq!(object.class, Class::Video);
        assert!(object.flags.contains(Flags::KEYFRAME));
        assert!(object.flags.contains(Flags::LAST_FRAGMENT));
        assert_eq!(
            object.total_len,
            u32::try_from(TIMESTAMP_PREFIX).unwrap() + 8
        );
        assert_eq!(object.frag_offset, 0);
        assert_eq!(timestamps(object.payload), Some((90_001, 89_999)));
        assert_eq!(&object.payload[TIMESTAMP_PREFIX..], unit.data);
        assert_eq!(packer.next_id(), 1);

        // Audio never carries the video keyframe bit, even if a source sets it.
        let audio = AccessUnit {
            track: Track::Audio,
            pts: 1,
            dts: 1,
            timescale: ts::TIMESCALE,
            keyframe: true,
            data: &[0xAA; 400],
        };
        let objects: Vec<_> = packer.pack(audio, &mut scratch).unwrap().collect();
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].class, Class::Audio);
        assert!(!objects[0].flags.contains(Flags::KEYFRAME));
        assert_eq!(objects[0].obj_id, 1, "the lane's ids continue");
    }

    /// The lane is in the header, not inferred from the id range: every
    /// fragment of every object names it, and the ids stay inside the lane's
    /// own range.
    #[test]
    fn a_packer_stamps_its_lane_on_every_fragment() {
        let data = vec![0xAB; 3000];
        let unit = AccessUnit {
            track: Track::Video,
            pts: 1,
            dts: 2,
            timescale: ts::TIMESCALE,
            keyframe: true,
            data: &data,
        };
        let base = 7u64 << 40;
        let mut packer = Packer::for_lane(7, base, DATAGRAM_BUDGET);
        let mut scratch = Vec::new();
        let objects: Vec<_> = packer.pack(unit, &mut scratch).unwrap().collect();

        assert_eq!(packer.lane(), 7);
        assert!(objects.len() > 1, "the unit fragments");
        for object in &objects {
            assert_eq!(object.lane, 7);
            assert_eq!(object.obj_id, base);
        }
        assert_eq!(packer.next_id(), base + 1, "one id per access unit");
    }

    #[test]
    fn a_large_access_unit_fragments_to_the_budget() {
        let data = vec![0xAB; 3000];
        let unit = AccessUnit {
            track: Track::Video,
            pts: 1,
            dts: 2,
            timescale: ts::TIMESCALE,
            keyframe: true,
            data: &data,
        };
        let mut packer = Packer::new(DATAGRAM_BUDGET);
        let mut scratch = Vec::new();
        let objects: Vec<_> = packer.pack(unit, &mut scratch).unwrap().collect();

        let total_len = u32::try_from(TIMESTAMP_PREFIX + data.len()).unwrap();
        assert_eq!(fragment_count(total_len, FRAGMENT_PAYLOAD), 3);
        assert_eq!(objects.len(), 3);
        assert_eq!(objects[0].frag_offset, 0);
        assert_eq!(objects[0].payload.len(), FRAGMENT_PAYLOAD);
        assert_eq!(
            objects[1].frag_offset,
            u32::try_from(FRAGMENT_PAYLOAD).unwrap()
        );
        assert_eq!(objects[1].payload.len(), FRAGMENT_PAYLOAD);
        assert_eq!(
            objects[2].frag_offset,
            u32::try_from(2 * FRAGMENT_PAYLOAD).unwrap()
        );
        assert_eq!(
            objects[2].payload.len(),
            usize::try_from(total_len).unwrap() - 2 * FRAGMENT_PAYLOAD
        );
        assert!(!objects[0].is_last());
        assert!(!objects[1].is_last());
        assert!(objects[2].is_last());

        let mut joined = Vec::new();
        for object in &objects {
            assert_eq!(object.obj_id, 0);
            assert_eq!(object.total_len, total_len);
            joined.extend_from_slice(object.payload);
        }
        assert_eq!(joined.len(), usize::try_from(total_len).unwrap());
        assert_eq!(&joined[TIMESTAMP_PREFIX..], &data[..]);
    }

    #[test]
    fn an_access_unit_past_the_object_ceiling_is_refused() {
        let data = vec![0u8; usize::try_from(MAX_OBJECT).unwrap()];
        let unit = AccessUnit {
            track: Track::Video,
            pts: 0,
            dts: 0,
            timescale: ts::TIMESCALE,
            keyframe: true,
            data: &data,
        };
        let mut packer = Packer::from_id(7, DATAGRAM_BUDGET);
        let mut scratch = Vec::new();
        assert!(matches!(
            packer.pack(unit, &mut scratch),
            Err(Error::TooLarge(_))
        ));
        assert_eq!(packer.next_id(), 7, "a refused unit consumes no id");
    }

    #[test]
    fn a_truncated_ts_segment_is_an_error() {
        let mut source = SegmentSource::new(Container::Ts, None).unwrap();
        // A whole number of packets minus a byte.
        assert!(matches!(
            source.feed(&[0x47; ts::TS_PACKET - 1]),
            Err(Error::Truncated("MPEG-TS segment"))
        ));
        assert!(matches!(
            source.feed(&[]),
            Err(Error::Truncated("MPEG-TS segment"))
        ));
        assert_eq!(source.access_units().len(), 0);
    }

    #[test]
    fn packs_the_ts_fixture() {
        let Some(path) = fixture("servermock/live/000000.ts") else {
            println!("captures/ is absent; skipping the TS packing test");
            return;
        };
        let bytes = read(&path);
        assert_eq!(Container::detect(&bytes), Some(Container::Ts));

        let source = SegmentSource::open(&bytes, None).expect("the TS segment demuxes");
        assert_eq!(source.container(), Container::Ts);
        let mut packer = Packer::new(DATAGRAM_BUDGET);
        let mut scratch = Vec::new();
        let report = pack_all(&source, &mut packer, &mut scratch);
        print_report("servermock/live/000000.ts", &report);

        assert!(report.access_units > 0);
        assert!(report.audio > 0);
        assert!(report.keyframes > 0, "the segment starts with an IDR");
        assert_eq!(
            report.audio_fragments, report.audio,
            "one AAC frame is one fragment"
        );
        assert!(
            report.largest > FRAGMENT_PAYLOAD,
            "a video keyframe has to fragment"
        );
        assert!(report.largest_fragments > 1);
        assert_eq!(
            packer.next_id(),
            u64::try_from(report.access_units).unwrap(),
            "one id per access unit"
        );
    }

    #[test]
    fn packs_the_h264_fmp4_fixture() {
        let Some(dir) = fixture("servermock-fmp4/landscape") else {
            println!("captures/ is absent; skipping the H.264 fMP4 packing test");
            return;
        };
        let init = read(&dir.join("init.mp4"));
        let segment = read(&dir.join("live/000000.m4s"));
        assert_eq!(Container::detect(&segment), Some(Container::Fmp4));

        // The init segment is not optional state the source guesses at.
        assert!(matches!(
            SegmentSource::open(&segment, None),
            Err(Error::MissingInit)
        ));

        let source = SegmentSource::open(&segment, Some(&init)).expect("the fMP4 segment demuxes");
        assert_eq!(source.container(), Container::Fmp4);
        let mut packer = Packer::new(DATAGRAM_BUDGET);
        let mut scratch = Vec::new();
        let report = pack_all(&source, &mut packer, &mut scratch);
        print_report("servermock-fmp4/landscape/live/000000.m4s", &report);

        assert!(report.access_units > 0);
        assert!(report.audio > 0);
        assert!(report.keyframes > 0);
        assert_eq!(
            report.audio_fragments, report.audio,
            "one AAC frame is one fragment"
        );
        assert!(
            report.largest > FRAGMENT_PAYLOAD,
            "a video keyframe has to fragment"
        );
        assert!(report.largest_fragments > 1);
    }

    #[test]
    fn packs_the_h265_fixture() {
        let Some(dir) =
            fixture("ozeniken-30s/ozeniken-portrait-dvr/tmp/ozeniken-portrait-dvr/0____")
        else {
            println!("captures/ is absent; skipping the H.265 fMP4 packing test");
            return;
        };
        let init = read(&dir.join("_init.mp4"));
        let segment = read(&dir.join("1795300590544.m4s"));
        assert_eq!(Container::detect(&segment), Some(Container::Fmp4));

        let source = SegmentSource::open(&segment, Some(&init)).expect("the fMP4 segment demuxes");
        let mut packer = Packer::new(DATAGRAM_BUDGET);
        let mut scratch = Vec::new();
        let report = pack_all(&source, &mut packer, &mut scratch);
        print_report("ozeniken/1795300590544.m4s", &report);

        assert!(report.access_units > 0);
        assert!(report.keyframes > 0, "an IRAP has to open the segment");
        // This source is low-bitrate portrait video (a ~10 s, 600-frame
        // segment at under 100 KB), so even its IRAPs are below the 1200-byte
        // budget: every access unit is one fragment. Fragmentation itself is
        // covered by the other two fixtures and by the synthetic unit test.
        assert_eq!(
            report.fragments, report.access_units,
            "every access unit here fits one fragment"
        );
    }

    /// Every video keyframe's presentation time on the broadcast timeline, in
    /// demux order, mapped the way the lane producer maps them: a segment
    /// starts at `segment_ms` and its first video access unit anchors the
    /// track's clock.
    fn media_keyframes(source: &SegmentSource, segment_ms: u64) -> Vec<u64> {
        let mut anchor: Option<i64> = None;
        let mut keyframes = Vec::new();
        for unit in source.access_units() {
            if unit.track != Track::Video {
                continue;
            }
            let container_ms = i64::try_from(unit.pts_ms()).unwrap();
            let offset =
                *anchor.get_or_insert_with(|| i64::try_from(segment_ms).unwrap() - container_ms);
            if unit.keyframe {
                keyframes.push(u64::try_from(container_ms + offset).unwrap());
            }
        }
        keyframes
    }

    /// A segment boundary is not a GOP boundary: this fixture's chunk carries
    /// five IRAPs, so a seek answer has to come from the access units and must
    /// land strictly before its target rather than on the next GOP.
    #[test]
    fn a_seek_lands_on_the_keyframe_before_its_target() {
        let Some(dir) = fixture("servermock-fmp4/landscape") else {
            println!("captures/ is absent; skipping the fMP4 seek test");
            return;
        };
        let init = read(&dir.join("init.mp4"));
        let segment = read(&dir.join("chunks/000.m4s"));
        let source = SegmentSource::open(&segment, Some(&init)).expect("the fMP4 chunk demuxes");

        let keyframes = media_keyframes(&source, 0);
        assert!(
            keyframes.len() >= 3,
            "the fixture's chunk has five IRAPs, which is why seek cannot use a segment boundary"
        );
        // Halfway between the second and third IRAP: unambiguously inside the
        // second GOP, not on a keyframe.
        let target = keyframes[1] + (keyframes[2] - keyframes[1]) / 2;
        assert!(keyframes[1] < target);
        assert_eq!(source.seek_keyframe(0, target), Some(keyframes[1]));
        assert!(
            source.seek_keyframe(0, target).unwrap() < target,
            "the answer never rounds up to the next GOP"
        );

        // A target before every keyframe still starts at one: the first.
        assert_eq!(source.seek_keyframe(0, 0), Some(keyframes[0]));
        // And with the segment starting later in the broadcast, the answer is
        // on that timeline, not the container's.
        assert_eq!(
            source.seek_keyframe(10_000, 10_000 + target),
            Some(10_000 + keyframes[1])
        );
    }

    /// A segment with no video access unit has no keyframe to align to, so a
    /// seek cannot invent one.
    #[test]
    fn a_seek_point_needs_a_video_keyframe() {
        // Two whole MPEG-TS packets with no PAT, PMT or PES: valid framing,
        // no samples.
        let Ok(source) = SegmentSource::open(&[0x47; 376], None) else {
            return;
        };
        assert_eq!(source.access_units().len(), 0);
        assert_eq!(source.seek_keyframe(0, 5_000), None);
    }

    #[test]
    fn a_truncated_fmp4_segment_is_an_error() {
        let Some(dir) = fixture("servermock-fmp4/landscape") else {
            println!("captures/ is absent; skipping the truncated-segment test");
            return;
        };
        let init = read(&dir.join("init.mp4"));
        let segment = read(&dir.join("live/000000.m4s"));
        let truncated = segment
            .get(..1024)
            .expect("the fixture is longer than 1 KiB");

        let mut source = SegmentSource::new(Container::Fmp4, Some(&init)).unwrap();
        assert!(source.feed(truncated).is_err());
        assert_eq!(
            source.access_units().len(),
            0,
            "a failed segment yields nothing"
        );
    }
}
