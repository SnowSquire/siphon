//! The Siphon backend: configuration, GQL, Hermes, notifications, the stream
//! registry and the admin connections.
//!
//! Everything that owns state runs in this one loop: admin connections, the
//! registry and the snapshot projection. Spawned components (schedulers, disk,
//! downloaders) publish through the [`Bus`], which the loop drains into every
//! connection. A snapshot is therefore a plain `&self` function, not a request.

use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;
use std::time::Instant;

use futures_util::FutureExt as _;
use futures_util::StreamExt as _;
use siphon_shared::rpc::ArenaSummary;
use siphon_shared::rpc::ChannelSwitches;
use siphon_shared::rpc::ChannelView;
use siphon_shared::rpc::Config;
use siphon_shared::rpc::ConnHealth;
use siphon_shared::rpc::DownloaderSummary;
use siphon_shared::rpc::LogLevel;
use siphon_shared::rpc::NotificationView;
use siphon_shared::rpc::PipelineView;
use siphon_shared::rpc::Request;
use siphon_shared::rpc::Response;
use siphon_shared::rpc::RpcError;
use siphon_shared::rpc::ServerMessage;
use siphon_shared::rpc::Snapshot;
use siphon_shared::rpc::StreamKey;
use siphon_shared::rpc::Update;
use siphon_shared::rpc::UpdateView;
use tracing::info;

use crate::balesh::CheapRng;
use crate::balesh::NanoId;
use crate::bus::Bus;
use crate::config;
use crate::config::ConfigError;
use crate::constants::NOTIFICATION_CAP;
use crate::gql;
use crate::gql::ChannelDetail;
use crate::gql::GqlError;
use crate::hermes::ConnectOutcome;
use crate::hermes::SubEntry;
use crate::hermes::SubState;
use crate::hermes::Topic;
use crate::http::Client;
use crate::http::ws::WsStream;
use crate::matcher::Matcher;
use crate::notifier;
use crate::registry::Registry;
use crate::rpc;
use crate::time::now_ms;
use crate::update;

/// One thing the daemon loop wakes for.
enum Event {
    /// One admin connection produced input or finished a write.
    Conn(rpc::ConnEvent),
    /// Hermes produced a text frame, or the socket is gone.
    Socket(Option<String>),
    /// A background job finished.
    Job(Option<Box<JobDone>>),
    /// Something published on the bus.
    Bus,
    /// The nearest deadline came due.
    Deadline,
}

/// A loop operation: connection I/O, jobs and the bus wake live in one set.
type Op = futures_util::future::LocalBoxFuture<'static, Event>;

fn accept_op(queue: &kanal::AsyncReceiver<rpc::AdminStream>) -> Op {
    let queue = queue.clone();
    Box::pin(async move { Event::Conn(rpc::accept(&queue).await) })
}

fn read_op(conn: usize, stream: Rc<rpc::AdminStream>) -> Op {
    Box::pin(async move { Event::Conn(rpc::read(conn, stream).await) })
}

fn write_op(conn: usize, stream: Rc<rpc::AdminStream>, buf: Vec<u8>) -> Op {
    Box::pin(async move { Event::Conn(rpc::write(conn, stream, buf).await) })
}

fn bus_op(bus: Rc<Bus>) -> Op {
    Box::pin(async move {
        bus.wait().await;
        Event::Bus
    })
}

/// The smallest free slot, so a closed connection's index is reused.
fn insert_conn(conns: &mut Vec<Option<rpc::Conn>>, conn: rpc::Conn) -> usize {
    if let Some(index) = conns.iter().position(Option::is_none) {
        conns[index] = Some(conn);
        index
    } else {
        conns.push(Some(conn));
        conns.len() - 1
    }
}

/// Everything the app needs at startup.
pub struct AppInit {
    pub config: Config,
    pub config_path: PathBuf,
    pub client: Rc<Client>,
    pub registry: Rc<Registry>,
    pub bus: Rc<Bus>,
    /// Accepted admin connections, from the listener task.
    pub accept: kanal::AsyncReceiver<rpc::AdminStream>,
    /// Finished background jobs.
    pub jobs: kanal::AsyncReceiver<Box<JobDone>>,
    pub jobs_tx: kanal::Sender<Box<JobDone>>,
    pub client_id: Option<String>,
}

/// One finished background job.
pub enum JobDone {
    AddResolved(
        String,
        Option<ChannelSwitches>,
        Box<Result<Option<ChannelDetail>, GqlError>>,
    ),
    LiveRefreshed(u64, Result<Vec<ChannelDetail>, GqlError>),
    SaveFinished(Result<(), ConfigError>),
    ToastShown,
    UpdateCheckFinished(Result<update::Release, update::UpdateError>),
    UpdateDownloadFinished(Result<PathBuf, update::UpdateError>),
    SubscribeCheckDue(Topic, NanoId, u64),
    ConnectFinished(Box<ConnectOutcome>),
    AvatarReady(u64, Option<String>),
}

/// One in-flight job. `!Send` is fine: everything stays on the reactor thread.
#[allow(clippy::struct_excessive_bools)]
pub struct App {
    pub(crate) config: Config,
    pub(crate) config_path: PathBuf,
    pub(crate) client: Rc<Client>,
    pub(crate) registry: Rc<Registry>,
    pub(crate) bus: Rc<Bus>,
    pub(crate) accept: kanal::AsyncReceiver<rpc::AdminStream>,
    pub(crate) jobs: kanal::AsyncReceiver<Box<JobDone>>,
    pub(crate) jobs_tx: kanal::Sender<Box<JobDone>>,
    /// One encoded frame reused for every connection.
    frame: Vec<u8>,
    /// When the next counter flush is due, while a UI is connected.
    telemetry_at: Instant,
    pub(crate) client_id: Option<String>,

    pub(crate) channels: HashMap<u64, ChannelDetail>,
    /// Avatar downloads in flight, so a refresh does not start a second one.
    pub(crate) avatars_pending: HashSet<u64>,
    pub(crate) pending: Vec<String>,
    pub(crate) conn: ConnHealth,
    /// Whether the Hermes socket should connect (off in debug unless a stand-in
    /// endpoint is configured).
    pub(crate) hermes_enabled: bool,
    pub(crate) error: Option<String>,

    pub(crate) matcher: Matcher,
    pub(crate) socket: Option<WsStream>,
    pub(crate) welcomed: bool,
    pub(crate) connecting: bool,
    pub(crate) keepalive_secs: u64,
    pub(crate) last_message: Instant,
    pub(crate) reconnect_at: Instant,
    pub(crate) reconnect_attempt: u32,
    pub(crate) quiet_until: HashMap<u64, Instant>,
    pub(crate) sub_ids: HashMap<Topic, SubEntry>,
    pub(crate) sub_gen: u64,
    pub(crate) rng: CheapRng,

    pub(crate) notifications: Vec<NotificationView>,
    pub(crate) next_notification: u64,

    pub(crate) update: Option<update::Release>,
    pub(crate) update_busy: bool,
    pub(crate) update_check_at: Instant,

    last_arena: ArenaSummary,
    last_downloader: DownloaderSummary,
    last_pipelines: HashMap<StreamKey, PipelineView>,

    pub(crate) pending_unsubscribes: Vec<(NanoId, Topic)>,

    save_dirty: bool,
    save_running: bool,
}

impl App {
    #[must_use]
    pub fn new(init: AppInit) -> Self {
        let AppInit {
            config,
            config_path,
            client,
            registry,
            bus,
            accept,
            jobs,
            jobs_tx,
            client_id,
        } = init;
        let matcher = crate::hermes::build_matcher(&config.filtered_words);
        // Hermes is wanted whenever there is a channel to watch; the endpoint is
        // whatever the edge is (Twitch, or the mock under `--debug`).
        let hermes_enabled = !config.channels.is_empty();
        let conn = if config.channels.is_empty() {
            ConnHealth::NoChannels
        } else {
            ConnHealth::Connecting
        };
        let channels: HashMap<u64, ChannelDetail> = HashMap::new();
        Self {
            config,
            config_path,
            client,
            registry,
            bus,
            accept,
            jobs,
            jobs_tx,
            frame: Vec::new(),
            telemetry_at: Instant::now(),
            client_id,
            channels,
            avatars_pending: HashSet::new(),
            pending: Vec::new(),
            conn,
            hermes_enabled,
            error: None,
            matcher,
            socket: None,
            welcomed: false,
            connecting: false,
            keepalive_secs: 15,
            last_message: Instant::now(),
            reconnect_at: Instant::now(),
            reconnect_attempt: 0,
            quiet_until: HashMap::new(),
            sub_ids: HashMap::new(),
            sub_gen: 0,
            rng: CheapRng::new(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0x853c_49e6_748f_ea9b, |elapsed| {
                        u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
                    }),
            ),
            notifications: Vec::new(),
            next_notification: 1,
            update: None,
            update_busy: false,
            update_check_at: Instant::now(),
            last_arena: ArenaSummary::default(),
            last_downloader: DownloaderSummary::default(),
            last_pipelines: HashMap::new(),
            pending_unsubscribes: Vec::new(),
            save_dirty: false,
            save_running: false,
        }
    }

    pub async fn run(mut self) {
        use futures_util::stream::FuturesUnordered;

        let mut conns: Vec<Option<rpc::Conn>> = Vec::new();
        let mut ops: FuturesUnordered<Op> = FuturesUnordered::new();
        ops.push(accept_op(&self.accept));
        ops.push(bus_op(Rc::clone(&self.bus)));

        loop {
            self.maybe_reconnect();
            if Instant::now() >= self.update_check_at {
                self.push_update_check();
            }

            let deadline = self.next_deadline(&conns);
            let event = {
                // Biased, socket first: a completed read is never dropped for a
                // connection event. The socket cannot move into the op set,
                // because one `WsStream` serves both directions and a send must
                // not queue behind a read.
                let socket_fut = async {
                    match self.socket.as_mut() {
                        Some(socket) => socket.next_text().await.ok().flatten(),
                        None => std::future::pending().await,
                    }
                }
                .fuse();
                let ops_fut = ops.next().fuse();
                let jobs = self.jobs.clone();
                let job_fut = jobs.recv().fuse();
                let timer = compio::time::sleep(deadline).fuse();

                futures_util::pin_mut!(socket_fut, ops_fut, job_fut, timer);
                futures_util::select_biased! {
                    message = socket_fut => Event::Socket(message),
                    some = ops_fut => match some {
                        Some(event) => event,
                        None => break,
                    },
                    job = job_fut => Event::Job(job.ok()),
                    () = timer => Event::Deadline,
                }
            };

            match event {
                Event::Conn(event) => self.handle_conn(event, &mut conns, &ops),
                Event::Socket(Some(text)) => self.handle_message(&text).await,
                Event::Socket(None) => {
                    tracing::info!(target: "hermes", "connection closed");
                    self.on_disconnect().await;
                }
                Event::Job(None) => break,
                Event::Job(Some(done)) => self.handle_job(*done).await,
                Event::Bus => ops.push(bus_op(Rc::clone(&self.bus))),
                Event::Deadline => self.on_tick().await,
            }

            self.flush_bus(&mut conns, &ops);
            self.flush_unsubscribes().await;
        }
    }

    /// Starts a Hermes connection if one is due. Called once per loop pass; it
    /// is cheap and makes the reconnect deadline a plain deadline.
    fn maybe_reconnect(&mut self) {
        if self.hermes_enabled
            && self.socket.is_none()
            && !self.connecting
            && !self.config.channels.is_empty()
            && Instant::now() >= self.reconnect_at
        {
            self.start_connect();
        }
    }

    fn next_deadline(&self, conns: &[Option<rpc::Conn>]) -> Duration {
        let now = Instant::now();
        let mut at = now + Duration::from_mins(1);
        if self.hermes_enabled
            && self.socket.is_none()
            && !self.connecting
            && !self.config.channels.is_empty()
        {
            at = at.min(self.reconnect_at);
        }
        at = at.min(self.update_check_at);
        if self.socket.is_some() {
            let alive = if self.welcomed {
                Duration::from_secs(self.keepalive_secs * crate::constants::HERMES_KEEPALIVE_MISSES)
            } else {
                crate::constants::HERMES_WELCOME_TIMEOUT
            };
            at = at.min(self.last_message + alive);
        }
        if conns.iter().flatten().any(|conn| conn.ready) {
            at = at.min(self.telemetry_at);
        }
        at.saturating_duration_since(now)
            .max(Duration::from_millis(10))
    }

    fn handle_conn(
        &mut self,
        event: rpc::ConnEvent,
        conns: &mut Vec<Option<rpc::Conn>>,
        ops: &futures_util::stream::FuturesUnordered<Op>,
    ) {
        match event {
            rpc::ConnEvent::Accepted(Ok(stream)) => {
                let id = insert_conn(conns, rpc::Conn::new(stream));
                let stream = conns[id].as_ref().map(|conn| Rc::clone(&conn.stream));
                if let Some(stream) = stream {
                    ops.push(read_op(id, stream));
                }
                ops.push(accept_op(&self.accept));
            }
            // The listener is gone; do not re-arm into a spin.
            rpc::ConnEvent::Accepted(Err(_)) => {}
            rpc::ConnEvent::Read {
                conn: id,
                result,
                buf,
            } => {
                match result {
                    Ok(0) | Err(_) => {
                        conns[id] = None;
                        return;
                    }
                    Ok(read) => {
                        if let Some(conn) = conns[id].as_mut() {
                            conn.decoder.push(&buf[..read]);
                        }
                    }
                }
                self.handle_frames(id, conns);
                if let Some(conn) = conns[id].as_ref() {
                    ops.push(read_op(id, Rc::clone(&conn.stream)));
                }
            }
            rpc::ConnEvent::Write {
                conn: id,
                result,
                mut buf,
            } => {
                let Some(conn) = conns[id].as_mut() else {
                    return;
                };
                conn.writing = false;
                if result.is_err() {
                    conns[id] = None;
                    return;
                }
                // `write_all` hands the buffer back untouched. It is the
                // *spare*, never `out`: bytes queued while the write was in
                // flight are still there and must not be replaced.
                buf.clear();
                conn.spare = buf;
            }
        }
    }

    /// Decodes and answers every complete frame already buffered.
    fn handle_frames(&mut self, id: usize, conns: &mut [Option<rpc::Conn>]) {
        let mut close = false;
        loop {
            let Some(conn) = conns[id].as_mut() else {
                return;
            };
            let payload = match conn.decoder.next_frame() {
                Ok(Some(payload)) => payload,
                Ok(None) => break,
                Err(_) => {
                    close = true;
                    break;
                }
            };
            match siphon_shared::rpc::codec::decode::<siphon_shared::rpc::ClientMessage>(&payload) {
                Ok(siphon_shared::rpc::ClientMessage::Hello {
                    magic,
                    protocol,
                    app_version,
                }) => {
                    if magic != siphon_shared::rpc::MAGIC
                        || protocol != siphon_shared::rpc::PROTOCOL_VERSION
                    {
                        let reason = format!(
                            "protocol {protocol} (backend speaks {})",
                            siphon_shared::rpc::PROTOCOL_VERSION
                        );
                        close = !conn.push(&ServerMessage::Rejected(reason));
                        break;
                    }
                    info!(%app_version, "rpc: client connected");
                    let snapshot = self.snapshot();
                    let queued = conn.push(&ServerMessage::Welcome {
                        protocol: siphon_shared::rpc::PROTOCOL_VERSION,
                        server_version: env!("CARGO_PKG_VERSION").to_owned(),
                    }) && conn.push(&ServerMessage::Snapshot(Box::new(snapshot)));
                    if !queued {
                        close = true;
                        break;
                    }
                    conn.ready = true;
                }
                Ok(siphon_shared::rpc::ClientMessage::Request {
                    id: request_id,
                    request,
                }) => {
                    let result = self.handle_request(request);
                    if !conn.push(&ServerMessage::Response {
                        id: request_id,
                        result,
                    }) {
                        close = true;
                        break;
                    }
                }
                Err(_) => {
                    close = true;
                    break;
                }
            }
        }
        if close {
            conns[id] = None;
        }
    }

    /// Encodes each queued update once and hands the bytes to every connection.
    fn flush_bus(
        &mut self,
        conns: &mut [Option<rpc::Conn>],
        ops: &futures_util::stream::FuturesUnordered<Op>,
    ) {
        for broadcast in self.bus.take_pending() {
            self.frame.clear();
            if siphon_shared::rpc::codec::encode(
                &ServerMessage::Update(broadcast.update),
                &mut self.frame,
            )
            .is_err()
            {
                continue;
            }
            for conn in conns.iter_mut().flatten() {
                if conn.ready {
                    conn.out.extend_from_slice(&self.frame);
                }
            }
        }
        for (id, slot) in conns.iter_mut().enumerate() {
            let Some(conn) = slot.as_mut() else {
                continue;
            };
            if conn.out.len() > rpc::MAX_OUT {
                *slot = None;
                continue;
            }
            if conn.ready && !conn.writing && !conn.out.is_empty() {
                conn.writing = true;
                let stream = Rc::clone(&conn.stream);
                std::mem::swap(&mut conn.out, &mut conn.spare);
                let buf = std::mem::take(&mut conn.spare);
                ops.push(write_op(id, stream, buf));
            }
        }
    }

    /// Runs one background future and delivers its result through the app
    /// queue, so no select ever polls a job future and drops a completed one.
    pub fn spawn_job<F: Future<Output = JobDone> + 'static>(&self, future: F) {
        let tx = self.jobs_tx.clone();
        compio::runtime::spawn(async move {
            let done = future.await;
            let _ = tx.send(Box::new(done));
        })
        .detach();
    }

    async fn handle_job(&mut self, done: JobDone) {
        match done {
            JobDone::AddResolved(login, switches, result) => {
                self.complete_add_login(login, switches, *result).await;
            }
            JobDone::LiveRefreshed(id, result) => {
                self.complete_live_refresh(id, result);
            }
            JobDone::SaveFinished(result) => {
                self.save_running = false;
                if let Err(error) = result {
                    self.set_error(format!("config save failed: {error}"));
                }
                if self.save_dirty {
                    self.spawn_save();
                }
            }
            JobDone::ToastShown => {}
            JobDone::UpdateCheckFinished(result) => {
                self.update_busy = false;
                match result {
                    Ok(release) => {
                        if update::newer_than_current(&release) {
                            tracing::info!(target: "update", "new release: {}", release.version);
                            self.update = Some(release.clone());
                            self.bus
                                .publish(Update::Update(Some(update_view(&release))));
                        } else if self.update.take().is_some() {
                            self.bus.publish(Update::Update(None));
                        }
                    }
                    Err(error) => {
                        tracing::info!(target: "update", "check failed: {error}");
                    }
                }
            }
            JobDone::UpdateDownloadFinished(result) => self.finish_update_download(result),
            JobDone::SubscribeCheckDue(topic, sub_id, attempt) => {
                let unconfirmed = matches!(
                    self.sub_ids.get(&topic),
                    Some(entry)
                        if entry.sub_id == sub_id
                            && entry.attempt == attempt
                            && entry.state == SubState::Pending
                );
                if unconfirmed {
                    // A live socket with a dead topic never heals on its own:
                    // run it through the socket reconnect so backoff and the
                    // welcome replay retry the topic.
                    tracing::info!(
                        target: "hermes",
                        "subscription to {topic} not confirmed, reconnecting"
                    );
                    if self.socket.is_some() {
                        self.on_disconnect().await;
                    }
                }
            }
            JobDone::ConnectFinished(outcome) => {
                self.complete_connect(*outcome).await;
            }
            JobDone::AvatarReady(id, path) => {
                self.avatars_pending.remove(&id);
                let Some(path) = path else {
                    return;
                };
                let flags = self
                    .config
                    .channels
                    .iter()
                    .find(|channel| channel.id == id)
                    .cloned();
                if let Some(user) = self.channels.get_mut(&id) {
                    user.avatar_path = Some(path);
                    let view = user.view(flags.as_ref());
                    self.bus.publish(Update::ChannelUpsert(Box::new(view)));
                }
            }
        }
    }

    fn handle_request(&mut self, request: Request) -> Result<Response, RpcError> {
        match request {
            Request::AddLogin { login, switches } => {
                self.request_add_login(&login, switches);
                Ok(Response::Ack)
            }
            Request::RemoveChannel(channel) => self.apply_remove_channel(channel),
            Request::SetNotifyTitleChanges(value) => {
                self.config.notify_title_changes = value;
                self.publish_config();
                self.queue_save();
                Ok(Response::Ack)
            }
            Request::SetSound(value) => {
                self.config.sound = value;
                self.publish_config();
                self.queue_save();
                Ok(Response::Ack)
            }
            Request::SetRecordDir(dir) => {
                // An empty path is how a settings field clears itself.
                let dir = dir.filter(|dir| !dir.trim().is_empty());
                self.config.record_dir.clone_from(&dir);
                self.registry.set_output(dir.map(PathBuf::from));
                self.publish_config();
                self.queue_save();
                Ok(Response::Ack)
            }
            Request::SetRecordDualFormat(value) => {
                self.config.record_dual_format = value;
                self.registry.set_dual_format(value);
                self.publish_config();
                self.queue_save();
                Ok(Response::Ack)
            }
            Request::AddFilteredWord(word) => Ok(self.apply_add_filtered_word(&word)),
            Request::RemoveFilteredWord(word) => self.apply_remove_filtered_word(&word),
            Request::SetRecord { channel, record } => self.apply_set_record(channel, record),
            Request::SetNotifyLive { channel, notify } => {
                self.apply_set_notify_live(channel, notify)
            }
            Request::SetNotifyTitles { channel, notify } => {
                self.apply_set_notify_titles(channel, notify)
            }
            Request::ClearError => {
                self.clear_error();
                Ok(Response::Ack)
            }
            Request::DismissNotification(id) => self.dismiss_notification(id),
            Request::ApplyUpdate => {
                self.handle_apply_update();
                Ok(Response::Ack)
            }
            Request::PipelineStop { key, force } => {
                self.registry.stop_pipeline(&key, force);
                Ok(Response::Ack)
            }
            Request::PipelineRestart(key) => {
                self.registry.restart_pipeline(&key);
                Ok(Response::Ack)
            }
            Request::DisconnectViewers(key) => {
                self.registry.disconnect_viewers(&key);
                Ok(Response::Ack)
            }
            Request::RecordOnce { login, quality } => self.apply_record_once(&login, quality),
            Request::GetSnapshot => Ok(Response::Snapshot(Box::new(self.snapshot()))),
            Request::GetStreamDetail(key) => self
                .registry
                .detail(&key, 256)
                .map_or(Err(RpcError::NotFound), |detail| {
                    Ok(Response::StreamDetail(Box::new(detail)))
                }),
            Request::ListSegments { key, limit } => self
                .registry
                .segments(&key, usize::try_from(limit).unwrap_or(usize::MAX))
                .map_or(Err(RpcError::NotFound), |segments| {
                    Ok(Response::Segments(segments))
                }),
            Request::GetArena => Ok(Response::Arena(self.registry.arena_summary())),
            Request::GetDownloader => Ok(Response::Downloader(self.registry.downloader_summary())),
            Request::GetDisk(key) => self
                .registry
                .disk_view(&key)
                .map_or(Err(RpcError::NotFound), |view| Ok(Response::Disk(view))),
        }
    }

    /// Snapshot built on demand from app state, the registry and metrics.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let mut channels = Vec::with_capacity(self.config.channels.len());
        for channel in &self.config.channels {
            let view = self.channels.get(&channel.id).map_or_else(
                || ChannelView {
                    id: channel.id,
                    login: channel.login.clone(),
                    display_name: channel
                        .display_name
                        .clone()
                        .unwrap_or_else(|| channel.login.clone()),
                    chat_color: gql::fallback_color(channel.id),
                    avatar_path: None,
                    live: false,
                    title: None,
                    game: None,
                    game_id: None,
                    viewers: None,
                    collaboration_viewers: None,
                    stream_start_ms: None,
                    last_live_ms: None,
                    stream_id: None,
                    error: None,
                    notify_live: channel.notify_live,
                    notify_titles: channel.notify_titles,
                    record: channel.record,
                },
                |detail| detail.view(Some(channel)),
            );
            channels.push(view);
        }
        Snapshot {
            config: self.config.clone(),
            conn: self.conn,
            channels,
            pending: self.pending.clone(),
            pipelines: self.registry.views(),
            arena: self.registry.arena_summary(),
            downloader: self.registry.downloader_summary(),
            notifications: self.notifications.clone(),
            logs: self.bus.logs(),
            error: self.error.clone(),
            update: self.update.as_ref().map(update_view),
        }
    }

    pub fn publish_config(&self) {
        self.bus.publish(Update::Config(self.config.clone()));
    }

    pub fn publish_pending(&self) {
        self.bus.publish(Update::Pending(self.pending.clone()));
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        let message = message.into();
        tracing::info!(target: "app", "{message}");
        self.bus.log(LogLevel::Error, "app", message.clone());
        self.error = Some(message.clone());
        self.bus.publish(Update::Error(Some(message)));
    }

    pub fn clear_error(&mut self) {
        if self.error.take().is_some() {
            self.bus.publish(Update::Error(None));
        }
    }

    /// Publishes telemetry that only the snapshot used to expose, so a UI
    /// showing live counters updates without polling.
    pub(crate) fn publish_telemetry(&mut self) {
        let arena = self.registry.arena_summary();
        if arena != self.last_arena {
            self.last_arena = arena;
            self.bus.publish(Update::Arena(arena));
        }
        let downloader = self.registry.downloader_summary();
        if downloader != self.last_downloader {
            self.last_downloader = downloader;
            self.bus.publish(Update::Downloader(downloader));
        }
        let views = self.registry.views();
        let mut seen = Vec::with_capacity(views.len());
        for view in views {
            let changed = self
                .last_pipelines
                .get(&view.key)
                .is_none_or(|old| old != &view);
            if changed {
                self.bus.publish(Update::PipelineUpsert(view.clone()));
            }
            seen.push(view.key.clone());
            self.last_pipelines.insert(view.key.clone(), view);
        }
        let removed: Vec<StreamKey> = self
            .last_pipelines
            .keys()
            .filter(|key| !seen.contains(key))
            .cloned()
            .collect();
        for key in removed {
            self.last_pipelines.remove(&key);
            self.bus.publish(Update::PipelineRemove(key));
        }
    }

    pub fn publish_conn_health(&mut self) {
        let conn = if self.socket.is_some() && self.welcomed {
            ConnHealth::Connected
        } else {
            ConnHealth::Connecting
        };
        self.conn = conn;
        self.bus.publish(Update::Conn(conn));
    }

    pub fn publish_no_channels(&mut self) {
        self.conn = ConnHealth::NoChannels;
        self.bus.publish(Update::Conn(ConnHealth::NoChannels));
    }

    fn queue_save(&mut self) {
        self.save_dirty = true;
        if !self.save_running {
            self.spawn_save();
        }
    }

    fn spawn_save(&mut self) {
        self.save_running = true;
        self.save_dirty = false;
        let config = self.config.clone();
        let path = self.config_path.clone();
        self.spawn_job(async move { JobDone::SaveFinished(config::save(&config, &path).await) });
    }

    pub fn queue_notification(
        &mut self,
        summary: String,
        body: String,
        image: Option<String>,
        login: Option<String>,
    ) {
        let id = self.next_notification;
        self.next_notification = self.next_notification.wrapping_add(1);
        let notification = NotificationView {
            id,
            at_ms: now_ms(),
            summary: summary.clone(),
            body: body.clone(),
            login: login.clone(),
            image: image.clone(),
            sound: self.config.sound,
        };
        self.notifications.push(notification.clone());
        if self.notifications.len() > NOTIFICATION_CAP {
            let oldest = self.notifications.remove(0);
            self.bus.publish(Update::NotificationDismissed(oldest.id));
        }
        self.bus.log_channel(
            LogLevel::Info,
            "notify",
            if body.is_empty() {
                summary.clone()
            } else {
                format!("{summary}: {body}")
            },
            login.clone(),
            None,
        );
        self.bus.publish(Update::Notification(notification));

        let client = Rc::clone(&self.client);
        let sound = self.config.sound;
        self.spawn_job(async move {
            notifier::show(
                &client,
                &summary,
                &body,
                sound,
                image.as_deref(),
                login.as_deref(),
            )
            .await;
            JobDone::ToastShown
        });
    }

    fn dismiss_notification(&mut self, id: u64) -> Result<Response, RpcError> {
        let before = self.notifications.len();
        self.notifications
            .retain(|notification| notification.id != id);
        if self.notifications.len() == before {
            return Err(RpcError::NotFound);
        }
        self.bus.publish(Update::NotificationDismissed(id));
        Ok(Response::Ack)
    }

    /// Validates an add request and queues the single resolve. Duplicates,
    /// in-flight resolves and empty input are ignored without touching disk.
    ///
    /// `switches` is how an undone removal comes back with the settings it
    /// had: the daemon applies them when the resolve lands, so the row never
    /// shows the defaults for a frame.
    pub fn request_add_login(
        &mut self,
        login: &str,
        switches: Option<ChannelSwitches>,
    ) -> Option<String> {
        let login = login.trim().to_lowercase();
        if login.chars().count() > 25 {
            self.set_error("a Twitch login is at most 25 characters");
            return None;
        }
        if login.is_empty()
            || self
                .config
                .channels
                .iter()
                .any(|channel| channel.login.eq_ignore_ascii_case(&login))
            || self
                .pending
                .iter()
                .any(|pending| pending.eq_ignore_ascii_case(&login))
        {
            return None;
        }
        self.pending.push(login.clone());
        self.clear_error();
        self.publish_pending();
        let client = Rc::clone(&self.client);
        let client_id = self.client_id.clone();
        let job_login = login.clone();
        self.spawn_job(async move {
            let result = gql::fetch_channel(&client, client_id.as_deref(), &job_login).await;
            JobDone::AddResolved(job_login, switches, Box::new(result))
        });
        Some(login)
    }

    pub(crate) async fn complete_add_login(
        &mut self,
        login: String,
        switches: Option<ChannelSwitches>,
        result: Result<Option<ChannelDetail>, GqlError>,
    ) {
        if !self
            .pending
            .iter()
            .any(|pending| pending.eq_ignore_ascii_case(&login))
        {
            return;
        }
        self.pending
            .retain(|pending| !pending.eq_ignore_ascii_case(&login));
        self.publish_pending();
        match result {
            Ok(Some(user)) => {
                tracing::info!(
                    target: "config",
                    "resolved {} to id {} ({})",
                    login,
                    user.id,
                    user.display_name
                );
                if self
                    .config
                    .channels
                    .iter()
                    .any(|existing| existing.id == user.id)
                {
                    return;
                }
                let id = user.id;
                let switches = switches.unwrap_or_default();
                self.bus.log(
                    LogLevel::Info,
                    "channel",
                    format!("added {} ({})", user.login, id),
                );
                self.config.channels.push(siphon_shared::rpc::Channel {
                    login: user.login.clone(),
                    id,
                    display_name: Some(user.display_name.clone()),
                    record: switches.record,
                    notify_live: switches.notify_live,
                    notify_titles: switches.notify_titles,
                });
                self.publish_config();
                self.track_channel(user);
                self.queue_save();
                self.ensure_subs(id);
                self.sync_channels();
                if switches.record && self.registry.can_record() {
                    self.registry.set_record(&login, true);
                }
                if self.welcomed {
                    self.subscribe_channel(id).await;
                }
            }
            Ok(None) => {
                tracing::info!(
                    target: "config",
                    "gql returned no channel named {login}, is it a typo?"
                );
                self.set_error(format!("channel {login} not found"));
            }
            Err(error) => {
                tracing::info!(target: "config", "channel resolution failed: {error}");
                self.set_error(format!("failed to resolve {login}: {error}"));
            }
        }
    }

    fn apply_remove_channel(&mut self, channel: u64) -> Result<Response, RpcError> {
        let Some(index) = self
            .config
            .channels
            .iter()
            .position(|candidate| candidate.id == channel)
        else {
            return Err(RpcError::NotFound);
        };
        let removed = self.config.channels.remove(index);
        self.registry.set_record(&removed.login, false);
        self.sync_channels();
        self.bus.log(
            LogLevel::Info,
            "channel",
            format!("removed {} ({})", removed.login, removed.id),
        );
        let removed = self.drop_channel(channel);
        self.pending_unsubscribes.extend(removed);
        self.publish_config();
        self.bus.publish(Update::ChannelRemove(channel));
        self.queue_save();
        Ok(Response::Ack)
    }

    /// Drops ids that no longer resolve (deleted/renamed): they disappear from
    /// the persisted list with an error instead of lingering.
    pub fn apply_prune_ids(&mut self, ids: &[u64]) {
        let pruned: Vec<String> = self
            .config
            .channels
            .iter()
            .filter(|channel| ids.contains(&channel.id))
            .map(|channel| {
                channel
                    .display_name
                    .clone()
                    .unwrap_or_else(|| channel.login.clone())
            })
            .collect();
        if pruned.is_empty() {
            return;
        }
        for id in ids {
            self.drop_channel(*id);
        }
        self.config
            .channels
            .retain(|channel| !ids.contains(&channel.id));
        self.publish_config();
        self.queue_save();
        let names = pruned.join(", ");
        if pruned.len() == 1 {
            self.set_error(format!("{names} no longer resolves and was removed"));
        } else {
            self.set_error(format!("{names} no longer resolve and were removed"));
        }
    }

    fn apply_add_filtered_word(&mut self, word: &str) -> Response {
        let word = word.trim().to_owned();
        if word.is_empty() {
            return Response::Ack;
        }
        let folded = word.to_lowercase();
        if self
            .config
            .filtered_words
            .iter()
            .any(|existing| existing.to_lowercase() == folded)
        {
            return Response::Ack;
        }
        self.config.filtered_words.push(word);
        self.config.version = Config::VERSION;
        self.matcher = crate::hermes::build_matcher(&self.config.filtered_words);
        self.publish_config();
        self.queue_save();
        Response::Ack
    }

    fn apply_remove_filtered_word(&mut self, word: &str) -> Result<Response, RpcError> {
        // By value, not by position: the view can be stale by the time the
        // click lands, and removing a position would delete a word the user
        // never saw.
        let Some(position) = self
            .config
            .filtered_words
            .iter()
            .position(|entry| entry == word)
        else {
            return Err(RpcError::Failed(format!("no filtered word {word:?}")));
        };
        self.config.filtered_words.remove(position);
        self.config.version = Config::VERSION;
        self.matcher = crate::hermes::build_matcher(&self.config.filtered_words);
        self.publish_config();
        self.queue_save();
        Ok(Response::Ack)
    }

    fn apply_record_once(
        &self,
        login: &str,
        quality: Option<String>,
    ) -> Result<Response, RpcError> {
        if !self.registry.can_record() {
            return Err(RpcError::Failed(
                "recording needs an output directory".to_owned(),
            ));
        }
        let login = login.trim().to_lowercase();
        if login.is_empty() {
            return Err(RpcError::BadRequest("empty login".to_owned()));
        }
        self.registry.record_once(StreamKey::new(login, quality));
        Ok(Response::Ack)
    }

    /// Mirrors the persisted follow list onto the registry's channel table.
    fn sync_channels(&self) {
        self.registry.sync_channels(&self.config.channels);
    }

    fn apply_set_record(&mut self, channel: u64, record: bool) -> Result<Response, RpcError> {
        let Some(entry) = self
            .config
            .channels
            .iter_mut()
            .find(|candidate| candidate.id == channel)
        else {
            return Err(RpcError::NotFound);
        };
        entry.record = record;
        let flags = entry.clone();
        let login = flags.login.clone();
        self.publish_config();
        self.publish_channel_flags(&flags);
        self.queue_save();
        self.bus.log(
            LogLevel::Info,
            "channel",
            format!(
                "recording {} for {login}",
                if record { "enabled" } else { "disabled" }
            ),
        );
        if record {
            if self.registry.can_record() {
                self.registry.set_record(&login, true);
            } else {
                self.set_error("recording needs an output directory");
            }
        } else {
            self.registry.set_record(&login, false);
        }
        Ok(Response::Ack)
    }

    fn apply_set_notify_live(&mut self, channel: u64, notify: bool) -> Result<Response, RpcError> {
        let Some(entry) = self
            .config
            .channels
            .iter_mut()
            .find(|candidate| candidate.id == channel)
        else {
            return Err(RpcError::NotFound);
        };
        entry.notify_live = notify;
        let flags = entry.clone();
        self.publish_config();
        self.publish_channel_flags(&flags);
        self.queue_save();
        Ok(Response::Ack)
    }

    fn apply_set_notify_titles(
        &mut self,
        channel: u64,
        notify: bool,
    ) -> Result<Response, RpcError> {
        let Some(entry) = self
            .config
            .channels
            .iter_mut()
            .find(|candidate| candidate.id == channel)
        else {
            return Err(RpcError::NotFound);
        };
        entry.notify_titles = notify;
        let flags = entry.clone();
        self.publish_config();
        self.publish_channel_flags(&flags);
        self.queue_save();
        Ok(Response::Ack)
    }

    /// Re-sends one channel row after a switch moved, so a controlled toggle
    /// in the UI follows the backend rather than snapping back.
    fn publish_channel_flags(&self, flags: &siphon_shared::rpc::Channel) {
        if let Some(detail) = self.channels.get(&flags.id) {
            self.bus
                .publish(Update::ChannelUpsert(Box::new(detail.view(Some(flags)))));
        }
    }

    fn handle_apply_update(&mut self) {
        let Some(offer) = self.update.clone() else {
            return;
        };
        if self.update_busy {
            return;
        }
        let installer = matches!(update::install_mode(), update::InstallMode::Msi)
            .then(|| offer.msi_url.clone())
            .flatten();
        if let Some(url) = installer {
            self.update_busy = true;
            let client = Rc::clone(&self.client);
            let version = offer.version;
            self.spawn_job(async move {
                JobDone::UpdateDownloadFinished(update::download_msi(&client, &url, &version).await)
            });
        } else {
            update::open_page(&offer.page_url);
        }
    }

    fn finish_update_download(&mut self, result: Result<PathBuf, update::UpdateError>) {
        match result {
            Ok(path) => match update::install_msi_and_relaunch(&path) {
                Ok(()) => {
                    tracing::info!(target: "update", "installer launched, stopping for upgrade");
                    // The RTSP listener has no shutdown path; exiting releases
                    // the binaries the installer replaces.
                    std::process::exit(0);
                }
                Err(error) => {
                    self.update_busy = false;
                    self.set_error(format!("couldn't launch installer: {error}"));
                }
            },
            Err(error) => {
                self.update_busy = false;
                self.set_error(format!("update download failed: {error}"));
            }
        }
    }

    pub fn push_update_check(&mut self) {
        self.update_check_at = Instant::now() + update::CHECK_INTERVAL;
        if self.update_busy {
            return;
        }
        self.update_busy = true;
        let client = Rc::clone(&self.client);
        self.spawn_job(
            async move { JobDone::UpdateCheckFinished(update::fetch_latest(&client).await) },
        );
    }
}

fn update_view(release: &update::Release) -> UpdateView {
    UpdateView {
        version: release.version.to_string(),
        page_url: release.page_url.clone(),
        installer: release.msi_url.is_some(),
    }
}
