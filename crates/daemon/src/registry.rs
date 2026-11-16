//! The pipeline registry: one runtime per `(login, quality)`, monitors as
//! desired state, and the watch/record lifecycle.
//!
//! A runtime exists only while it is recording or watched. Recording starts
//! on `stream-up` (or when record is toggled on), a DESCRIBE attaches the
//! media engine to an existing recording runtime, and the last viewer of a
//! non-recording runtime drops it together with the channel entry.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::poll_fn;
use std::path::PathBuf;
use std::rc::Rc;
use std::task::Poll;
use std::task::Waker;

use futures_util::future::LocalBoxFuture;
use kanal::AsyncSender;
use siphon_rpc::ArenaSummary;
use siphon_rpc::DiskView;
use siphon_rpc::DownloaderSummary;
use siphon_rpc::LogLevel;
use siphon_rpc::PipelineState;
use siphon_rpc::PipelineView;
use siphon_rpc::SegmentRecord;
use siphon_rpc::StreamDetail;
use siphon_rpc::StreamKey;
use siphon_rpc::Update;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::arena::ArenaSet;
use crate::bus::Bus;
use crate::constants::DEBUG_CHANNEL_TITLE;
use crate::constants::DEBUG_DVR_URL;
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
use crate::variant::VariantPlaylistError;
use crate::variant::choose_variant;
use crate::variant::debug_variant;
use crate::variant::get_variants;

/// Options shared by every stream the registry creates.
pub struct Options {
    pub client_id: Option<String>,
    pub auth_token: Option<String>,
    pub low_latency: bool,
    pub debug: bool,
    pub max_streams: usize,
    pub output: Option<PathBuf>,
    pub notify: Option<Notifier>,
    pub keep_runs: bool,
}

/// Everything the registry shares between streams.
pub struct Registry {
    client: Rc<Client>,
    arena: Rc<ArenaSet>,
    downloaders: AsyncSender<ToDownloader>,
    options: Options,
    bus: Rc<Bus>,
    downloader_metrics: Rc<DownloaderMetrics>,
    /// Resolved logins, kept while any stream of that channel exists.
    channels: RefCell<HashMap<String, Rc<Channel>>>,
    /// Live streams, by `(login, quality)`.
    streams: RefCell<HashMap<StreamKey, Rc<StreamRuntime>>>,
    /// Recording desired, whether or not a runtime exists yet.
    monitors: RefCell<HashSet<StreamKey>>,
    /// One-shot recordings: they end with the stream and are not persisted.
    once: RefCell<HashSet<StreamKey>>,
    /// Explicitly stopped pipelines; `stream-up` must not restart them.
    suppressed: RefCell<HashSet<StreamKey>>,
    gate: Gate,
    /// Disk recordings that finished, so the stream entry can be released.
    finished: kanal::Sender<StreamKey>,
    finished_rx: RefCell<Option<kanal::Receiver<StreamKey>>>,
}

/// A channel's cached resolution: the title, the variant list and Twitch's
/// broadcast id (the disk output's stream id).
struct Channel {
    title: String,
    variants: Vec<Variant>,
    broadcast_id: u64,
}

/// One running pipeline.
struct StreamRuntime {
    key: StreamKey,
    title: String,
    stream: Rc<Stream>,
    dvr: Rc<Dvr>,
    client: Rc<Client>,
    wakes: Rc<WakeSet>,
    live_window: Rc<RefCell<Vec<Segment>>>,
    registration: RefCell<Option<kanal::Sender<ConsumerRegistration>>>,
    reader_wake: Rc<ConsumerWake>,
    disk: RefCell<Option<Rc<DiskOutput>>>,
    disk_metrics: RefCell<Option<Rc<DiskMetrics>>>,
    recording: Cell<bool>,
    media_spawned: Cell<bool>,
    dvr_spawned: Cell<bool>,
    broadcast_id: u64,
    debug: bool,
    metrics: Rc<StreamMetrics>,
}

impl StreamRuntime {
    fn stop(&self) {
        self.stream.stop();
        self.dvr.stop();
        self.reader_wake.wake();
        self.registration.borrow_mut().take();
        if let Some(disk) = self.disk.borrow_mut().take() {
            disk.stop();
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
            (!self.debug).then(|| self.key.login.clone()),
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
        Self {
            client,
            arena,
            downloaders,
            options,
            bus,
            downloader_metrics,
            channels: RefCell::new(HashMap::new()),
            streams: RefCell::new(HashMap::new()),
            monitors: RefCell::new(HashSet::new()),
            once: RefCell::new(HashSet::new()),
            suppressed: RefCell::new(HashSet::new()),
            gate: Gate::default(),
            finished: finished_tx,
            finished_rx: RefCell::new(Some(finished_rx)),
        }
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
    pub const fn can_record(&self) -> bool {
        self.options.output.is_some()
    }

    async fn resolve_uri(&self, uri: &str) -> Result<Rc<Stream>, ResolveError> {
        let key = parse_stream_key(uri).ok_or(ResolveError::BAD_REQUEST)?;

        if let Some(runtime) = self.streams.borrow().get(&key).cloned() {
            if runtime.upgrade() {
                self.publish_pipeline(&runtime);
            }
            return Ok(Rc::clone(&runtime.stream));
        }

        let gate_key = format!(
            "stream:{}:{}",
            key.login,
            key.quality.as_deref().unwrap_or_default()
        );
        let _guard = self.gate.acquire(&gate_key).await;

        // Another DESCRIBE may have created it while this one waited.
        if let Some(runtime) = self.streams.borrow().get(&key).cloned() {
            if runtime.upgrade() {
                self.publish_pipeline(&runtime);
            }
            return Ok(Rc::clone(&runtime.stream));
        }

        let channel = self.channel(&key.login).await?;
        let Some(variant) = choose_variant(&channel.variants, key.quality.as_deref()) else {
            debug!(
                login = %key.login,
                quality = key.quality.as_deref().unwrap_or("<first>"),
                "daemon: no such variant"
            );
            return Err(ResolveError::NOT_FOUND);
        };
        self.ensure_capacity()?;
        // `spawn_runtime` is synchronous, so no other task can take the last
        // slot between the capacity check and the insert.
        let runtime = self.spawn_runtime(&key, &channel, variant, false);
        runtime.upgrade();
        self.streams
            .borrow_mut()
            .insert(key.clone(), Rc::clone(&runtime));
        self.publish_pipeline(&runtime);
        Ok(Rc::clone(&runtime.stream))
    }

    /// Get or build the channel entry. One GQL fetch per login, shared by every
    /// quality requested while the channel has streams.
    async fn channel(&self, login: &str) -> Result<Rc<Channel>, ResolveError> {
        if let Some(channel) = self.channels.borrow().get(login) {
            return Ok(Rc::clone(channel));
        }
        let _guard = self.gate.acquire(&format!("channel:{login}")).await;
        if let Some(channel) = self.channels.borrow().get(login) {
            return Ok(Rc::clone(channel));
        }

        let channel = if self.options.debug {
            Rc::new(Channel {
                title: DEBUG_CHANNEL_TITLE.to_owned(),
                variants: vec![debug_variant()],
                broadcast_id: 0,
            })
        } else {
            let resolved = get_variants(
                &self.client,
                self.options.client_id.as_deref(),
                self.options.auth_token.as_deref(),
                login,
                self.options.low_latency,
            )
            .await;
            match resolved {
                Ok((variants, broadcast_id)) => Rc::new(Channel {
                    title: login.to_owned(),
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
            }
        };

        self.channels
            .borrow_mut()
            .insert(login.to_owned(), Rc::clone(&channel));
        Ok(channel)
    }

    /// Drop idle streams, then refuse a new one when the limit is reached.
    ///
    /// A watched, recording or desired-monitor stream is never evicted.
    fn ensure_capacity(&self) -> Result<(), ResolveError> {
        let mut idle = Vec::new();
        {
            let monitors = self.monitors.borrow();
            self.streams.borrow_mut().retain(|key, runtime| {
                if runtime.stream.has_sessions()
                    || runtime.recording.get()
                    || monitors.contains(key)
                    || self.once.borrow().contains(key)
                {
                    return true;
                }
                idle.push((key.clone(), Rc::clone(runtime)));
                false
            });
        }
        for (key, runtime) in idle {
            info!(
                login = %key.login,
                quality = key.quality.as_deref().unwrap_or("<first>"),
                "daemon: dropping idle stream"
            );
            runtime.stop();
            self.publish_removed(&key);
            self.prune_channel(&key.login);
        }

        if self.streams.borrow().len() < self.options.max_streams {
            Ok(())
        } else {
            Err(ResolveError::UNAVAILABLE)
        }
    }

    fn spawn_runtime(
        &self,
        key: &StreamKey,
        channel: &Channel,
        variant: &Variant,
        record: bool,
    ) -> Rc<StreamRuntime> {
        let stream = Rc::new(Stream::new(key.clone(), channel.title.clone()));
        let metrics = StreamMetrics::new(
            key.clone(),
            Some(variant.id.to_string()),
            Rc::clone(&self.bus),
        );

        let dvr = Dvr::new(self.options.debug.then(|| DEBUG_DVR_URL.to_owned()));
        dvr.set_live_variant(&variant.id);

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
            title: channel.title.clone(),
            stream,
            dvr,
            client: Rc::clone(&self.client),
            wakes,
            live_window,
            registration: RefCell::new(Some(registration)),
            reader_wake,
            disk: RefCell::new(None),
            disk_metrics: RefCell::new(None),
            recording: Cell::new(false),
            media_spawned: Cell::new(false),
            dvr_spawned: Cell::new(false),
            broadcast_id: channel.broadcast_id,
            debug: self.options.debug,
            metrics,
        });

        if record {
            self.start_recording(&runtime);
        }

        info!(
            login = %key.login,
            quality = key.quality.as_deref().unwrap_or("<first>"),
            variant = %variant.id,
            recording = record,
            "daemon: stream started"
        );
        runtime
    }

    fn start_recording(&self, runtime: &Rc<StreamRuntime>) {
        if runtime.recording.replace(true) {
            return;
        }
        let Some(output) = self.options.output.clone() else {
            runtime.recording.set(false);
            self.bus
                .publish(Update::Error(Some("recording needs --output".to_owned())));
            return;
        };
        runtime.ensure_dvr();
        let Some(registration) = runtime.registration.borrow().clone() else {
            runtime.recording.set(false);
            return;
        };
        let metrics = Rc::new(DiskMetrics::new());
        *runtime.disk_metrics.borrow_mut() = Some(Rc::clone(&metrics));
        let disk = Rc::new(crate::disk::spawn_disk_output(
            DiskOptions {
                output,
                stream_id: runtime.broadcast_id,
                keep_runs: self.options.keep_runs,
                notifier: self.options.notify.clone(),
                dvr: Rc::clone(&runtime.dvr),
                client: Rc::clone(&runtime.client),
                metrics,
            },
            &runtime.wakes,
            &self.downloaders,
            &registration,
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

        *runtime.disk.borrow_mut() = Some(disk);
        info!(
            login = %runtime.key.login,
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
            Some(runtime.key.login.clone()),
            Some(runtime.broadcast_id),
            None,
        );
        self.publish_pipeline(runtime);
    }

    fn stop_recording(&self, runtime: &Rc<StreamRuntime>) {
        if !runtime.recording.replace(false) {
            return;
        }
        if let Some(disk) = runtime.disk.borrow_mut().take() {
            disk.stop();
        }
        self.bus.log_channel(
            LogLevel::Info,
            "recording",
            "recording stopped",
            Some(runtime.key.login.clone()),
            Some(runtime.broadcast_id),
            None,
        );
        self.publish_pipeline(runtime);
    }

    fn wanted(&self, key: &StreamKey) -> bool {
        self.monitors.borrow().contains(key) || self.once.borrow().contains(key)
    }

    /// A channel went live with recording on: start its monitor runtime now.
    pub fn monitor_set(self: &Rc<Self>, login: &str) {
        let key = StreamKey::new(login, None);
        self.suppressed.borrow_mut().remove(&key);
        self.monitors.borrow_mut().insert(key.clone());
        if let Some(runtime) = self.streams.borrow().get(&key).cloned() {
            self.start_recording(&runtime);
            return;
        }
        let registry = Rc::clone(self);
        compio::runtime::spawn(async move {
            registry.activate_monitor(key).await;
        })
        .detach();
    }

    /// Record one stream without persisting a monitor.
    pub fn record_once(self: &Rc<Self>, key: StreamKey) {
        self.suppressed.borrow_mut().remove(&key);
        self.once.borrow_mut().insert(key.clone());
        if let Some(runtime) = self.streams.borrow().get(&key).cloned() {
            self.start_recording(&runtime);
            return;
        }
        let registry = Rc::clone(self);
        compio::runtime::spawn(async move {
            registry.activate_monitor(key).await;
        })
        .detach();
    }

    async fn activate_monitor(self: Rc<Self>, key: StreamKey) {
        let gate_key = format!(
            "stream:{}:{}",
            key.login,
            key.quality.as_deref().unwrap_or_default()
        );
        let _guard = self.gate.acquire(&gate_key).await;
        if !self.wanted(&key)
            || self.suppressed.borrow().contains(&key)
            || self.streams.borrow().contains_key(&key)
        {
            return;
        }
        if self.options.output.is_none() {
            self.bus
                .publish(Update::Error(Some("recording needs --output".to_owned())));
            return;
        }
        let Ok(channel) = self.channel(&key.login).await else {
            self.bus.publish(Update::Error(Some(format!(
                "recording {}: channel is offline",
                key.login
            ))));
            return;
        };
        let Some(variant) = best_variant(&channel.variants) else {
            self.bus.publish(Update::Error(Some(format!(
                "recording {}: no variant",
                key.login
            ))));
            return;
        };
        if self.streams.borrow().len() >= self.options.max_streams {
            self.bus.publish(Update::Error(Some(format!(
                "recording {}: stream capacity reached",
                key.login
            ))));
            return;
        }
        let runtime = self.spawn_runtime(&key, &channel, variant, true);
        self.streams.borrow_mut().insert(key, Rc::clone(&runtime));
        self.publish_pipeline(&runtime);
    }

    /// Recording was turned off for a channel: stop the disk, keep viewers.
    pub fn monitor_clear(&self, login: &str) {
        let keys: Vec<StreamKey> = {
            let mut monitors = self.monitors.borrow_mut();
            let keys = monitors
                .iter()
                .filter(|key| key.login == login)
                .cloned()
                .collect::<Vec<_>>();
            for key in &keys {
                monitors.remove(key);
            }
            keys
        };
        for key in keys {
            let runtime = self.streams.borrow().get(&key).cloned();
            if let Some(runtime) = runtime {
                self.stop_recording(&runtime);
                if runtime.stream.has_sessions() || self.once.borrow().contains(&key) {
                    self.publish_pipeline(&runtime);
                } else {
                    self.remove_runtime(&key);
                }
            }
        }
    }

    /// Hermes `stream-down`. Recording runtimes stop through the scheduler's
    /// `ENDLIST` so the tail is not cut; watch-only runtimes stay for their
    /// viewers.
    pub fn stream_down(&self, login: &str) {
        self.once.borrow_mut().retain(|key| key.login != login);
        self.suppressed
            .borrow_mut()
            .retain(|key| key.login != login);
        let keys: Vec<StreamKey> = self
            .streams
            .borrow()
            .keys()
            .filter(|key| key.login == login)
            .cloned()
            .collect();
        for key in keys {
            let runtime = self.streams.borrow().get(&key).cloned();
            let Some(runtime) = runtime else { continue };
            if runtime.recording.get() || runtime.stream.has_sessions() {
                continue;
            }
            self.remove_runtime(&key);
        }
    }

    /// Stops serving. Without `force` a watched pipeline keeps running and
    /// only its recording stops; with `force` the sessions are disconnected.
    pub fn stop_pipeline(&self, key: &StreamKey, force: bool) {
        self.suppressed.borrow_mut().insert(key.clone());
        let runtime = self.streams.borrow().get(key).cloned();
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
        self.suppressed.borrow_mut().remove(key);
        self.remove_runtime(key);
        if self.wanted(key) {
            let registry = Rc::clone(self);
            let key = key.clone();
            compio::runtime::spawn(async move {
                registry.activate_monitor(key).await;
            })
            .detach();
        }
    }

    pub fn disconnect_viewers(&self, key: &StreamKey) {
        if let Some(runtime) = self.streams.borrow().get(key) {
            runtime.stream.disconnect_viewers();
        }
    }

    /// The disk recorder for `key` finished. Release the stream when nobody is
    /// watching it any more.
    fn finish_stream(&self, key: &StreamKey) {
        let runtime = self.streams.borrow().get(key).cloned();
        let Some(runtime) = runtime else {
            return;
        };
        runtime.recording.set(false);
        runtime.disk.borrow_mut().take();
        if runtime.stream.has_sessions() {
            self.publish_pipeline(&runtime);
            return;
        }
        info!(
            login = %key.login,
            quality = key.quality.as_deref().unwrap_or("<first>"),
            "daemon: recording finished, stream stopped"
        );
        self.remove_runtime(key);
    }

    /// The last viewer left. Keep a recording or monitored runtime.
    fn release_stream(&self, key: &StreamKey) {
        let keep = self.streams.borrow().get(key).is_none_or(|runtime| {
            runtime.stream.has_sessions() || runtime.recording.get() || self.wanted(key)
        });
        if keep {
            return;
        }
        info!(
            login = %key.login,
            quality = key.quality.as_deref().unwrap_or("<first>"),
            "daemon: stream stopped, last viewer left"
        );
        self.remove_runtime(key);
    }

    fn remove_runtime(&self, key: &StreamKey) {
        self.once.borrow_mut().remove(key);
        let removed = self.streams.borrow_mut().remove(key);
        let Some(runtime) = removed else {
            return;
        };
        runtime.stop();
        self.publish_removed(key);
        self.prune_channel(&key.login);
    }

    fn prune_channel(&self, login: &str) {
        if self
            .streams
            .borrow()
            .keys()
            .any(|stream| stream.login == login)
        {
            return;
        }
        if self.channels.borrow_mut().remove(login).is_some() {
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
        PipelineView {
            key: runtime.key.clone(),
            title: runtime.title.clone(),
            variant: metrics.variant(),
            state,
            pinned: runtime.recording.get() || self.wanted(&runtime.key),
            recording: runtime.recording.get(),
            sessions: runtime.stream.session_count(),
            position_ms: runtime.stream.position_ms(),
            live_edge_ms: runtime.stream.live_edge_ms(),
            segment_tail: metrics.segment_tail.get(),
            prefetch_tail: metrics.prefetch_tail.get(),
            window: metrics.window.get(),
            polls: metrics.polls.get(),
            poll_failures: metrics.poll_failures.get(),
            last_error,
            disk: runtime
                .disk_metrics
                .borrow()
                .as_ref()
                .map(|metrics| DiskView {
                    active: metrics.active.get(),
                    runs: metrics.runs.get(),
                    segments: metrics.segments.get(),
                    bytes: metrics.bytes.get(),
                    position_ms: metrics.position_ms.get(),
                    last_error: metrics.last_error.borrow().clone(),
                }),
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
        let runtime = self.streams.borrow().get(key).cloned()?;
        Some(StreamDetail {
            pipeline: self.pipeline_view(&runtime),
            segments: runtime.metrics.recent(limit),
        })
    }

    #[must_use]
    pub fn disk_view(&self, key: &StreamKey) -> Option<DiskView> {
        let runtime = self.streams.borrow().get(key).cloned()?;
        let metrics = runtime.disk_metrics.borrow().clone()?;
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
    pub fn segments(&self, key: &StreamKey, limit: usize) -> Option<Vec<SegmentRecord>> {
        self.streams
            .borrow()
            .get(key)
            .map(|runtime| runtime.metrics.recent(limit))
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

/// The best variant by Twitch's `SCORE`; the playlist is sorted descending,
/// but selecting explicitly keeps a monitor off a first-listed audio track.
fn best_variant(variants: &[Variant]) -> Option<&Variant> {
    variants
        .iter()
        .max_by_key(|variant| variant.score.map_or(0, std::num::NonZeroU16::get))
}

impl StreamResolver for Registry {
    fn resolve<'a>(&'a self, uri: &'a str) -> LocalBoxFuture<'a, Result<Rc<Stream>, ResolveError>> {
        Box::pin(self.resolve_uri(uri))
    }

    fn release(&self, stream: &Rc<Stream>) {
        self.release_stream(stream.key());
    }
}

/// One async mutex per key, without three dependency crates.
///
/// Resolution is rare (one DESCRIBE per stream, one GQL round trip per channel),
/// so a keyed gate is enough: waiters are woken all at once and re-check, which
/// happens at most twice per resolution.
#[derive(Default)]
struct Gate {
    busy: RefCell<HashSet<String>>,
    waiters: RefCell<Vec<Waker>>,
}

struct GateGuard<'a> {
    gate: &'a Gate,
    key: String,
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
    async fn acquire(&self, key: &str) -> GateGuard<'_> {
        loop {
            if self.busy.borrow_mut().insert(key.to_owned()) {
                return GateGuard {
                    gate: self,
                    key: key.to_owned(),
                };
            }
            poll_fn(|context| {
                // Re-check before parking so a release cannot be lost.
                if !self.busy.borrow().contains(key) {
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

    fn run<F: Future>(future: F) -> F::Output {
        compio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(future)
    }

    #[test]
    fn the_gate_serialises_the_same_key_only() {
        let gate = Gate::default();
        run(async {
            let first = gate.acquire("a").await;
            let other = gate.acquire("b").await;
            drop(other);

            // The same key must wait until the first guard is dropped.
            let wait = gate.acquire("a");
            let timeout = compio::time::sleep(Duration::from_millis(20));
            pin_mut!(wait, timeout);
            assert!(
                matches!(select(wait, timeout).await, Either::Right(_)),
                "a second resolution of the same key must wait"
            );

            drop(first);
            let second = gate.acquire("a").await;
            drop(second);
        });
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
