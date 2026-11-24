//! The lane control plane: one playhead estimate per watched stream, per
//! connection.
//!
//! A lane is a reader position and a rate, not a queue (`notes/riffle.md`,
//! "Push, not pull"). The client reports where it is and how fast it is going;
//! the daemon keeps the estimate and pushes while
//! `sent_position < predicted_playhead + buffer_ms`.
//!
//! The estimate lives here and the pushing happens in
//! [`crate::media::lane`]: a lane's playback numbers are copied *in* to a
//! [`LaneHandle`] when the client reports, and the counters the producer task
//! writes are copied *out* when a row is built. That is why nothing borrows a
//! `RefCell` across an `.await` — the handle is a set of `Cell`s, so a read is
//! a copy and a write is a store, from either task.
//!
//! Lanes live on the connection that opened them, in [`riffle::Conn`], so one
//! UI cannot see or close another's. The claim a lane holds on a pipeline is
//! the registry's (`Registry::attach`/`detach`), because that outlives any one
//! connection.
//!
//! [`riffle::Conn`]: crate::riffle::Conn

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;
use std::time::Instant;

use siphon_shared::riffle::ConnStats;
use siphon_shared::riffle::LaneState;
use siphon_shared::riffle::LaneView;
use siphon_shared::riffle::StreamKey;
use siphon_shared::riffle::StreamPlayback;

use crate::media::wire::Track;

/// The client's buffer is clamped to these, in milliseconds
/// (`notes/riffle.md`, "Push, not pull").
pub const MIN_BUFFER_MS: u32 = 1_000;
pub const MAX_BUFFER_MS: u32 = 120_000;

/// A lane this close to the live edge is an edge lane and gets its datagrams
/// queued ahead of a catch-up lane's (`notes/riffle.md`, "Multiple lanes").
pub const EDGE_MARGIN_MS: u64 = 2_000;

/// A dropped video object keeps the picture smeared for about a GOP, so a lane
/// with a video drop inside this window is "recovering".
pub const RECOVERY_MS: u64 = 4_000;

/// The rate a lane may report. A modulator nudges 0.98–1.02; anything further
/// is catch-up or a stall, and a zero or negative rate would freeze the
/// estimate instead of advancing it.
pub const MIN_RATE: f32 = 0.25;
const MAX_RATE: f32 = 4.0;

/// What the client last reported, plus when. One `Copy` value, so it moves
/// between the app and the producer task by value and never behind a borrow.
#[derive(Clone, Copy, Debug)]
pub struct Playback {
    /// The position last reported, with `PlaybackPosition::Live` already
    /// resolved to the pipeline's live edge at that moment.
    reported_ms: u64,
    rate: f32,
    /// The client's RAM, clamped by [`MIN_BUFFER_MS`]/[`MAX_BUFFER_MS`].
    buffer_ms: u32,
    /// When `reported_ms` was reported. The estimate is `reported_ms` advanced
    /// by `rate` over the time since.
    at: Instant,
}

impl Playback {
    /// Where the daemon thinks the client is: the reported position, advanced by
    /// the reported rate over the elapsed time.
    ///
    /// This is the daemon's whole flow control (`notes/riffle.md`, "Push, not
    /// pull"), so it is exact integer arithmetic at the point that matters: the
    /// elapsed seconds are fed to `Duration`, never truncated by a float to
    /// integer cast.
    #[must_use]
    pub fn playhead_ms(&self, now: Instant) -> u64 {
        let elapsed_secs = now.saturating_duration_since(self.at).as_secs_f32();
        let advance = Duration::from_secs_f32(elapsed_secs * self.rate).as_millis();
        self.reported_ms
            .saturating_add(u64::try_from(advance).unwrap_or(u64::MAX))
    }

    #[must_use]
    pub const fn position_ms(&self) -> u64 {
        self.reported_ms
    }

    #[must_use]
    pub const fn buffer_ms(&self) -> u32 {
        self.buffer_ms
    }

    #[must_use]
    pub const fn rate(&self) -> f32 {
        self.rate
    }
}

/// Where a `(re)started` producer must begin: the segment that holds the
/// keyframe a seek resolved to, and that keyframe's presentation time on the
/// broadcast timeline.
///
/// A seek is answered before the producer that serves it exists, so the answer
/// travels through the lane handle: the app writes it, and the producer the
/// connection starts takes it once when it begins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Start {
    /// Playlist sequence of the segment to begin at. Whole segments before it
    /// are skipped, because the replay window reaches back further.
    pub sequence: u64,
    /// The keyframe's presentation time, in broadcast milliseconds. Access
    /// units before it belong to the part of the segment the client would
    /// discard.
    pub pts_ms: u64,
}

/// One lane's live state: the app writes the playback numbers, the lane's
/// producer task writes the counters, and both read the other side as plain
/// values.
///
/// Every field is a `Cell`, so a handle can be cloned into the producer and
/// still be read by the app's loop and the snapshot without a borrow existing
/// across an await. Nothing here is large enough for a lock to be worth it, and
/// there is one reactor thread anyway.
#[derive(Debug, Default)]
struct Shared {
    playback: Cell<Option<Playback>>,
    /// Where a producer that has not started yet must begin, written by a seek
    /// or an absolute open and taken once by the producer.
    start: Cell<Option<Start>>,
    /// End of the newest live segment, in broadcast milliseconds; `0` when the
    /// pipeline has published nothing yet.
    live_edge_ms: Cell<u64>,
    /// The media timestamp of the last access unit handed to the sender, in
    /// broadcast milliseconds, per track — indexed by [`Track`]'s slot. Per
    /// track because video and audio are timed independently and an access unit
    /// at the same millisecond on the other track is not the same unit.
    sent_ms: [Cell<u64>; 2],
    sent_any: [Cell<bool>; 2],
    dropped_video: Cell<u32>,
    dropped_audio: Cell<u32>,
    /// When the last object was dropped, for `recovering` and
    /// [`ConnStats::last_drop_age_ms`].
    last_drop: Cell<Option<Instant>>,
}

/// The index [`Shared::sent_ms`] keeps one track's position under.
const fn slot(track: Track) -> usize {
    match track {
        Track::Video => 0,
        Track::Audio => 1,
    }
}

/// A cloneable view of one lane's state, for the task that feeds it.
///
/// The methods that return `_ms` are the push rule's inputs and outputs; the
/// ones that start with `note_` are the producer's writes.
#[derive(Clone, Debug, Default)]
pub struct LaneHandle(Rc<Shared>);

impl LaneHandle {
    /// The current estimate, or `None` before the app has written one.
    #[must_use]
    pub fn playback(&self) -> Option<Playback> {
        self.0.playback.get()
    }

    #[must_use]
    pub fn playhead_ms(&self, now: Instant) -> u64 {
        self.0
            .playback
            .get()
            .map_or(0, |playback| playback.playhead_ms(now))
    }

    #[must_use]
    pub fn buffer_ms(&self) -> u32 {
        self.0
            .playback
            .get()
            .map_or(MIN_BUFFER_MS, |playback| playback.buffer_ms())
    }

    /// Where the pipeline is, in broadcast milliseconds; `0` when unknown.
    #[must_use]
    pub fn live_edge_ms(&self) -> u64 {
        self.0.live_edge_ms.get()
    }

    /// Whether this lane is at the live edge, and so is queued ahead of a lane
    /// that is catching up.
    ///
    /// A pipeline with no published segment yet has no edge to be behind, so a
    /// lane on it counts as an edge lane: there is nothing to demote it below.
    #[must_use]
    pub fn at_edge(&self, now: Instant) -> bool {
        let edge = self.0.live_edge_ms.get();
        edge == 0 || edge.saturating_sub(self.playhead_ms(now)) <= EDGE_MARGIN_MS
    }

    /// The push rule: send while the last thing sent is still inside the
    /// client's buffer.
    ///
    /// `true` before anything has been sent, because an empty buffer is never
    /// full. At the live edge this reduces to "forward on arrival": the rule
    /// compares against a playhead moving at real time and a send position that
    /// cannot pass the edge, because the data does not exist yet.
    #[must_use]
    pub fn may_send(&self, now: Instant) -> bool {
        if !self.has_sent() {
            return true;
        }
        self.sent_ms()
            < self
                .playhead_ms(now)
                .saturating_add(u64::from(self.buffer_ms()))
    }

    /// Whether any access unit has been handed to the sender yet.
    #[must_use]
    pub fn has_sent(&self) -> bool {
        self.0.sent_any[0].get() || self.0.sent_any[1].get()
    }

    /// Whether this track has already received an access unit at or after
    /// `media_ms` — i.e. whether this one is part of a window being replayed.
    ///
    /// A paused lane re-registers against the retained window so it can pick up
    /// where it stopped, and that window starts before what it has already
    /// sent. Those units are neither dropped nor sent again.
    #[must_use]
    pub fn already_sent(&self, track: Track, media_ms: u64) -> bool {
        let index = slot(track);
        self.0.sent_any[index].get() && media_ms <= self.0.sent_ms[index].get()
    }

    /// An access unit this far behind the playhead is superseded and skipped
    /// whole — the datamosh drop (`notes/riffle.md`, "Video: unreliable ordered").
    ///
    /// The margin is a moment, not a GOP: an AU that just went by is the one the
    /// client is about to present, and dropping it would stutter far more often
    /// than it saves a byte. One second of slack covers a report interval and a
    /// segment's jitter without ever shipping a whole segment of the past.
    #[must_use]
    pub fn superseded(&self, media_ms: u64, now: Instant) -> bool {
        media_ms.saturating_add(SUPERSEDE_MARGIN_MS) < self.playhead_ms(now)
    }

    /// The newest media timestamp handed to the sender, across both tracks.
    #[must_use]
    pub fn sent_ms(&self) -> u64 {
        self.0.sent_ms[0].get().max(self.0.sent_ms[1].get())
    }

    /// What was pushed past the playhead, for `LaneView::buffered_ahead_ms`.
    #[must_use]
    pub fn buffered_ahead_ms(&self, now: Instant) -> u64 {
        self.sent_ms().saturating_sub(self.playhead_ms(now))
    }

    /// Counts one access unit handed to the carrier.
    pub fn note_sent(&self, track: Track, media_ms: u64) {
        let index = slot(track);
        self.0.sent_ms[index].set(media_ms);
        self.0.sent_any[index].set(true);
    }

    /// Counts one access unit skipped because the playhead had already passed
    /// it.
    pub fn note_drop(&self, track: Track, now: Instant) {
        match track {
            Track::Video => self
                .0
                .dropped_video
                .set(self.0.dropped_video.get().saturating_add(1)),
            Track::Audio => self
                .0
                .dropped_audio
                .set(self.0.dropped_audio.get().saturating_add(1)),
        }
        self.0.last_drop.set(Some(now));
    }

    #[must_use]
    pub fn dropped_video(&self) -> u32 {
        self.0.dropped_video.get()
    }

    #[must_use]
    pub fn dropped_audio(&self) -> u32 {
        self.0.dropped_audio.get()
    }

    /// Milliseconds since the last object of any class was dropped, or
    /// [`ConnStats::last_drop_age_ms`]'s sentinel.
    #[must_use]
    pub fn last_drop_age_ms(&self, now: Instant) -> u32 {
        self.0.last_drop.get().map_or(u32::MAX, |at| {
            let elapsed = now.saturating_duration_since(at).as_millis();
            u32::try_from(elapsed).unwrap_or(u32::MAX)
        })
    }

    fn write(&self, playback: Playback, live_edge_ms: u64) {
        self.0.playback.set(Some(playback));
        self.0.live_edge_ms.set(live_edge_ms);
    }

    /// Where a producer that has not started yet must begin, taken once.
    ///
    /// A seek that resumes forward or backward moves the start; only the
    /// producer that is about to read the window cares, and it takes it when it
    /// registers.
    #[must_use]
    pub fn take_start(&self) -> Option<Start> {
        self.0.start.take()
    }

    /// Places the lane at a GOP-aligned start.
    ///
    /// The estimate restarts at `start.pts_ms` — the keyframe the daemon will
    /// actually serve, not the requested target — and nothing counts as already
    /// sent, because a backward seek lands before what this lane has already
    /// delivered. The producer the caller starts next takes the start from
    /// [`Self::take_start`].
    pub fn begin_at(&self, start: Start, live_edge_ms: u64, now: Instant) {
        let (rate, buffer_ms) = self
            .0
            .playback
            .get()
            .map_or((1.0, MIN_BUFFER_MS), |playback| {
                (playback.rate(), playback.buffer_ms())
            });
        self.0.start.set(Some(start));
        self.write(
            Playback {
                reported_ms: start.pts_ms,
                rate,
                buffer_ms,
                at: now,
            },
            live_edge_ms,
        );
        for sent in &self.0.sent_ms {
            sent.set(0);
        }
        for any in &self.0.sent_any {
            any.set(false);
        }
    }
}

/// How far behind the playhead an access unit is still worth sending.
const SUPERSEDE_MARGIN_MS: u64 = 1_000;

/// One attached lane's state.
pub struct Lane {
    /// The pipeline this lane watches.
    key: StreamKey,
    /// The playback estimate and the counters, shared with the producer task.
    handle: LaneHandle,
    /// Unix milliseconds when the lane was opened.
    started_ms: u64,
}

impl Lane {
    /// Where the daemon thinks the client is: the reported position, advanced by
    /// the reported rate over the elapsed time.
    #[must_use]
    pub fn playhead_ms(&self, now: Instant) -> u64 {
        self.handle.playhead_ms(now)
    }

    /// The client's position as it last reported it.
    #[must_use]
    pub fn position_ms(&self) -> u64 {
        self.handle
            .playback()
            .map_or(0, |playback| playback.position_ms())
    }

    /// The state the row shows.
    ///
    /// `Paused` is real now: it means the producer has pushed the whole of the
    /// client's buffer and is waiting for the playhead to advance.
    #[must_use]
    pub fn state(&self, now: Instant, window_start_ms: u64) -> LaneState {
        if window_start_ms > 0 && self.position_ms() < window_start_ms {
            return LaneState::Starved;
        }
        let buffer = u64::from(self.handle.buffer_ms());
        if buffer > 0 && self.handle.buffered_ahead_ms(now) >= buffer {
            return LaneState::Paused;
        }
        LaneState::Attached
    }

    #[must_use]
    pub fn view(&self, now: Instant, window_start_ms: u64) -> LaneView {
        LaneView {
            key: self.key.clone(),
            state: self.state(now, window_start_ms),
            position_ms: self.position_ms(),
            playhead_ms: self.playhead_ms(now),
            buffered_ahead_ms: self.handle.buffered_ahead_ms(now),
            rate: self
                .handle
                .playback()
                .map_or(1.0, |playback| playback.rate()),
            dropped_video: self.handle.dropped_video(),
            dropped_audio: self.handle.dropped_audio(),
            // A video drop inside one GOP's worth of time is what "recovering"
            // means: the picture smears for about that long and then heals at an
            // IDR (`notes/riffle.md`, "Video: unreliable ordered").
            recovering: self.handle.dropped_video() > 0
                && self.handle.last_drop_age_ms(now) < u32::try_from(RECOVERY_MS).unwrap_or(0),
            started_ms: self.started_ms,
        }
    }
}

/// The lanes of one connection.
#[derive(Default)]
pub struct Lanes {
    /// At most a handful, and keyed by the wire key so a re-open finds its own
    /// lane. Insertion order is not meaningful: the UI keys its rows.
    lanes: HashMap<StreamKey, Lane>,
}

impl Lanes {
    /// Attaches a lane, or updates the one already attached: re-opening a key
    /// is idempotent. `position_ms` is the caller's resolved position, already
    /// mapped from the reported `PlaybackPosition`, and `live_edge_ms` is where
    /// the pipeline is right now (`0` when it has published nothing).
    ///
    /// `true` means the key was new, so the caller has to attach the pipeline
    /// and spawn the lane's producer.
    pub fn open(
        &mut self,
        key: StreamKey,
        playback: StreamPlayback,
        position_ms: u64,
        live_edge_ms: u64,
        now: Instant,
        wall_ms: u64,
    ) -> bool {
        let state = Playback {
            reported_ms: position_ms,
            rate: sanitize_rate(playback.rate),
            buffer_ms: playback.buffer_ms.clamp(MIN_BUFFER_MS, MAX_BUFFER_MS),
            at: now,
        };
        if let Some(lane) = self.lanes.get_mut(&key) {
            lane.handle.write(state, live_edge_ms);
            return false;
        }
        let handle = LaneHandle::default();
        handle.write(state, live_edge_ms);
        self.lanes.insert(
            key.clone(),
            Lane {
                key,
                handle,
                started_ms: wall_ms,
            },
        );
        true
    }

    /// One lane's shared state, for the task that feeds it.
    #[must_use]
    pub fn handle(&self, key: &StreamKey) -> Option<LaneHandle> {
        self.lanes.get(key).map(|lane| lane.handle.clone())
    }

    /// Moves an attached lane to a GOP-aligned start: the estimate and the
    /// producer that the caller starts next both begin at `start`.
    ///
    /// `false` means the lane is not attached, so there is nothing to place.
    pub fn begin_at(&self, key: &StreamKey, start: Start, live_edge_ms: u64, now: Instant) -> bool {
        let Some(lane) = self.lanes.get(key) else {
            return false;
        };
        lane.handle.begin_at(start, live_edge_ms, now);
        true
    }

    #[must_use]
    pub fn contains(&self, key: &StreamKey) -> bool {
        self.lanes.contains_key(key)
    }

    /// Closes one lane. `true` means it was attached, so the pipeline claim has
    /// to be released.
    pub fn remove(&mut self, key: &StreamKey) -> bool {
        self.lanes.remove(key).is_some()
    }

    /// One lane's row.
    #[must_use]
    pub fn view(&self, key: &StreamKey, now: Instant, window_start_ms: u64) -> Option<LaneView> {
        self.lanes
            .get(key)
            .map(|lane| lane.view(now, window_start_ms))
    }

    /// Every lane's row, in no particular order. `window_start_ms` resolves the
    /// retained window per lane, because it moves while the connection lives.
    #[must_use]
    pub fn views(
        &self,
        now: Instant,
        window_start_ms: impl Fn(&StreamKey) -> u64,
    ) -> Vec<LaneView> {
        self.lanes
            .values()
            .map(|lane| lane.view(now, window_start_ms(&lane.key)))
            .collect()
    }

    /// The keys, for releasing every claim when a connection goes away.
    pub fn iter(&self) -> impl Iterator<Item = &StreamKey> {
        self.lanes.keys()
    }

    /// The connection's drop counters, summed over its lanes.
    ///
    /// Datamosh makes drops normal, so they are reported: without them "smearing
    /// because the link is lossy" is indistinguishable from "smearing because
    /// something is broken" (`notes/riffle.md`, "Telemetry"). Datagram send
    /// failures are the carrier's, not a lane's, and the caller adds them.
    #[must_use]
    pub fn stats(&self, now: Instant) -> ConnStats {
        let mut stats = ConnStats {
            // Nothing has been dropped yet, which is what the sentinel means.
            last_drop_age_ms: u32::MAX,
            ..ConnStats::default()
        };
        for lane in self.lanes.values() {
            stats.dropped_video = stats
                .dropped_video
                .saturating_add(lane.handle.dropped_video());
            stats.dropped_audio = stats
                .dropped_audio
                .saturating_add(lane.handle.dropped_audio());
            stats.last_drop_age_ms = stats
                .last_drop_age_ms
                .min(lane.handle.last_drop_age_ms(now));
        }
        stats
    }
}

/// A rate from the wire, clamped to what the estimate can use.
const fn sanitize_rate(rate: f32) -> f32 {
    if rate.is_finite() {
        rate.clamp(MIN_RATE, MAX_RATE)
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use siphon_shared::riffle::PlaybackPosition;

    use super::*;

    fn key(channel: u64) -> StreamKey {
        StreamKey::new(channel, None)
    }

    fn playback(position: PlaybackPosition, rate: f32, buffer_ms: u32) -> StreamPlayback {
        StreamPlayback {
            position,
            rate,
            buffer_ms,
        }
    }

    /// The estimate is the reported position advanced by rate × elapsed.
    #[test]
    fn the_playhead_advances_with_rate_and_elapsed_time() {
        let start = Instant::now();
        let mut lanes = Lanes::default();
        lanes.open(
            key(7),
            playback(PlaybackPosition::Absolute { pts_ms: 10_000 }, 1.0, 30_000),
            10_000,
            0,
            start,
            1_000,
        );

        let rate_two = start + Duration::from_secs(2);
        assert_eq!(
            lanes.view(&key(7), rate_two, 0).unwrap().playhead_ms,
            12_000
        );

        // A slower lane advances less over the same time.
        lanes.open(
            key(7),
            playback(PlaybackPosition::Absolute { pts_ms: 10_000 }, 0.5, 30_000),
            10_000,
            0,
            start,
            1_000,
        );
        assert_eq!(
            lanes.view(&key(7), rate_two, 0).unwrap().playhead_ms,
            11_000
        );

        // Re-reporting resets the clock, so the estimate never double-counts.
        lanes.open(
            key(7),
            playback(PlaybackPosition::Absolute { pts_ms: 20_000 }, 1.0, 30_000),
            20_000,
            0,
            rate_two,
            1_000,
        );
        assert_eq!(
            lanes.view(&key(7), rate_two, 0).unwrap().playhead_ms,
            20_000
        );
    }

    /// Re-opening an attached key updates it in place: one lane, one claim, and
    /// the latest position.
    #[test]
    fn re_opening_a_lane_is_idempotent() {
        let start = Instant::now();
        let mut lanes = Lanes::default();
        assert!(lanes.open(
            key(7),
            playback(PlaybackPosition::Live, 1.0, 30_000),
            5_000,
            5_000,
            start,
            1_000,
        ));
        assert!(!lanes.open(
            key(7),
            playback(PlaybackPosition::Absolute { pts_ms: 1_000 }, 2.0, 60_000),
            1_000,
            1_000,
            start,
            2_000,
        ));

        let view = lanes.view(&key(7), start, 0).unwrap();
        assert_eq!(view.position_ms, 1_000);
        assert_eq!(view.rate, 2.0);
        assert_eq!(view.started_ms, 1_000, "the original attach time stays");
        assert_eq!(lanes.iter().count(), 1);
    }

    #[test]
    fn closing_a_lane_removes_it() {
        let mut lanes = Lanes::default();
        lanes.open(
            key(7),
            playback(PlaybackPosition::Live, 1.0, 30_000),
            0,
            0,
            Instant::now(),
            1_000,
        );
        assert!(lanes.remove(&key(7)));
        assert!(!lanes.contains(&key(7)));
        assert!(lanes.view(&key(7), Instant::now(), 0).is_none());
        assert_eq!(lanes.views(Instant::now(), |_| 0).len(), 0);
        // Closing twice is not an error, it is just nothing left to release.
        assert!(!lanes.remove(&key(7)));
    }

    /// A position the retained window has moved past is starved; one inside it,
    /// or a window that is not known yet, is attached.
    #[test]
    fn the_retained_window_decides_whether_a_lane_is_starved() {
        let start = Instant::now();
        let mut lanes = Lanes::default();
        lanes.open(
            key(7),
            playback(PlaybackPosition::Absolute { pts_ms: 10_000 }, 1.0, 30_000),
            10_000,
            0,
            start,
            1_000,
        );

        assert_eq!(
            lanes.view(&key(7), start, 0).unwrap().state,
            LaneState::Attached
        );
        assert_eq!(
            lanes.view(&key(7), start, 10_000).unwrap().state,
            LaneState::Attached
        );
        assert_eq!(
            lanes.view(&key(7), start, 10_001).unwrap().state,
            LaneState::Starved
        );
    }

    #[test]
    fn a_buffer_is_clamped_and_a_bad_rate_falls_back_to_one() {
        let start = Instant::now();
        let mut lanes = Lanes::default();
        lanes.open(
            key(7),
            playback(PlaybackPosition::Live, f32::NAN, 0),
            0,
            0,
            start,
            1_000,
        );
        let view = lanes.view(&key(7), start, 0).unwrap();
        assert_eq!(view.rate, 1.0);
        assert_eq!(lanes.handle(&key(7)).unwrap().buffer_ms(), MIN_BUFFER_MS);

        lanes.open(
            key(7),
            playback(PlaybackPosition::Live, 1.0, u32::MAX),
            0,
            0,
            start,
            1_000,
        );
        assert_eq!(lanes.handle(&key(7)).unwrap().buffer_ms(), MAX_BUFFER_MS);
    }

    /// The push rule, read off the handle exactly as the producer reads it.
    #[test]
    fn the_push_rule_stops_at_the_buffer_and_resumes_when_the_playhead_advances() {
        let start = Instant::now();
        let mut lanes = Lanes::default();
        lanes.open(
            key(7),
            playback(PlaybackPosition::Absolute { pts_ms: 0 }, 1.0, 1_000),
            0,
            0,
            start,
            1_000,
        );
        let handle = lanes.handle(&key(7)).unwrap();

        assert!(handle.may_send(start), "an empty buffer is never full");
        handle.note_sent(Track::Video, 999);
        assert!(handle.may_send(start), "inside the buffer");
        handle.note_sent(Track::Video, 1_000);
        assert!(!handle.may_send(start), "the buffer is full");

        // A second of playback opens a second of buffer.
        let later = start + Duration::from_secs(1);
        assert!(handle.may_send(later));
        assert_eq!(handle.buffered_ahead_ms(later), 0);
        handle.note_sent(Track::Video, 1_500);
        assert_eq!(handle.buffered_ahead_ms(later), 500);
        // An access unit the replay window hands back is not sent twice.
        assert!(handle.already_sent(Track::Video, 900));
        assert!(!handle.already_sent(Track::Video, 1_501));
        assert!(!handle.already_sent(Track::Audio, 0));
    }

    /// An access unit the playhead has already passed is superseded, and it is
    /// counted on the track it was dropped from.
    #[test]
    fn an_access_unit_behind_the_playhead_is_superseded() {
        let start = Instant::now();
        let mut lanes = Lanes::default();
        lanes.open(
            key(7),
            playback(PlaybackPosition::Absolute { pts_ms: 10_000 }, 1.0, 30_000),
            10_000,
            0,
            start,
            1_000,
        );
        let handle = lanes.handle(&key(7)).unwrap();

        assert!(!handle.superseded(10_000, start));
        assert!(
            !handle.superseded(9_500, start),
            "a moment behind is on time"
        );
        assert!(handle.superseded(8_000, start));

        handle.note_drop(Track::Video, start);
        handle.note_drop(Track::Video, start);
        handle.note_drop(Track::Audio, start);
        assert_eq!(handle.dropped_video(), 2);
        assert_eq!(handle.dropped_audio(), 1);

        let view = lanes.view(&key(7), start, 0).unwrap();
        assert_eq!(view.dropped_video, 2);
        assert_eq!(view.dropped_audio, 1);
        assert!(view.recovering, "a video drop now is a smear now");
        let after = start + Duration::from_secs(RECOVERY_MS / 1_000 + 1);
        assert!(!lanes.view(&key(7), after, 0).unwrap().recovering);
    }

    /// A GOP-aligned start moves the estimate back and forgets what was sent:
    /// a backward seek lands before what the lane has already delivered, so
    /// nothing may be suppressed as a replay.
    #[test]
    fn a_seek_places_the_lane_at_the_keyframe_and_forgets_what_was_sent() {
        let start = Instant::now();
        let mut lanes = Lanes::default();
        lanes.open(
            key(7),
            playback(PlaybackPosition::Absolute { pts_ms: 60_000 }, 1.0, 30_000),
            60_000,
            60_000,
            start,
            1_000,
        );
        let handle = lanes.handle(&key(7)).unwrap();
        handle.note_sent(Track::Video, 59_000);
        handle.note_sent(Track::Audio, 59_020);
        assert!(handle.already_sent(Track::Video, 58_000));

        let seek = Start {
            sequence: 42,
            pts_ms: 12_000,
        };
        assert!(lanes.begin_at(&key(7), seek, 60_000, start));
        assert_eq!(handle.take_start(), Some(seek));
        assert_eq!(handle.take_start(), None, "the producer takes it once");
        assert_eq!(handle.playback().unwrap().position_ms(), 12_000);
        assert_eq!(handle.playhead_ms(start), 12_000);
        assert!(!handle.has_sent());
        assert!(!handle.already_sent(Track::Video, 11_999));
        // The rate and the buffer belong to the client and survive the seek.
        assert_eq!(handle.buffer_ms(), 30_000);

        assert!(!lanes.begin_at(&key(8), seek, 0, start), "not attached");
    }

    /// The connection's stats are the lanes' counters summed, and a lane that
    /// has not dropped anything does not make the connection look recovering.
    #[test]
    fn the_connection_stats_sum_the_lanes() {
        let start = Instant::now();
        let mut lanes = Lanes::default();
        assert_eq!(lanes.stats(start).last_drop_age_ms, u32::MAX);
        assert!(!lanes.stats(start).recovering());

        lanes.open(
            key(7),
            playback(PlaybackPosition::Live, 1.0, 30_000),
            0,
            0,
            start,
            1_000,
        );
        lanes.open(
            key(8),
            playback(PlaybackPosition::Live, 1.0, 30_000),
            0,
            0,
            start,
            1_000,
        );
        lanes
            .handle(&key(7))
            .unwrap()
            .note_drop(Track::Video, start);
        lanes
            .handle(&key(8))
            .unwrap()
            .note_drop(Track::Audio, start);

        let stats = lanes.stats(start);
        assert_eq!(stats.dropped_video, 1);
        assert_eq!(stats.dropped_audio, 1);
        assert_eq!(stats.last_drop_age_ms, 0);
        assert!(stats.recovering(), "a video drop a moment ago");
    }

    /// A lane with everything it was sent is paused, not attached: that is what
    /// the producer's stop looks like from the UI.
    #[test]
    fn a_full_buffer_reads_as_paused() {
        let start = Instant::now();
        let mut lanes = Lanes::default();
        lanes.open(
            key(7),
            playback(PlaybackPosition::Absolute { pts_ms: 0 }, 1.0, 1_000),
            0,
            0,
            start,
            1_000,
        );
        lanes
            .handle(&key(7))
            .unwrap()
            .note_sent(Track::Video, 1_000);
        assert_eq!(
            lanes.view(&key(7), start, 0).unwrap().state,
            LaneState::Paused
        );
        assert_eq!(
            lanes.view(&key(7), start, 0).unwrap().buffered_ahead_ms,
            1_000
        );
        // Starvation outranks a full buffer: nothing can be sent at all.
        assert_eq!(
            lanes.view(&key(7), start, 1).unwrap().state,
            LaneState::Starved
        );
    }
}
