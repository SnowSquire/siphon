use std::cell::RefCell;
use std::future::poll_fn;
use std::rc::Rc;
use std::task::Poll;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use eros::IntoUnion;
use eros::ReshapeUnion;
use futures_util::future::Either;
use futures_util::future::select;
use futures_util::pin_mut;
use kanal::AsyncSender;
use kanal::Receiver;
use kanal::Sender;
use thiserror::Error;
use tracing::Level;
use tracing::debug;
use tracing::debug_span;
use tracing::info;
use tracing::span;
use tracing::warn;

use crate::arena::ArenaSet;
use crate::constants::MAX_MANIFEST;
use crate::downloader::ToDownloader;
use crate::http::Client;
use crate::metrics::FetchStats;
use crate::metrics::StreamMetrics;
use crate::playlist::Event;
use crate::playlist::ManifestParseError;
use crate::playlist::Playlist;
use crate::spool::ArenaFull;
use crate::spool::ConsumerWake;
use crate::spool::Reader;
use crate::spool::Spool;
use crate::spool::WakeSet;
use crate::utils::RetainUnordered;

struct PrefetchSegment {
    expected_id: u64,
    url: Rc<str>,
    reader: Reader,
}

#[derive(Debug)]
pub struct ConsumerRegistration {
    pub tx: Sender<SegmentMessage>,
    /// Replay the current window before live segments. Disk and the raw-TS
    /// output want this; a live RTSP session does not (it would start ~30 s
    /// behind and never catch up at the 1x pacer).
    pub backlog: bool,
    /// Drop individual messages when the bounded queue is full instead of
    /// disconnecting. RTSP keeps only the newest live segment during normal
    /// playback and replays the live window at a DVR handover, so losing a
    /// message during a download stall must not kill the output.
    pub lossy: bool,
}

#[derive(Debug)]
struct Consumer {
    tx: Sender<SegmentMessage>,
    lossy: bool,
}

impl Consumer {
    fn send(&self, message: SegmentMessage, reason: &'static str) -> bool {
        match self.tx.try_send(message) {
            Ok(true) => true,
            Ok(false) if self.lossy => {
                debug!(reason, "dropping message: consumer lagging");
                true
            }
            Ok(false) => {
                warn!(
                    reason,
                    queued = crate::constants::MAX_CONSUMER_LAG,
                    "dropping consumer: lagging behind"
                );
                let _ = self.tx.close();
                false
            }
            Err(error) => {
                debug!(%error, reason, "dropping consumer: channel closed");
                let _ = self.tx.close();
                false
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Segment {
    /// Presentation time since the start of the broadcast. The RTSP output maps
    /// this to NPT so clients can show the absolute position in the stream.
    pub timestamp_ms: u64,
    /// Absolute `EXT-X-PROGRAM-DATE-TIME` (Unix epoch ms), or `0` when the
    /// playlist publishes none. The disk output aligns live and DVR by this.
    pub pdt_ms: u64,
    /// Length of the segment, so a reader can compute `start + duration`.
    pub duration_ms: u64,
    pub sequence_num: u64,
    pub reader: Reader,
}

#[derive(Debug)]
#[allow(dead_code)] // consumed by the Riffle media lane
pub enum SegmentMessage {
    Segment(Segment),
    Prefetch {
        expected_sequence: u64,
        /// Playlist time of this segment, extrapolated from the last complete
        /// segment; prefetch tags do not carry an elapsed time.
        timestamp_ms: u64,
        reader: Reader,
    },
    Discontinuity(),
    /// The stream's `#EXT-X-MAP` segment, which every consumer must see before
    /// media. The disk output records it; the RTSP sink is TS-only and ignores
    /// it.
    Init {
        reader: Reader,
    },
    /// Seek request from the RTSP control plane (broadcast milliseconds). Only
    /// the RTSP consumer sends this; the scheduler never does.
    Seek(u64),
}

impl From<Segment> for SegmentMessage {
    fn from(value: Segment) -> Self {
        Self::Segment(value)
    }
}

#[derive(Error, Debug)]
#[error("manifest ended")]
struct Offline;

#[derive(Error, Debug)]
#[error("manifest is not valid utf-8")]
struct Utf8;

#[derive(Error, Debug)]
#[error("duplicate dispatch")]
struct Dispatch;

/// The daemon dropped the stream: the registration channel has no senders left,
/// so there can never be another consumer.
#[derive(Error, Debug)]
#[error("stream stopped")]
struct Stopped;

/// The open sum of everything the scheduler loop can fail with. Each function
/// below lists only the subset it can actually produce.
type SchedulerErrors = (
    crate::http::Error,
    ManifestParseError,
    ArenaFull,
    kanal::SendError,
    Utf8,
    Offline,
    Dispatch,
    Stopped,
);

/// Resolves when a consumer registers, re-checking the channel after parking so
/// a registration that races with the park is not missed.
///
/// `None` means the registration channel is closed and empty: the daemon dropped
/// the stream because its last viewer left. That has to be observable: a task
/// waiting for a consumer that can never arrive would hold its spools forever.
async fn next_reader(
    receive: Receiver<ConsumerRegistration>,
    wake: Rc<ConsumerWake>,
) -> Option<ConsumerRegistration> {
    poll_fn(move |context| {
        match receive.try_recv() {
            Ok(Some(reader)) => return Poll::Ready(Some(reader)),
            Ok(None) => {}
            Err(_) => return Poll::Ready(None),
        }
        wake.park(context);
        // The channel is the latch: a registration that landed between the
        // check above and the park is still in the queue, so checking it again
        // is what closes the race. The waker only saves the poll interval.
        match receive.try_recv() {
            Ok(Some(reader)) => Poll::Ready(Some(reader)),
            Ok(None) => Poll::Pending,
            Err(_) => Poll::Ready(None),
        }
    })
    .await
}

pub struct StreamScheduler {
    client: Rc<Client>,
    arena: Rc<ArenaSet>,
    wakes: Rc<WakeSet>,
    stream_url: Box<str>,
    downloaders: AsyncSender<ToDownloader>,
    receive: Receiver<ConsumerRegistration>,
    reader_wake: Rc<ConsumerWake>,

    init: Option<(String, Reader)>,

    segments: Vec<Segment>,
    prefetches: Vec<PrefetchSegment>,

    /// Snapshot of `segments` for the RTSP engine. When VOD playback reaches
    /// the end of the DVR playlist, the segments between there and the live
    /// edge are still in the live window; replaying them bridges the DVR lag
    /// instead of jumping to the newest live segment.
    live_window: Rc<RefCell<Vec<Segment>>>,

    readers: Vec<Consumer>,

    segment_tail: u64,
    prefetch_tail: u64,
    /// The stream was parked with nobody reading, so the next reload starts at
    /// the live edge instead of replaying the playlist's window.
    resume_at_edge: bool,
    last_segment_dur: Duration,
    metrics: Option<Rc<StreamMetrics>>,
}

impl StreamScheduler {
    pub fn new(
        client: Rc<Client>,
        arena: Rc<ArenaSet>,
        wakes: Rc<WakeSet>,
        stream_url: Box<str>,
        downloaders: AsyncSender<ToDownloader>,
        receive: Receiver<ConsumerRegistration>,
        reader_wake: Rc<ConsumerWake>,
    ) -> Self {
        Self {
            client,
            arena,
            wakes,
            stream_url,
            downloaders,
            receive,
            reader_wake,

            init: None,

            segments: Vec::new(),
            prefetches: Vec::new(),

            live_window: Rc::new(RefCell::new(Vec::new())),

            readers: Vec::new(),

            segment_tail: 0,
            prefetch_tail: 0,
            resume_at_edge: false,
            last_segment_dur: Duration::from_secs(5),
            metrics: None,
        }
    }

    /// Share the scheduler's live playlist window with another consumer (the
    /// RTSP engine, which bridges the DVR lag at a seek handover).
    #[must_use]
    pub fn with_live_window(mut self, live_window: Rc<RefCell<Vec<Segment>>>) -> Self {
        self.live_window = live_window;
        self
    }

    /// Attach the pipeline's counters and segment ring.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Rc<StreamMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    pub async fn main(mut self) {
        use crate::constants::SCHEDULER_BACKOFF_MAX as MAX_BACKOFF;
        use crate::constants::SCHEDULER_BACKOFF_MIN as MIN_BACKOFF;

        let span = span!(Level::INFO, "scheduler");
        let _ = &span;
        info!("{}", self.stream_url);

        let mut backoff = MIN_BACKOFF;
        loop {
            match self.mainloop().await {
                Ok(()) => backoff = MIN_BACKOFF,
                Err(error) => {
                    if let Some(metrics) = &self.metrics {
                        metrics
                            .poll_failures
                            .set(metrics.poll_failures.get().saturating_add(1));
                        metrics.set_error(Some(error.to_string()));
                    }
                    if error.is_inner::<Offline>() {
                        info!("stream ended");
                        return;
                    }
                    if error.is_inner::<Stopped>() {
                        info!("stream stopped: no consumers left");
                        return;
                    }

                    // `reload` applies events as it parses, so a failure after
                    // the first event leaves the window half-updated and it must
                    // be reset. Transport failures happen before any event is
                    // applied: the window is intact, and resetting would discard
                    // segments that are still live.
                    if error.is_inner::<crate::http::Error>() {
                        warn!(%error, ?backoff, "manifest fetch failed, retrying");
                    } else {
                        warn!(%error, ?backoff, "scheduler reset");
                        self.reset();
                    }

                    compio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }

    fn reset(&mut self) {
        // Sequences rewind, but consumers filter on a monotonically increasing
        // sequence, so they have to drop that filter first. Without this a
        // connected client silently drops every segment emitted after a reset.
        self.broadcast_discontinuity();
        self.segment_tail = 0;
        self.prefetch_tail = 0;
        self.last_segment_dur = Duration::from_secs(5);
        self.init = None;
        self.segments.clear();
        self.prefetches.clear();
        self.live_window.borrow_mut().clear();
    }

    /// Adopt every queued registration. `false` means the channel is closed and
    /// empty: the stream was dropped.
    fn handle_new_readers(&mut self) -> bool {
        loop {
            match self.receive.try_recv() {
                Ok(Some(registration)) => self.adopt(registration),
                Ok(None) => return true,
                Err(_) => return false,
            }
        }
    }

    /// Register one new consumer: hand it the init segment and (optionally) the
    /// tail of the window, or drop it when its bounded queue cannot take them.
    fn adopt(&mut self, registration: ConsumerRegistration) {
        const BACKLOG: usize = crate::constants::MAX_CONSUMER_LAG - 2;

        let ConsumerRegistration { tx, backlog, lossy } = registration;
        debug!(backlog, lossy, "new sender");
        let consumer = Consumer { tx, lossy };
        if let Some(init) = &self.init
            && !consumer.send(
                SegmentMessage::Init {
                    reader: init.1.clone(),
                },
                "new reader init",
            )
        {
            return;
        }

        if backlog {
            // A late joiner gets the newest part of the window, not all of it:
            // the whole backlog has to fit in the bounded queue alongside live
            // segments, or every new client would be dropped immediately.
            let skip = self.segments.len().saturating_sub(BACKLOG);
            for segment in self.segments.iter().skip(skip) {
                if !consumer.send(segment.clone().into(), "new reader backlog") {
                    return;
                }
            }
        }

        self.readers.push(consumer);
    }

    /// Polls nothing and fetches nothing while no consumer is registered.
    ///
    /// A parked scheduler also drops its window: those spools pin arena slots,
    /// and nobody is going to read them. The registration channel stays open, so
    /// the next viewer (or the disk re-attaching) resumes it in place.
    async fn park_if_idle(&mut self) -> bool {
        if !self.readers.is_empty() {
            return true;
        }
        debug!("no consumers, parking");
        self.segments.clear();
        self.prefetches.clear();
        self.init = None;
        self.live_window.borrow_mut().clear();
        self.resume_at_edge = true;
        loop {
            match next_reader(self.receive.clone(), Rc::clone(&self.reader_wake)).await {
                Some(registration) => {
                    self.adopt(registration);
                    if !self.readers.is_empty() {
                        return true;
                    }
                }
                None => return false,
            }
        }
    }

    async fn mainloop(&mut self) -> eros::Result<(), SchedulerErrors> {
        use crate::constants::EDGE_CATCHUP_POLL as CATCHUP_POLL;
        use crate::constants::EDGE_POLL_MAX as MAX_EDGE_POLL;
        use crate::constants::EDGE_POLL_MIN as MIN_EDGE_POLL;

        let start = Instant::now();
        let now: u64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let span = span!(Level::INFO, "mainloop", time = now);
        let _ = &span;
        if let Some(metrics) = &self.metrics {
            metrics.polls.set(metrics.polls.get().saturating_add(1));
        }

        // Consumers first: a playlist that ends on this very fetch would
        // otherwise dispatch its whole window to nobody, and the consumer (a
        // recorder starting against a broadcast that just ended) would never
        // see a segment at all.
        if !self.handle_new_readers() {
            return Err(Stopped).union();
        }
        if !self.park_if_idle().await {
            return Err(Stopped).union();
        }

        let new_segments = self.reload().await?;

        self.prefetches
            .retain_unordered(|p| p.expected_id >= self.segment_tail);

        let sleep_dur = if new_segments == 0 {
            (self.last_segment_dur / 4).clamp(MIN_EDGE_POLL, MAX_EDGE_POLL)
        } else {
            CATCHUP_POLL
        };
        if let Some(sleep_time) = sleep_dur.checked_sub(start.elapsed())
            && !self.idle(sleep_time).await
        {
            return Err(Stopped).union();
        }

        Ok(())
    }

    /// Sleep out the poll interval, waking early whenever a consumer connects.
    /// Returns `false` when the registration channel closed (stream dropped).
    async fn idle(&mut self, sleep_time: Duration) -> bool {
        let deadline = Instant::now() + sleep_time;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return true;
            }

            let sleep = compio::time::sleep(remaining);
            let wait = next_reader(self.receive.clone(), Rc::clone(&self.reader_wake));
            pin_mut!(sleep, wait);

            match select(sleep, wait).await {
                Either::Left(((), _)) => return true,
                Either::Right((None, _)) => return false,
                Either::Right((Some(reader), _)) => {
                    self.adopt(reader);
                    if !self.handle_new_readers() {
                        return false;
                    }
                }
            }
        }
    }

    async fn dispatch_main(
        &mut self,
        url: &str,
        expected_sequence: u64,
        timestamp_ms: u64,
        pdt_ms: u64,
        duration_ms: u64,
    ) -> eros::Result<(), (ArenaFull, kanal::SendError)> {
        let prefetch = self
            .prefetches
            .iter()
            .position(|s| s.expected_id == expected_sequence && s.url.as_ref() == url)
            .map(|idx| self.prefetches.swap_remove(idx));

        debug!(
            sequence = expected_sequence,
            prefetched = prefetch.is_some(),
            "segment dispatched"
        );

        let duration = Duration::from_millis(duration_ms);
        let span = debug_span!(
            "main segment",
            sequence_id = expected_sequence,
            was_prefetched = prefetch.is_some(),
            duration = duration.as_secs_f64(),
        );

        let (segment, writer) = if let Some(prefetch) = prefetch {
            (
                Segment {
                    sequence_num: expected_sequence,
                    reader: prefetch.reader,
                    timestamp_ms,
                    pdt_ms,
                    duration_ms,
                },
                None,
            )
        } else {
            let (writer, reader) = Spool::open(&self.arena, &self.wakes).union()?;
            (
                Segment {
                    sequence_num: expected_sequence,
                    reader,
                    timestamp_ms,
                    pdt_ms,
                    duration_ms,
                },
                Some(writer),
            )
        };
        self.segments.push(segment.clone());

        if let Some(writer) = writer {
            self.downloaders
                .send(ToDownloader::FetchURL {
                    url: Rc::from(url),
                    writer,
                    span: span.clone(),
                    done: None,
                    stats: self.metrics.as_ref().map(|metrics| FetchStats {
                        metrics: Rc::clone(metrics),
                        sequence: expected_sequence,
                        timestamp_ms,
                        duration_ms,
                        pdt_ms,
                    }),
                })
                .await
                .union()?;
        }

        self.readers
            .retain_unordered(|reader| reader.send(segment.clone().into(), "segment"));

        self.last_segment_dur = duration;

        Ok(())
    }

    async fn dispatch_prefetch(
        &mut self,
        url: &str,
        expected_sequence: u64,
    ) -> eros::Result<(), (ArenaFull, kanal::SendError, Dispatch)> {
        let prefetch = self
            .prefetches
            .iter()
            .find(|s| s.expected_id == expected_sequence);

        if prefetch.is_some() {
            return Err(Dispatch).union();
        }
        debug!(sequence = expected_sequence, "prefetch dispatched");

        let (writer, reader) = Spool::open(&self.arena, &self.wakes).union()?;
        let span = debug_span!("prefetch segment", sequence_id = expected_sequence);
        let url = Rc::<str>::from(url);
        let duration_ms = u64::try_from(self.last_segment_dur.as_millis()).unwrap_or(0);
        let timestamp_ms = self.segments.last().map_or(0, |segment| {
            let offset = expected_sequence.saturating_sub(segment.sequence_num + 1);
            segment
                .timestamp_ms
                .saturating_add(duration_ms.saturating_mul(offset + 1))
        });

        self.downloaders
            .send(ToDownloader::FetchURL {
                url: url.clone(),
                writer,
                span,
                done: None,
                stats: self.metrics.as_ref().map(|metrics| FetchStats {
                    metrics: Rc::clone(metrics),
                    sequence: expected_sequence,
                    timestamp_ms,
                    duration_ms,
                    pdt_ms: 0,
                }),
            })
            .await
            .union()?;

        self.readers.retain_unordered(|consumer| {
            consumer.send(
                SegmentMessage::Prefetch {
                    expected_sequence,
                    timestamp_ms,
                    reader: reader.clone(),
                },
                "prefetch",
            )
        });
        self.prefetches.push(PrefetchSegment {
            expected_id: expected_sequence,
            url,
            reader,
        });

        Ok(())
    }

    fn broadcast_discontinuity(&mut self) {
        self.readers.retain_unordered(|reader| {
            reader.send(SegmentMessage::Discontinuity(), "discontinuity")
        });
    }

    async fn apply_init(&mut self, uri: &str) -> eros::Result<(), (ArenaFull, kanal::SendError)> {
        if let Some((old, _)) = &self.init {
            if old.as_str() == uri {
                return Ok(());
            }
            debug!(old, uri, "init segment changed mid-stream");
        }

        let span = debug_span!("init segment");
        let (writer, reader) = Spool::open(&self.arena, &self.wakes).union()?;
        self.downloaders
            .send(ToDownloader::FetchURL {
                url: uri.into(),
                writer,
                span,
                done: None,
                stats: None,
            })
            .await
            .union()?;
        self.init = Some((uri.into(), reader.clone()));
        // Media must not overtake the new map: existing consumers get it now,
        // while a consumer adopted later gets it from `adopt`.
        self.readers.retain_unordered(|consumer| {
            consumer.send(
                SegmentMessage::Init {
                    reader: reader.clone(),
                },
                "init change",
            )
        });

        Ok(())
    }

    async fn reload(&mut self) -> eros::Result<u64, SchedulerErrors> {
        let mut response = self.client.get(&self.stream_url).await.union()?;
        let body = response.body.read_to_end(MAX_MANIFEST).await.union()?;
        let manifest = std::str::from_utf8(&body).map_err(|_| Utf8).union()?;

        let original_prefetch_tail = self.prefetch_tail;
        // Dropout protection repeats the final sequence at every checkpoint, so
        // only the last annotation is a meaningful canary.
        let mut last_checkpoint = None;

        // A media playlist's `EXT-X-MAP` applies to the following media until the
        // next map. Holding it until a *new* media item avoids re-announcing the
        // window's old map (the top of a refreshed playlist) and then the newer
        // map again on every reload.
        let mut pending_init = None;
        for event in Playlist::new(manifest) {
            match event.union()? {
                Event::MediaSequence(front) => {
                    debug!("new front {front}");
                    if self.resume_at_edge {
                        // Resume at the live edge: the window before the park is
                        // older than any consumer that just arrived can use.
                        self.resume_at_edge = false;
                        self.segment_tail = self.segment_tail.max(front);
                        self.prefetch_tail = self.prefetch_tail.max(front);
                    }
                    self.segments.retain(|s| s.sequence_num >= front);
                }
                Event::ProgramDateTime(pdt_ms) => {
                    debug!(pdt_ms, "new program date time");
                }
                Event::Init(init) => pending_init = Some(init.uri),
                Event::Discontinuity => self.broadcast_discontinuity(),
                Event::Checkpoint { annotated, next } => last_checkpoint = Some((annotated, next)),
                Event::Ended => {
                    info!("stream offline");
                    return Err(Offline).union();
                }
                Event::Media(media) => {
                    // Ads consume sequence numbers but are never downloaded.
                    if media.is_ad || media.sequence < self.segment_tail {
                        continue;
                    }

                    if let Some(uri) = pending_init.take() {
                        debug!(sequence = media.sequence, uri, "applying playlist map");
                        self.apply_init(uri).await.widen()?;
                    }
                    self.segment_tail = media.sequence + 1;
                    self.prefetch_tail = self.prefetch_tail.max(self.segment_tail);
                    self.dispatch_main(
                        media.uri,
                        media.sequence,
                        media.timestamp_ms,
                        media.pdt_ms.unwrap_or(0),
                        media.duration_ms,
                    )
                    .await
                    .widen()?;
                }
                Event::Prefetch { uri, sequence } => {
                    if sequence < self.prefetch_tail {
                        continue;
                    }

                    self.prefetch_tail = sequence + 1;
                    self.dispatch_prefetch(uri, sequence).await.widen()?;
                }
            }
        }

        if let Some((annotated, next)) = last_checkpoint
            && annotated > next
        {
            warn!(
                tls = annotated,
                sequence = next,
                "live sequence annotation ahead of positional sequence"
            );
        }

        (*self.live_window.borrow_mut()).clone_from(&self.segments);
        if let Some(metrics) = &self.metrics {
            metrics.segment_tail.set(self.segment_tail);
            metrics.prefetch_tail.set(self.prefetch_tail);
            metrics
                .window
                .set(u32::try_from(self.segments.len()).unwrap_or(u32::MAX));
            metrics.set_error(None);
        }
        Ok(self.prefetch_tail - original_prefetch_tail)
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;

    #[test]
    fn a_full_consumer_queue_is_dropped() {
        let (tx, rx) = kanal::bounded(1);
        let consumer = Consumer { tx, lossy: false };

        assert!(consumer.send(SegmentMessage::Discontinuity(), "first"));
        // No receiver poll happened, so the queue is full: the consumer is dropped.
        assert!(!consumer.send(SegmentMessage::Discontinuity(), "second"));

        // Closing discards whatever was queued, so the consumer sees a closed
        // channel immediately.
        assert!(
            consumer
                .tx
                .try_send(SegmentMessage::Discontinuity())
                .is_err()
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_lossy_consumer_keeps_going_when_full() {
        let (tx, rx) = kanal::bounded(1);
        let consumer = Consumer { tx, lossy: true };

        assert!(consumer.send(SegmentMessage::Discontinuity(), "first"));
        assert!(consumer.send(SegmentMessage::Discontinuity(), "second"));
        assert!(consumer.send(SegmentMessage::Discontinuity(), "third"));
        assert!(rx.try_recv().is_ok());
    }

    fn run<F: Future>(future: F) -> F::Output {
        compio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(future)
    }

    #[test]
    fn a_reader_already_queued_is_taken_without_waiting() {
        let (queue, receive) = kanal::unbounded();
        let (consumer, _keep) = kanal::bounded::<SegmentMessage>(1);
        queue
            .send(ConsumerRegistration {
                tx: consumer,
                backlog: true,
                lossy: false,
            })
            .expect("queue");

        // Would hang if the first check did not see the queued sender.
        let taken = run(next_reader(receive, ConsumerWake::new())).expect("registration");
        assert!(taken.tx.try_send(SegmentMessage::Discontinuity()).is_ok());
    }

    #[test]
    fn a_reader_registered_during_the_wait_wakes_it() {
        let (queue, receive) = kanal::unbounded();
        let wake = ConsumerWake::new();
        let signalled = run(async {
            let signal = Rc::clone(&wake);
            compio::runtime::spawn(async move {
                compio::time::sleep(Duration::from_millis(10)).await;
                let (consumer, _keep) = kanal::bounded::<SegmentMessage>(1);
                queue
                    .send(ConsumerRegistration {
                        tx: consumer,
                        backlog: true,
                        lossy: false,
                    })
                    .expect("queue");
                signal.wake();
            })
            .detach();

            let wait = next_reader(receive, Rc::clone(&wake));
            let guard = compio::time::sleep(Duration::from_secs(5));
            pin_mut!(wait, guard);
            matches!(select(wait, guard).await, Either::Left(_))
        });
        assert!(signalled);
    }

    #[test]
    fn a_dropped_stream_closes_the_registration_latch() {
        let (queue, receive) = kanal::unbounded::<ConsumerRegistration>();
        // No senders left and nothing queued: the daemon dropped the stream.
        drop(queue);
        assert!(run(next_reader(receive, ConsumerWake::new())).is_none());
    }
}
