//! Complete-broadcast disk recording.
//!
//! One broadcast becomes one directory with two append-only files:
//! `<output>/<stream_id>/segs` (media) and `index` (v4 records, mirrored by
//! `index.hexpat`). The index read in order *is* the archive; the files are only
//! ever written forward, plus the one hole a run leaves behind.
//!
//! # Two sources, one file
//!
//! - **Live** — the scheduler's consumer fan-out. It starts immediately and is
//!   the only source that exists if no DVR playlist is ever published. It writes
//!   numbered *runs* (`live.NNN.{segs,index}`), splitting one at every timestamp
//!   gap (an ad, a reconnect) and at every map change.
//! - **DVR** — the archive playlist. It has the content the live playlist
//!   replaced with ads and the part before the client joined, in whole ~10 s
//!   chunks, and it owns the canonical `segs`/`index`.
//!
//! `segs` stays in timeline order, so a run's span inside it is a *hole*: the
//! DVR writes whole chunks only, so it writes the chunk that crosses the run's
//! start, parks at that boundary, and once the run closes it reserves a hole for
//! the run's *uncovered middle*. A detached copy moves those bytes in while the
//! DVR moves on; the DVR never waits for a copy. The run's leading and trailing
//! records are trimmed because the DVR's own chunks already supply them.
//!
//! The last run is the handoff: the DVR reaching the run that is still recording
//! drops the live recorder, that run becomes the final hole, and the DVR follows
//! the publication edge alone.
//!
//! # What makes a crash survivable
//!
//! 1. `segs` bytes are durable before a record points at them, and a hole's
//!    `Run` marker is durable before the DVR writes past it.
//! 2. A run pair is deleted only after its `Run` marker is complete, so the
//!    source of every pending hole is still on disk when resume looks for it.
//! 3. The marker carries the copy's source and destination, so a pending hole is
//!    redone in place without a progress cursor. A missing pair degrades that one
//!    hole into an abandoned (zero-filled) one.
//! 4. Each record keeps its own source's sequence number. Nothing maps DVR
//!    numbers to live ones, because Twitch's numbering is not recoverable.
//! 5. `pdt_ms` is the only absolute clock and the only alignment key: the trim is
//!    by PDT, and `pdt - elapsed` must agree across the two sources within a
//!    second, or the hole is abandoned rather than silently spliced.
//!
//! Control state is one [`Phase`]: whether there is a canonical, where the DVR
//! cursor is, and whether the live recorder may still write.
//!
//! On-disk format: `index.hexpat`. Design and rationale:
//! `notes/disk-complete-recording.md`.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use compio::buf::IoBuf;
use compio::fs::File;
use compio::fs::OpenOptions;
use compio::io::AsyncWriteAtExt;
use futures_util::future::Either;
use futures_util::future::select;
use futures_util::pin_mut;
use kanal::AsyncReceiver;
use kanal::AsyncSender;
use kanal::Receiver;
use kanal::Sender;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;
use xxhash_rust::xxh3::Xxh3;
use xxhash_rust::xxh3::xxh3_128;

use self::index::HEADER_LEN;
use self::index::INDEX_VERSION;
use self::index::InitRecord;
use self::index::RUN_BODY;
use self::index::RUN_COMPLETE_OFFSET;
use self::index::Record;
use self::index::RunRecord;
use self::index::SEGMENT_BODY;
use self::index::SOURCE_DVR;
use self::index::SOURCE_LIVE;
use self::index::SegmentRecord;
use self::io::append_live_segment;
use self::io::copy_between;
use self::io::fetch_map;
use self::io::read_exact_at;
use self::io::read_reader_bytes;
use self::io::read_run_init;
use self::io::spawn_copy;
use self::io::spawn_fetch;
use self::io::write_discontinuities;
use self::io::write_init;
use self::io::write_init_bytes;
use self::io::write_record;
use self::plan::Clock;
use self::plan::DiscontinuityMark;
use self::plan::HolePlan;
use self::plan::RunRecordMeta;
use self::plan::chunk_time_of;
use self::plan::hole_end;
use self::plan::origin_of;
use self::plan::plan_hole;
use self::plan::record_duration;
use self::plan::record_time;
use self::resume::create_canonical;
use self::resume::create_pair;
use self::resume::ms_u32;
use self::resume::open_canonical;
use self::resume::read_canonical;
use self::resume::rename_canonical_to_run;
use self::resume::run_paths;
use self::resume::scan_run_number;
use crate::downloader::FetchOutcome;
use crate::downloader::ToDownloader;
use crate::http::Client;
use crate::metrics::DiskMetrics;
use crate::notify::Notifier;
use crate::rtsp::dvr::Dvr;
use crate::rtsp::dvr::DvrSegment;
use crate::rtsp::dvr::DvrTimeline;
use crate::scheduler::ConsumerRegistration;
use crate::scheduler::Segment;
use crate::scheduler::SegmentMessage;
use crate::spool::ArenaSet;
use crate::spool::ConsumerWake;
use crate::spool::SlotView;
use crate::spool::WakeSet;

/// A jump larger than this between consecutive live records is a gap (an ad or
/// a reconnect), never normal timestamp jitter.
const GAP_TOLERANCE_MS: u64 = 250;
/// The broadcast's `pdt - elapsed` origin must agree within this tolerance, or
/// the two sources are not talking about the same content.
const ORIGIN_TOLERANCE_MS: i64 = 1_000;
/// How long discovery may take, and how long after the broadcast start it may
/// keep looking (a DVR playlist is not published immediately).
const DISCOVERY_WAIT: Duration = Duration::from_secs(15);
const DISCOVERY_AFTER_START: Duration = Duration::from_secs(30);
/// No DVR progress for this long means the recording cannot advance.
const DVR_STALL: Duration = Duration::from_secs(60);
/// Bytes read/written per copy block.
const COPY_BLOCK: usize = 256 * 1024;
/// Cap on an `EXT-X-MAP` / live init segment. Real inits are a few KiB; this is
/// only a guard against a wrong URL returning a whole segment or a playlist.
const MAX_INIT_BYTES: usize = 8 * 1024 * 1024;
mod index;
mod io;
mod plan;
mod resume;

#[cfg(test)]
mod tests;

#[derive(Debug, thiserror::Error)]
#[allow(variant_size_differences)]
pub enum DiskOutputError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid index header (want v{INDEX_VERSION})")]
    InvalidHeader,
    #[error("invalid index record: {0}")]
    Record(String),
    #[error("http: {0}")]
    Http(#[from] crate::http::Error),
}

impl DiskOutputError {
    /// A stale/incompatible index is the only error that exits the process; a
    /// full disk or a failed fetch stops the disk subsystem and keeps RTSP.
    const fn fatal(&self) -> bool {
        matches!(self, Self::InvalidHeader)
    }
}

enum DiskOutcome {
    Done,
    Fatal(String),
}

/// Options for one broadcast's disk output.
pub struct DiskOptions {
    pub output: PathBuf,
    pub stream_id: u64,
    pub keep_runs: bool,
    pub notifier: Option<Notifier>,
    pub dvr: Rc<Dvr>,
    /// Fetches `EXT-X-MAP` objects; the DVR media itself goes through the
    /// downloader queue.
    pub client: Rc<Client>,
    /// Read by `GetDisk`.
    pub metrics: Rc<DiskMetrics>,
}

/// Handle to the recording task. `wait` resolves when the disk subsystem stops;
/// `Some` carries a fatal error that must exit the process.
pub struct DiskOutput {
    stop: Sender<()>,
    done: Receiver<DiskOutcome>,
}

impl DiskOutput {
    pub fn stop(&self) {
        let _ = self.stop.try_send(());
    }

    pub async fn wait(&self) -> Option<String> {
        let done = self.done.clone().to_async();
        match done.recv().await {
            Ok(DiskOutcome::Fatal(error)) => Some(error),
            _ => None,
        }
    }
}

pub fn spawn_disk_output(
    options: DiskOptions,
    wakes: &Rc<WakeSet>,
    downloaders: &AsyncSender<ToDownloader>,
    add_output: &Sender<ConsumerRegistration>,
) -> DiskOutput {
    let (live_tx, live_rx) = kanal::unbounded();
    let (stop_tx, stop_rx) = kanal::bounded(1);
    let (done_tx, done_rx) = kanal::bounded(1);

    let registration = ConsumerRegistration {
        tx: live_tx,
        // The last ~36 s are the only part of the pre-join broadcast the live
        // source can still see, so record them before the DVR is known.
        backlog: true,
        lossy: false,
    };
    if let Err(error) = add_output.send(registration) {
        error!(%error, "failed to register disk output");
    }

    let live_wake = ConsumerWake::new();
    wakes.register(&live_wake);

    let downloaders = downloaders.clone();
    let metrics = Rc::clone(&options.metrics);
    let notifier = options.notifier.clone();
    compio::runtime::spawn(async move {
        metrics.active.set(true);
        let result = run_disk(options, live_rx, stop_rx, &downloaders, live_wake).await;
        metrics.active.set(false);
        if let Err(error) = &result {
            *metrics.last_error.borrow_mut() = Some(error.to_string());
        }
        match result {
            Ok(()) => {
                let _ = done_tx.send(DiskOutcome::Done);
            }
            Err(error) => {
                error!(?error, "disk output stopped");
                // Stopping for any reason other than "we are done" contradicts
                // the expectation that the archive is being written.
                if let Some(notifier) = notifier {
                    notifier.notify(format!("disk: recording stopped: {error}"));
                }
                let outcome = if error.fatal() {
                    DiskOutcome::Fatal(error.to_string())
                } else {
                    DiskOutcome::Done
                };
                let _ = done_tx.send(outcome);
            }
        }
    })
    .detach();

    DiskOutput {
        stop: stop_tx,
        done: done_rx,
    }
}

// Disk state

/// What the live recorder is doing. Orthogonal to where the DVR cursor is: the
/// two together are the whole control state of the recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Live {
    /// Segments are still arriving and are recorded into runs.
    Recording,
    /// The DVR reached the run that is still recording: the main loop drains and
    /// closes it, then the DVR continues past it.
    Draining,
    /// Nothing more will come from the live playlist.
    Done,
}

/// A run the DVR cursor cannot pass: its hole is only sized once the run closes.
/// Holds the chunk that crosses the run's start, whose `pdt - start` is the
/// other side of the alignment check.
#[derive(Clone, Debug)]
struct Parked {
    pub(super) run: u32,
    /// Time the DVR resumes at once the run's hole is reserved.
    pub(super) hole_start: u64,
    pub(super) crossing: DvrSegment,
}

/// Where the recording is in its life. Every combination the old flags allowed
/// but never meant (a handoff with no parked run, a canonical with no DVR) is
/// unrepresentable here.
#[derive(Clone, Debug)]
enum Phase {
    /// Discovery in progress: live segments go into runs and there is no
    /// canonical yet.
    Discovering,
    /// The canonical is the DVR file.
    Dvr { live: Live, parked: Option<Parked> },
    /// No DVR ever appeared: the canonical is the live stream directly.
    Promoted,
}

impl Phase {
    /// The DVR file owns the canonical and nothing is parked.
    const fn following() -> Self {
        Self::Dvr {
            live: Live::Recording,
            parked: None,
        }
    }

    const fn dvr(&self) -> bool {
        matches!(self, Self::Dvr { .. })
    }

    const fn promoted(&self) -> bool {
        matches!(self, Self::Promoted)
    }

    /// The live side's state; a non-DVR recording only ever records.
    const fn live(&self) -> Live {
        match self {
            Self::Dvr { live, .. } => *live,
            Self::Discovering | Self::Promoted => Live::Recording,
        }
    }

    const fn parked(&self) -> Option<&Parked> {
        match self {
            Self::Dvr { parked, .. } => parked.as_ref(),
            Self::Discovering | Self::Promoted => None,
        }
    }
}

type MapKey = (String, Option<(u64, u64)>);
type MapCache = HashMap<MapKey, (u128, Vec<u8>)>;

struct Canonical {
    pub(super) segs: File,
    pub(super) index: File,
    pub(super) segs_end: u64,
    pub(super) index_end: u64,
    pub(super) init_counter: u64,
    /// Hash of the `Init` record currently in effect at the tail.
    pub(super) active_init: Option<u128>,
}

struct RunData {
    pub(super) number: u32,
    pub(super) segs_path: PathBuf,
    pub(super) index_path: PathBuf,
    pub(super) records: Vec<RunRecordMeta>,
    pub(super) segs_len: u64,
    pub(super) index_len: u64,
    pub(super) init_counter: u64,
    /// The run's leading `EXT-X-MAP`, if the live playlist had one. A map change
    /// closes the run, so a run has at most one init and it is always first.
    pub(super) init: Option<InitRecord>,
    /// `Some` only for the run currently being recorded.
    pub(super) files: Option<(File, File)>,
    pub(super) first_segment: Option<SegmentRecord>,
    pub(super) last_segment: Option<SegmentRecord>,
}

impl RunData {
    fn first_time(&self, clock: Clock, live_origin: Option<i64>) -> Option<u64> {
        self.first_segment
            .as_ref()
            .map(|record| record_time(record, clock, live_origin))
    }

    fn last_end(&self, clock: Clock, live_origin: Option<i64>) -> Option<u64> {
        self.last_segment.as_ref().map(|record| {
            record_time(record, clock, live_origin).saturating_add(record_duration(record))
        })
    }

    fn origin(&self) -> Option<i64> {
        self.first_segment
            .as_ref()
            .and_then(|record| origin_of(record.pdt_ms, u64::from(record.elapsed_ms)))
    }
}

struct InFlightChunk {
    pub(super) chunk: DvrSegment,
    pub(super) dst_offset: u64,
    pub(super) written: u64,
    pub(super) hasher: Xxh3,
}

struct CopyJob {
    pub(super) run: u32,
    pub(super) dst_segs: u64,
    pub(super) dst_index: u64,
    pub(super) segs_len: u64,
    pub(super) records: Vec<u8>,
    pub(super) written: u64,
    pub(super) segs_path: PathBuf,
    pub(super) index_path: PathBuf,
}

#[allow(clippy::struct_excessive_bools)] // a state machine, not flags of a data type
struct Disk {
    pub(super) options: DiskOptions,
    pub(super) dir: PathBuf,
    pub(super) client: Rc<Client>,
    pub(super) live_rx: Option<AsyncReceiver<SegmentMessage>>,
    pub(super) events_rx: AsyncReceiver<Event>,
    pub(super) events_tx: Sender<Event>,
    pub(super) downloaders: AsyncSender<ToDownloader>,
    pub(super) live_wake: Rc<ConsumerWake>,
    pub(super) dvr: Rc<Dvr>,
    pub(super) dvr_arena: Rc<ArenaSet>,
    pub(super) dvr_wakes: Rc<WakeSet>,
    /// `(uri, byterange)` → `(init hash, bytes)`, so an unchanged map is fetched
    /// once and can still be inserted if the canonical's active map changes.
    pub(super) map_cache: MapCache,

    /// The whole control state: what the canonical is, where the DVR cursor is
    /// and what the live recorder may still do.
    pub(super) phase: Phase,
    pub(super) canonical: Option<Canonical>,
    pub(super) current_run: Option<RunData>,
    pub(super) closed_runs: VecDeque<RunData>,
    pub(super) next_run: u32,
    pub(super) live_edge: Option<(u64, u64, bool)>, // (start, end, pdt domain)
    pub(super) live_origin: Option<i64>,
    pub(super) live_open: bool,
    /// Live-playlist discontinuities waiting for the archive to reach them.
    pub(super) discontinuities: Vec<DiscontinuityMark>,
    /// A `Discontinuity` tag arrived; the next live segment supplies its time.
    pub(super) pending_discontinuity: bool,

    pub(super) clock: Option<Clock>,
    pub(super) dvr_origin: Option<i64>,
    pub(super) dvr_next_start_ms: u64,
    pub(super) resume_abs: Option<u64>,
    pub(super) resume_clock: Option<Clock>,
    pub(super) in_flight: Option<InFlightChunk>,
    pub(super) copies: Vec<CopyJob>,
    pub(super) dvr_stall: u64,
    pub(super) stall_armed: Option<u64>,
    pub(super) dvr_seen_url: bool,

    pub(super) stop: bool,
    pub(super) finished: bool,
}

enum Event {
    ChunkPart(SlotView),
    ChunkDone(FetchOutcome),
    CopyPart { run: u32, data: Vec<u8> },
    CopyDone { run: u32 },
    CopyFailed { run: u32, error: String },
    DvrUpdate,
    DiscoveryDeadline,
    DvrStall(u64),
    Stop,
}

async fn run_disk(
    options: DiskOptions,
    live_rx: Receiver<SegmentMessage>,
    stop_rx: Receiver<()>,
    downloaders: &AsyncSender<ToDownloader>,
    live_wake: Rc<ConsumerWake>,
) -> Result<(), DiskOutputError> {
    let dir = options.output.join(options.stream_id.to_string());
    compio::fs::create_dir_all(&dir).await?;

    let (events_tx, events_rx) = kanal::unbounded();
    let dvr = Rc::clone(&options.dvr);
    let client = Rc::clone(&options.client);
    let disk = Disk {
        options,
        dir,
        client,
        live_rx: Some(live_rx.to_async()),
        events_rx: events_rx.to_async(),
        events_tx: events_tx.clone(),
        downloaders: downloaders.clone(),
        live_wake,
        dvr: Rc::clone(&dvr),
        dvr_arena: Rc::new(ArenaSet::new()),
        dvr_wakes: WakeSet::new(),
        map_cache: HashMap::new(),
        phase: Phase::Discovering,
        canonical: None,
        current_run: None,
        closed_runs: VecDeque::new(),
        next_run: 0,
        live_edge: None,
        live_origin: None,
        live_open: true,
        discontinuities: Vec::new(),
        pending_discontinuity: false,
        clock: None,
        dvr_origin: None,
        dvr_next_start_ms: 0,
        resume_abs: None,
        resume_clock: None,
        in_flight: None,
        copies: Vec::new(),
        dvr_stall: 0,
        stall_armed: None,
        dvr_seen_url: false,
        stop: false,
        finished: false,
    };

    spawn_stop_proxy(stop_rx, events_tx.clone());
    spawn_dvr_watcher(Rc::clone(&dvr), events_tx.clone());
    spawn_discovery_timer(&dvr, events_tx.clone());

    let mut disk = disk;
    disk.startup().await?;
    disk.run().await
}

fn spawn_stop_proxy(stop_rx: Receiver<()>, events: Sender<Event>) {
    compio::runtime::spawn(async move {
        let stop_rx = stop_rx.to_async();
        let _ = stop_rx.recv().await;
        let _ = events.send(Event::Stop);
    })
    .detach();
}

fn spawn_dvr_watcher(dvr: Rc<Dvr>, events: Sender<Event>) {
    compio::runtime::spawn(async move {
        let wake = ConsumerWake::new();
        dvr.subscribe(&wake);
        let mut seen = dvr.generation();
        loop {
            WaitGeneration {
                dvr: &dvr,
                wake: &wake,
                seen,
            }
            .await;
            seen = dvr.generation();
            if events.send(Event::DvrUpdate).is_err() {
                return;
            }
        }
    })
    .detach();
}

struct WaitGeneration<'a> {
    pub(super) dvr: &'a Dvr,
    pub(super) wake: &'a ConsumerWake,
    pub(super) seen: u64,
}

impl Future for WaitGeneration<'_> {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.dvr.generation() == self.seen {
            self.wake.park(context);
        }
        if self.dvr.generation() == self.seen {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    }
}

fn spawn_discovery_timer(dvr: &Rc<Dvr>, events: Sender<Event>) {
    if dvr.url().is_some() {
        return;
    }
    let wait = discovery_wait(dvr);
    compio::runtime::spawn(async move {
        compio::time::sleep(wait).await;
        let _ = events.send(Event::DiscoveryDeadline);
    })
    .detach();
}

fn discovery_wait(dvr: &Dvr) -> Duration {
    let now = now_ms();
    let mut wait = DISCOVERY_WAIT;
    if let Some(start) = dvr.broadcast_start_ms() {
        let after_start = start
            .saturating_add(millis_u64(DISCOVERY_AFTER_START))
            .saturating_sub(now);
        wait = wait.max(Duration::from_millis(after_start));
    }
    wait
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn block_take(remaining: u64) -> usize {
    usize::try_from(remaining.min(COPY_BLOCK as u64)).unwrap_or(COPY_BLOCK)
}

/// Spawn a one-shot stall watchdog. The generation makes a later progress
/// invalidate it without keeping a timer handle around.
fn spawn_stall_timer(generation: u64, events: Sender<Event>) {
    compio::runtime::spawn(async move {
        compio::time::sleep(DVR_STALL).await;
        let _ = events.send(Event::DvrStall(generation));
    })
    .detach();
}

impl Disk {
    fn notify(&self, message: impl Into<String>) {
        if let Some(notifier) = &self.options.notifier {
            notifier.notify(message);
        }
    }

    fn notify_error(&self, message: impl Into<String>) {
        let message = message.into();
        warn!("{message}");
        self.notify(message);
    }

    /// Resume, or start from nothing: the index decides which of the note's
    /// five resume cases applies.
    async fn startup(&mut self) -> Result<(), DiskOutputError> {
        let has_url = self.dvr.url().is_some();
        let snapshot =
            read_canonical(&self.dir, self.options.stream_id, self.options.keep_runs).await?;

        let Some(snapshot) = snapshot else {
            self.next_run = scan_run_number(&self.dir);
            if has_url {
                // Discovery won the race with the disk task (debug, or a resume
                // whose URL is already known): start the canonical immediately.
                let canonical = create_canonical(&self.dir, self.options.stream_id).await?;
                self.enter_dvr(canonical);
            }
            debug!(
                phase = ?self.phase,
                next_run = self.next_run,
                "disk: recording started"
            );
            return Ok(());
        };

        self.next_run = snapshot.next_run;
        self.live_origin = snapshot.live_origin;
        if snapshot.has_dvr {
            // DVR records: the canonical is the DVR file. Without a URL the
            // discovery deadline decides; DVR -> live is not automatic.
            self.resume_abs = Some(snapshot.resume_abs);
            self.resume_clock = snapshot.resume_clock;
            let canonical = open_canonical(&self.dir, &snapshot).await?;
            self.enter_dvr(canonical);
            self.dvr_seen_url = has_url;
        } else if has_url {
            // A promoted live recording that now has a DVR: rename it to a run,
            // start a fresh canonical and prefill from the broadcast start.
            let run = rename_canonical_to_run(&self.dir, self.next_run, &snapshot).await?;
            self.closed_runs.push_back(run);
            self.next_run = self.next_run.saturating_add(1);
            let canonical = create_canonical(&self.dir, self.options.stream_id).await?;
            self.enter_dvr(canonical);
            self.dvr_next_start_ms = 0;
        } else {
            self.canonical = Some(open_canonical(&self.dir, &snapshot).await?);
            self.phase = Phase::Promoted;
            self.live_open = true;
        }

        debug!(
            phase = ?self.phase,
            next_run = self.next_run,
            "disk: recording started"
        );
        Ok(())
    }

    /// Take the DVR file as the canonical from now on.
    fn enter_dvr(&mut self, canonical: Canonical) {
        self.canonical = Some(canonical);
        self.phase = Phase::following();
        self.dvr_seen_url = true;
    }

    async fn run(&mut self) -> Result<(), DiskOutputError> {
        while !self.finished {
            if self.stop {
                self.finish().await?;
                break;
            }

            match self.next_incoming().await {
                Some(Incoming::Live(Ok(message))) => self.on_live(message).await?,
                Some(Incoming::Live(Err(_))) => {
                    self.live_open = false;
                    self.on_live_closed().await?;
                }
                Some(Incoming::Event(Ok(event))) => self.on_event(event).await?,
                Some(Incoming::Event(Err(_))) | None => break,
            }

            if self.phase.live() == Live::Draining {
                self.drain_live().await?;
            }

            self.pump().await?;
            self.check_done();
        }
        Ok(())
    }

    /// Whatever happened next: a live message while the live source is open,
    /// otherwise a disk event. The events channel closing ends the recording.
    async fn next_incoming(&self) -> Option<Incoming> {
        let events = self.events_rx.recv();
        if !self.live_open {
            return Some(Incoming::Event(events.await));
        }
        let Some(live_rx) = &self.live_rx else {
            return Some(Incoming::Event(events.await));
        };
        let live = live_rx.recv();
        pin_mut!(live, events);
        match select(live, events).await {
            Either::Left((message, _)) => Some(Incoming::Live(message)),
            Either::Right((event, _)) => Some(Incoming::Event(event)),
        }
    }

    async fn on_event(&mut self, event: Event) -> Result<(), DiskOutputError> {
        match event {
            Event::ChunkPart(view) => self.on_chunk_part(view).await?,
            Event::ChunkDone(outcome) => self.on_chunk_done(outcome).await?,
            Event::CopyPart { run, data } => self.on_copy_part(run, data).await?,
            Event::CopyDone { run } => self.on_copy_done(run).await?,
            Event::CopyFailed { run, error } => {
                self.notify_error(format!("disk: run {run} copy failed: {error}"));
                return Err(DiskOutputError::Io(std::io::Error::other(error)));
            }
            Event::DvrUpdate => self.on_dvr_update().await?,
            Event::DiscoveryDeadline => self.on_discovery_deadline().await?,
            Event::DvrStall(generation) => {
                if generation == self.dvr_stall {
                    self.notify_error("disk: DVR made no progress; recording stopped");
                    return Err(DiskOutputError::Io(std::io::Error::other("DVR stalled")));
                }
            }
            Event::Stop => self.stop = true,
        }
        Ok(())
    }

    async fn on_dvr_update(&mut self) -> Result<(), DiskOutputError> {
        self.dvr_stall = self.dvr_stall.wrapping_add(1);
        self.stall_armed = None;
        if self.dvr.url().is_none() || self.dvr_seen_url {
            return Ok(());
        }
        self.dvr_seen_url = true;
        if matches!(self.phase, Phase::Discovering) {
            let canonical = create_canonical(&self.dir, self.options.stream_id).await?;
            self.enter_dvr(canonical);
            info!("disk: DVR playlist discovered, switching to the canonical DVR file");
        } else if self.phase.promoted() {
            self.notify(
                "disk: DVR playlist appeared after the live promotion; continuing live-only",
            );
        }
        Ok(())
    }

    async fn on_discovery_deadline(&mut self) -> Result<(), DiskOutputError> {
        if self.dvr.url().is_some() {
            return Ok(());
        }
        // A broadcast that started less than 30 s ago gets 30 s from its start.
        if let Some(start) = self.dvr.broadcast_start_ms() {
            let deadline = start.saturating_add(millis_u64(DISCOVERY_AFTER_START));
            if now_ms() < deadline {
                let wait = Duration::from_millis(deadline.saturating_sub(now_ms()));
                let events = self.events_tx.clone();
                compio::runtime::spawn(async move {
                    compio::time::sleep(wait).await;
                    let _ = events.send(Event::DiscoveryDeadline);
                })
                .detach();
                return Ok(());
            }
        }
        if matches!(self.phase, Phase::Discovering) {
            self.notify(
                "disk: no DVR playlist found; promoting the live recording to the canonical file",
            );
            self.promote().await?;
        } else if self.phase.dvr() {
            self.notify_error("disk: DVR playlist disappeared; recording stopped");
            return Err(DiskOutputError::Io(std::io::Error::other(
                "DVR disappeared",
            )));
        }
        Ok(())
    }

    async fn on_live(&mut self, message: SegmentMessage) -> Result<(), DiskOutputError> {
        if self.phase.live() != Live::Recording {
            return Ok(());
        }
        match message {
            SegmentMessage::Segment(segment) => {
                if self.pending_discontinuity {
                    // The tag precedes the segment it applies to, so this
                    // segment's timestamp is where the new domain starts.
                    self.pending_discontinuity = false;
                    self.discontinuities.push(DiscontinuityMark {
                        pdt_ms: segment.pdt_ms,
                        elapsed_ms: segment.timestamp_ms,
                    });
                }
                if self.phase.promoted() {
                    self.write_promoted(segment).await
                } else {
                    self.write_live(segment).await
                }
            }
            // fMP4 map. TS streams carry none. A map change closes the current
            // run, so every run is self-contained: its init is always leading.
            SegmentMessage::Init { mut reader } => {
                if self.phase.promoted() {
                    let bytes =
                        read_reader_bytes(&mut reader, &self.live_wake, MAX_INIT_BYTES).await?;
                    if let Some(canonical) = &mut self.canonical {
                        write_init_bytes(canonical, SOURCE_LIVE, &bytes).await?;
                    }
                    return Ok(());
                }
                if self
                    .current_run
                    .as_ref()
                    .is_some_and(|run| run.first_segment.is_some())
                {
                    self.close_current_run().await?;
                }
                if self.current_run.is_none() {
                    let number = self.next_run;
                    self.next_run = self.next_run.saturating_add(1);
                    self.open_run(number).await?;
                }
                if let Some(run) = &mut self.current_run
                    && let Some((segs, index)) = &mut run.files
                {
                    let mut counter = run.init_counter;
                    let init = write_init(
                        segs,
                        index,
                        &mut run.segs_len,
                        &mut run.index_len,
                        &mut counter,
                        SOURCE_LIVE,
                        &mut reader,
                        &self.live_wake,
                    )
                    .await?;
                    run.init_counter = counter;
                    run.init = Some(init);
                }
                Ok(())
            }
            SegmentMessage::Discontinuity() => {
                self.pending_discontinuity = true;
                Ok(())
            }
            SegmentMessage::Prefetch { .. } | SegmentMessage::Seek(_) => Ok(()),
        }
    }

    async fn write_promoted(&mut self, segment: Segment) -> Result<(), DiskOutputError> {
        debug!(
            sequence = segment.sequence_num,
            "disk: writing live segment"
        );
        let (pdt, elapsed) = (segment.pdt_ms, segment.timestamp_ms);
        let Some(canonical) = &mut self.canonical else {
            return Ok(());
        };
        // A promoted archive is written in playlist order, so this segment's own
        // time is the exact position of any discontinuity announced before it.
        let clock = if pdt != 0 { Clock::Pdt } else { Clock::Elapsed };
        write_discontinuities(
            canonical,
            &mut self.discontinuities,
            if pdt != 0 { pdt } else { elapsed },
            clock,
            self.live_origin,
        )
        .await?;
        let Some(_) = append_live_segment(
            &mut canonical.segs,
            &mut canonical.index,
            &mut canonical.segs_end,
            &mut canonical.index_end,
            segment,
            &self.live_wake,
            &self.options.metrics,
        )
        .await?
        else {
            return Ok(());
        };
        if let Some(origin) = origin_of(pdt, elapsed) {
            self.live_origin = Some(origin);
        }
        Ok(())
    }

    /// A jump in the live timestamps means content the live playlist never
    /// carried: an ad break or a reconnect. It closes the run.
    const fn is_gap(&self, pdt: u64, elapsed: u64) -> bool {
        let Some((last_start, last_end, pdt_domain)) = self.live_edge else {
            return false;
        };
        let start = if pdt != 0 && pdt_domain { pdt } else { elapsed };
        start > last_end.saturating_add(GAP_TOLERANCE_MS)
            || start.saturating_add(GAP_TOLERANCE_MS) < last_start
    }

    async fn write_live(&mut self, segment: Segment) -> Result<(), DiskOutputError> {
        let pdt = segment.pdt_ms;
        let elapsed = segment.timestamp_ms;
        let duration = segment.duration_ms;

        if self.is_gap(pdt, elapsed) {
            debug!(
                previous = ?self.live_edge,
                sequence = segment.sequence_num,
                "disk: live timestamp gap, splitting the run"
            );
            // An empty run may have just been opened for a new map. Keep it: its
            // init is exactly what the first media after the gap needs.
            if self
                .current_run
                .as_ref()
                .is_some_and(|run| run.first_segment.is_some())
            {
                self.close_current_run().await?;
            }
        }

        if self.current_run.is_none() {
            let number = self.next_run;
            self.next_run = self.next_run.saturating_add(1);
            self.open_run(number).await?;
        }

        let Some(run) = &mut self.current_run else {
            return Ok(());
        };
        let Some((segs, index)) = &mut run.files else {
            return Ok(());
        };
        let record_offset = run.index_len;
        let Some(record) = append_live_segment(
            segs,
            index,
            &mut run.segs_len,
            &mut run.index_len,
            segment,
            &self.live_wake,
            &self.options.metrics,
        )
        .await?
        else {
            // A failed download leaves no bytes. The next segment's timestamp
            // jump closes this run and the DVR fills the missing span.
            return Ok(());
        };
        run.records.push(RunRecordMeta {
            segment: record.clone(),
            record_offset,
            record_len: 1 + SEGMENT_BODY,
        });
        run.first_segment.get_or_insert_with(|| record.clone());
        run.last_segment = Some(record);

        if let Some(origin) = origin_of(pdt, elapsed) {
            self.live_origin = Some(origin);
        }
        let start = if pdt != 0 { pdt } else { elapsed };
        self.live_edge = Some((start, start.saturating_add(duration), pdt != 0));
        Ok(())
    }

    async fn open_run(&mut self, number: u32) -> Result<(), DiskOutputError> {
        let (segs_path, index_path) = run_paths(&self.dir, number);
        let (segs, index) = create_pair(&segs_path, &index_path, self.options.stream_id).await?;

        self.options
            .metrics
            .runs
            .set(self.options.metrics.runs.get().saturating_add(1));
        debug!(run = number, "disk: opened live run");
        self.current_run = Some(RunData {
            number,
            segs_path,
            index_path,
            records: Vec::new(),
            segs_len: 0,
            index_len: HEADER_LEN as u64,
            init_counter: 0,
            init: None,
            files: Some((segs, index)),
            first_segment: None,
            last_segment: None,
        });
        Ok(())
    }

    async fn close_current_run(&mut self) -> Result<(), DiskOutputError> {
        let Some(mut run) = self.current_run.take() else {
            return Ok(());
        };
        if let Some((segs, index)) = run.files.take() {
            segs.sync_all().await?;
            index.sync_all().await?;
        }
        // An empty run has nothing to record (the DVR supplies everything).
        if run.first_segment.is_some() {
            debug!(
                run = run.number,
                bytes = run.segs_len,
                records = run.records.len(),
                "disk: closed live run"
            );
            self.closed_runs.push_back(run);
        } else {
            let _ = compio::fs::remove_file(&run.segs_path).await;
            let _ = compio::fs::remove_file(&run.index_path).await;
        }
        Ok(())
    }

    /// Park the DVR cursor at a run's trailing edge.
    fn park(&mut self, parked: Parked) {
        if let Phase::Dvr { parked: slot, .. } = &mut self.phase {
            *slot = Some(parked);
        }
    }

    const fn unpark(&mut self) -> Option<Parked> {
        match &mut self.phase {
            Phase::Dvr { parked, .. } => parked.take(),
            Phase::Discovering | Phase::Promoted => None,
        }
    }

    const fn set_live(&mut self, live: Live) {
        if let Phase::Dvr { live: slot, .. } = &mut self.phase {
            *slot = live;
        }
    }

    async fn drain_live(&mut self) -> Result<(), DiskOutputError> {
        self.set_live(Live::Done);
        // The DVR is authoritative from the live edge on. Anything still queued
        // is content the DVR chunk we just wrote (and the chunks that follow)
        // already supplies, so it is dropped rather than becoming one more run.
        if let Some(rx) = &self.live_rx {
            while matches!(rx.as_sync().try_recv(), Ok(Some(_))) {}
        }
        self.live_open = false;
        // Drop the receiver so the scheduler removes the disk consumer instead
        // of queueing segments nobody reads (which would pin arena slots).
        self.live_rx = None;
        self.close_current_run().await?;
        info!("disk: DVR reached the live edge, live recorder handed off");
        Ok(())
    }

    async fn on_live_closed(&mut self) -> Result<(), DiskOutputError> {
        self.close_current_run().await?;
        if self.phase.promoted() {
            self.finished = true;
        } else if matches!(self.phase, Phase::Discovering) {
            self.promote().await?;
            self.finished = true;
        } else {
            // Keep going until the DVR playlist says ENDLIST and every chunk has
            // been written.
            self.pump().await?;
        }
        Ok(())
    }

    /// No DVR: merge every live run into the canonical file. The runs may have
    /// been split at ads, so their records are concatenated with timestamp
    /// jumps; there is no hole to fill.
    async fn promote(&mut self) -> Result<(), DiskOutputError> {
        self.close_current_run().await?;
        // No DVR ever arrived, so no chunk can place these: run media is
        // concatenated here and a marker would have to be replayed by hand.
        if !self.discontinuities.is_empty() {
            debug!(
                dropped = self.discontinuities.len(),
                "disk: dropping unplaceable discontinuity markers"
            );
            self.discontinuities.clear();
        }
        let mut canonical = create_canonical(&self.dir, self.options.stream_id).await?;
        let runs: Vec<RunData> = self.closed_runs.drain(..).collect();

        for run in &runs {
            let source = OpenOptions::new().read(true).open(&run.segs_path).await?;
            let media_start = run.init.as_ref().map_or(0, |init| {
                init.byte_offset.saturating_add(u64::from(init.byte_length))
            });
            if let Some(init) = &run.init {
                let bytes = read_exact_at(
                    &source,
                    init.byte_offset,
                    usize::try_from(init.byte_length).unwrap_or(usize::MAX),
                )
                .await?;
                if xxh3_128(&bytes) != init.hash {
                    return Err(DiskOutputError::Record(format!(
                        "run {} init failed verification",
                        run.number
                    )));
                }
                write_init_bytes(&mut canonical, SOURCE_LIVE, &bytes).await?;
            }
            let dst_start = canonical.segs_end;
            let media_len = run.segs_len.saturating_sub(media_start);
            copy_between(
                &source,
                media_start,
                &mut canonical.segs,
                dst_start,
                media_len,
            )
            .await?;

            // The run index's byte offsets start at `media_start`; rebasing is
            // the append offset minus that base. A resumed promoted file has the
            // same shape (its init is at the front).
            let first_offset = run
                .records
                .first()
                .map_or(media_start, |meta| meta.segment.byte_offset);
            let delta = i64::try_from(dst_start).unwrap_or(i64::MAX)
                - i64::try_from(first_offset).unwrap_or(0);
            let mut buffer = Vec::new();
            for meta in &run.records {
                let mut record = Record::Segment(meta.segment.clone());
                record.rebase(delta, &mut canonical.init_counter);
                record.encode(&mut buffer);
            }
            canonical
                .index
                .write_all_at(std::mem::take(&mut buffer), canonical.index_end)
                .await
                .0?;
            canonical.index_end += run
                .records
                .iter()
                .map(|meta| u64::from(meta.record_len))
                .sum::<u64>();
            canonical.segs_end += media_len;
        }
        canonical.segs.sync_all().await?;
        canonical.index.sync_all().await?;
        for run in &runs {
            if !self.options.keep_runs {
                let _ = compio::fs::remove_file(&run.segs_path).await;
                let _ = compio::fs::remove_file(&run.index_path).await;
            }
        }
        self.canonical = Some(canonical);
        self.phase = Phase::Promoted;
        info!("disk: promoted the live recording to the canonical file");
        Ok(())
    }

    async fn on_chunk_part(&mut self, view: SlotView) -> Result<(), DiskOutputError> {
        let Some(in_flight) = &mut self.in_flight else {
            return Ok(());
        };
        let len = view.as_init().len() as u64;
        in_flight.hasher.update(view.as_init());
        let Some(canonical) = &mut self.canonical else {
            return Ok(());
        };
        canonical
            .segs
            .write_all_at(view, in_flight.dst_offset + in_flight.written)
            .await
            .0?;
        in_flight.written += len;
        Ok(())
    }

    async fn on_chunk_done(&mut self, outcome: FetchOutcome) -> Result<(), DiskOutputError> {
        let Some(in_flight) = self.in_flight.take() else {
            return Ok(());
        };
        let Some(canonical) = &mut self.canonical else {
            return Ok(());
        };

        if outcome == FetchOutcome::Failed {
            // The bytes before the failure are not a chunk. Drop them and leave a
            // timestamp jump; a failed early/edge chunk is exactly the case the
            // run/hole model expects.
            canonical.segs.set_len(in_flight.dst_offset).await?;
            self.notify_error(format!(
                "disk: DVR chunk {} failed; leaving a gap",
                in_flight.chunk.sequence
            ));
            self.dvr_stall = self.dvr_stall.wrapping_add(1);
            self.stall_armed = None;
            if let Some(parked) = self.unpark()
                && let Some(run) = self.take_run(parked.run)
            {
                // Without the crossing chunk there is no aligned hole; abandon
                // the run and let the DVR's later chunks supply what they can.
                self.abandon_run(run, in_flight.chunk.start_ms, in_flight.chunk.start_ms)
                    .await?;
            }
            self.dvr_next_start_ms = in_flight
                .chunk
                .start_ms
                .saturating_add(in_flight.chunk.duration_ms);
            return Ok(());
        }

        if in_flight.written == 0 {
            self.notify_error(format!(
                "disk: DVR chunk {} was empty; leaving a gap",
                in_flight.chunk.sequence
            ));
            self.dvr_next_start_ms = in_flight
                .chunk
                .start_ms
                .saturating_add(in_flight.chunk.duration_ms);
            return Ok(());
        }

        let clock = self.clock.unwrap_or(Clock::Elapsed);
        let chunk_time = chunk_time_of(&in_flight.chunk, clock, self.dvr_origin);
        write_discontinuities(
            canonical,
            &mut self.discontinuities,
            chunk_time,
            clock,
            self.live_origin,
        )
        .await?;

        let record = SegmentRecord {
            source: SOURCE_DVR,
            sequence: in_flight.chunk.sequence,
            elapsed_ms: ms_u32(in_flight.chunk.start_ms),
            pdt_ms: in_flight.chunk.pdt_ms,
            duration_ms: ms_u32(in_flight.chunk.duration_ms),
            byte_offset: in_flight.dst_offset,
            byte_length: u32::try_from(in_flight.written).unwrap_or(u32::MAX),
            hash: in_flight.hasher.digest128(),
            flags: 0,
        };
        write_record(
            &mut canonical.index,
            canonical.index_end,
            &Record::Segment(record),
        )
        .await?;
        canonical.index_end += u64::from(1 + SEGMENT_BODY);
        canonical.segs_end = in_flight.dst_offset + in_flight.written;
        self.dvr_next_start_ms = in_flight
            .chunk
            .start_ms
            .saturating_add(in_flight.chunk.duration_ms);
        self.dvr_stall = self.dvr_stall.wrapping_add(1);
        self.stall_armed = None;
        debug!(
            sequence = in_flight.chunk.sequence,
            bytes = in_flight.written,
            "disk: wrote DVR chunk"
        );
        Ok(())
    }

    async fn on_copy_part(&mut self, run: u32, data: Vec<u8>) -> Result<(), DiskOutputError> {
        let Some(job) = self.copies.iter_mut().find(|job| job.run == run) else {
            return Ok(());
        };
        let Some(canonical) = &mut self.canonical else {
            return Ok(());
        };
        let len = data.len() as u64;
        canonical
            .segs
            .write_all_at(data, job.dst_segs + job.written)
            .await
            .0?;
        job.written += len;
        Ok(())
    }

    async fn on_copy_done(&mut self, run: u32) -> Result<(), DiskOutputError> {
        let Some(job) = self.copies.iter().position(|job| job.run == run) else {
            return Ok(());
        };
        let job = self.copies.remove(job);
        let Some(canonical) = &mut self.canonical else {
            return Ok(());
        };
        if job.written != job.segs_len {
            self.notify_error(format!(
                "disk: run {run} copy was short ({} of {} bytes)",
                job.written, job.segs_len
            ));
            return Err(DiskOutputError::Io(std::io::Error::other("short copy")));
        }

        canonical
            .index
            .write_all_at(job.records, job.dst_index + u64::from(1 + RUN_BODY))
            .await
            .0?;
        canonical.segs.sync_data().await?;
        canonical
            .index
            .write_all_at(vec![1u8], job.dst_index + RUN_COMPLETE_OFFSET)
            .await
            .0?;
        canonical.index.sync_data().await?;

        if !self.options.keep_runs {
            let _ = compio::fs::remove_file(&job.segs_path).await;
            let _ = compio::fs::remove_file(&job.index_path).await;
        }
        info!(run, bytes = job.segs_len, "disk: filled run hole");
        Ok(())
    }

    fn next_unprocessed_run(&self) -> Option<&RunData> {
        self.closed_runs.front().or(self.current_run.as_ref())
    }

    /// Drive the DVR: fetch the chunk the cursor points at, park when it reaches
    /// a run, and continue once that run's hole is reserved.
    async fn pump(&mut self) -> Result<(), DiskOutputError> {
        loop {
            if !self.phase.dvr() || self.stop || self.finished {
                return Ok(());
            }
            if self.in_flight.is_some() {
                return Ok(());
            }
            let Some(timeline) = self.dvr.snapshot() else {
                self.arm_stall();
                return Ok(());
            };
            self.ensure_clock(&timeline);

            // The cursor cannot pass a run until its hole is sized, and a hole
            // is only sized once the run closes.
            if let Some(number) = self.phase.parked().map(|parked| parked.run) {
                if self.run_is_closed(number) {
                    self.reserve_parked_hole(&timeline).await?;
                    continue;
                }
                if self.run_is_open(number) && self.phase.live() == Live::Recording {
                    // The DVR reached the run that is still recording: this is
                    // the handoff point. The main loop drains and closes it.
                    self.set_live(Live::Draining);
                }
                self.arm_stall();
                return Ok(());
            }

            if self.phase.live() == Live::Done {
                // A run created after the handoff is already covered by the DVR
                // (or by the chunk that will be published next); discard it.
                self.discard_closed_runs("post-handoff").await;
            } else if !self.live_open && self.next_unprocessed_run().is_none() {
                // The live source closed before any run existed: the DVR owns the
                // whole archive and there is nothing left to protect.
                self.set_live(Live::Done);
            }

            // The DVR has published everything it will ever have and the cursor
            // is past its last chunk: any remaining live run is past the end of
            // the broadcast and cannot be holed. Drop it rather than deadlock.
            if timeline.ended && !self.live_open && self.next_chunk(&timeline).is_none() {
                self.discard_closed_runs("past the DVR end").await;
                self.check_done();
                return Ok(());
            }

            // On resume the DVR continues from the last recorded chunk before the
            // new live run is considered.
            if self.resume_abs.is_some() {
                let Some(chunk) = self.next_chunk(&timeline) else {
                    self.arm_stall();
                    return Ok(());
                };
                self.start_fetch(chunk, false, &timeline).await?;
                return Ok(());
            }

            if self.phase.live() != Live::Done && self.next_unprocessed_run().is_none() {
                // No live segment yet: wait for the live start before prefilling.
                self.arm_stall();
                return Ok(());
            }

            let Some(chunk) = self.next_chunk(&timeline) else {
                if self.timeline_done(&timeline) {
                    self.check_done();
                } else {
                    self.arm_stall();
                }
                return Ok(());
            };

            if self.phase.live() == Live::Done {
                self.start_fetch(chunk, false, &timeline).await?;
                return Ok(());
            }

            let Some(run) = self.next_unprocessed_run() else {
                return Ok(());
            };
            let clock = self.clock.unwrap_or(Clock::Elapsed);
            let Some(run_start) = run.first_time(clock, self.live_origin) else {
                return Ok(());
            };
            let run_number = run.number;
            let chunk_start = chunk_time_of(&chunk, clock, self.dvr_origin);
            let chunk_end = chunk_start.saturating_add(chunk.duration_ms);

            if chunk_end <= run_start {
                // Pre-fill or the ad gap before the run: the whole chunk is
                // content the live path never had.
                self.start_fetch(chunk, false, &timeline).await?;
                return Ok(());
            }
            if chunk_start <= run_start {
                // This chunk crosses the run's start. Write it whole; the run's
                // leading records are trimmed and supplied by it.
                self.start_fetch(chunk, true, &timeline).await?;
                return Ok(());
            }

            // The DVR starts after the run (retention window): the run has no
            // recoverable leading content. The first available chunk is the
            // hole's start.
            self.park(Parked {
                run: run_number,
                hole_start: chunk_start,
                crossing: chunk.clone(),
            });
            if self.run_is_closed(run_number) {
                self.reserve_parked_hole(&timeline).await?;
                continue;
            }
            if self.run_is_open(run_number) && self.phase.live() != Live::Done {
                self.set_live(Live::Draining);
            }
            return Ok(());
        }
    }

    /// The parked run has closed: reserve its hole and let the DVR move on.
    async fn reserve_parked_hole(&mut self, timeline: &DvrTimeline) -> Result<(), DiskOutputError> {
        let Some(parked) = self.unpark() else {
            return Ok(());
        };
        let Some(run) = self.take_run(parked.run) else {
            debug!(run = parked.run, "disk: parked run is gone");
            return Ok(());
        };
        self.process_hole(run, &parked, timeline).await
    }

    async fn discard_closed_runs(&mut self, why: &str) {
        while let Some(run) = self.closed_runs.pop_front() {
            debug!(run = run.number, why, "disk: discarding live run");
            self.delete_run(&run).await;
        }
    }

    fn arm_stall(&mut self) {
        if self.stall_armed == Some(self.dvr_stall) {
            return;
        }
        if !self.dvr_seen_url {
            return;
        }
        self.stall_armed = Some(self.dvr_stall);
        spawn_stall_timer(self.dvr_stall, self.events_tx.clone());
    }

    fn next_chunk(&self, timeline: &DvrTimeline) -> Option<DvrSegment> {
        if let Some(resume) = self.resume_abs {
            // Match by absolute time on resume, which survives re-chunking.
            let clock = self.resume_clock.or(self.clock).unwrap_or(Clock::Elapsed);
            return timeline
                .segments
                .iter()
                .find(|chunk| chunk_time_of(chunk, clock, self.dvr_origin) >= resume)
                .cloned();
        }
        let index = timeline
            .segments
            .partition_point(|chunk| chunk.start_ms < self.dvr_next_start_ms);
        timeline.segments.get(index).cloned()
    }

    async fn start_fetch(
        &mut self,
        chunk: DvrSegment,
        crossing: bool,
        timeline: &DvrTimeline,
    ) -> Result<(), DiskOutputError> {
        self.ensure_map(&chunk, timeline).await?;
        let Some(canonical) = &self.canonical else {
            return Ok(());
        };
        if self.resume_abs.is_some() {
            // Only apply the resume cursor once.
            self.resume_abs = None;
            self.clock = self.resume_clock.or(self.clock);
            self.dvr_next_start_ms = chunk.start_ms;
        }
        if crossing {
            let clock = self.clock.unwrap_or(Clock::Elapsed);
            let hole_start =
                chunk_time_of(&chunk, clock, self.dvr_origin).saturating_add(chunk.duration_ms);
            let Some(run) = self.next_unprocessed_run().map(|run| run.number) else {
                return Ok(());
            };
            // Direct field write: `canonical` is borrowed for `dst_offset` below.
            if let Phase::Dvr { parked: slot, .. } = &mut self.phase {
                *slot = Some(Parked {
                    run,
                    hole_start,
                    crossing: chunk.clone(),
                });
            }
        }
        let dst_offset = canonical.segs_end;
        self.in_flight = Some(InFlightChunk {
            chunk: chunk.clone(),
            dst_offset,
            written: 0,
            hasher: Xxh3::new(),
        });
        spawn_fetch(
            chunk,
            Rc::clone(&self.dvr_arena),
            Rc::clone(&self.dvr_wakes),
            self.downloaders.clone(),
            self.events_tx.clone(),
        );
        self.dvr_stall = self.dvr_stall.wrapping_add(1);
        self.stall_armed = None;
        Ok(())
    }

    /// Make the chunk's `EXT-X-MAP` the active map, inserting it only when its
    /// hash differs. This runs before `dst_offset` is taken, so the chunk (and
    /// everything after it) is naturally shifted by the init size.
    async fn ensure_map(
        &mut self,
        chunk: &DvrSegment,
        timeline: &DvrTimeline,
    ) -> Result<(), DiskOutputError> {
        let Some(index) = chunk.map else {
            return Ok(());
        };
        let Some(map) = timeline.maps.get(index) else {
            return Ok(());
        };
        let key = (map.uri.clone(), map.byterange);
        let (hash, bytes) = if let Some((hash, bytes)) = self.map_cache.get(&key) {
            (*hash, bytes.clone())
        } else {
            let bytes = fetch_map(&self.client, map).await?;
            let hash = xxh3_128(&bytes);
            self.map_cache.insert(key, (hash, bytes.clone()));
            (hash, bytes)
        };
        if self
            .canonical
            .as_ref()
            .is_some_and(|canonical| canonical.active_init == Some(hash))
        {
            return Ok(());
        }
        if let Some(canonical) = &mut self.canonical {
            write_init_bytes(canonical, SOURCE_DVR, &bytes).await?;
        }
        Ok(())
    }

    fn ensure_clock(&mut self, timeline: &DvrTimeline) {
        if self.clock.is_some() {
            return;
        }
        let dvr_origin = (0..timeline.segments.len()).find_map(|index| timeline.origin_ms(index));
        if let Some(clock) = self.resume_clock {
            self.clock = Some(clock);
            self.dvr_origin = dvr_origin;
            return;
        }
        let live_has_pdt = self
            .next_unprocessed_run()
            .and_then(|run| run.first_segment.as_ref())
            .is_some_and(|record| record.pdt_ms != 0);
        if dvr_origin.is_none() || !live_has_pdt {
            if self.next_unprocessed_run().is_none() {
                return;
            }
            self.clock = Some(Clock::Elapsed);
        } else {
            self.clock = Some(Clock::Pdt);
        }
        self.dvr_origin = dvr_origin;
    }

    /// Size the parked run's hole, reserve it and start the copy: the run's
    /// uncovered middle goes into the canonical in front of the DVR's next
    /// chunk, and its edges are trimmed because the DVR wrote those chunks whole.
    async fn process_hole(
        &mut self,
        run: RunData,
        parked: &Parked,
        timeline: &DvrTimeline,
    ) -> Result<(), DiskOutputError> {
        let clock = self.clock.unwrap_or(Clock::Elapsed);
        let Some(run_end) = run.last_end(clock, self.live_origin) else {
            self.abandon_run(run, parked.hole_start, parked.hole_start)
                .await?;
            self.advance_past(timeline, parked.hole_start, clock);
            return Ok(());
        };
        let end = hole_end(timeline, run_end, clock, self.dvr_origin).unwrap_or(parked.hole_start);
        let hole_end = end.max(parked.hole_start);
        let plan = plan_hole(
            &run.records,
            parked.hole_start,
            hole_end,
            clock,
            self.live_origin,
        );

        if self.origin_mismatch(&run, parked, timeline, clock, hole_end) {
            self.notify_error(format!(
                "disk: DVR/live alignment mismatch around run {}; leaving a zero-filled hole",
                run.number
            ));
            self.abandon_run(run, parked.hole_start, hole_end).await?;
            self.advance_past(timeline, hole_end, clock);
            return Ok(());
        }

        if plan.segs_len == 0 || plan.records == 0 {
            // The run lies entirely inside a DVR chunk; no copy is needed.
            self.write_zero_filled_marker(&run, &HolePlan::default())
                .await?;
            self.delete_run(&run).await;
            self.advance_past(timeline, hole_end, clock);
            return Ok(());
        }

        // The live map belongs before the live media, so it is inserted before
        // the Run marker (and therefore before the DVR resumes).
        self.insert_run_init(&run).await?;

        let Some(canonical) = &mut self.canonical else {
            return Ok(());
        };
        let dst_segs = canonical.segs_end;
        let dst_index = canonical.index_end;

        // Rebase and renumber the kept records before reserving the hole, so a
        // failure here cannot leave a half-reserved index.
        let mut rebased =
            Vec::with_capacity(usize::try_from(plan.record_len).unwrap_or(usize::MAX));
        let mut init_counter = canonical.init_counter;
        let delta = i64::try_from(dst_segs).unwrap_or(i64::MAX)
            - i64::try_from(plan.source_segs_offset).unwrap_or(0);
        let mut kept = 0usize;
        for meta in &run.records {
            let time = record_time(&meta.segment, clock, self.live_origin);
            let record_end = time.saturating_add(record_duration(&meta.segment));
            if time >= parked.hole_start && record_end <= hole_end {
                let mut record = Record::Segment(meta.segment.clone());
                record.rebase(delta, &mut init_counter);
                record.encode(&mut rebased);
                kept += 1;
            }
        }
        if kept != plan.records {
            // The plan counted the same records; a mismatch means the metadata
            // changed mid-flight, which is a bug, not a disk condition.
            self.notify_error("disk: inconsistent run plan");
            return Err(DiskOutputError::Io(std::io::Error::other(
                "inconsistent run plan",
            )));
        }

        let marker = Record::Run(RunRecord {
            run: run.number,
            dst_segs_offset: dst_segs,
            segs_len: plan.segs_len,
            source_segs_offset: plan.source_segs_offset,
            source_index_offset: plan.source_index_offset,
            record_len: plan.record_len,
            complete: 0,
        });
        write_record(&mut canonical.index, dst_index, &marker).await?;
        canonical.index_end += u64::from(1 + RUN_BODY) + u64::from(plan.record_len);
        canonical.segs_end += plan.segs_len;
        // The marker must be durable before the DVR writes anything past it.
        canonical.index.sync_data().await?;

        self.copies.push(CopyJob {
            run: run.number,
            dst_segs,
            dst_index,
            segs_len: plan.segs_len,
            records: rebased,
            written: 0,
            segs_path: run.segs_path.clone(),
            index_path: run.index_path.clone(),
        });
        spawn_copy(
            run.number,
            run.segs_path.clone(),
            plan.source_segs_offset,
            plan.segs_len,
            self.events_tx.clone(),
        );

        info!(
            run = run.number,
            segs_len = plan.segs_len,
            records = plan.records,
            "disk: reserved run hole"
        );
        self.advance_past(timeline, hole_end, clock);
        Ok(())
    }

    async fn insert_run_init(&mut self, run: &RunData) -> Result<(), DiskOutputError> {
        let Some(init) = &run.init else {
            return Ok(());
        };
        if self
            .canonical
            .as_ref()
            .is_some_and(|canonical| canonical.active_init == Some(init.hash))
        {
            return Ok(());
        }
        let Some(bytes) = read_run_init(run).await? else {
            return Ok(());
        };
        if let Some(canonical) = &mut self.canonical {
            write_init_bytes(canonical, SOURCE_LIVE, &bytes).await?;
        }
        Ok(())
    }

    async fn abandon_run(
        &mut self,
        run: RunData,
        hole_start: u64,
        hole_end: u64,
    ) -> Result<(), DiskOutputError> {
        let clock = self.clock.unwrap_or(Clock::Elapsed);
        let plan = plan_hole(&run.records, hole_start, hole_end, clock, self.live_origin);
        self.write_zero_filled_marker(&run, &plan).await?;
        self.delete_run(&run).await;
        Ok(())
    }

    /// Reserve a complete `Run` marker whose bytes are never written, so the
    /// span stays zero-filled: the run was covered by a DVR chunk, or its hole
    /// was abandoned. Either way it is final, not pending.
    async fn write_zero_filled_marker(
        &mut self,
        run: &RunData,
        plan: &HolePlan,
    ) -> Result<(), DiskOutputError> {
        let Some(canonical) = &mut self.canonical else {
            return Ok(());
        };
        let marker = Record::Run(RunRecord {
            run: run.number,
            dst_segs_offset: canonical.segs_end,
            segs_len: plan.segs_len,
            source_segs_offset: plan.source_segs_offset,
            source_index_offset: plan.source_index_offset,
            record_len: 0,
            complete: 1,
        });
        write_record(&mut canonical.index, canonical.index_end, &marker).await?;
        canonical.index_end += u64::from(1 + RUN_BODY);
        canonical.segs_end += plan.segs_len;
        canonical.index.sync_data().await?;
        Ok(())
    }

    fn origin_mismatch(
        &self,
        run: &RunData,
        parked: &Parked,
        timeline: &DvrTimeline,
        clock: Clock,
        hole_end_time: u64,
    ) -> bool {
        if clock != Clock::Pdt {
            return false;
        }
        let Some(live) = run.origin() else {
            return false;
        };
        let Some(start_dvr) = origin_of(parked.crossing.pdt_ms, parked.crossing.start_ms) else {
            return false;
        };
        if (live - start_dvr).abs() > ORIGIN_TOLERANCE_MS {
            return true;
        }
        let end_chunk = timeline
            .segments
            .iter()
            .rev()
            .find(|chunk| chunk_time_of(chunk, clock, self.dvr_origin) <= hole_end_time);
        end_chunk
            .and_then(|chunk| origin_of(chunk.pdt_ms, chunk.start_ms))
            .is_some_and(|end_dvr| (live - end_dvr).abs() > ORIGIN_TOLERANCE_MS)
    }

    fn advance_past(&mut self, timeline: &DvrTimeline, time: u64, clock: Clock) {
        if let Some(index) = timeline
            .segments
            .iter()
            .position(|chunk| chunk_time_of(chunk, clock, self.dvr_origin) >= time)
        {
            self.dvr_next_start_ms = timeline.segments[index].start_ms;
        } else {
            self.dvr_next_start_ms = timeline.end_ms;
        }
    }

    fn run_is_closed(&self, run: u32) -> bool {
        self.closed_runs
            .iter()
            .any(|candidate| candidate.number == run)
    }

    fn run_is_open(&self, run: u32) -> bool {
        self.current_run
            .as_ref()
            .is_some_and(|candidate| candidate.number == run)
    }

    fn take_run(&mut self, run: u32) -> Option<RunData> {
        if let Some(index) = self
            .closed_runs
            .iter()
            .position(|candidate| candidate.number == run)
        {
            return self.closed_runs.remove(index);
        }
        None
    }

    async fn delete_run(&self, run: &RunData) {
        if !self.options.keep_runs {
            let _ = compio::fs::remove_file(&run.segs_path).await;
            let _ = compio::fs::remove_file(&run.index_path).await;
        }
    }

    fn timeline_done(&self, timeline: &DvrTimeline) -> bool {
        timeline.ended && self.next_chunk(timeline).is_none()
    }

    fn check_done(&mut self) {
        if self.stop || self.finished {
            return;
        }
        if self.phase.promoted() {
            if !self.live_open {
                self.finished = true;
            }
            return;
        }
        if !self.phase.dvr() {
            return;
        }
        if !self.live_open
            && self.phase.live() != Live::Draining
            && self.phase.parked().is_none()
            && self.in_flight.is_none()
            && self.copies.is_empty()
            && self.closed_runs.is_empty()
            && self.current_run.is_none()
            && self
                .dvr
                .snapshot()
                .is_some_and(|timeline| self.timeline_done(&timeline))
        {
            self.finished = true;
        }
    }

    async fn finish(&mut self) -> Result<(), DiskOutputError> {
        if let Some(canonical) = &mut self.canonical {
            canonical.segs.sync_all().await?;
            canonical.index.sync_all().await?;
        }
        if matches!(self.phase, Phase::Discovering) {
            self.promote().await?;
        }
        Ok(())
    }
}

enum Incoming {
    Live(Result<SegmentMessage, kanal::ReceiveError>),
    Event(Result<Event, kanal::ReceiveError>),
}
