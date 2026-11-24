//! The UI's receiving half of the media lane: objects back into access units.
//!
//! The daemon pushes one object per frame, and this module reassembles: the
//! header names the lane (`crates/shared/src/riffle/object.rs`), so it
//! demultiplexes by `(lane, class)` before reassembling by `obj_id` and
//! `frag_offset`. Fragments may arrive out of order, and two objects or two
//! lanes may interleave; the reassembly is per object, not per connection.
//!
//! # Two carriers, one assembler
//!
//! The bytes are the same either way. A QUIC DATAGRAM frame carries one object;
//! on the local pipe, one object is the payload of a class-1 frame on the
//! control stream. [`Source`] is the only place that difference exists, so the
//! reassembly, the deadlines and the counters are shared.
//!
//! # Whole objects only
//!
//! A missing fragment dooms the object: a truncated access unit is not a
//! cheaper picture, it is a broken bitstream. So an object is dropped whole —
//! and counted — as soon as a newer object on the same lane completes, or when
//! its deadline passes. Nothing partial ever reaches the sink.
//!
//! # Client-side counters
//!
//! Delivered and dropped counts live here, in the UI. The daemon's `ConnStats`
//! is the send side (`lost` is a datagram the carrier refused); a receive-side
//! loss is a different number on a different machine, so folding it into the
//! daemon's counters would make one number mean two things.
//!
//! There is no decoder or player yet: [`run`] is the network task, and the
//! counters plus the last delivered unit's lane and `pts` are the surface a
//! view can read. The bytes are handed to a [`Sink`] only for the length of the
//! call, so a future decoder can copy them into a packet without this module
//! growing a per-access-unit allocation.

// The datagram handle and the receiver are `!Send` by design: compio runs the
// whole worker on one thread. This is the same deliberate exception `riffle`
// takes.
#![allow(clippy::future_not_send)]

use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;

use siphon_shared::riffle::Class;
use siphon_shared::riffle::Flags;
use siphon_shared::riffle::codec::Frame;
use siphon_shared::riffle::object::Object;
use siphon_shared::riffle::object::TIMESTAMP_PREFIX;
use siphon_shared::riffle::quic::Datagrams;

use crate::riffle::UiEvent;

/// How long a fragment may wait for the rest of its object.
///
/// Live Twitch media is a couple of seconds behind the sender at most, and a
/// reordered datagram arrives within a path's worth of packets; half a second
/// is far past either, so an object still incomplete then is genuinely lost.
const REASSEMBLY_DEADLINE: Duration = Duration::from_millis(500);

/// How often the network task wakes to expire abandoned partials.
const TICK: Duration = Duration::from_millis(100);

/// How often the counters are pushed to the window.
const REPORT_INTERVAL: Duration = Duration::from_millis(250);

/// Buffers kept for reuse, so a stream does not allocate one per access unit.
const POOL_DEPTH: usize = 16;

/// The classes the datagram lane carries. Control rides the bidirectional
/// stream and blobs their own; neither has a timestamp prefix, so neither is
/// assembled here.
const fn is_media(class: Class) -> bool {
    matches!(class, Class::Video | Class::Audio)
}

fn slot(class: Class) -> usize {
    usize::from(class.as_u8())
}

/// One reassembled access unit, borrowed for the length of [`Sink::deliver`].
// The production sink is `()` until there is a decoder, so no field is read
// yet; this is the contract a real sink receives, not internal state.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub struct AccessUnit<'a> {
    pub lane: u16,
    pub class: Class,
    /// Presentation time, from the object's 16-byte payload prefix, in the
    /// track's timescale.
    pub pts: i64,
    /// Decode time, from the same prefix.
    pub dts: i64,
    pub keyframe: bool,
    /// The access unit itself: the prefix has been stripped.
    pub bytes: &'a [u8],
}

/// Where completed access units go. Held only for the call, so the assembler
/// can take its reassembly buffer back when the sink returns.
pub trait Sink {
    fn deliver(&mut self, unit: AccessUnit<'_>);
}

/// The production sink until there is a decoder: nothing consumes the bytes,
/// but the counters and the last unit still describe what arrived.
impl Sink for () {
    fn deliver(&mut self, _unit: AccessUnit<'_>) {}
}

/// What the window can show of the receive side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MediaView {
    pub delivered: u64,
    /// Dropped objects, by class.
    pub dropped_video: u64,
    pub dropped_audio: u64,
    pub dropped_control: u64,
    pub dropped_blob: u64,
    /// How long ago the last whole-object drop happened.
    pub last_drop_age_ms: Option<u64>,
    /// The lane of the last delivered access unit.
    pub last_lane: Option<u16>,
    /// Its presentation time, in the track's timescale.
    pub last_pts: Option<i64>,
}

/// Receive-side counters. Local to this client, by design: see the module docs.
#[derive(Debug, Default)]
pub struct MediaStats {
    delivered: u64,
    dropped: [u64; 4],
    last_drop: Option<Instant>,
    last_lane: Option<u16>,
    last_pts: Option<i64>,
}

impl MediaStats {
    #[must_use]
    pub const fn delivered(&self) -> u64 {
        self.delivered
    }

    #[must_use]
    pub fn dropped(&self, class: Class) -> u64 {
        self.dropped[slot(class)]
    }

    /// The counters as the window reads them, with the drop age resolved at
    /// `now` because an age is only meaningful at a moment.
    #[must_use]
    pub fn view(&self, now: Instant) -> MediaView {
        MediaView {
            delivered: self.delivered(),
            dropped_video: self.dropped(Class::Video),
            dropped_audio: self.dropped(Class::Audio),
            dropped_control: self.dropped(Class::Control),
            dropped_blob: self.dropped(Class::Blob),
            last_drop_age_ms: self.last_drop.map(|at| {
                u64::try_from(now.saturating_duration_since(at).as_millis()).unwrap_or(u64::MAX)
            }),
            last_lane: self.last_lane,
            last_pts: self.last_pts,
        }
    }

    const fn note_delivered(&mut self, lane: u16, pts: i64) {
        self.delivered = self.delivered.saturating_add(1);
        self.last_lane = Some(lane);
        self.last_pts = Some(pts);
    }

    fn note_drop(&mut self, class: Class, now: Instant) {
        let count = &mut self.dropped[slot(class)];
        *count = count.saturating_add(1);
        self.last_drop = Some(now);
    }
}

/// One object's bytes and the ranges that have arrived.
///
/// The ranges are kept sorted and merged, so "complete" is one comparison
/// against `total_len` and a duplicate fragment changes nothing.
#[derive(Default)]
struct Buffer {
    bytes: Vec<u8>,
    ranges: Vec<(usize, usize)>,
}

impl Buffer {
    fn insert(&mut self, start: usize, payload: &[u8]) {
        let end = start.saturating_add(payload.len());
        if let Some(dst) = self.bytes.get_mut(start..end) {
            dst.copy_from_slice(payload);
        }
        self.cover(start, end);
    }

    /// Adds one half-open range, then merges everything it touches.
    fn cover(&mut self, start: usize, end: usize) {
        let at = self.ranges.partition_point(|&(from, _)| from < start);
        self.ranges.insert(at, (start, end));
        let mut index = 0;
        while index + 1 < self.ranges.len() {
            let (_, left_end) = self.ranges[index];
            let (right_start, right_end) = self.ranges[index + 1];
            if right_start <= left_end {
                self.ranges[index].1 = left_end.max(right_end);
                self.ranges.remove(index + 1);
            } else {
                index += 1;
            }
        }
    }

    fn covered(&self) -> u64 {
        self.ranges.iter().fold(0u64, |sum, &(start, end)| {
            sum.saturating_add(u64::try_from(end.saturating_sub(start)).unwrap_or(0))
        })
    }
}

/// Buffers returned after a delivery or a drop, so a steady stream allocates
/// nothing per access unit once the pool has warmed up.
#[derive(Default)]
struct Pool {
    buffers: Vec<Buffer>,
}

impl Pool {
    fn take(&mut self, total_len: u32) -> Buffer {
        let mut buffer = self.buffers.pop().unwrap_or_default();
        buffer.bytes.clear();
        buffer
            .bytes
            .resize(usize::try_from(total_len).unwrap_or(0), 0);
        buffer.ranges.clear();
        buffer
    }

    fn give(&mut self, mut buffer: Buffer) {
        buffer.bytes.clear();
        buffer.ranges.clear();
        if self.buffers.len() < POOL_DEPTH {
            self.buffers.push(buffer);
        }
    }
}

/// One object being reassembled.
struct Partial {
    obj_id: u64,
    class: Class,
    keyframe: bool,
    total_len: u32,
    first_seen: Instant,
    buffer: Buffer,
}

impl Partial {
    fn is_complete(&self) -> bool {
        self.buffer.covered() == u64::from(self.total_len)
    }
}

/// The per-lane reassembly state: the highest completed object id, and one
/// pending list per class so `(lane, class)` is the demultiplex key.
struct LaneState {
    last_completed: Option<u64>,
    pending: [Vec<Partial>; 4],
}

impl Default for LaneState {
    fn default() -> Self {
        Self {
            last_completed: None,
            pending: std::array::from_fn(|_| Vec::new()),
        }
    }
}

/// Turns the connection's datagrams back into whole access units.
pub struct MediaReceiver<S: Sink> {
    sink: S,
    lanes: HashMap<u16, LaneState>,
    pool: Pool,
    stats: MediaStats,
}

impl<S: Sink> MediaReceiver<S> {
    #[must_use]
    pub fn new(sink: S) -> Self {
        Self {
            sink,
            lanes: HashMap::new(),
            pool: Pool::default(),
            stats: MediaStats::default(),
        }
    }

    #[must_use]
    pub fn view(&self, now: Instant) -> MediaView {
        self.stats.view(now)
    }

    /// Feeds one object in its datagram form: a QUIC datagram, or the payload
    /// of a class-1 frame on the pipe. A whole object is delivered to the sink
    /// if this fragment completed one; otherwise nothing happens but
    /// bookkeeping.
    pub fn push(&mut self, datagram: &[u8], now: Instant) {
        // A live stream delivers often enough that this is the only expiry
        // needed; the network task's tick covers an idle connection.
        self.expire(now);
        let Ok(object) = Object::decode_datagram(datagram) else {
            return;
        };
        if !is_media(object.class) {
            return;
        }
        let Ok(start) = usize::try_from(object.frag_offset) else {
            return;
        };

        let Self {
            sink,
            lanes,
            pool,
            stats,
        } = self;
        let lane = lanes.entry(object.lane).or_default();
        // A completed object is the lane's high-water mark. Anything at or
        // below it is either a duplicate of what was delivered or an object
        // whose moment has passed.
        if lane
            .last_completed
            .is_some_and(|last| object.obj_id <= last)
        {
            return;
        }

        let pending = &mut lane.pending[slot(object.class)];
        let index = pending
            .iter()
            .position(|partial| partial.obj_id == object.obj_id)
            .unwrap_or_else(|| {
                pending.push(Partial {
                    obj_id: object.obj_id,
                    class: object.class,
                    keyframe: object.flags.contains(Flags::KEYFRAME),
                    total_len: object.total_len,
                    first_seen: now,
                    buffer: pool.take(object.total_len),
                });
                pending.len() - 1
            });
        let partial = &mut pending[index];
        // A fragment that disagrees with the object it claims to belong to is
        // not recoverable: drop the object whole rather than splice it.
        if partial.total_len != object.total_len {
            let partial = pending.swap_remove(index);
            stats.note_drop(partial.class, now);
            pool.give(partial.buffer);
            return;
        }
        partial.buffer.insert(start, object.payload);
        if !partial.is_complete() {
            return;
        }

        let partial = pending.swap_remove(index);
        lane.last_completed = Some(object.obj_id);
        // Every older object on this lane is now doomed, whatever its class:
        // the newer object took its place in the timeline.
        for older in &mut lane.pending {
            let mut at = 0;
            while at < older.len() {
                if older[at].obj_id < object.obj_id {
                    let dropped = older.swap_remove(at);
                    stats.note_drop(dropped.class, now);
                    pool.give(dropped.buffer);
                } else {
                    at += 1;
                }
            }
        }

        let Some((pts, dts)) = timestamps(&partial.buffer.bytes) else {
            stats.note_drop(partial.class, now);
            pool.give(partial.buffer);
            return;
        };
        stats.note_delivered(object.lane, pts);
        sink.deliver(AccessUnit {
            lane: object.lane,
            class: partial.class,
            pts,
            dts,
            keyframe: partial.keyframe,
            bytes: &partial.buffer.bytes[TIMESTAMP_PREFIX..],
        });
        pool.give(partial.buffer);
    }

    /// Drops every partial whose deadline has passed, counting each object
    /// once. Called on every datagram and by the network task's tick.
    pub fn expire(&mut self, now: Instant) {
        let Self {
            lanes, pool, stats, ..
        } = self;
        for lane in lanes.values_mut() {
            for pending in &mut lane.pending {
                let mut at = 0;
                while at < pending.len() {
                    if now.saturating_duration_since(pending[at].first_seen) >= REASSEMBLY_DEADLINE
                    {
                        let dropped = pending.swap_remove(at);
                        stats.note_drop(dropped.class, now);
                        pool.give(dropped.buffer);
                    } else {
                        at += 1;
                    }
                }
            }
        }
    }
}

/// `pts` then `dts`, both big-endian `i64`. `None` when the object is shorter
/// than the prefix, which only a corrupt media object can be.
fn timestamps(payload: &[u8]) -> Option<(i64, i64)> {
    let pts = payload.get(..8)?;
    let dts = payload.get(8..TIMESTAMP_PREFIX)?;
    Some((
        i64::from_be_bytes(pts.try_into().ok()?),
        i64::from_be_bytes(dts.try_into().ok()?),
    ))
}

/// Where media objects arrive from.
///
/// The two carriers hand over the same bytes — one object in the datagram form
/// — so this is the whole of the difference between them.
pub enum Source {
    /// The datagram channel of a QUIC connection.
    Datagrams(Datagrams),
    /// Class-1 frames on the local pipe, taken off the control stream.
    Frames(kanal::AsyncReceiver<Frame>),
}

/// What one wait produced.
enum Arrival {
    Datagram(Vec<u8>),
    Frame(Frame),
    /// The carrier is gone: the connection closed.
    Closed,
    /// Nothing arrived before the tick.
    Idle,
}

async fn arrive(source: &Source) -> Arrival {
    match source {
        Source::Datagrams(datagrams) => match compio::time::timeout(TICK, datagrams.recv()).await {
            Ok(Some(datagram)) => Arrival::Datagram(datagram),
            Ok(None) => Arrival::Closed,
            Err(_) => Arrival::Idle,
        },
        Source::Frames(frames) => match compio::time::timeout(TICK, frames.recv()).await {
            Ok(Ok(frame)) => Arrival::Frame(frame),
            Ok(Err(_)) => Arrival::Closed,
            Err(_) => Arrival::Idle,
        },
    }
}

/// Reads objects from either carrier until the connection closes, then reports
/// the final counters. Runs on the RPC worker's compio thread, never the frame
/// loop.
pub async fn run(source: Source, events: kanal::Sender<UiEvent>) {
    let mut receiver = MediaReceiver::new(());
    let mut last_report = Instant::now();
    loop {
        match arrive(&source).await {
            Arrival::Datagram(datagram) => receiver.push(&datagram, Instant::now()),
            Arrival::Frame(frame) => receiver.push(frame.payload(), Instant::now()),
            Arrival::Closed => break,
            Arrival::Idle => receiver.expire(Instant::now()),
        }
        let now = Instant::now();
        if now.saturating_duration_since(last_report) >= REPORT_INTERVAL {
            last_report = now;
            let _ = events.try_send(UiEvent::Media(receiver.view(now)));
        }
    }
    let _ = events.try_send(UiEvent::Media(receiver.view(Instant::now())));
}

#[cfg(test)]
mod tests {
    use siphon_shared::riffle::object::DATAGRAM_HEADER;

    use super::*;

    /// One delivered unit, copied so a test can look at it after the sink call.
    #[derive(Debug, PartialEq, Eq)]
    struct Delivered {
        lane: u16,
        class: Class,
        pts: i64,
        dts: i64,
        keyframe: bool,
        bytes: Vec<u8>,
    }

    #[derive(Default)]
    struct Collected {
        units: Vec<Delivered>,
    }

    impl Sink for Collected {
        fn deliver(&mut self, unit: AccessUnit<'_>) {
            self.units.push(Delivered {
                lane: unit.lane,
                class: unit.class,
                pts: unit.pts,
                dts: unit.dts,
                keyframe: unit.keyframe,
                bytes: unit.bytes.to_vec(),
            });
        }
    }

    fn receiver() -> MediaReceiver<Collected> {
        MediaReceiver::new(Collected::default())
    }

    /// The prefixed payload cut into no more than `chunk` bytes, encoded as the
    /// datagrams one object becomes.
    fn fragments(
        lane: u16,
        class: Class,
        obj_id: u64,
        (pts, dts): (i64, i64),
        keyframe: bool,
        data: &[u8],
        chunk: usize,
    ) -> Vec<Vec<u8>> {
        let mut payload = Vec::with_capacity(TIMESTAMP_PREFIX + data.len());
        payload.extend_from_slice(&pts.to_be_bytes());
        payload.extend_from_slice(&dts.to_be_bytes());
        payload.extend_from_slice(data);
        let total_len = u32::try_from(payload.len()).expect("a test object fits");
        let mut datagrams = Vec::new();
        let mut offset = 0;
        while offset < payload.len() {
            let end = (offset + chunk).min(payload.len());
            let start = u32::try_from(offset).unwrap();
            let mut flags = if keyframe {
                Flags::KEYFRAME
            } else {
                Flags::NONE
            };
            if end == payload.len() {
                flags = flags.union(Flags::LAST_FRAGMENT);
            }
            let object = Object {
                class,
                lane,
                flags,
                obj_id,
                total_len,
                frag_offset: start,
                payload: &payload[offset..end],
            };
            let mut datagram = vec![0u8; DATAGRAM_HEADER + (end - offset)];
            let written = object
                .encode_datagram(&mut datagram)
                .expect("a test datagram encodes");
            datagram.truncate(written);
            datagrams.push(datagram);
            offset = end;
        }
        datagrams
    }

    #[test]
    fn fragments_in_order_deliver_one_access_unit() {
        let now = Instant::now();
        let data = vec![0xAB; 250];
        let mut receiver = receiver();
        for datagram in fragments(4, Class::Video, 9, (90_001, 89_999), true, &data, 100) {
            receiver.push(&datagram, now);
        }
        assert_eq!(receiver.sink.units.len(), 1);
        let unit = &receiver.sink.units[0];
        assert_eq!(unit.lane, 4);
        assert_eq!(unit.class, Class::Video);
        assert!(unit.keyframe);
        assert_eq!(unit.bytes, data);
        assert_eq!(receiver.stats.delivered(), 1);
    }

    #[test]
    fn fragments_out_of_order_deliver_one_access_unit() {
        let now = Instant::now();
        let data = vec![0xCD; 250];
        let mut datagrams = fragments(4, Class::Video, 9, (1, 2), false, &data, 100);
        datagrams.reverse();
        let mut receiver = receiver();
        for datagram in datagrams {
            receiver.push(&datagram, now);
        }
        assert_eq!(receiver.sink.units.len(), 1);
        assert_eq!(receiver.sink.units[0].bytes, data);
    }

    #[test]
    fn interleaved_lanes_and_objects_reassemble_separately() {
        let now = Instant::now();
        let video = vec![0x11; 250];
        let audio = vec![0x22; 40];
        let video_fragments = fragments(1, Class::Video, 3, (10, 10), true, &video, 100);
        let audio_fragments = fragments(2, Class::Audio, 40, (20, 20), false, &audio, 30);
        let mut interleaved = Vec::new();
        let mut video_fragments = video_fragments.into_iter();
        let mut audio_fragments = audio_fragments.into_iter();
        loop {
            let video = video_fragments.next();
            let audio = audio_fragments.next();
            if video.is_none() && audio.is_none() {
                break;
            }
            interleaved.extend(video);
            interleaved.extend(audio);
        }

        let mut receiver = receiver();
        for datagram in interleaved {
            receiver.push(&datagram, now);
        }
        assert_eq!(receiver.sink.units.len(), 2);
        let video_unit = receiver
            .sink
            .units
            .iter()
            .find(|unit| unit.class == Class::Video)
            .expect("the video object arrived");
        let audio_unit = receiver
            .sink
            .units
            .iter()
            .find(|unit| unit.class == Class::Audio)
            .expect("the audio object arrived");
        assert_eq!(video_unit.lane, 1);
        assert_eq!(video_unit.bytes, video);
        assert_eq!(audio_unit.lane, 2);
        assert_eq!(audio_unit.bytes, audio);
        assert_eq!(receiver.stats.delivered(), 2);
    }

    #[test]
    fn a_missing_fragment_drops_the_whole_access_unit() {
        let now = Instant::now();
        let mut broken = fragments(1, Class::Video, 10, (1, 1), false, &[0x33; 250], 100);
        // Lose the middle fragment: the object can never complete.
        broken.remove(1);
        let newer = fragments(1, Class::Video, 11, (2, 2), false, &[0x44; 40], 100);

        let mut receiver = receiver();
        for datagram in broken {
            receiver.push(&datagram, now);
        }
        for datagram in newer {
            receiver.push(&datagram, now);
        }
        assert_eq!(receiver.sink.units.len(), 1, "only the complete object");
        assert_eq!(receiver.sink.units[0].bytes, vec![0x44; 40]);
        assert_eq!(
            receiver.stats.dropped(Class::Video),
            1,
            "the partial object is counted once"
        );
        assert!(receiver.view(now).last_drop_age_ms.is_some());
    }

    #[test]
    fn a_stale_object_id_is_discarded() {
        let now = Instant::now();
        let mut receiver = receiver();
        for datagram in fragments(1, Class::Video, 5, (1, 1), false, &[0x55; 40], 100) {
            receiver.push(&datagram, now);
        }
        // An older id arrives late; it is not a loss, it is simply behind.
        for datagram in fragments(1, Class::Video, 4, (0, 0), false, &[0x66; 40], 100) {
            receiver.push(&datagram, now);
        }
        assert_eq!(receiver.sink.units.len(), 1);
        assert_eq!(receiver.sink.units[0].bytes, vec![0x55; 40]);
        assert_eq!(receiver.stats.delivered(), 1);
        assert_eq!(receiver.stats.dropped(Class::Video), 0);
    }

    #[test]
    fn a_duplicate_completes_once() {
        let now = Instant::now();
        let mut datagrams = fragments(1, Class::Audio, 7, (1, 1), false, &[0x77; 250], 100);
        let duplicate = datagrams[0].clone();
        datagrams.insert(1, duplicate);
        let mut receiver = receiver();
        for datagram in datagrams {
            receiver.push(&datagram, now);
        }
        // Audio is sent twice by design: a second copy of the whole object is
        // also stale by the high-water mark.
        for datagram in fragments(1, Class::Audio, 7, (1, 1), false, &[0x77; 250], 100) {
            receiver.push(&datagram, now);
        }
        assert_eq!(receiver.sink.units.len(), 1);
        assert_eq!(receiver.stats.delivered(), 1);
    }

    #[test]
    fn the_timestamp_prefix_round_trips() {
        let now = Instant::now();
        let data = vec![0x88; 40];
        let mut receiver = receiver();
        for datagram in fragments(6, Class::Audio, 1, (-90_123, -90_456), false, &data, 100) {
            receiver.push(&datagram, now);
        }
        let unit = &receiver.sink.units[0];
        assert_eq!(unit.pts, -90_123);
        assert_eq!(unit.dts, -90_456);
        assert_eq!(unit.bytes, data, "the prefix is not part of the bytes");
        assert_eq!(receiver.view(now).last_lane, Some(6));
        assert_eq!(receiver.view(now).last_pts, Some(-90_123));
    }

    /// The pipe delivers one object per class-1 frame on the control stream.
    /// Framed and read back the way the worker does it, the assembler must
    /// produce exactly the unit the datagram path produces — same bytes, same
    /// times, same keyframe bit.
    #[test]
    fn a_pipe_frame_delivers_the_same_access_unit_as_a_datagram() {
        use siphon_shared::riffle::codec::FrameClass;
        use siphon_shared::riffle::codec::FrameDecoder;
        use siphon_shared::riffle::codec::encode_media;

        let now = Instant::now();
        let data = vec![0x4C; 250];
        let datagrams = fragments(3, Class::Video, 12, (90_000, 89_998), true, &data, 100);

        let mut from_datagrams = receiver();
        for datagram in &datagrams {
            from_datagrams.push(datagram, now);
        }

        // What the daemon writes: each object framed, class byte included.
        let mut wire = Vec::new();
        for datagram in &datagrams {
            encode_media(datagram, &mut wire).expect("a media frame encodes");
        }
        let mut decoder = FrameDecoder::new();
        decoder.push(&wire);
        let mut from_pipe = receiver();
        while let Some(frame) = decoder.next_frame().expect("a whole frame") {
            assert_eq!(frame.class(), FrameClass::Media);
            from_pipe.push(frame.payload(), now);
        }

        assert_eq!(from_pipe.sink.units.len(), 1);
        assert_eq!(from_pipe.sink.units, from_datagrams.sink.units);
        assert_eq!(from_pipe.stats.delivered(), 1);
    }

    #[test]
    fn an_abandoned_partial_expires() {
        let now = Instant::now();
        let mut datagrams = fragments(1, Class::Video, 2, (1, 1), false, &[0x99; 250], 100);
        datagrams.pop();
        let mut receiver = receiver();
        for datagram in datagrams {
            receiver.push(&datagram, now);
        }
        assert_eq!(receiver.sink.units.len(), 0);
        receiver.expire(now + REASSEMBLY_DEADLINE);
        assert_eq!(receiver.stats.dropped(Class::Video), 1);
        assert_eq!(receiver.sink.units.len(), 0, "no partial is delivered");
    }
}
