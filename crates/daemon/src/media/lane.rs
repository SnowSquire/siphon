//! The media lane: one producer per lane, one sender per connection.
//!
//! Everything that leaves the daemon as media does so through a
//! [`LaneProducer`], and every datagram the connection sends goes through one
//! [`MediaSender`]. The split is what makes priority decidable in one place: a
//! producer encodes an access unit into fragments and offers them, and the
//! sender queues them high or low and drains high first. Audio and an edge lane
//! are high; a catch-up lane's video is low (`notes/riffle.md`, "Multiple
//! lanes").
//!
//! # The push rule
//!
//! The producer sends while `sent_position < playhead + buffer_ms` and never
//! faster than data exists, which is the whole flow control. When the rule
//! stops it, it stops *reading*: the rest of the segment is dropped and the
//! lane's consumer registration is thrown away until the playhead advances.
//! Nothing queues per lane, which is why an unlimited number of lanes costs the
//! daemon nothing but a cursor (`notes/riffle.md`, "Push, not pull").
//!
//! # Drops
//!
//! An access unit the playhead has already passed is superseded — skipped whole
//! and counted, never trimmed or partially sent, because a truncated access
//! unit is a broken bitstream rather than a cheaper picture. That is the
//! datamosh drop, and it is the sender-side win.
//!
//! # Tasks and lifetime
//!
//! A producer owns one spool reader at a time and exits when its segment
//! channel closes, which is what [`ProducerHandle`] does from the app side: the
//! handle closes the channel the scheduler feeds, and the sleep a paused
//! producer waits on ends with the handle.

use std::cell::Cell;
use std::cell::RefCell;
use std::future::poll_fn;
use std::rc::Rc;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;
use std::time::Instant;

use compio::buf::IoBuf as _;
use futures_util::FutureExt as _;
use futures_util::future::Either;
use futures_util::future::select;
use futures_util::pin_mut;
use siphon_shared::riffle::Class;
use siphon_shared::riffle::StreamKey;
use siphon_shared::riffle::codec;
use siphon_shared::riffle::object::DATAGRAM_HEADER;
use siphon_shared::riffle::quic::Datagrams;
use tracing::debug;
use tracing::warn;

use crate::lane::LaneHandle;
use crate::lane::Start;
use crate::media::wire::AccessUnit;
use crate::media::wire::AccessUnits;
use crate::media::wire::Container;
use crate::media::wire::DATAGRAM_BUDGET;
use crate::media::wire::Packer;
use crate::media::wire::SegmentSource;
use crate::media::wire::Track;
use crate::riffle::Outbound;
use crate::scheduler::ConsumerRegistration;
use crate::scheduler::Segment;
use crate::scheduler::SegmentMessage;
use crate::spool::ConsumerWake;
use crate::spool::Reader;

/// One datagram's queue priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    /// Audio, and any lane at the live edge. Drained first.
    High,
    /// Video from a lane that is catching up: it has no deadline.
    Low,
}

impl Priority {
    /// Audio has absolute priority inside a lane, and an edge lane outranks a
    /// lane that is still filling its buffer (`notes/riffle.md`, "Audio" and
    /// "Multiple lanes"). Those are two rules and one bit: what is left is a
    /// catch-up lane's video, which nothing is waiting for.
    #[must_use]
    pub const fn of(class: Class, at_edge: bool) -> Self {
        match class {
            Class::Audio => Self::High,
            _ if at_edge => Self::High,
            _ => Self::Low,
        }
    }
}

/// Where a producer's datagrams go: the connection's media sender, or a test's
/// collector.
///
/// `offer` copies, because a queued datagram has to outlive the packer's
/// scratch; the sender takes its buffer from a pool, so a steady stream costs
/// no allocations.
pub trait Sink {
    /// Bytes one datagram may be right now, object header included — the same
    /// number as the carrier's `max_size`.
    fn budget(&self) -> usize;

    /// Queues one encoded datagram, waiting for space rather than dropping it.
    ///
    /// A live lane cannot be paced faster than the carrier accepts, and a whole
    /// segment is offered in one pass, so the queue fills before the sender task
    /// has had a turn. Waiting is backpressure; dropping a fragment of a
    /// keyframe would break the picture for a queue-depth's worth of reasons.
    async fn offer(&self, priority: Priority, datagram: &[u8]);
}

/// Datagrams one priority may hold before one is dropped.
///
/// A second of a 1080p60 edge lane in fragments is a few hundred; this is past
/// that, so a full queue means the carrier has stopped accepting rather than a
/// burst.
pub const QUEUE_DEPTH: usize = 512;

/// Datagram buffers kept for reuse. The sender frees each one back after the
/// carrier has copied it.
const POOL_DEPTH: usize = 64;

/// Object ids one lane may consume before the next lane's range starts.
///
/// `obj_id` is monotonic for the life of a connection and never reset
/// (`notes/riffle.md`, "One header"). A lane at 60 fps for 24 hours is ~5.2
/// million objects, so this leaves three orders of magnitude of headroom and
/// keeps two lanes on one connection from ever sharing an id.
pub const LANE_ID_SPAN: u64 = 1 << 40;

/// One wake, like the spool's: tasks park here and the other side wakes them.
///
/// A list rather than one slot because several lanes' producers may be waiting
/// for queue space on the same connection; waking only the last one to park
/// would starve the others.
#[derive(Default)]
struct Wakes {
    wakers: RefCell<Vec<Waker>>,
}

impl Wakes {
    fn park(&self, context: &std::task::Context<'_>) {
        self.wakers.borrow_mut().push(context.waker().clone());
    }

    fn wake_all(&self) {
        for waker in self.wakers.borrow_mut().drain(..) {
            waker.wake();
        }
    }
}

/// Buffers a sent datagram hands back, so a steady stream allocates nothing.
#[derive(Clone, Default)]
struct DatagramPool(Rc<RefCell<Vec<Vec<u8>>>>);

impl DatagramPool {
    fn take(&self) -> Vec<u8> {
        self.0.borrow_mut().pop().unwrap_or_default()
    }

    fn give(&self, mut buffer: Vec<u8>) {
        buffer.clear();
        let mut pool = self.0.borrow_mut();
        if pool.len() < POOL_DEPTH {
            pool.push(buffer);
        }
    }
}

/// A connection's datagram counters.
#[derive(Debug, Default)]
pub struct MediaStats {
    failed: Cell<u32>,
}

impl MediaStats {
    /// Datagrams the carrier refused — too large for the path, unsupported, or
    /// a closed connection — plus the ones dropped because a queue was full.
    /// This is the local send side of "lost", as opposed to a packet the
    /// network dropped, and it is what a snapshot reports.
    #[must_use]
    pub const fn failed(&self) -> u32 {
        self.failed.get()
    }

    fn note_failed(&self) {
        self.failed.set(self.failed.get().saturating_add(1));
    }
}

/// Bytes one pipe media frame may carry.
///
/// A pipe has no path MTU, so a fragment is not sized by one: 64 KiB keeps the
/// frame count and the 5-byte framing amortized over a whole access unit without
/// making the loop's write buffer lumpy. QUIC's budget comes from the path and
/// is read per segment instead.
pub const PIPE_BUDGET: usize = 64 * 1024;

/// How much of a pipe connection's out buffer media may take.
///
/// The buffer belongs to the control plane too, and `MAX_OUT` drops a
/// connection that outgrows it. Media stops at half, so a lane can never be the
/// reason a connection loses control frames: the tail the control plane gets is
/// always at least this much, whatever the lane is doing.
pub const PIPE_MEDIA_OUT: usize = crate::riffle::MAX_OUT / 2;

/// Wakes the daemon loop when a pipe frame has been queued.
///
/// The loop owns the writer, so it is the only thing that can arm a write; a
/// bucket of media that arrived while it was parked must not wait for the next
/// deadline. One waiter, like the client's wake, so an idle loop cannot grow it.
#[derive(Default)]
pub struct PipeSignal {
    queued: Cell<bool>,
    waker: Cell<Option<Waker>>,
}

impl PipeSignal {
    /// A frame is queued: the loop has something to write.
    fn signal(&self) {
        self.queued.set(true);
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    /// Resolves once a frame has been queued since the last call.
    pub async fn wait(&self) {
        poll_fn(|context| {
            if self.queued.replace(false) {
                return Poll::Ready(());
            }
            self.waker.set(Some(context.waker().clone()));
            // A signal between the check above and the park would otherwise be
            // lost; the flag is the latch, and the waker only saves a poll.
            if self.queued.replace(false) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
}

/// What a datagram handed to the carrier became.
enum Carried {
    /// The carrier took the whole frame.
    Queued,
    /// There is no room right now; the caller keeps it and tries again.
    Full,
    /// The carrier refused it, so it is lost and counted.
    Failed,
}

/// Where one connection's media goes: one frame per object fragment either way,
/// but two very different writers.
#[derive(Clone)]
enum Carrier {
    /// QUIC: one DATAGRAM frame per fragment, sent from the sender task.
    Datagrams(Datagrams),
    /// The local pipe: a class-1 frame appended to the connection's out buffer,
    /// the same one the control plane writes, so one writer carries both.
    Pipe {
        out: Outbound,
        /// Wakes the loop after a drain pass: nothing else may write `out`.
        wake: Rc<PipeSignal>,
    },
}

impl Carrier {
    /// The most one frame may carry, which is what the packer is built from.
    fn budget(&self) -> usize {
        match self {
            Self::Datagrams(datagrams) => datagrams.max_size().unwrap_or(DATAGRAM_BUDGET),
            Self::Pipe { .. } => PIPE_BUDGET,
        }
    }

    fn send(&self, datagram: &[u8]) -> Carried {
        match self {
            Self::Datagrams(datagrams) => match datagrams.send(datagram) {
                Ok(()) => Carried::Queued,
                Err(error) => {
                    debug!(%error, "media: datagram not queued");
                    Carried::Failed
                }
            },
            Self::Pipe { out, .. } => {
                let mut out = out.borrow_mut();
                if out.len() + codec::FRAME_PREFIX + datagram.len() > PIPE_MEDIA_OUT {
                    return Carried::Full;
                }
                if codec::encode_media(datagram, &mut out).is_err() {
                    return Carried::Failed;
                }
                Carried::Queued
            }
        }
    }

    /// Tells the loop the out buffer has something to write. A no-op for QUIC,
    /// whose frames are the carrier's own queue.
    fn notify(&self) {
        if let Self::Pipe { wake, .. } = self {
            wake.signal();
        }
    }

    const fn is_pipe(&self) -> bool {
        matches!(self, Self::Pipe { .. })
    }
}

/// The one task per connection that turns queued datagrams into frames on the
/// carrier.
///
/// Cloneable: every lane's producer holds one to offer into. The task drains
/// `high` completely before it looks at `low`, so a video burst from a
/// catch-up lane cannot delay an audio frame from any lane.
#[derive(Clone)]
pub struct MediaSender {
    high: kanal::Sender<Vec<u8>>,
    low: kanal::Sender<Vec<u8>>,
    /// A datagram was queued: wake the sender task.
    queued: Rc<Wakes>,
    /// The sender drained: wake the producers waiting for space.
    space: Rc<Wakes>,
    pool: DatagramPool,
    carrier: Carrier,
    stats: Rc<MediaStats>,
}

impl MediaSender {
    /// Starts the sender for one QUIC connection.
    ///
    /// The task owns a clone of the datagram handle and exits when both queues
    /// close, which happens when the connection — and every producer holding a
    /// clone of this — is dropped.
    #[must_use]
    pub fn spawn_datagrams(datagrams: Datagrams) -> Self {
        Self::spawn(Carrier::Datagrams(datagrams))
    }

    /// Starts the sender for one pipe connection.
    ///
    /// `out` is the connection's own out buffer, shared with the control plane,
    /// and `wake` is how the loop is told to write it.
    #[must_use]
    pub fn spawn_pipe(out: Outbound, wake: Rc<PipeSignal>) -> Self {
        Self::spawn(Carrier::Pipe { out, wake })
    }

    fn spawn(carrier: Carrier) -> Self {
        let (high_tx, high_rx) = kanal::bounded(QUEUE_DEPTH);
        let (low_tx, low_rx) = kanal::bounded(QUEUE_DEPTH);
        let queued = Rc::new(Wakes::default());
        let space = Rc::new(Wakes::default());
        let pool = DatagramPool::default();
        let stats = Rc::new(MediaStats::default());
        compio::runtime::spawn(run(
            high_rx,
            low_rx,
            Rc::clone(&queued),
            Rc::clone(&space),
            carrier.clone(),
            pool.clone(),
            Rc::clone(&stats),
        ))
        .detach();
        Self {
            high: high_tx,
            low: low_tx,
            queued,
            space,
            pool,
            carrier,
            stats,
        }
    }

    #[must_use]
    pub fn stats(&self) -> Rc<MediaStats> {
        Rc::clone(&self.stats)
    }

    /// Room was made in the out buffer: a pipe lane may fill it again. A no-op
    /// for QUIC, whose queue is the carrier's rather than the loop's.
    pub fn resume(&self) {
        if self.carrier.is_pipe() {
            self.queued.wake_all();
        }
    }
}

impl Sink for MediaSender {
    fn budget(&self) -> usize {
        self.carrier.budget().max(DATAGRAM_HEADER + 1)
    }

    async fn offer(&self, priority: Priority, datagram: &[u8]) {
        let queue = match priority {
            Priority::High => &self.high,
            Priority::Low => &self.low,
        };
        // Wait for room. A closed queue is space: the send below then fails
        // once and is counted, rather than parking a producer forever.
        poll_fn(|context| {
            if !queue.is_full() {
                return Poll::Ready(());
            }
            self.space.park(context);
            if queue.is_full() {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;

        let mut buffer = self.pool.take();
        buffer.extend_from_slice(datagram);
        match queue.try_send(buffer) {
            Ok(true) => self.queued.wake_all(),
            // `Ok(false)` is a race with another producer filling the last
            // slot; the next `offer` waits again. `Err` is the sender task
            // gone, and that datagram is lost.
            Ok(false) => {}
            Err(_) => self.stats.note_failed(),
        }
    }
}

/// Sends everything one priority has that the carrier will take, oldest first.
///
/// `held` is the item the carrier had no room for last time: it is tried before
/// the queue, so a priority is never reordered and nothing is dropped to make
/// room. `true` means at least one frame left, so a producer waiting for space
/// may now have it.
fn send_priority(
    carrier: &Carrier,
    pool: &DatagramPool,
    stats: &MediaStats,
    held: &mut Option<Vec<u8>>,
    queue: &kanal::Receiver<Vec<u8>>,
) -> bool {
    let mut sent = false;
    loop {
        let buffer = if let Some(buffer) = held.take() {
            buffer
        } else {
            match queue.try_recv() {
                Ok(Some(buffer)) => buffer,
                Ok(None) | Err(_) => break,
            }
        };
        match carrier.send(&buffer) {
            Carried::Queued => {
                pool.give(buffer);
                sent = true;
            }
            Carried::Full => {
                *held = Some(buffer);
                break;
            }
            Carried::Failed => {
                stats.note_failed();
                pool.give(buffer);
                sent = true;
            }
        }
    }
    sent
}

async fn run(
    high: kanal::Receiver<Vec<u8>>,
    low: kanal::Receiver<Vec<u8>>,
    queued: Rc<Wakes>,
    space: Rc<Wakes>,
    carrier: Carrier,
    pool: DatagramPool,
    stats: Rc<MediaStats>,
) {
    // An item the carrier had no room for. It is invisible to the latch below:
    // until the loop drains the out buffer (`resume`), retrying it would spin.
    let mut held_high: Option<Vec<u8>> = None;
    let mut held_low: Option<Vec<u8>> = None;
    loop {
        let open = poll_fn(|context| {
            let sent = send_priority(&carrier, &pool, &stats, &mut held_high, &high)
                | send_priority(&carrier, &pool, &stats, &mut held_low, &low);
            if sent {
                // Room was freed: every producer waiting for it may go on.
                space.wake_all();
                carrier.notify();
            }
            if high.is_closed() && low.is_closed() {
                return Poll::Ready(false);
            }
            queued.park(context);
            // The queue is the latch: a datagram offered between the drain
            // above and the park is still in it, so this second look is what
            // closes the race. The waker only saves a poll.
            let pending = (held_high.is_none() && !high.is_empty())
                || (held_low.is_none() && !low.is_empty());
            if pending {
                Poll::Ready(true)
            } else {
                Poll::Pending
            }
        })
        .await;
        if !open {
            // Everything queued has been sent and nothing can be offered again.
            return;
        }
    }
}

/// Everything a media lane needs to read one pipeline.
pub struct LaneConsumer {
    /// The pipeline's consumer registration channel.
    pub slot: kanal::Sender<ConsumerRegistration>,
    /// The wake this lane's spool reads park on. It has to be one of the
    /// pipeline's, because that is the set a publisher wakes.
    pub wake: Rc<ConsumerWake>,
}

/// A lane's input: the segment messages the scheduler dispatches to it.
///
/// Registering is not a one-off. A producer that the push rule stops throws its
/// registration away so nothing queues up behind it, and registers again when
/// the playhead has advanced — that is the "stop reading" half of the rule.
struct SegmentStream {
    /// The pipeline's registration channel, for (re-)registering.
    slot: kanal::Sender<ConsumerRegistration>,
    /// Our end of the current registration. Closing it is what makes the
    /// scheduler drop us and what wakes a parked `recv`.
    tx: kanal::Sender<SegmentMessage>,
    messages: kanal::AsyncReceiver<SegmentMessage>,
}

impl SegmentStream {
    /// Registers a consumer. `backlog` replays the pipeline's retained window
    /// first, which is what a lane that is behind the edge needs.
    fn register(slot: kanal::Sender<ConsumerRegistration>, backlog: bool) -> Option<Self> {
        let (tx, rx) = kanal::bounded(crate::constants::MAX_CONSUMER_LAG);
        let registration = ConsumerRegistration {
            tx: tx.clone(),
            backlog,
            // A full queue drops an individual segment rather than
            // disconnecting the lane: the push rule, not the queue, decides
            // what this lane needs, and it re-registers anyway.
            lossy: true,
        };
        slot.send(registration).ok()?;
        Some(Self {
            slot,
            tx,
            messages: rx.to_async(),
        })
    }

    /// Registers again after a pause. A paused lane is behind by construction,
    /// so it replays the window: that is where the segment it needs next is.
    fn reregister(&mut self) {
        match Self::register(self.slot.clone(), true) {
            Some(next) => *self = next,
            // The pipeline is gone; nothing will ever be dispatched again. The
            // closed channel is what ends the producer.
            None => {
                let _ = self.tx.close();
            }
        }
    }

    /// Drops the registration, so the scheduler stops dispatching to this lane.
    ///
    /// This is a real close and not a full queue: the scheduler unregisters a
    /// consumer only when its channel is closed, which is exactly the signal
    /// this sends.
    fn close_registration(&self) {
        let _ = self.tx.close();
    }

    /// The next message, or `None` once the lane has been closed under us.
    async fn next(&self) -> Option<SegmentMessage> {
        self.messages.recv().await.ok()
    }

    fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Our end of the current registration, for the handle that closes it.
    fn control(&self) -> kanal::Sender<SegmentMessage> {
        self.tx.clone()
    }
}

/// Stops one lane's producer.
///
/// Closing the segment channel is the stop signal: the producer's `recv`
/// returns at once, and the scheduler sees a closed consumer instead of a full
/// one — the only thing that unregisters it. Dropping the stop sender ends the
/// sleep a paused producer is in.
#[must_use = "dropping the handle stops the lane"]
pub struct ProducerHandle {
    tx: kanal::Sender<SegmentMessage>,
    stop: Option<kanal::Sender<()>>,
}

impl ProducerHandle {
    /// Ends the producer now. Idempotent.
    pub fn stop(&mut self) {
        let _ = self.tx.close();
        self.stop = None;
    }
}

impl Drop for ProducerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The ceiling on how long a paused producer sleeps before it re-checks the
/// push rule.
///
/// The playhead advances by wall clock, so the deadline is computable, but a
/// `StreamUpdate` or a seek can move it forward early and this keeps the
/// estimate from being stale for long.
const PAUSE_RECHECK: Duration = Duration::from_millis(500);

/// What feeding one segment ended with.
enum Outcome {
    Done,
    /// The client's buffer is full: stop reading until the playhead moves.
    Paused,
    /// The lane was closed.
    Closed,
}

/// One lane's media producer.
pub struct LaneProducer<S: Sink> {
    key: StreamKey,
    handle: LaneHandle,
    stream: SegmentStream,
    sink: Rc<S>,
    stop: kanal::AsyncReceiver<()>,

    /// The demuxer, which carries state across segments. Rebuilt when the
    /// container changes or a new init segment arrives.
    source: Option<SegmentSource>,
    /// The fMP4 init segment, kept because a rebuild needs it.
    init: Option<Vec<u8>>,
    /// A seek's start, until the segment that holds its keyframe arrives.
    start: Option<Start>,

    packer: Packer,
    /// One segment's bytes, reused.
    segment: Vec<u8>,
    /// The packer's payload buffer, reused.
    scratch: Vec<u8>,
    /// One encoded datagram, reused and sized to the carrier's budget.
    datagram: Vec<u8>,

    /// Per track, the offset that maps the container's clock onto the
    /// broadcast's millisecond timeline. Taken from the first access unit of
    /// each track and kept for the life of the producer, because a track's
    /// clock runs continuously.
    anchor: [Option<i64>; 2],

    wake: Rc<ConsumerWake>,
}

impl<S: Sink + 'static> LaneProducer<S> {
    /// Starts the producer for one lane.
    ///
    /// `None` means the pipeline has no scheduler left to feed this lane, so
    /// the caller has to fail the open rather than leave a lane that will never
    /// receive anything.
    #[must_use]
    pub fn spawn(
        key: StreamKey,
        handle: LaneHandle,
        consumer: LaneConsumer,
        sink: Rc<S>,
        lane: u64,
    ) -> Option<ProducerHandle> {
        // A lane at the live edge wants what is published from now on: anything
        // older is either already at the client or superseded. A lane that is
        // behind wants the retained window, which is the only place the segment
        // it needs next can be — and that includes a lane a seek placed, whose
        // starting segment may be older still.
        let start = handle.take_start();
        let backlog = start.is_some() || !handle.at_edge(Instant::now());
        let stream = SegmentStream::register(consumer.slot, backlog)?;
        let control = stream.control();
        let (stop_tx, stop_rx) = kanal::bounded(1);
        let budget = sink.budget();
        // The header's lane field is 16 bits; ids stay disjoint on the full
        // `u64`, so a lane past 65535 would share a tag but never an id.
        let lane_id = u16::try_from(lane).unwrap_or(u16::MAX);
        let producer = Self {
            key,
            handle,
            stream,
            sink,
            stop: stop_rx.to_async(),
            source: None,
            init: None,
            start,
            packer: Packer::for_lane(lane_id, lane.wrapping_mul(LANE_ID_SPAN), budget),
            segment: Vec::new(),
            scratch: Vec::new(),
            datagram: vec![0; budget],
            anchor: [None, None],
            wake: consumer.wake,
        };
        compio::runtime::spawn(producer.run()).detach();
        Some(ProducerHandle {
            tx: control,
            stop: Some(stop_tx),
        })
    }

    async fn run(mut self) {
        loop {
            if self.stream.is_closed() {
                break;
            }
            let Some(message) = self.stream.next().await else {
                break;
            };
            match message {
                SegmentMessage::Init { reader } => self.read_init(reader).await,
                SegmentMessage::Segment(segment) => {
                    // A seek's window replay reaches back before the segment it
                    // picked, so whole segments are skipped until the one that
                    // holds the keyframe. A segment past it means the picked one
                    // rotated out of the window: starting at that segment's own
                    // first unit is the closest the daemon can still get.
                    let align = if let Some(start) = self.start.take() {
                        if segment.sequence_num < start.sequence {
                            self.start = Some(start);
                            continue;
                        }
                        (segment.sequence_num == start.sequence).then_some(start.pts_ms)
                    } else {
                        None
                    };
                    if !self.feed(segment, align).await {
                        break;
                    }
                }
                SegmentMessage::Discontinuity() => {
                    // The timelines restart, so the anchors are stale.
                    self.source = None;
                    self.anchor = [None, None];
                }
                // A prefetch pointer and a control-plane seek carry no media
                // this producer owns.
                SegmentMessage::Prefetch { .. } | SegmentMessage::Seek(_) => {}
            }
        }
        debug!(channel = self.key.channel, "media: lane producer stopped");
    }

    /// Reads the whole of one spool into [`Self::segment`].
    ///
    /// `false` means the lane was closed while reading.
    async fn read_segment(&mut self, reader: &mut Reader) -> bool {
        self.segment.clear();
        loop {
            if self.stream.is_closed() {
                return false;
            }
            let next = reader.next_view(&self.wake);
            let stop = self.stop.recv();
            pin_mut!(next, stop);
            match select(next, stop).await {
                Either::Left((Some(view), _)) => self.segment.extend_from_slice(view.as_init()),
                Either::Left((None, _)) => return true,
                Either::Right(_) => return false,
            }
        }
    }

    async fn read_init(&mut self, mut reader: Reader) {
        if !self.read_segment(&mut reader).await {
            return;
        }
        let bytes = std::mem::take(&mut self.segment);
        debug!(bytes = bytes.len(), "media: init segment");
        self.init = Some(bytes);
        // The sample-entry framing and the codec records are in the init
        // segment, so any source built before it is stale.
        self.source = None;
        self.anchor = [None, None];
    }

    /// Demuxes one segment and feeds its access units. `false` ends the lane.
    ///
    /// `align` is the keyframe a seek starts at, when this is the segment that
    /// holds it.
    async fn feed(&mut self, segment: Segment, align: Option<u64>) -> bool {
        let Segment {
            timestamp_ms,
            mut reader,
            ..
        } = segment;
        if !self.read_segment(&mut reader).await {
            return false;
        }
        let Some(container) = Container::detect(&self.segment) else {
            warn!(
                channel = self.key.channel,
                "media: unknown segment container"
            );
            return true;
        };
        if self.source.as_ref().map(SegmentSource::container) != Some(container) {
            let init = self.init.as_deref();
            match SegmentSource::new(container, init) {
                Ok(source) => self.source = Some(source),
                Err(error) => {
                    warn!(%error, "media: cannot demux this segment");
                    return true;
                }
            }
        }
        let Some(mut source) = self.source.take() else {
            return true;
        };
        if let Err(error) = source.feed(&self.segment) {
            warn!(%error, "media: segment refused");
            self.source = Some(source);
            return true;
        }
        // The carrier's budget is per path and moves with PMTU discovery, so it
        // is read here rather than once for the life of the connection. The
        // datagram buffer follows it.
        let budget = self.sink.budget().max(DATAGRAM_HEADER + 1);
        self.packer.set_datagram_budget(budget);
        if self.datagram.len() < budget {
            self.datagram.resize(budget, 0);
        }
        // The iterator borrows the source, which is out of `self` for exactly
        // this reason: packing needs `self` mutably.
        let outcome = self
            .feed_units(source.access_units(), timestamp_ms, align)
            .await;
        self.source = Some(source);
        match outcome {
            Outcome::Done => true,
            Outcome::Closed => false,
            Outcome::Paused => {
                self.pause().await;
                !self.stream.is_closed()
            }
        }
    }

    /// Packs and offers every access unit of one segment.
    async fn feed_units(
        &mut self,
        units: AccessUnits<'_>,
        segment_ms: u64,
        align: Option<u64>,
    ) -> Outcome {
        for unit in units {
            if self.stream.is_closed() {
                return Outcome::Closed;
            }
            // The anchor is taken before the seek filter: it is the mapping for
            // the whole track, and the units before the keyframe are still part
            // of the segment's clock.
            let media_ms = self.anchor(unit, segment_ms);
            // A seek starts at the located keyframe. What precedes it is the
            // part of the broadcast the client discards, so it is never sent —
            // and it is not loss, so it is not counted as a drop.
            if let Some(start) = align
                && media_ms < start
            {
                continue;
            }
            let now = Instant::now();

            // The push rule first: a paused lane does not read at all, so
            // nothing here is a drop.
            if !self.handle.may_send(now) {
                return Outcome::Paused;
            }
            // A replayed window starts with what this lane already has.
            if self.handle.already_sent(unit.track, media_ms) {
                continue;
            }
            // An access unit the playhead has passed is superseded whole.
            if self.handle.superseded(media_ms, now) {
                self.handle.note_drop(unit.track, now);
                continue;
            }

            let priority = Priority::of(unit.track.class(), self.handle.at_edge(now));
            let fragments = match self.packer.pack(unit, &mut self.scratch) {
                Ok(fragments) => fragments,
                Err(error) => {
                    warn!(%error, "media: access unit refused");
                    continue;
                }
            };
            for fragment in fragments {
                let Ok(written) = fragment.encode_datagram(&mut self.datagram) else {
                    warn!("media: fragment does not encode");
                    continue;
                };
                self.sink.offer(priority, &self.datagram[..written]).await;
                // Audio is sent twice rather than retransmitted: +16 KB/s buys
                // near-zero loss without a round trip, and a round trip is the
                // only thing that could make it late (`notes/riffle.md`,
                // "Audio"). The copy is identical, so a receiver that gets both
                // sees one object twice and one that loses either is whole.
                if unit.track == Track::Audio {
                    self.sink
                        .offer(Priority::High, &self.datagram[..written])
                        .await;
                }
            }
            self.handle.note_sent(unit.track, media_ms);
        }
        Outcome::Done
    }

    /// Maps one access unit onto the broadcast's millisecond timeline.
    ///
    /// The playlist says where a segment starts; the container says where its
    /// samples are. The difference is the track's origin, and it is constant, so
    /// it is taken once per track and reused.
    fn anchor(&mut self, unit: AccessUnit<'_>, segment_ms: u64) -> u64 {
        let slot = match unit.track {
            Track::Video => 0,
            Track::Audio => 1,
        };
        let container_ms = unit.pts_ms();
        let offset = if let Some(offset) = self.anchor[slot] {
            offset
        } else {
            let offset =
                i64::try_from(segment_ms).unwrap_or(0) - i64::try_from(container_ms).unwrap_or(0);
            self.anchor[slot] = Some(offset);
            offset
        };
        let media = i64::try_from(container_ms).unwrap_or(0) + offset;
        u64::try_from(media).unwrap_or(0)
    }

    /// Stops reading until the push rule allows more, then registers again.
    ///
    /// While paused the lane holds no registration, so the scheduler does not
    /// queue segments for it and no spool slot is pinned by a lane that is not
    /// reading.
    async fn pause(&mut self) {
        self.stream.close_registration();
        loop {
            let now = Instant::now();
            if self.handle.may_send(now) {
                break;
            }
            let sleep = compio::time::sleep(self.pause_for(now)).fuse();
            let stop = self.stop.recv().fuse();
            pin_mut!(sleep, stop);
            match select(sleep, stop).await {
                Either::Left(((), _)) => {}
                Either::Right(_) => return,
            }
        }
        self.stream.reregister();
    }

    /// How long the playhead needs to advance before one more access unit fits.
    fn pause_for(&self, now: Instant) -> Duration {
        let ahead = self.handle.buffered_ahead_ms(now);
        let buffer = u64::from(self.handle.buffer_ms());
        // The playhead advances by wall clock, so this is a deadline and not a
        // guess: the buffer is a duration of media and the rate is how fast the
        // client consumes it.
        let needed = ahead.saturating_sub(buffer).saturating_add(1);
        let rate = self
            .handle
            .playback()
            .map_or(1.0, |playback| playback.rate())
            .max(crate::lane::MIN_RATE);
        Duration::from_millis(needed)
            .div_f32(rate)
            .clamp(Duration::from_millis(20), PAUSE_RECHECK)
    }
}

/// Serves one pipeline's consumer registrations from a test's own segments.
///
/// A stand-in for the scheduler: it adopts every registration it is handed and
/// forwards what the test publishes, so the media lane is exercised end to end
/// over the real carrier without a playlist server. `consumers` counts the live
/// registrations, which is how a test sees a lane unregister.
///
/// It also keeps the pipeline's retained window, which the real scheduler
/// fills: `place` and a seek read it, and a lane that registers with `backlog`
/// is replayed from it. There is no map here, so this covers MPEG-TS fixtures.
#[cfg(all(test, not(miri)))]
pub fn spawn_fixture_feed(
    receive: kanal::Receiver<ConsumerRegistration>,
    source: kanal::AsyncReceiver<SegmentMessage>,
    consumers: Rc<Cell<usize>>,
    live_window: Rc<RefCell<Vec<Segment>>>,
) {
    compio::runtime::spawn(async move {
        let mut sinks: Vec<kanal::Sender<SegmentMessage>> = Vec::new();
        while let Ok(message) = source.recv().await {
            while let Ok(Some(registration)) = receive.try_recv() {
                // A lane that is starting behind the edge — a paused producer
                // or one a seek placed — replays the window, exactly as the
                // scheduler's own adoption does.
                if registration.backlog {
                    for segment in live_window.borrow().iter() {
                        let _ = registration
                            .tx
                            .try_send(SegmentMessage::Segment(segment.clone()));
                    }
                }
                sinks.push(registration.tx);
                consumers.set(consumers.get() + 1);
            }
            if let SegmentMessage::Segment(segment) = &message {
                live_window.borrow_mut().push(segment.clone());
            }
            sinks.retain(|sink| {
                let forwarded = match &message {
                    SegmentMessage::Segment(segment) => {
                        sink.try_send(SegmentMessage::Segment(segment.clone()))
                    }
                    _ => Ok(false),
                };
                if forwarded.is_ok() {
                    // `Ok(false)` is a full queue: the real scheduler's lossy
                    // consumer drops the message and keeps the producer.
                    true
                } else {
                    consumers.set(consumers.get().saturating_sub(1));
                    false
                }
            });
        }
    })
    .detach();
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::path::Path;
    use std::path::PathBuf;

    use siphon_shared::riffle::PlaybackPosition;
    use siphon_shared::riffle::StreamPlayback;
    use siphon_shared::riffle::object::Object;

    use super::*;
    use crate::arena::ArenaSet;
    use crate::lane::Lanes;
    use crate::spool::Spool;
    use crate::spool::WakeSet;
    use crate::spool::Writer;

    /// A sink that keeps what a producer offers, so a decision can be checked
    /// without a carrier in the way.
    #[derive(Default)]
    struct Collector {
        offered: RefCell<Vec<(Priority, Vec<u8>)>>,
        budget: Cell<usize>,
    }

    impl Collector {
        fn with_budget(budget: usize) -> Rc<Self> {
            Rc::new(Self {
                offered: RefCell::new(Vec::new()),
                budget: Cell::new(budget),
            })
        }

        fn count(&self) -> usize {
            self.offered.borrow().len()
        }

        fn offered(&self) -> Vec<(Priority, Vec<u8>)> {
            self.offered.borrow().clone()
        }
    }

    impl Sink for Collector {
        fn budget(&self) -> usize {
            self.budget.get()
        }

        async fn offer(&self, priority: Priority, datagram: &[u8]) {
            self.offered
                .borrow_mut()
                .push((priority, datagram.to_vec()));
        }
    }

    /// The fixtures are gitignored, so a checkout without `captures/` skips.
    fn fixture(relative: &str) -> Option<PathBuf> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../captures")
            .join(relative);
        path.exists().then_some(path)
    }

    /// Writes a whole segment into a spool and hands over its reader, with the
    /// sequence and playlist time the scheduler would give it. Closing the
    /// writer is what makes the reader see the end of the segment, exactly as a
    /// completed fetch does.
    fn spooled_at(
        arena: &Rc<ArenaSet>,
        bytes: &[u8],
        sequence_num: u64,
        timestamp_ms: u64,
    ) -> Segment {
        let wakes = WakeSet::new();
        let (mut writer, reader) = Spool::open(arena, &wakes).expect("spool");
        write(&mut writer, bytes);
        drop(writer);
        Segment {
            timestamp_ms,
            pdt_ms: 0,
            duration_ms: 2_000,
            sequence_num,
            reader,
        }
    }

    fn write(writer: &mut Writer, bytes: &[u8]) {
        let mut offset = 0;
        while offset < bytes.len() {
            let tail = writer.tail().expect("spool capacity");
            let len = tail.len().min(bytes.len() - offset);
            tail[..len].copy_from_slice(&bytes[offset..offset + len]);
            writer.commit(len);
            offset += len;
        }
    }

    /// One lane's producer, its sink and the scheduler's end of its segment
    /// queue — the whole media path with the carrier and the playlist replaced.
    struct Harness {
        collector: Rc<Collector>,
        lanes: Lanes,
        producer: Option<ProducerHandle>,
        /// The producer's consumer channel, as the scheduler would hold it.
        feed: kanal::Sender<SegmentMessage>,
        /// The pipeline's registration channel: a restarted producer registers
        /// again, exactly as one that the push rule stopped does.
        slot: kanal::Sender<ConsumerRegistration>,
        registrations: kanal::Receiver<ConsumerRegistration>,
        arena: Rc<ArenaSet>,
    }

    impl Harness {
        fn new(position_ms: u64, rate: f32, buffer_ms: u32, budget: usize) -> Self {
            let key = StreamKey::new(1, None);
            let mut lanes = Lanes::default();
            lanes.open(
                key.clone(),
                StreamPlayback {
                    position: PlaybackPosition::Absolute {
                        pts_ms: position_ms,
                    },
                    rate,
                    buffer_ms,
                },
                position_ms,
                0,
                Instant::now(),
                0,
            );
            let handle = lanes.handle(&key).expect("the lane opened");
            let (slot, registrations) = kanal::unbounded();
            let consumer = LaneConsumer {
                slot: slot.clone(),
                wake: ConsumerWake::new(),
            };
            let collector = Collector::with_budget(budget);
            let producer = LaneProducer::spawn(key, handle, consumer, Rc::clone(&collector), 0)
                .expect("the producer registers");
            let registration = registrations
                .try_recv()
                .expect("the registration is queued")
                .expect("one registration");
            Self {
                collector,
                lanes,
                producer: Some(producer),
                feed: registration.tx,
                slot,
                registrations,
                arena: Rc::new(ArenaSet::new()),
            }
        }

        fn handle(&self) -> LaneHandle {
            self.lanes.handle(&StreamKey::new(1, None)).expect("lane")
        }

        fn segment(&self, bytes: &[u8]) {
            self.segment_at(bytes, 1, 0);
        }

        fn segment_at(&self, bytes: &[u8], sequence_num: u64, timestamp_ms: u64) {
            let segment = spooled_at(&self.arena, bytes, sequence_num, timestamp_ms);
            let _ = self.feed.try_send(SegmentMessage::Segment(segment));
        }

        /// The `EXT-X-MAP` segment, as the scheduler delivers it before any
        /// media that needs it.
        fn init(&self, bytes: &[u8]) {
            let wakes = WakeSet::new();
            let (mut writer, reader) = Spool::open(&self.arena, &wakes).expect("spool");
            write(&mut writer, bytes);
            drop(writer);
            let _ = self.feed.try_send(SegmentMessage::Init { reader });
        }

        /// Stops the producer and starts another, the way a seek does: the lane
        /// is placed at `start` and a fresh producer takes it from the handle.
        fn restart(&mut self, start: Start) {
            self.producer.take();
            let key = StreamKey::new(1, None);
            let handle = self.handle();
            handle.begin_at(start, 0, Instant::now());
            let consumer = LaneConsumer {
                slot: self.slot.clone(),
                wake: ConsumerWake::new(),
            };
            let producer =
                LaneProducer::spawn(key, handle, consumer, Rc::clone(&self.collector), 0)
                    .expect("the producer registers again");
            let registration = self
                .registrations
                .try_recv()
                .expect("the registration is queued")
                .expect("one registration");
            self.feed = registration.tx;
            self.producer = Some(producer);
        }

        fn stop(&mut self) {
            self.producer.take();
        }
    }

    /// Drives the reactor until `done`, so a task can make progress.
    async fn until(mut done: impl FnMut() -> bool) {
        for _ in 0..400 {
            if done() {
                return;
            }
            compio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the lane did not settle");
    }

    fn ts_fixture() -> Option<Vec<u8>> {
        fixture("servermock/live/000000.ts").map(|path| std::fs::read(path).expect("fixture"))
    }

    /// The priority rule: audio is never demoted, and an edge lane's video
    /// outranks a catch-up lane's.
    #[test]
    fn priority_is_audio_and_the_edge_first() {
        assert_eq!(Priority::of(Class::Audio, false), Priority::High);
        assert_eq!(Priority::of(Class::Audio, true), Priority::High);
        assert_eq!(Priority::of(Class::Video, true), Priority::High);
        assert_eq!(Priority::of(Class::Video, false), Priority::Low);
    }

    /// The push rule, at the producer: with a buffer far below the segment's
    /// length it stops inside the segment instead of reading all of it, and it
    /// never leaves more than the buffer ahead of the playhead.
    #[test]
    fn the_producer_stops_at_the_clients_buffer() {
        let Some(bytes) = ts_fixture() else {
            println!("captures/ is absent; skipping the push-rule test");
            return;
        };
        let total = SegmentSource::open(&bytes, None)
            .expect("the fixture demuxes")
            .access_units()
            .len();
        let runtime = compio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            // A 2 s segment and the smallest buffer the client may ask for.
            let mut harness = Harness::new(0, 1.0, 1_000, 1_182);
            harness.segment(&bytes);
            let handle = harness.handle();
            until(|| handle.sent_ms() > 0).await;
            until(|| handle.sent_ms() >= 900).await;
            compio::time::sleep(Duration::from_millis(100)).await;
            let now = Instant::now();
            assert!(
                handle.buffered_ahead_ms(now) <= u64::from(handle.buffer_ms()),
                "sent {} ms, playhead {} ms, buffer {}",
                handle.sent_ms(),
                handle.playhead_ms(now),
                handle.buffer_ms()
            );
            // The producer stopped *inside* the segment: what was left of it
            // was never read, so fewer access units were offered than it holds.
            let offered = harness.collector.offered();
            let mut ids: Vec<u64> = offered
                .iter()
                .map(|(_, datagram)| {
                    Object::decode_datagram(datagram)
                        .expect("a valid object header")
                        .obj_id
                })
                .collect();
            ids.dedup();
            assert_ne!(ids.len(), 0, "the lane sent something");
            assert!(
                ids.len() < total,
                "the producer packed {} of {total} access units, so it did not stop",
                ids.len()
            );
            harness.stop();
        });
    }

    /// An access unit the playhead has already passed is dropped whole and
    /// counted; nothing of it is sent.
    #[test]
    fn a_playhead_past_the_segment_drops_it() {
        let Some(bytes) = ts_fixture() else {
            println!("captures/ is absent; skipping the supersede test");
            return;
        };
        let runtime = compio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            // The client is 10 s into the broadcast; this segment starts at 0.
            let mut harness = Harness::new(10_000, 1.0, 30_000, 1_182);
            harness.segment(&bytes);
            let handle = harness.handle();
            until(|| handle.dropped_video() > 0).await;
            until(|| handle.dropped_audio() > 0).await;
            assert_eq!(
                harness.collector.count(),
                0,
                "a superseded access unit is not sent at all"
            );
            let view = harness
                .lanes
                .view(&StreamKey::new(1, None), Instant::now(), 0)
                .expect("lane");
            assert!(view.dropped_video > 0);
            assert!(view.recovering, "a video drop now is a smear now");
            assert_eq!(view.buffered_ahead_ms, 0, "nothing was pushed");
            harness.stop();
        });
    }

    /// An fMP4 lane cannot demux a media segment without its init segment, and
    /// with it the access units are the same objects as MPEG-TS's.
    #[test]
    fn an_fmp4_lane_uses_the_init_segment_it_was_sent() {
        let Some(dir) = fixture("servermock-fmp4/landscape") else {
            println!("captures/ is absent; skipping the fMP4 lane test");
            return;
        };
        let init = std::fs::read(dir.join("init.mp4")).expect("init fixture");
        let bytes = std::fs::read(dir.join("live/000000.m4s")).expect("segment fixture");
        let runtime = compio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let mut harness = Harness::new(0, 1.0, 30_000, 1_182);
            // Media before the map is refused, not guessed at.
            harness.segment(&bytes);
            let handle = harness.handle();
            compio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(handle.sent_ms(), 0);
            assert_eq!(harness.collector.count(), 0);

            harness.init(&init);
            harness.segment(&bytes);
            until(|| handle.sent_ms() > 0).await;
            let offered = harness.collector.offered();
            let objects: Vec<_> = offered
                .iter()
                .map(|(_, datagram)| {
                    Object::decode_datagram(datagram).expect("a valid object header")
                })
                .collect();
            assert!(
                objects.iter().any(|object| object.class == Class::Video
                    && object
                        .flags
                        .contains(siphon_shared::riffle::Flags::KEYFRAME)),
                "the fMP4 segment opens with an IRAP"
            );
            assert!(objects.iter().any(|object| object.class == Class::Audio));
            harness.stop();
        });
    }

    /// A seek backwards restarts the producer at the segment that holds the
    /// target, not at the point it had already reached: the first video access
    /// unit it offers afterward is that segment's keyframe.
    #[test]
    fn a_seek_backwards_restarts_the_producer_at_the_containing_segment() {
        let Some(dir) = fixture("servermock-fmp4/landscape") else {
            println!("captures/ is absent; skipping the seek restart test");
            return;
        };
        let init = std::fs::read(dir.join("init.mp4")).expect("init fixture");
        let first = std::fs::read(dir.join("live/000000.m4s")).expect("segment fixture");
        let second = std::fs::read(dir.join("live/000001.m4s")).expect("segment fixture");
        // The first video access unit of the first segment, in the container's
        // own clock: what the restarted producer must reproduce exactly.
        let expected = SegmentSource::open(&first, Some(&init))
            .expect("the fixture demuxes")
            .access_units()
            .find(|unit| unit.track == Track::Video)
            .expect("the segment has video")
            .pts;
        let runtime = compio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            // Play both segments to the end, so the lane's sent position is
            // past the seek target.
            let mut harness = Harness::new(0, 1.0, 30_000, 1_182);
            harness.init(&init);
            harness.segment_at(&first, 1, 0);
            let handle = harness.handle();
            until(|| handle.sent_ms() >= 1_900).await;
            harness.segment_at(&second, 2, 2_000);
            until(|| handle.sent_ms() >= 3_900).await;
            // Let the old producer see the stop: nothing it still holds may
            // arrive after the baseline and be mistaken for the new one's.
            harness.stop();
            until(|| harness.feed.is_closed()).await;
            let before = harness.collector.count();

            // Seek to the first segment's keyframe. The scheduler re-sends the
            // map and replays the window, as it does for a paused lane.
            let keyframe = SegmentSource::open(&first, Some(&init))
                .expect("the fixture demuxes")
                .seek_keyframe(0, 0)
                .expect("a keyframe");
            harness.restart(Start {
                sequence: 1,
                pts_ms: keyframe,
            });
            harness.init(&init);
            harness.segment_at(&first, 1, 0);
            harness.segment_at(&second, 2, 2_000);
            until(|| harness.collector.count() > before).await;

            // The first video object after the restart is the one the seek
            // named, not the one the lane had reached.
            let offered = harness.collector.offered();
            let (pts, _dts) = offered[before..]
                .iter()
                .find_map(|(_, datagram)| {
                    let object = Object::decode_datagram(datagram).expect("a valid object header");
                    (object.class == Class::Video && object.frag_offset == 0)
                        .then(|| crate::media::wire::timestamps(object.payload))
                        .flatten()
                })
                .expect("the restarted producer offers video");
            assert_eq!(pts, expected, "the seek segment's keyframe opens it");
            assert_eq!(
                handle
                    .playback()
                    .expect("the lane has playback")
                    .position_ms(),
                keyframe,
                "the estimate moved to the keyframe with the producer"
            );
            harness.stop();
        });
    }

    /// No datagram is ever bigger than the sink's budget, however large the
    /// access unit: this is the constant the packer must not use.
    #[test]
    fn no_fragment_exceeds_the_budget_it_was_given() {
        let Some(bytes) = ts_fixture() else {
            println!("captures/ is absent; skipping the budget test");
            return;
        };
        let runtime = compio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            // Smaller than any real path, so the packer has to fragment more
            // than the reference budget would.
            let budget = 300;
            let mut harness = Harness::new(0, 1.0, 30_000, budget);
            harness.segment(&bytes);
            let handle = harness.handle();
            until(|| handle.sent_ms() > 500).await;
            let offered = harness.collector.offered();
            assert_ne!(offered.len(), 0, "the lane offered datagrams");
            let mut fragmented = false;
            for (_, datagram) in &offered {
                assert!(
                    datagram.len() <= budget,
                    "a datagram is {} bytes, past the {budget} byte budget",
                    datagram.len()
                );
                let object = Object::decode_datagram(datagram).expect("a valid object header");
                if object.frag_offset > 0 {
                    fragmented = true;
                }
                assert_eq!(
                    object.payload.len() + DATAGRAM_HEADER,
                    datagram.len(),
                    "the header and the payload are the whole datagram"
                );
            }
            assert!(
                fragmented,
                "an access unit larger than the budget must fragment"
            );
            harness.stop();
        });
    }

    /// Dropping the handle ends the producer and gives the scheduler its
    /// channel back, which is how a lane is unregistered — never because a
    /// queue was full.
    #[test]
    fn dropping_the_handle_stops_the_producer() {
        let Some(bytes) = ts_fixture() else {
            println!("captures/ is absent; skipping the teardown test");
            return;
        };
        let runtime = compio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let mut harness = Harness::new(0, 1.0, 30_000, 1_182);
            harness.segment(&bytes);
            let handle = harness.handle();
            until(|| harness.collector.count() > 0).await;

            let sent = harness.collector.count();
            harness.stop();
            // The producer exits on the closed channel and drops its receiver,
            // which is what the scheduler's `try_send` then sees.
            until(|| harness.feed.is_closed()).await;
            let more = harness.collector.count();
            assert_eq!(more, sent, "a stopped producer offers nothing else");
            assert_eq!(handle.dropped_video(), 0, "the teardown is not a drop");
        });
    }
}
