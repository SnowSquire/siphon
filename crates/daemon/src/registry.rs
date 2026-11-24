//! The pipeline registry: one runtime per `(login, quality)`, the followed
//! channels with their record preference, and the watch/record lifecycle.
//!
//! A runtime exists only while it is recording or watched. Recording starts on
//! `stream-up` (or when record is toggled on), a DESCRIBE attaches the media
//! engine to an existing recording runtime, and the last viewer of a
//! non-recording runtime drops it together with the channel entry.
//!
//! The registry is a field of the app loop, so it has no handle on the reactor:
//! work that has to run after the call returns — an activation, a recording
//! that finished — goes back to that loop as a [`Job`] through its job queue.
//! Its state is behind `RefCell` because the loop calls it from `&self`
//! contexts too (a snapshot, a connection close).

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::poll_fn;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::rc::Rc;
use std::task::Poll;
use std::task::Waker;

use kanal::AsyncSender;
use siphon_shared::riffle::ArenaSummary;
use siphon_shared::riffle::DiskView;
use siphon_shared::riffle::DownloaderSummary;
use siphon_shared::riffle::LogLevel;
use siphon_shared::riffle::PipelineState;
use siphon_shared::riffle::PipelineView;
use siphon_shared::riffle::RpcError;
use siphon_shared::riffle::StreamDetail;
use siphon_shared::riffle::StreamKey;
use siphon_shared::riffle::Update;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::app::JobDone;
use crate::arena::ArenaSet;
use crate::bus::Bus;
use crate::disk::DiskOptions;
use crate::disk::DiskOutput;
use crate::downloader::ToDownloader;
use crate::http::Client;
use crate::media::dvr::Dvr;
use crate::media::lane::LaneConsumer;
use crate::metrics::DiskMetrics;
use crate::metrics::DownloaderMetrics;
use crate::metrics::StreamMetrics;
use crate::notify::Notifier;
use crate::scheduler::ConsumerRegistration;
use crate::scheduler::Segment;
use crate::scheduler::StreamScheduler;
use crate::spool::ConsumerWake;
use crate::spool::WakeSet;
/// Why a channel could not be resolved into a pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ResolveError(u16);

impl ResolveError {
    const NOT_FOUND: Self = Self(404);
    const UNAVAILABLE: Self = Self(503);

    const fn status(self) -> u16 {
        self.0
    }
}

use crate::variant::Variant;
use crate::variant::VariantKind;
use crate::variant::VariantPlaylistError;
use crate::variant::best_variant;
use crate::variant::choose_variant;
use crate::variant::get_variants;

/// Options shared by every stream the registry creates.
pub struct Options {
    pub client_id: Option<String>,
    pub auth_token: Option<String>,
    pub low_latency: bool,
    /// Recording settings at startup. The live values are on the registry,
    /// because the UI can change them while the daemon runs.
    pub recording: Recording,
    pub notify: Option<Notifier>,
    pub keep_runs: bool,
}

/// Where complete broadcasts go and whether the second orientation is kept.
pub struct Recording {
    pub output: Option<PathBuf>,
    /// Record both orientations of dual-format broadcasts.
    pub dual_format: bool,
}

/// Work the registry hands back to the app loop.
///
/// The registry is owned by [`App`](crate::app::App), so it cannot clone itself
/// into a task to finish something later. A call that has to resolve a channel
/// over the network, or react to a recorder ending, queues one of these instead
/// and the loop performs it on its next pass. It is the same queue the loop's
/// other background work returns through, never a channel of its own.
#[derive(Clone, Debug)]
pub enum Job {
    /// Start the recording pipeline for a wanted channel, unless it is running.
    Activate(StreamKey),
    /// Start a one-shot recording of the broadcast that is live right now.
    ActivateOnce(StreamKey),
    /// The disk recorder for this key ended, so its runtime can be released.
    Finished(StreamKey),
}

/// Everything the registry shares between streams.
pub struct Registry {
    client: Rc<Client>,
    arena: Rc<ArenaSet>,
    downloaders: AsyncSender<ToDownloader>,
    options: Options,
    /// Disk output root. `None` disables recording. Mutable because the UI can
    /// move it while the daemon runs; a running recording keeps its directory.
    output: RefCell<Option<PathBuf>>,
    /// Record both orientations of a dual-format broadcast.
    dual_format: Cell<bool>,
    bus: Rc<Bus>,
    downloader_metrics: Rc<DownloaderMetrics>,
    /// Every channel the registry serves: the followed ones from config, plus
    /// any an RTSP DESCRIBE pulled in on demand.
    channels: RefCell<Vec<Channel>>,
    /// Live streams, by `(login, quality)`. At most a handful, so the scan is
    /// cheaper than hashing two strings.
    streams: RefCell<HashMap<StreamId, Rc<StreamRuntime>>>,
    gate: Gate,
    /// Where a [`Job`] goes: the app loop's queue, drained every pass.
    jobs: kanal::Sender<Box<JobDone>>,
    /// Test-only: the next pipeline spawned serves its consumers from here
    /// instead of a playlist, so the media lane can be fed fixture segments.
    #[cfg(all(test, not(miri)))]
    fixture: RefCell<Option<kanal::AsyncReceiver<crate::scheduler::SegmentMessage>>>,
    /// Test-only: how many consumers that feed currently serves.
    #[cfg(all(test, not(miri)))]
    fixture_consumers: Rc<Cell<usize>>,
}

/// A channel row: the followed preference, the identity, and once fetched, the
/// broadcast data every quality of that channel shares.
#[derive(Clone)]
struct Channel {
    login: String,
    /// Twitch channel id: what a stream is keyed by. Known from the follow list,
    /// or learned from GQL for a channel an RTSP DESCRIBE pulled in.
    id: Option<u64>,
    /// Display name, from the follow list or GQL.
    title: Option<String>,
    /// The persisted "record when live" preference, mirrored from the config.
    record: bool,
    /// The user stopped this broadcast. Compared against the broadcast id, so a
    /// restream (a new id) is not suppressed.
    stopped: Option<NonZeroU64>,
    /// False for a channel an RTSP DESCRIBE pulled in, so it can be dropped
    /// when its last pipeline goes away.
    followed: bool,
    data: Option<Rc<ChannelData>>,
}

/// What one GQL + usher round trip learns about a live channel.
struct ChannelData {
    id: u64,
    login: String,
    title: String,
    variants: Vec<Variant>,
    broadcast_id: u64,
}

/// One pipeline's identity: the channel id and the variant, never the login.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct StreamId {
    channel: u64,
    quality: Option<Box<str>>,
}

/// Where a lane's pipeline is right now, as scalars.
///
/// Read from the live window under one borrow; a `Segment` is never copied out,
/// because it owns a `Reader` and cloning it pins an arena slot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LanePlace {
    /// End of the newest live segment, in broadcast milliseconds.
    pub position_ms: u64,
    /// The oldest position this runtime can still serve, or `0` when it is not
    /// known yet (nothing published, or the runtime is gone).
    pub window_start_ms: u64,
    /// Playlist sequence of the newest live segment, or `0`.
    pub sequence: u64,
}

impl StreamId {
    fn new(channel: u64, quality: Option<&str>) -> Self {
        Self {
            channel,
            quality: quality.map(Box::from),
        }
    }
}

/// One running pipeline.
pub struct StreamRuntime {
    key: StreamKey,
    /// The channel's login: display, logging and the DVR's archive lookup. Not
    /// identity; `key.channel` is.
    login: String,
    title: String,
    /// Set when the pipeline was stopped on purpose. Nothing can be watching
    /// yet, so this is the only reason a runtime outlives its recording.
    stopped: Cell<bool>,
    dvr: Rc<Dvr>,
    client: Rc<Client>,
    wakes: Rc<WakeSet>,
    live_window: Rc<RefCell<Vec<Segment>>>,
    registration: RefCell<Option<kanal::Sender<ConsumerRegistration>>>,
    reader_wake: Rc<ConsumerWake>,
    /// The disk recorder while this pipeline is recording.
    recording: RefCell<Option<Recorder>>,
    dvr_spawned: Cell<bool>,
    broadcast_id: u64,
    /// `STABLE-VARIANT-ID` of the variant this runtime serves; names its
    /// recording directory.
    variant: Box<str>,
    /// That variant's kind, so the second recording can be the other
    /// orientation.
    variant_kind: VariantKind,
    metrics: Rc<StreamMetrics>,
    /// Lanes attached to this runtime across every connection. A watch-only
    /// runtime lives until the last one detaches; a recording keeps it either
    /// way.
    viewers: Cell<u32>,
}

struct Recorder {
    disk: Rc<DiskOutput>,
    metrics: Rc<DiskMetrics>,
}

impl StreamRuntime {
    fn is_recording(&self) -> bool {
        self.recording.borrow().is_some()
    }

    /// Everything a media lane needs to read this pipeline: the registration
    /// channel and a wake registered with the spools' wake set.
    ///
    /// `None` once the scheduler has gone, which is a pipeline that will never
    /// publish again: a lane opened on it could not be fed, so the caller has to
    /// fail the open rather than serve a lane that produces nothing.
    pub fn lane_consumer(&self) -> Option<LaneConsumer> {
        let slot = self.registration.borrow().clone()?;
        let wake = ConsumerWake::new();
        self.wakes.register(&wake);
        Some(LaneConsumer { slot, wake })
    }

    fn stop(&self) {
        self.stopped.set(true);
        self.dvr.stop();
        self.reader_wake.wake();
        self.registration.borrow_mut().take();
        if let Some(recording) = self.recording.borrow_mut().take() {
            recording.disk.stop();
        }
    }

    fn ensure_dvr(&self) {
        if self.dvr_spawned.replace(true) {
            return;
        }
        crate::media::dvr::spawn(
            Rc::clone(&self.client),
            Some(self.login.clone()),
            Rc::clone(&self.dvr),
        );
    }

    /// Where this pipeline is right now: the live edge, the retained window's
    /// front, and the newest sequence. Scalars only, under one borrow.
    ///
    /// The retained window is the DVR archive's front when one has been
    /// discovered, otherwise the live playlist's oldest segment. A watch-only
    /// runtime does not spawn the DVR, so that half is usually absent.
    pub(crate) fn place(&self) -> LanePlace {
        let (position_ms, live_start_ms, sequence) = {
            let window = self.live_window.borrow();
            let first = window.first().map(|segment| segment.timestamp_ms);
            let last = window.last();
            (
                last.map_or(0, |segment| {
                    segment.timestamp_ms.saturating_add(segment.duration_ms)
                }),
                first,
                last.map_or(0, |segment| segment.sequence_num),
            )
        };
        let dvr_start_ms = self
            .dvr
            .snapshot()
            .and_then(|timeline| timeline.segments.first().map(|segment| segment.start_ms));
        let window_start_ms = match (dvr_start_ms, live_start_ms) {
            (Some(dvr), Some(live)) => dvr.min(live),
            (Some(dvr), None) => dvr,
            (None, live) => live.unwrap_or(0),
        };
        LanePlace {
            position_ms,
            window_start_ms,
            sequence,
        }
    }
}

impl Registry {
    #[must_use]
    pub fn new(
        client: Rc<Client>,
        arena: Rc<ArenaSet>,
        downloaders: AsyncSender<ToDownloader>,
        downloader_metrics: Rc<DownloaderMetrics>,
        options: Options,
        bus: Rc<Bus>,
        jobs: kanal::Sender<Box<JobDone>>,
    ) -> Self {
        let output = RefCell::new(options.recording.output.clone());
        let dual_format = Cell::new(options.recording.dual_format);
        Self {
            client,
            arena,
            downloaders,
            options,
            output,
            dual_format,
            bus,
            downloader_metrics,
            channels: RefCell::new(Vec::new()),
            streams: RefCell::new(HashMap::new()),
            gate: Gate::default(),
            jobs,
            #[cfg(all(test, not(miri)))]
            fixture: RefCell::new(None),
            #[cfg(all(test, not(miri)))]
            fixture_consumers: Rc::new(Cell::new(0)),
        }
    }

    /// Test-only: the next pipeline spawned reads from `source` instead of the
    /// playlist, and reports how many consumers it is serving.
    ///
    /// This is the seam the media lane's end-to-end test uses: the registry,
    /// the app loop, the QUIC carrier and the lane are all real; only the
    /// segment source is a fixture, which is what a `--debug` run would be.
    #[cfg(all(test, not(miri)))]
    pub fn fixture_source(
        &self,
    ) -> (
        kanal::Sender<crate::scheduler::SegmentMessage>,
        Rc<Cell<usize>>,
    ) {
        let (tx, rx) = kanal::unbounded();
        *self.fixture.borrow_mut() = Some(rx.to_async());
        (tx, Rc::clone(&self.fixture_consumers))
    }

    /// Mirrors the persisted follow list onto the channel table. Called at
    /// startup and whenever the UI adds, removes or toggles a channel.
    pub fn sync_channels(&self, channels: &[siphon_shared::riffle::Channel]) {
        let mut rows = self.channels.borrow_mut();
        for row in &mut *rows {
            let Some(channel) = channels.iter().find(|c| c.login == row.login) else {
                row.followed = false;
                row.record = false;
                continue;
            };
            row.followed = true;
            row.record = channel.record;
            row.id = Some(channel.id);
            row.title = Some(
                channel
                    .display_name
                    .clone()
                    .unwrap_or_else(|| channel.login.clone()),
            );
        }
        for channel in channels {
            if rows.iter().any(|row| row.login == channel.login) {
                continue;
            }
            rows.push(Channel {
                login: channel.login.clone(),
                id: Some(channel.id),
                title: Some(
                    channel
                        .display_name
                        .clone()
                        .unwrap_or_else(|| channel.login.clone()),
                ),
                record: channel.record,
                stopped: None,
                followed: true,
                data: None,
            });
        }
    }

    /// Seeds the fetched half of a channel row, so a test can drive the
    /// lifecycle without GQL or usher.
    #[cfg(test)]
    pub fn seed_channel(
        &self,
        login: &str,
        id: u64,
        record: bool,
        variants: Vec<Variant>,
        broadcast_id: u64,
    ) {
        self.ensure_row(login);
        self.update_row(login, |row| {
            row.id = Some(id);
            row.title = Some(login.to_owned());
            row.record = record;
            row.followed = true;
            row.data = Some(Rc::new(ChannelData {
                id,
                login: login.to_owned(),
                title: login.to_owned(),
                variants,
                broadcast_id,
            }));
        });
    }

    /// Performs one queued job. The app loop is the only caller: the registry
    /// cannot reach the reactor on its own, so it asks the loop to.
    pub async fn apply(&self, job: Job) {
        match job {
            Job::Activate(key) => self.activate(key).await,
            Job::ActivateOnce(key) => self.activate_once(key).await,
            Job::Finished(key) => self.finish_stream(&key),
        }
    }

    #[must_use]
    pub fn can_record(&self) -> bool {
        self.output.borrow().is_some()
    }

    /// Point new recordings at another directory. A running recording keeps the
    /// directory it opened, so a change never splits one broadcast across two.
    pub fn set_output(&self, output: Option<PathBuf>) {
        *self.output.borrow_mut() = output;
    }

    /// Record both orientations, applied to what is recording right now and to
    /// every later start. The primary recording is unaffected either way: a
    /// single-format broadcast is always recorded, whichever orientation it is.
    pub fn set_dual_format(&self, dual: bool) {
        if self.dual_format.replace(dual) == dual {
            return;
        }
        let recording: Vec<Rc<StreamRuntime>> = self
            .streams
            .borrow()
            .values()
            .filter(|runtime| runtime.is_recording() && runtime.key.quality.is_none())
            .cloned()
            .collect();
        for runtime in recording {
            if dual {
                self.maybe_record_second_orientation(&runtime);
            } else {
                self.stop_second_orientation(&runtime);
            }
        }
    }

    /// Get or build the channel row. Followed channels already exist.
    fn ensure_row(&self, login: &str) -> bool {
        let mut rows = self.channels.borrow_mut();
        if rows.iter().any(|row| row.login == login) {
            return true;
        }
        rows.push(Channel {
            login: login.to_owned(),
            id: None,
            title: None,
            record: false,
            stopped: None,
            followed: false,
            data: None,
        });
        false
    }

    fn channel_row(&self, login: &str) -> Option<Channel> {
        self.channels
            .borrow()
            .iter()
            .find(|row| row.login == login)
            .cloned()
    }

    fn update_row(&self, login: &str, f: impl FnOnce(&mut Channel)) {
        if let Some(row) = self
            .channels
            .borrow_mut()
            .iter_mut()
            .find(|row| row.login == login)
        {
            f(row);
        }
    }

    /// The internal key for a wire key. The wire already carries the channel id.
    fn stream_id(key: &StreamKey) -> StreamId {
        StreamId::new(key.channel, key.quality.as_deref())
    }

    /// The running pipeline for a wire key.
    fn runtime(&self, key: &StreamKey) -> Option<Rc<StreamRuntime>> {
        self.streams.borrow().get(&Self::stream_id(key)).cloned()
    }

    /// The login of the channel with this id, from the channel table.
    fn login_of(&self, channel: u64) -> Option<String> {
        self.channels
            .borrow()
            .iter()
            .find(|row| row.id == Some(channel))
            .map(|row| row.login.clone())
    }

    fn update_row_by_id(&self, channel: u64, f: impl FnOnce(&mut Channel)) {
        if let Some(row) = self
            .channels
            .borrow_mut()
            .iter_mut()
            .find(|row| row.id == Some(channel))
        {
            f(row);
        }
    }

    /// Get or build the fetched half of a channel entry. One GQL fetch per
    /// login, shared by every quality requested while the channel has streams.
    async fn channel_data(&self, login: &str) -> Result<Rc<ChannelData>, ResolveError> {
        self.ensure_row(login);
        if let Some(data) = self.channel_row(login).and_then(|row| row.data) {
            return Ok(data);
        }
        let _guard = self.gate.acquire(gate_key(login, None)).await;
        if let Some(data) = self.channel_row(login).and_then(|row| row.data) {
            return Ok(data);
        }

        // The daemon owns the login to id mapping: a followed channel arrives
        // with its id through `sync_channels`. A login it was never told about
        // is not a channel this registry serves.
        let Some(row) = self.channel_row(login) else {
            return Err(ResolveError::NOT_FOUND);
        };
        let Some(id) = row.id else {
            debug!(
                login,
                "daemon: login is not followed, refusing to invent a stream"
            );
            return Err(ResolveError::NOT_FOUND);
        };
        let title = row.title.unwrap_or_else(|| login.to_owned());

        let resolved = get_variants(
            &self.client,
            self.options.client_id.as_deref(),
            self.options.auth_token.as_deref(),
            login,
            self.options.low_latency,
        )
        .await;
        let data = match resolved {
            Ok((variants, broadcast_id)) => Rc::new(ChannelData {
                id,
                login: login.to_owned(),
                title,
                variants,
                broadcast_id,
            }),
            Err(error) => {
                // Offline is a 404; anything else (GQL, TLS, usher) may be
                // transient, so tell the client to try again.
                let status = match &error {
                    VariantPlaylistError::NoVariants(_) => ResolveError::NOT_FOUND,
                    _ => ResolveError::UNAVAILABLE,
                };
                warn!(%error, login, status = status.status(), "daemon: channel resolution failed");
                return Err(status);
            }
        };

        self.update_row(login, |row| {
            row.title = Some(data.title.clone());
            row.data = Some(Rc::clone(&data));
        });
        Ok(data)
    }

    fn spawn_runtime(
        &self,
        key: &StreamKey,
        data: &ChannelData,
        variant: &Variant,
        record: bool,
    ) -> Rc<StreamRuntime> {
        let key = key.clone().with_stream(data.broadcast_id);
        let metrics = StreamMetrics::new(
            key.clone(),
            Some(variant.id.to_string()),
            Rc::clone(&self.bus),
        );

        let dvr = Dvr::new(mock_dvr_url(variant));
        dvr.set_live_variant(variant);

        let wakes = WakeSet::new();
        let live_window = Rc::new(RefCell::new(Vec::new()));
        let (registration, receive) = kanal::unbounded();
        let reader_wake = ConsumerWake::new();

        #[cfg(all(test, not(miri)))]
        let feed_receive = receive.clone();
        let scheduler = StreamScheduler::new(
            Rc::clone(&self.client),
            Rc::clone(&self.arena),
            Rc::clone(&wakes),
            variant.url.clone(),
            self.downloaders.clone(),
            receive,
            Rc::clone(&reader_wake),
        )
        .with_live_window(Rc::clone(&live_window))
        .with_metrics(Rc::clone(&metrics));
        // A test can serve this pipeline's consumers from fixture segments
        // instead of the playlist, so the media lane is exercised end to end
        // without an HTTP edge.
        #[cfg(all(test, not(miri)))]
        let fixture = self.fixture.borrow_mut().take();
        #[cfg(all(test, not(miri)))]
        let fixture_consumers = Rc::clone(&self.fixture_consumers);
        #[cfg(all(test, not(miri)))]
        match fixture {
            Some(source) => {
                crate::media::lane::spawn_fixture_feed(feed_receive, source, fixture_consumers);
            }
            None => compio::runtime::spawn(scheduler.main()).detach(),
        }
        #[cfg(not(all(test, not(miri))))]
        compio::runtime::spawn(scheduler.main()).detach();

        let runtime = Rc::new(StreamRuntime {
            key: key.clone(),
            login: data.login.clone(),
            title: data.title.clone(),
            stopped: Cell::new(false),
            dvr,
            client: Rc::clone(&self.client),
            wakes,
            live_window,
            registration: RefCell::new(Some(registration)),
            reader_wake,
            recording: RefCell::new(None),
            dvr_spawned: Cell::new(false),
            broadcast_id: data.broadcast_id,
            variant: variant.id.clone(),
            variant_kind: variant.kind.clone(),
            metrics,
            viewers: Cell::new(0),
        });

        if record {
            self.start_recording(&runtime);
        }

        info!(
            login = %data.login,
            quality = key.quality.as_deref().unwrap_or("<first>"),
            variant = %variant.id,
            recording = record,
            "daemon: stream started"
        );
        runtime
    }

    fn start_recording(&self, runtime: &Rc<StreamRuntime>) {
        if runtime.is_recording() {
            return;
        }
        let Some(output) = self.output.borrow().clone() else {
            self.bus.publish(Update::Error(Some(
                "recording needs an output directory".to_owned(),
            )));
            return;
        };
        runtime.ensure_dvr();
        let Some(registration) = runtime.registration.borrow().clone() else {
            return;
        };
        let metrics = Rc::new(DiskMetrics::new());
        let disk = Rc::new(crate::disk::spawn_disk_output(
            DiskOptions {
                output,
                stream_id: runtime.broadcast_id,
                variant: runtime.variant.clone(),
                keep_runs: self.options.keep_runs,
                notifier: self.options.notify.clone(),
                dvr: Rc::clone(&runtime.dvr),
                client: Rc::clone(&runtime.client),
                metrics: Rc::clone(&metrics),
                live: true,
            },
            &runtime.wakes,
            &self.downloaders,
            Some(&registration),
        ));

        let jobs = self.jobs.clone();
        let key = runtime.key.clone();
        let watcher = Rc::clone(&disk);
        compio::runtime::spawn(async move {
            if let Some(error) = watcher.wait().await {
                error!(%error, "daemon: fatal disk output error");
                std::process::exit(1);
            }
            enqueue(&jobs, Job::Finished(key));
        })
        .detach();

        *runtime.recording.borrow_mut() = Some(Recorder { disk, metrics });
        info!(
            login = %runtime.login,
            quality = runtime.key.quality.as_deref().unwrap_or("<first>"),
            "daemon: recording started"
        );
        self.bus.log_channel(
            LogLevel::Info,
            "recording",
            format!(
                "recording started ({})",
                runtime.key.quality.as_deref().unwrap_or("best")
            ),
            Some(runtime.login.clone()),
            NonZeroU64::new(runtime.broadcast_id),
        );
        self.publish_pipeline(runtime);
        self.maybe_record_second_orientation(runtime);
    }

    /// A dual-format broadcast carries a second variant group, and a recorder
    /// consumes one variant's spool, so the other orientation needs its own
    /// pipeline. It is keyed by that variant id, which is also what stops this
    /// recursing: only a `quality: None` runtime ever asks for one.
    ///
    /// The other orientation is the *opposite* of the primary's, so a
    /// single-format broadcast (portrait included) has none and is simply
    /// recorded as its primary variant.
    fn maybe_record_second_orientation(&self, runtime: &Rc<StreamRuntime>) {
        if !self.dual_format.get() || runtime.key.quality.is_some() {
            return;
        }
        let Some(other_kind) = other_orientation(&runtime.variant_kind) else {
            return;
        };
        let Some((portrait, variant)) = self.orientation_variant(&runtime.login, &other_kind)
        else {
            return;
        };
        let Some(data) = self.channel_row(&runtime.login).and_then(|row| row.data) else {
            return;
        };
        let id = StreamId::new(data.id, Some(portrait.as_str()));
        if let Some(existing) = self.streams.borrow().get(&id).cloned() {
            // Already serving that quality for a viewer; record it as well.
            self.start_recording(&existing);
            return;
        }
        let key = StreamKey::new(data.id, Some(portrait));
        let sibling = self.spawn_runtime(&key, &data, &variant, true);
        self.streams.borrow_mut().insert(id, Rc::clone(&sibling));
        self.publish_pipeline(&sibling);
        info!(
            login = %runtime.login,
            quality = %key.quality.as_deref().unwrap_or("<first>"),
            "daemon: second orientation started"
        );
    }

    /// The variant id and definition of `kind`, when this broadcast carries it.
    fn orientation_variant(&self, login: &str, kind: &VariantKind) -> Option<(String, Variant)> {
        let data = self.channel_row(login)?.data?;
        data.variants
            .iter()
            .find(|variant| &variant.kind == kind)
            .map(|variant| (variant.id.to_string(), variant.clone()))
    }

    fn stop_recording(&self, runtime: &Rc<StreamRuntime>) {
        let Some(recording) = runtime.recording.borrow_mut().take() else {
            return;
        };
        recording.disk.stop();
        self.bus.log_channel(
            LogLevel::Info,
            "recording",
            "recording stopped",
            Some(runtime.login.clone()),
            NonZeroU64::new(runtime.broadcast_id),
        );
        self.publish_pipeline(runtime);
        self.stop_second_orientation(runtime);
    }

    /// The second orientation has no owner of its own: it lives as long as the
    /// primary recording, unless a viewer asked for that quality directly.
    fn stop_second_orientation(&self, runtime: &Rc<StreamRuntime>) {
        if runtime.key.quality.is_some() {
            return;
        }
        let Some(other_kind) = other_orientation(&runtime.variant_kind) else {
            return;
        };
        let Some((portrait, _)) = self.orientation_variant(&runtime.login, &other_kind) else {
            return;
        };
        let key = StreamKey::new(runtime.key.channel, Some(portrait));
        if let Some(sibling) = self.runtime(&key) {
            self.stop_recording(&sibling);
            // The primary recording is the only owner of that quality, but a
            // lane watching it directly keeps its pipeline alive.
            if sibling.viewers.get() == 0 {
                self.remove_runtime(&key);
            }
        }
    }

    /// The user's persisted record preference for a followed channel.
    fn wanted(&self, login: &str) -> bool {
        self.channel_row(login).is_some_and(|row| row.record)
    }

    /// Record toggled for a channel. Turning it off keeps a watched pipeline.
    pub fn set_record(&self, login: &str, record: bool) {
        self.ensure_row(login);
        self.update_row(login, |row| row.record = record);
        if record {
            self.update_row(login, |row| row.stopped = None);
            self.monitor_live(login);
        } else {
            self.monitor_clear(login);
        }
    }

    /// A live broadcast of a wanted channel: attach recording now, or start the
    /// pipeline when it is not running yet.
    pub fn monitor_live(&self, login: &str) {
        if !self.wanted(login) {
            return;
        }
        if let Some(runtime) = self
            .streams
            .borrow()
            .values()
            .find(|runtime| runtime.login == login && runtime.key.quality.is_none())
            .cloned()
        {
            self.start_recording(&runtime);
            return;
        }
        let Some(channel) = self.channel_row(login).and_then(|row| row.id) else {
            return;
        };
        // Resolving the channel is a GQL + usher round trip, so the loop starts
        // it rather than this call.
        enqueue(&self.jobs, Job::Activate(StreamKey::new(channel, None)));
    }

    /// Record the broadcast that is live right now, without persisting
    /// anything. An offline channel is a `NotFound`.
    pub fn record_once(&self, key: StreamKey) {
        if let Some(runtime) = self.runtime(&key) {
            self.start_recording(&runtime);
            return;
        }
        enqueue(&self.jobs, Job::ActivateOnce(key));
    }

    async fn activate(&self, key: StreamKey) {
        let Some(login) = self.login_of(key.channel) else {
            return;
        };
        let _guard = self
            .gate
            .acquire(gate_key(&login, key.quality.as_deref()))
            .await;
        if !self.wanted(&login) {
            return;
        }
        let Ok(data) = self.channel_data(&login).await else {
            self.bus.publish(Update::Error(Some(format!(
                "recording {login}: channel is offline"
            ))));
            return;
        };
        let id = StreamId::new(data.id, key.quality.as_deref());
        if self.streams.borrow().contains_key(&id) {
            return;
        }
        if self
            .channel_row(&login)
            .is_some_and(|row| row.stopped == NonZeroU64::new(data.broadcast_id))
        {
            return;
        }
        let Some(variant) = best_variant(&data.variants) else {
            self.bus.publish(Update::Error(Some(format!(
                "recording {login}: no variant"
            ))));
            return;
        };
        let runtime = self.spawn_runtime(&key, &data, variant, true);
        self.streams.borrow_mut().insert(id, Rc::clone(&runtime));
        self.publish_pipeline(&runtime);
    }

    async fn activate_once(&self, key: StreamKey) {
        let Some(login) = self.login_of(key.channel) else {
            return;
        };
        let _guard = self
            .gate
            .acquire(gate_key(&login, key.quality.as_deref()))
            .await;
        let Ok(data) = self.channel_data(&login).await else {
            self.bus.publish(Update::Error(Some(format!(
                "recording {login}: channel is offline"
            ))));
            return;
        };
        let id = StreamId::new(data.id, key.quality.as_deref());
        if let Some(runtime) = self.streams.borrow().get(&id).cloned() {
            self.start_recording(&runtime);
            return;
        }
        let Some(variant) = choose_variant(&data.variants, key.quality.as_deref()) else {
            self.bus.publish(Update::Error(Some(format!(
                "recording {login}: no such variant"
            ))));
            return;
        };
        let runtime = self.spawn_runtime(&key, &data, variant, true);
        self.streams.borrow_mut().insert(id, Rc::clone(&runtime));
        self.publish_pipeline(&runtime);
    }

    /// Attaches one lane as a consumer of `key`'s pipeline, spawning it when it
    /// is not running.
    ///
    /// This is the consumer the old DESCRIBE used to be: a lane keeps a
    /// watch-only runtime alive, and [`Self::detach`] releases it. A key that is
    /// already attached takes a second claim rather than spawning a second
    /// pipeline; the lane set itself is per connection and the app only calls
    /// this for a lane it does not already hold.
    pub async fn attach(&self, key: &StreamKey) -> Result<Rc<StreamRuntime>, RpcError> {
        if let Some(runtime) = self.runtime(key) {
            runtime.viewers.set(runtime.viewers.get().saturating_add(1));
            return Ok(runtime);
        }
        let Some(login) = self.login_of(key.channel) else {
            return Err(RpcError::NotFound);
        };
        let _guard = self
            .gate
            .acquire(gate_key(&login, key.quality.as_deref()))
            .await;
        if let Some(runtime) = self.runtime(key) {
            runtime.viewers.set(runtime.viewers.get().saturating_add(1));
            return Ok(runtime);
        }
        // Offline is the channel's answer, not an unknown one: `login_of`
        // already established that this channel exists.
        let data = self
            .channel_data(&login)
            .await
            .map_err(|_| RpcError::Failed(format!("{login} is offline")))?;
        let Some(variant) = choose_variant(&data.variants, key.quality.as_deref()) else {
            return Err(RpcError::BadRequest(format!(
                "{login} has no {} variant",
                key.quality.as_deref().unwrap_or("default")
            )));
        };
        let id = StreamId::new(data.id, key.quality.as_deref());
        let runtime = self.spawn_runtime(key, &data, variant, false);
        runtime.viewers.set(1);
        self.streams.borrow_mut().insert(id, Rc::clone(&runtime));
        self.publish_pipeline(&runtime);
        info!(
            login = %runtime.login,
            quality = key.quality.as_deref().unwrap_or("<first>"),
            "daemon: lane attached"
        );
        Ok(runtime)
    }

    /// Releases one lane's claim on its pipeline.
    ///
    /// The runtime goes away when this was the last lane, nothing is recording
    /// and the channel's record preference is off — the same condition the old
    /// `release_stream` used. A runtime that is already gone (stopped, or the
    /// broadcast ended) is not an error.
    pub fn detach(&self, key: &StreamKey) {
        let Some(runtime) = self.runtime(key) else {
            return;
        };
        let viewers = runtime.viewers.get().saturating_sub(1);
        runtime.viewers.set(viewers);
        if viewers > 0 || runtime.is_recording() || self.wanted(&runtime.login) {
            return;
        }
        info!(
            login = %runtime.login,
            quality = key.quality.as_deref().unwrap_or("<first>"),
            "daemon: lane detached, stream stopped"
        );
        self.remove_runtime(&runtime.key);
    }

    /// Where the pipeline behind `key` is right now, if it is running.
    #[must_use]
    pub fn place(&self, key: &StreamKey) -> Option<LanePlace> {
        self.runtime(key).map(|runtime| runtime.place())
    }

    /// The oldest position `key`'s pipeline can still serve; `0` when unknown.
    #[must_use]
    pub fn window_start_ms(&self, key: &StreamKey) -> u64 {
        self.place(key).map_or(0, |place| place.window_start_ms)
    }

    /// Recording was turned off for a channel: stop the disk, keep viewers.
    pub fn monitor_clear(&self, login: &str) {
        let entries: Vec<(StreamId, Rc<StreamRuntime>)> = self
            .streams
            .borrow()
            .iter()
            .filter(|(_, runtime)| runtime.login == login)
            .map(|(id, runtime)| (id.clone(), Rc::clone(runtime)))
            .collect();
        for (id, runtime) in entries {
            self.stop_recording(&runtime);
            // The recording is what a monitor owns; a lane is another consumer
            // and keeps the runtime until it detaches.
            if runtime.viewers.get() > 0 {
                continue;
            }
            self.remove_runtime_id(&id, &runtime.key);
        }
    }

    /// Hermes `stream-down`. Recording runtimes stop through the scheduler's
    /// `ENDLIST` so the tail is not cut; watch-only runtimes stay for their
    /// viewers. The stop latch is per broadcast, so it is cleared here.
    pub fn stream_down(&self, login: &str) {
        self.update_row(login, |row| {
            row.stopped = None;
            // The broadcast is over; the variants and broadcast id are stale.
            row.data = None;
        });
        let entries: Vec<(StreamId, Rc<StreamRuntime>)> = self
            .streams
            .borrow()
            .iter()
            .filter(|(_, runtime)| runtime.login == login)
            .map(|(id, runtime)| (id.clone(), Rc::clone(runtime)))
            .collect();
        for (id, runtime) in entries {
            if runtime.is_recording() {
                continue;
            }
            // A lane is the consumer that keeps a watch-only runtime alive; the
            // broadcast ending is the client's cue to close it.
            if runtime.viewers.get() > 0 {
                continue;
            }
            self.remove_runtime_id(&id, &runtime.key);
        }
    }

    /// Stops serving. Without `force` a watched pipeline keeps running and
    /// only its recording stops; with `force` it is torn down, which leaves its
    /// lanes [`LaneState::Starved`](siphon_shared::riffle::LaneState::Starved)
    /// until they close.
    pub fn stop_pipeline(&self, key: &StreamKey, force: bool) {
        let runtime = self.runtime(key);
        if let Some(runtime) = &runtime {
            let stopped = NonZeroU64::new(runtime.broadcast_id);
            self.update_row_by_id(key.channel, |row| row.stopped = stopped);
        }
        let Some(runtime) = runtime else {
            return;
        };
        self.stop_recording(&runtime);
        if !force && runtime.viewers.get() > 0 {
            // A lane is watching: `force` is what disconnects viewers, so
            // without it the pipeline outlives its recording.
            return;
        }
        self.remove_runtime(key);
    }

    pub fn restart_pipeline(&self, key: &StreamKey) {
        self.update_row_by_id(key.channel, |row| row.stopped = None);
        self.remove_runtime(key);
        let wanted = self
            .login_of(key.channel)
            .is_some_and(|login| self.wanted(&login));
        if wanted {
            enqueue(&self.jobs, Job::Activate(key.clone()));
        }
    }

    pub const fn disconnect_viewers(&self, key: &StreamKey) {
        // Nothing can be watching until the Riffle media path exists, so there
        // is no viewer to disconnect. The intent stays accepted rather than
        // erroring on a surface the UI still offers.
        let _ = (self, key);
    }

    /// The disk recorder for `key` finished. Release the stream when nobody is
    /// watching it any more.
    fn finish_stream(&self, key: &StreamKey) {
        let Some(runtime) = self.runtime(key) else {
            return;
        };
        runtime.recording.borrow_mut().take();
        info!(
            login = %runtime.login,
            quality = key.quality.as_deref().unwrap_or("<first>"),
            viewers = runtime.viewers.get(),
            "daemon: recording finished"
        );
        // The recording was one consumer; a lane is another.
        if runtime.viewers.get() > 0 {
            self.publish_pipeline(&runtime);
            return;
        }
        self.remove_runtime(key);
    }

    fn remove_runtime(&self, key: &StreamKey) {
        let id = Self::stream_id(key);
        self.remove_runtime_id(&id, key);
    }

    fn remove_runtime_id(&self, id: &StreamId, key: &StreamKey) {
        let removed = self.streams.borrow_mut().remove(id);
        let Some(runtime) = removed else {
            return;
        };
        let login = runtime.login.clone();
        runtime.stop();
        // Publish the runtime's own key, not the caller's: a caller may hold an
        // unresolved one (stream id 0), and the UI keys its rows on what the
        // snapshot gave it.
        let _ = key;
        self.publish_removed(&runtime.key);
        self.prune_channel(&login);
    }

    fn prune_channel(&self, login: &str) {
        if self
            .streams
            .borrow()
            .values()
            .any(|runtime| runtime.login == login)
        {
            return;
        }
        let mut rows = self.channels.borrow_mut();
        let followed = rows.iter().any(|row| row.login == login && row.followed);
        if followed {
            // Keep the row, its preference and the fetched data: another
            // quality of the same broadcast may still need it.
            return;
        }
        if rows.iter().any(|row| row.login == login) {
            rows.retain(|row| row.login != login);
            debug!(login, "daemon: channel entry dropped");
        }
    }

    fn publish_pipeline(&self, runtime: &Rc<StreamRuntime>) {
        self.bus
            .publish(Update::PipelineUpsert(self.pipeline_view(runtime)));
    }

    fn publish_removed(&self, key: &StreamKey) {
        self.bus.publish(Update::PipelineRemove(key.clone()));
    }

    fn pipeline_view(&self, runtime: &Rc<StreamRuntime>) -> PipelineView {
        let metrics = &runtime.metrics;
        let last_error = metrics.last_error.borrow().clone();
        let live_edge_ms = runtime
            .live_window
            .borrow()
            .last()
            .map_or(0, |segment| segment.timestamp_ms + segment.duration_ms);
        let state = if runtime.stopped.get() {
            PipelineState::Stopped
        } else if metrics.segment_tail.get() > 0 {
            PipelineState::Live
        } else if last_error.is_some() {
            PipelineState::Failed
        } else {
            PipelineState::Starting
        };
        let recording = runtime.recording.borrow();
        let disk = recording.as_ref().map(|recording| DiskView {
            active: recording.metrics.active.get(),
            runs: recording.metrics.runs.get(),
            segments: recording.metrics.segments.get(),
            bytes: recording.metrics.bytes.get(),
            position_ms: recording.metrics.position_ms.get(),
            last_error: recording.metrics.last_error.borrow().clone(),
        });
        PipelineView {
            key: runtime.key.clone(),
            title: runtime.title.clone(),
            variant: metrics.variant(),
            state,
            pinned: recording.is_some() || self.wanted(&runtime.login),
            recording: recording.is_some(),
            sessions: runtime.viewers.get(),
            position_ms: live_edge_ms,
            live_edge_ms,
            segment_tail: metrics.segment_tail.get(),
            prefetch_tail: metrics.prefetch_tail.get(),
            window: metrics.window.get(),
            polls: metrics.polls.get(),
            poll_failures: metrics.poll_failures.get(),
            last_error,
            disk,
        }
    }

    /// Pipeline rows for the snapshot.
    #[must_use]
    pub fn views(&self) -> Vec<PipelineView> {
        self.streams
            .borrow()
            .values()
            .map(|runtime| self.pipeline_view(runtime))
            .collect()
    }

    #[must_use]
    pub fn detail(&self, key: &StreamKey, limit: usize) -> Option<StreamDetail> {
        let runtime = self.runtime(key)?;
        Some(StreamDetail {
            pipeline: self.pipeline_view(&runtime),
            segments: runtime.metrics.recent(limit),
        })
    }

    #[must_use]
    pub fn segments(
        &self,
        key: &StreamKey,
        limit: usize,
    ) -> Option<Vec<siphon_shared::riffle::SegmentRecord>> {
        self.runtime(key)
            .map(|runtime| runtime.metrics.recent(limit))
    }

    #[must_use]
    pub fn disk_view(&self, key: &StreamKey) -> Option<DiskView> {
        let runtime = self.runtime(key)?;
        let recording = runtime.recording.borrow();
        let metrics = recording.as_ref().map(|recording| &recording.metrics)?;
        Some(DiskView {
            active: metrics.active.get(),
            runs: metrics.runs.get(),
            segments: metrics.segments.get(),
            bytes: metrics.bytes.get(),
            position_ms: metrics.position_ms.get(),
            last_error: metrics.last_error.borrow().clone(),
        })
    }

    #[must_use]
    pub fn arena_summary(&self) -> ArenaSummary {
        ArenaSummary {
            live_slots: u32::try_from(self.arena.live_slots()).unwrap_or(u32::MAX),
            arenas: u32::try_from(self.arena.arena_count()).unwrap_or(u32::MAX),
            high_water: u32::try_from(self.arena.high_water()).unwrap_or(u32::MAX),
            alloc_failures: self.arena.alloc_failures(),
            spool_cap_failures: self.arena.spool_cap_failures(),
            backoffs: self.arena.backoffs(),
        }
    }

    #[must_use]
    pub fn downloader_summary(&self) -> DownloaderSummary {
        DownloaderSummary {
            queued: self.downloader_metrics.queued.get(),
            capacity: self.downloader_metrics.capacity,
            in_flight: self.downloader_metrics.in_flight.get(),
            retries: self.downloader_metrics.retries.get(),
        }
    }
}

/// Hands one job to the app loop.
///
/// The queue is unbounded and the loop drains it on every pass, so this neither
/// blocks nor drops a job. It is a free function because the disk watcher — a
/// spawned task, not the loop — is one of the callers.
fn enqueue(jobs: &kanal::Sender<Box<JobDone>>, job: Job) {
    let _ = jobs.send(Box::new(JobDone::Registry(job)));
}

/// The orientation the *other* recording would be, if the broadcast has one.
const fn other_orientation(kind: &VariantKind) -> Option<VariantKind> {
    match kind {
        VariantKind::Landscape => Some(VariantKind::Portrait),
        VariantKind::Portrait => Some(VariantKind::Landscape),
        VariantKind::Audio | VariantKind::ERmtp => None,
    }
}

/// A fixed DVR playlist for the mock edge, whose archive route is not Twitch's.
///
/// Keyed off the variant URL so there is no mode flag in the registry: anything
/// served by the edge `--debug` named gets the mock's DVR path, everything else
/// is probed the way a real broadcast is. Dev builds only.
#[cfg(debug_assertions)]
fn mock_dvr_url(variant: &Variant) -> Option<String> {
    let base = crate::constants::mock_base()?;
    variant.url.strip_prefix(base).map(|_| {
        let file = if variant.kind == VariantKind::Portrait {
            "portrait-dvr.m3u8"
        } else {
            "index-dvr.m3u8"
        };
        format!("{base}/{file}")
    })
}

#[cfg(not(debug_assertions))]
const fn mock_dvr_url(_: &Variant) -> Option<String> {
    None
}

/// `(login, quality)` collapsed to one gate key, so no string is allocated.
fn gate_key(login: &str, quality: Option<&str>) -> u64 {
    let login = xxhash_rust::xxh3::xxh3_64(login.as_bytes());
    let quality = quality.map_or(0, |quality| xxhash_rust::xxh3::xxh3_64(quality.as_bytes()));
    login ^ quality.rotate_left(32)
}

/// One async mutex per key, without three dependency crates.
///
/// Resolution is rare (one DESCRIBE per stream, one GQL round trip per channel),
/// so a keyed gate is enough: waiters are woken all at once and re-check, which
/// happens at most twice per resolution.
#[derive(Default)]
struct Gate {
    busy: RefCell<HashSet<u64>>,
    waiters: RefCell<Vec<Waker>>,
}

struct GateGuard<'a> {
    gate: &'a Gate,
    key: u64,
}

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        self.gate.busy.borrow_mut().remove(&self.key);
        for waker in self.gate.waiters.borrow_mut().drain(..) {
            waker.wake();
        }
    }
}

impl Gate {
    async fn acquire(&self, key: u64) -> GateGuard<'_> {
        loop {
            if self.busy.borrow_mut().insert(key) {
                return GateGuard { gate: self, key };
            }
            poll_fn(|context| {
                // Re-check before parking so a release cannot be lost.
                if !self.busy.borrow().contains(&key) {
                    return Poll::Ready(());
                }
                self.waiters.borrow_mut().push(context.waker().clone());
                Poll::Pending
            })
            .await;
        }
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::future::Future;
    use std::time::Duration;

    use futures_util::future::Either;
    use futures_util::future::select;
    use futures_util::pin_mut;

    use super::*;
    use crate::variant::debug_portrait_variant;
    use crate::variant::debug_variant;

    fn run<F: Future>(future: F) -> F::Output {
        compio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(future)
    }

    #[test]
    fn the_gate_serialises_the_same_key_only() {
        let gate = Gate::default();
        run(async {
            let first = gate.acquire(1).await;
            let other = gate.acquire(2).await;
            drop(other);

            // The same key must wait until the first guard is dropped.
            let wait = gate.acquire(1);
            let timeout = compio::time::sleep(Duration::from_millis(20));
            pin_mut!(wait, timeout);
            assert!(
                matches!(select(wait, timeout).await, Either::Right(_)),
                "a second resolution of the same key must wait"
            );

            drop(first);
            let second = gate.acquire(1).await;
            drop(second);
        });
    }

    /// A registry plus the app loop's job queue, so a test can perform the work
    /// the real loop would.
    fn new_registry(bus: &Rc<Bus>) -> (Registry, kanal::AsyncReceiver<Box<JobDone>>) {
        let (downloaders, _rx) = kanal::bounded(4);
        let (jobs_tx, jobs_rx) = kanal::unbounded();
        let registry = Registry::new(
            Rc::new(Client::new()),
            Rc::new(ArenaSet::new()),
            downloaders.to_async(),
            Rc::new(DownloaderMetrics::new(4)),
            Options {
                client_id: None,
                auth_token: None,
                low_latency: true,
                recording: Recording {
                    output: None,
                    dual_format: false,
                },
                notify: None,
                keep_runs: false,
            },
            Rc::clone(bus),
            jobs_tx,
        );
        (registry, jobs_rx.to_async())
    }

    #[test]
    fn set_record_starts_a_pipeline_and_publishes_it() {
        run(async {
            let bus = Bus::new();
            let (registry, jobs) = new_registry(&bus);
            registry.seed_channel("sunnys", 7, false, vec![debug_variant("mock")], 42);
            registry.set_record("sunnys", true);
            // The loop performs what the registry queues; here the test is it.
            let done = compio::time::timeout(Duration::from_secs(1), jobs.recv())
                .await
                .expect("activation queued")
                .expect("queue open");
            let JobDone::Registry(job) = *done else {
                panic!("the registry queues its own work");
            };
            registry.apply(job).await;
            let pending = bus.take_pending();
            assert!(
                pending.iter().any(|broadcast| matches!(
                    &broadcast.update,
                    Update::PipelineUpsert(view) if view.key.channel == 7
                )),
                "no pipeline update in {pending:?}"
            );
            assert_eq!(registry.views().len(), 1);
        });
    }

    #[test]
    fn attach_is_idempotent_and_the_last_detach_releases_the_pipeline() {
        run(async {
            let bus = Bus::new();
            let (registry, _jobs) = new_registry(&bus);
            registry.seed_channel("sunnys", 7, false, vec![debug_variant("mock")], 42);

            let key = StreamKey::new(7, None);
            let first = registry.attach(&key).await.expect("attach");
            assert_eq!(first.viewers.get(), 1);
            // A second lane takes a second claim on the same runtime.
            let second = registry.attach(&key).await.expect("attach");
            assert!(Rc::ptr_eq(&first, &second));
            assert_eq!(second.viewers.get(), 2);
            assert_eq!(registry.views().len(), 1);

            registry.detach(&key);
            assert_eq!(first.viewers.get(), 1);
            assert_eq!(registry.views().len(), 1);
            registry.detach(&key);
            assert!(registry.views().is_empty(), "the last lane releases it");
            // A lane whose pipeline is already gone is not an error.
            registry.detach(&key);

            assert!(matches!(
                registry.attach(&StreamKey::new(8, None)).await,
                Err(RpcError::NotFound)
            ));
        });
    }

    #[test]
    fn the_primary_recording_follows_the_broadcast_orientation() {
        // Dual format: the landscape source is the primary and portrait is the
        // second recording. Portrait only: portrait *is* the primary, and there
        // is no other orientation to add.
        let dual = vec![debug_variant("mock"), debug_portrait_variant("mock")];
        assert_eq!(
            best_variant(&dual).map(|variant| &variant.kind),
            Some(&VariantKind::Landscape)
        );
        let portrait_only = vec![debug_portrait_variant("mock")];
        assert_eq!(
            best_variant(&portrait_only).map(|variant| &variant.kind),
            Some(&VariantKind::Portrait)
        );
        assert!(
            other_orientation(&VariantKind::Portrait)
                .is_some_and(|kind| kind == VariantKind::Landscape)
        );
        assert!(other_orientation(&VariantKind::Audio).is_none());
    }

    #[test]
    fn quality_selects_the_variant_and_none_takes_the_first() {
        let variants = vec![
            debug_variant("mock"),
            Variant {
                id: "audio_only".into(),
                ..debug_variant("mock")
            },
        ];
        assert_eq!(
            choose_variant(&variants, Some("audio_only")).map(|variant| variant.id.as_ref()),
            Some("audio_only")
        );
        assert_eq!(
            choose_variant(&variants, None).map(|variant| variant.id.as_ref()),
            Some("1080p")
        );
        assert!(choose_variant(&variants, Some("720p60")).is_none());
    }
}
