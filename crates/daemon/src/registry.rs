//! The pipeline registry: one runtime per `(login, quality)`, the followed
//! channels with their record preference, and the watch/record lifecycle.
//!
//! A runtime exists only while it is recording or watched. Recording starts on
//! `stream-up` (or when record is toggled on), a DESCRIBE attaches the media
//! engine to an existing recording runtime, and the last viewer of a
//! non-recording runtime drops it together with the channel entry.
//!
//! The registry is still shared with the RTSP server (`Rc<Registry>`), so its
//! state is behind `RefCell`. Everything else funnels through the daemon's
//! single loop.

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

use futures_util::future::LocalBoxFuture;
use kanal::AsyncSender;
use siphon_shared::rpc::ArenaSummary;
use siphon_shared::rpc::DiskView;
use siphon_shared::rpc::DownloaderSummary;
use siphon_shared::rpc::LogLevel;
use siphon_shared::rpc::PipelineState;
use siphon_shared::rpc::PipelineView;
use siphon_shared::rpc::StreamDetail;
use siphon_shared::rpc::StreamKey;
use siphon_shared::rpc::Update;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::arena::ArenaSet;
use crate::bus::Bus;
use crate::disk::DiskOptions;
use crate::disk::DiskOutput;
use crate::downloader::ToDownloader;
use crate::http::Client;
use crate::metrics::DiskMetrics;
use crate::metrics::DownloaderMetrics;
use crate::metrics::StreamMetrics;
use crate::notify::Notifier;
use crate::rtsp::ResolveError;
use crate::rtsp::Stream;
use crate::rtsp::StreamResolver;
use crate::rtsp::dvr::Dvr;
use crate::rtsp::parse_stream_key;
use crate::scheduler::ConsumerRegistration;
use crate::scheduler::Segment;
use crate::scheduler::StreamScheduler;
use crate::spool::ConsumerWake;
use crate::spool::WakeSet;
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
    /// Disk recordings that finished, so the stream entry can be released.
    finished: kanal::Sender<StreamKey>,
    finished_rx: RefCell<Option<kanal::Receiver<StreamKey>>>,
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

impl StreamId {
    fn new(channel: u64, quality: Option<&str>) -> Self {
        Self {
            channel,
            quality: quality.map(Box::from),
        }
    }
}

/// One running pipeline.
struct StreamRuntime {
    key: StreamKey,
    /// The channel's login: display, logging and the DVR's archive lookup. Not
    /// identity; `key.channel` is.
    login: String,
    title: String,
    stream: Rc<Stream>,
    dvr: Rc<Dvr>,
    client: Rc<Client>,
    wakes: Rc<WakeSet>,
    live_window: Rc<RefCell<Vec<Segment>>>,
    registration: RefCell<Option<kanal::Sender<ConsumerRegistration>>>,
    reader_wake: Rc<ConsumerWake>,
    /// The disk recorder while this pipeline is recording.
    recording: RefCell<Option<Recorder>>,
    media_spawned: Cell<bool>,
    dvr_spawned: Cell<bool>,
    broadcast_id: u64,
    /// `STABLE-VARIANT-ID` of the variant this runtime serves; names its
    /// recording directory.
    variant: Box<str>,
    /// That variant's kind, so the second recording can be the other
    /// orientation.
    variant_kind: VariantKind,
    metrics: Rc<StreamMetrics>,
}

struct Recorder {
    disk: Rc<DiskOutput>,
    metrics: Rc<DiskMetrics>,
}

impl StreamRuntime {
    fn is_recording(&self) -> bool {
        self.recording.borrow().is_some()
    }

    fn stop(&self) {
        self.stream.stop();
        self.dvr.stop();
        self.reader_wake.wake();
        self.registration.borrow_mut().take();
        if let Some(recording) = self.recording.borrow_mut().take() {
            recording.disk.stop();
        }
    }

    /// Spawns the media engine if this runtime was monitor-only until now.
    /// Returns whether anything changed.
    fn upgrade(&self) -> bool {
        if self.media_spawned.replace(true) {
            return false;
        }
        self.ensure_dvr();
        let Some(registration) = self.registration.borrow().clone() else {
            return false;
        };
        crate::rtsp::spawn_media(
            Rc::clone(&self.stream),
            Rc::clone(&self.client),
            Rc::clone(&self.dvr),
            &self.wakes,
            &registration,
            Rc::clone(&self.live_window),
            &self.reader_wake,
        );
        true
    }

    fn ensure_dvr(&self) {
        if self.dvr_spawned.replace(true) {
            return;
        }
        crate::rtsp::dvr::spawn(
            Rc::clone(&self.client),
            Some(self.login.clone()),
            Rc::clone(&self.dvr),
        );
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
    ) -> Self {
        let (finished_tx, finished_rx) = kanal::unbounded();
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
            finished: finished_tx,
            finished_rx: RefCell::new(Some(finished_rx)),
        }
    }

    /// Mirrors the persisted follow list onto the channel table. Called at
    /// startup and whenever the UI adds, removes or toggles a channel.
    pub fn sync_channels(&self, channels: &[siphon_shared::rpc::Channel]) {
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

    /// Drain the recordings that finished. One task for the whole registry.
    pub fn watch_finished(self: &Rc<Self>) {
        let Some(rx) = self.finished_rx.borrow_mut().take() else {
            return;
        };
        let registry = Rc::clone(self);
        compio::runtime::spawn(async move {
            let rx = rx.to_async();
            while let Ok(key) = rx.recv().await {
                registry.finish_stream(&key);
            }
        })
        .detach();
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

    async fn resolve_uri(&self, uri: &str) -> Result<Rc<Stream>, ResolveError> {
        let parsed = parse_stream_key(uri).ok_or(ResolveError::BAD_REQUEST)?;
        let data = self.channel_data(&parsed.login).await?;
        let key = StreamKey::new(data.id, parsed.quality.clone());
        let id = Self::stream_id(&key);

        if let Some(runtime) = self.streams.borrow().get(&id).cloned() {
            if runtime.upgrade() {
                self.publish_pipeline(&runtime);
            }
            return Ok(Rc::clone(&runtime.stream));
        }

        let _guard = self
            .gate
            .acquire(gate_key(&parsed.login, key.quality.as_deref()))
            .await;

        // Another DESCRIBE may have created it while this one waited.
        if let Some(runtime) = self.streams.borrow().get(&id).cloned() {
            if runtime.upgrade() {
                self.publish_pipeline(&runtime);
            }
            return Ok(Rc::clone(&runtime.stream));
        }

        let Some(variant) = choose_variant(&data.variants, key.quality.as_deref()) else {
            debug!(
                login = %parsed.login,
                quality = key.quality.as_deref().unwrap_or("<first>"),
                "daemon: no such variant"
            );
            return Err(ResolveError::NOT_FOUND);
        };
        let runtime = self.spawn_runtime(&key, &data, variant, false);
        // A DESCRIBE is what a viewer is: attach the media engine now, before
        // the control connection waits for the first segment's codec config.
        runtime.upgrade();
        self.streams.borrow_mut().insert(id, Rc::clone(&runtime));
        self.publish_pipeline(&runtime);
        Ok(Rc::clone(&runtime.stream))
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
                warn!(%error, login, status = status.status, "daemon: channel resolution failed");
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
        let stream = Rc::new(Stream::new(key.clone(), data.title.clone()));
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
        compio::runtime::spawn(scheduler.main()).detach();

        let runtime = Rc::new(StreamRuntime {
            key: key.clone(),
            login: data.login.clone(),
            title: data.title.clone(),
            stream,
            dvr,
            client: Rc::clone(&self.client),
            wakes,
            live_window,
            registration: RefCell::new(Some(registration)),
            reader_wake,
            recording: RefCell::new(None),
            media_spawned: Cell::new(false),
            dvr_spawned: Cell::new(false),
            broadcast_id: data.broadcast_id,
            variant: variant.id.clone(),
            variant_kind: variant.kind.clone(),
            metrics,
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

        let finished = self.finished.clone();
        let key = runtime.key.clone();
        let watcher = Rc::clone(&disk);
        compio::runtime::spawn(async move {
            if let Some(error) = watcher.wait().await {
                error!(%error, "daemon: fatal disk output error");
                std::process::exit(1);
            }
            let _ = finished.send(key);
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
            // Nobody owns it but the primary recording; a viewer of that quality
            // keeps its pipeline alive.
            if !sibling.stream.has_sessions() {
                self.remove_runtime(&key);
            }
        }
    }

    /// The user's persisted record preference for a followed channel.
    fn wanted(&self, login: &str) -> bool {
        self.channel_row(login).is_some_and(|row| row.record)
    }

    /// Record toggled for a channel. Turning it off keeps a watched pipeline.
    pub fn set_record(self: &Rc<Self>, login: &str, record: bool) {
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
    pub fn monitor_live(self: &Rc<Self>, login: &str) {
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
        let key = StreamKey::new(channel, None);
        let registry = Rc::clone(self);
        compio::runtime::spawn(async move {
            registry.activate(key).await;
        })
        .detach();
    }

    /// Record the broadcast that is live right now, without persisting
    /// anything. An offline channel is a `NotFound`.
    pub fn record_once(self: &Rc<Self>, key: StreamKey) {
        if let Some(runtime) = self.runtime(&key) {
            self.start_recording(&runtime);
            return;
        }
        let registry = Rc::clone(self);
        compio::runtime::spawn(async move {
            registry.activate_once(key).await;
        })
        .detach();
    }

    async fn activate(self: Rc<Self>, key: StreamKey) {
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

    async fn activate_once(self: Rc<Self>, key: StreamKey) {
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
            if runtime.stream.has_sessions() {
                self.publish_pipeline(&runtime);
            } else {
                self.remove_runtime_id(&id, &runtime.key);
            }
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
            if runtime.is_recording() || runtime.stream.has_sessions() {
                continue;
            }
            self.remove_runtime_id(&id, &runtime.key);
        }
    }

    /// Stops serving. Without `force` a watched pipeline keeps running and
    /// only its recording stops; with `force` the sessions are disconnected.
    pub fn stop_pipeline(&self, key: &StreamKey, force: bool) {
        let runtime = self.runtime(key);
        if let Some(runtime) = &runtime {
            let stopped = NonZeroU64::new(runtime.broadcast_id);
            self.update_row_by_id(key.channel, |row| row.stopped = stopped);
        }
        let Some(runtime) = runtime else {
            return;
        };
        if force {
            runtime.stream.disconnect_viewers();
        } else if runtime.stream.has_sessions() {
            self.stop_recording(&runtime);
            self.publish_pipeline(&runtime);
            return;
        }
        self.remove_runtime(key);
    }

    pub fn restart_pipeline(self: &Rc<Self>, key: &StreamKey) {
        self.update_row_by_id(key.channel, |row| row.stopped = None);
        self.remove_runtime(key);
        let wanted = self
            .login_of(key.channel)
            .is_some_and(|login| self.wanted(&login));
        if wanted {
            let registry = Rc::clone(self);
            let key = key.clone();
            compio::runtime::spawn(async move {
                registry.activate(key).await;
            })
            .detach();
        }
    }

    pub fn disconnect_viewers(&self, key: &StreamKey) {
        if let Some(runtime) = self.runtime(key) {
            runtime.stream.disconnect_viewers();
        }
    }

    /// The disk recorder for `key` finished. Release the stream when nobody is
    /// watching it any more.
    fn finish_stream(&self, key: &StreamKey) {
        let Some(runtime) = self.runtime(key) else {
            return;
        };
        runtime.recording.borrow_mut().take();
        if runtime.stream.has_sessions() {
            self.publish_pipeline(&runtime);
            return;
        }
        info!(
            login = %runtime.login,
            quality = key.quality.as_deref().unwrap_or("<first>"),
            "daemon: recording finished, stream stopped"
        );
        self.remove_runtime(key);
    }

    /// The last viewer left. Keep a recording or wanted runtime.
    fn release_stream(&self, key: &StreamKey) {
        let Some(runtime) = self.runtime(key) else {
            return;
        };
        if runtime.stream.has_sessions() || runtime.is_recording() || self.wanted(&runtime.login) {
            return;
        }
        info!(
            login = %runtime.login,
            quality = key.quality.as_deref().unwrap_or("<first>"),
            "daemon: stream stopped, last viewer left"
        );
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
        self.publish_removed(key);
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
        let state = if runtime.stream.stopped() {
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
            sessions: runtime.stream.session_count(),
            position_ms: runtime.stream.position_ms(),
            live_edge_ms: runtime.stream.live_edge_ms(),
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
    ) -> Option<Vec<siphon_shared::rpc::SegmentRecord>> {
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
/// served by the mock gets the mock's DVR path, everything else is probed the
/// way a real broadcast is. Compiled out of release builds.
fn mock_dvr_url(variant: &Variant) -> Option<String> {
    #[cfg(debug_assertions)]
    {
        let base = crate::constants::MOCK_BASE;
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
    {
        let _ = variant;
        None
    }
}

impl StreamResolver for Registry {
    fn resolve<'a>(&'a self, uri: &'a str) -> LocalBoxFuture<'a, Result<Rc<Stream>, ResolveError>> {
        Box::pin(self.resolve_uri(uri))
    }

    fn release(&self, stream: &Rc<Stream>) {
        self.release_stream(stream.key());
    }
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

    #[test]
    fn set_record_starts_a_pipeline_and_publishes_it() {
        run(async {
            let client = Rc::new(Client::new());
            let (tx, _rx) = kanal::bounded(4);
            let bus = Bus::new();
            let registry = Rc::new(Registry::new(
                client,
                Rc::new(ArenaSet::new()),
                tx.to_async(),
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
                Rc::clone(&bus),
            ));
            registry.seed_channel("sunnys", 7, false, vec![debug_variant()], 42);
            registry.set_record("sunnys", true);
            compio::time::sleep(Duration::from_millis(20)).await;
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
    fn the_primary_recording_follows_the_broadcast_orientation() {
        // Dual format: the landscape source is the primary and portrait is the
        // second recording. Portrait only: portrait *is* the primary, and there
        // is no other orientation to add.
        let dual = vec![debug_variant(), debug_portrait_variant()];
        assert_eq!(
            best_variant(&dual).map(|variant| &variant.kind),
            Some(&VariantKind::Landscape)
        );
        let portrait_only = vec![debug_portrait_variant()];
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
            debug_variant(),
            Variant {
                id: "audio_only".into(),
                ..debug_variant()
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
