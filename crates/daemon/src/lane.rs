//! The lane control plane: one playhead estimate per watched stream, per
//! connection.
//!
//! A lane is a reader position and a rate, not a queue (`notes/riffle.md`,
//! "Push, not pull"). The client reports where it is and how fast it is going;
//! the daemon keeps the estimate and will push while
//! `sent_position < predicted_playhead + buffer_ms`. Nothing here pushes: this
//! module is the bookkeeping the media lane will read, and the value the UI
//! draws a row from.
//!
//! Lanes live on the connection that opened them, in [`riffle::Conn`], so one
//! UI cannot see or close another's. The claim a lane holds on a pipeline is
//! the registry's (`Registry::attach`/`detach`), because that outlives any one
//! connection.
//!
//! [`riffle::Conn`]: crate::riffle::Conn

use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;

use siphon_shared::riffle::LaneState;
use siphon_shared::riffle::LaneView;
use siphon_shared::riffle::StreamKey;
use siphon_shared::riffle::StreamPlayback;

/// The client's buffer is clamped to these, in milliseconds
/// (`notes/riffle.md`, "Push, not pull").
pub const MIN_BUFFER_MS: u32 = 1_000;
pub const MAX_BUFFER_MS: u32 = 120_000;

/// The rate a lane may report. A modulator nudges 0.98–1.02; anything further
/// is catch-up or a stall, and a zero or negative rate would freeze the
/// estimate instead of advancing it.
const MIN_RATE: f32 = 0.25;
const MAX_RATE: f32 = 4.0;

/// One attached lane's state.
pub struct Lane {
    /// The pipeline this lane watches.
    key: StreamKey,
    /// The position last reported, with `PlaybackPosition::Live` already
    /// resolved to the pipeline's live edge at that moment.
    reported_ms: u64,
    rate: f32,
    /// The client's RAM, clamped by [`MIN_BUFFER_MS`]/[`MAX_BUFFER_MS`].
    buffer_ms: u32,
    /// When `reported_ms` was reported. The estimate is `reported_ms` advanced
    /// by `rate` over the time since.
    at: Instant,
    /// Unix milliseconds when the lane was opened.
    started_ms: u64,
}

impl Lane {
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

    /// The client's position as it last reported it.
    #[must_use]
    pub const fn position_ms(&self) -> u64 {
        self.reported_ms
    }

    /// Whether the retained window still holds the reported position.
    ///
    /// `window_start_ms` is the oldest position the pipeline can still serve;
    /// `0` means it is not known yet (no segment has been published, or the
    /// runtime is gone), which never starves a lane. A position the window has
    /// moved past is [`LaneState::Starved`], and `notes/riffle.md` says the
    /// media lane clamps it forward and reports that through `StreamStarted`.
    const fn state(&self, window_start_ms: u64) -> LaneState {
        // `Paused` is not produced: it means "feeding, but the client's buffer
        // is full", and nothing is filling a buffer yet. The media lane will
        // report it once `buffered_ahead_ms` is real.
        if window_start_ms > 0 && self.reported_ms < window_start_ms {
            LaneState::Starved
        } else {
            LaneState::Attached
        }
    }

    #[must_use]
    pub fn view(&self, now: Instant, window_start_ms: u64) -> LaneView {
        LaneView {
            key: self.key.clone(),
            state: self.state(window_start_ms),
            position_ms: self.reported_ms,
            playhead_ms: self.playhead_ms(now),
            // Nothing is being pushed yet, so nothing is ahead of the playhead.
            // The media lane fills this from what it has sent per lane.
            buffered_ahead_ms: 0,
            rate: self.rate,
            // Whole-AU drops are a property of the carrier, and there is no
            // carrier yet.
            dropped_video: 0,
            dropped_audio: 0,
            recovering: false,
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
    /// mapped from the reported `PlaybackPosition`.
    ///
    /// `true` means the key was new, so the caller has to attach the pipeline.
    pub fn open(
        &mut self,
        key: StreamKey,
        playback: StreamPlayback,
        position_ms: u64,
        now: Instant,
        wall_ms: u64,
    ) -> bool {
        let rate = sanitise_rate(playback.rate);
        let buffer_ms = playback.buffer_ms.clamp(MIN_BUFFER_MS, MAX_BUFFER_MS);
        if let Some(lane) = self.lanes.get_mut(&key) {
            lane.reported_ms = position_ms;
            lane.rate = rate;
            lane.buffer_ms = buffer_ms;
            lane.at = now;
            return false;
        }
        self.lanes.insert(
            key.clone(),
            Lane {
                key,
                reported_ms: position_ms,
                rate,
                buffer_ms,
                at: now,
                started_ms: wall_ms,
            },
        );
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

    /// The reported position of one lane, for a seek that cannot place the
    /// pipeline.
    #[must_use]
    pub fn position_ms(&self, key: &StreamKey) -> Option<u64> {
        self.lanes.get(key).map(Lane::position_ms)
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
}

/// A rate from the wire, clamped to what the estimate can use.
const fn sanitise_rate(rate: f32) -> f32 {
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
            start,
            1_000,
        ));
        assert!(!lanes.open(
            key(7),
            playback(PlaybackPosition::Absolute { pts_ms: 1_000 }, 2.0, 60_000),
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
            start,
            1_000,
        );
        let view = lanes.view(&key(7), start, 0).unwrap();
        assert_eq!(view.rate, 1.0);
        assert_eq!(lanes.lanes[&key(7)].buffer_ms, MIN_BUFFER_MS);

        lanes.open(
            key(7),
            playback(PlaybackPosition::Live, 1.0, u32::MAX),
            0,
            start,
            1_000,
        );
        assert_eq!(lanes.lanes[&key(7)].buffer_ms, MAX_BUFFER_MS);
    }
}
