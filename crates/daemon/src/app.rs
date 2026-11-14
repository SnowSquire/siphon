//! The Siphon backend: configuration, GQL, Hermes, notifications, monitors and
//! the snapshot/update projection.
//!
//! One task owns all mutable state. RPC connections send it [`AppEvent`]s and
//! subscribe to the [`Bus`]; no `RefCell` borrow is ever held across an
//! `.await`.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;
use std::time::Instant;

use futures_util::FutureExt as _;
use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use siphon_rpc::ChannelView;
use siphon_rpc::Config;
use siphon_rpc::ConnHealth;
use siphon_rpc::NotificationView;
use siphon_rpc::Request;
use siphon_rpc::Response;
use siphon_rpc::RpcError;
use siphon_rpc::ServerMessage;
use siphon_rpc::Snapshot;
use siphon_rpc::Update;
use siphon_rpc::UpdateView;

use crate::balesh::CheapRng;
use crate::balesh::NanoId;
use crate::bus::Bus;
use crate::config;
use crate::config::ConfigError;
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
use crate::time::now_ms;
use crate::update;

/// One event the app loop multiplexes.
enum Event {
    App(AppEvent),
    Text(String),
    SocketClosed,
    Tick,
    Job(Box<JobDone>),
}

/// Most notifications kept for the UI center.
const NOTIFICATION_CAP: usize = 50;

/// One RPC connection's request, or its first snapshot.
pub enum AppEvent {
    Snapshot(kanal::Sender<Snapshot>),
    Request {
        id: u64,
        request: Request,
        control: kanal::Sender<ServerMessage>,
    },
}

/// What the RPC server tasks use to reach the app.
#[derive(Clone)]
pub struct AppHandle {
    tx: kanal::Sender<AppEvent>,
    bus: Rc<Bus>,
}

impl AppHandle {
    #[must_use]
    pub const fn new(tx: kanal::Sender<AppEvent>, bus: Rc<Bus>) -> Self {
        Self { tx, bus }
    }

    /// The bus this connection subscribes to.
    #[must_use]
    pub fn bus(&self) -> Rc<Bus> {
        Rc::clone(&self.bus)
    }

    /// Forwards one request; the app answers through `control`.
    pub fn request(&self, id: u64, request: Request, control: &kanal::Sender<ServerMessage>) {
        let _ = self.tx.try_send(AppEvent::Request {
            id,
            request,
            control: control.clone(),
        });
    }

    /// Asks the app for a fresh snapshot. `None` means it did not answer.
    pub async fn snapshot(&self) -> Option<Snapshot> {
        let (tx, rx) = kanal::bounded(1);
        self.tx.try_send(AppEvent::Snapshot(tx)).ok()?;
        compio::time::timeout(Duration::from_secs(5), rx.to_async().recv())
            .await
            .ok()?
            .ok()
    }
}

/// Everything the app needs at startup.
pub struct AppInit {
    pub config: Config,
    pub config_path: PathBuf,
    pub client: Rc<Client>,
    pub registry: Rc<Registry>,
    pub bus: Rc<Bus>,
    pub events: kanal::AsyncReceiver<AppEvent>,
    pub client_id: Option<String>,
    pub debug: bool,
}

/// One finished background job.
pub enum JobDone {
    AddResolved(String, Result<Option<ChannelDetail>, GqlError>),
    LiveRefreshed(u64, Result<Vec<ChannelDetail>, GqlError>),
    SaveFinished(Result<(), ConfigError>),
    ToastShown,
    UpdateCheckFinished(Result<update::Release, update::UpdateError>),
    UpdateDownloadFinished(Result<PathBuf, update::UpdateError>),
    SubscribeCheckDue(Topic, NanoId, u64),
    ConnectFinished(Box<ConnectOutcome>),
}

/// One in-flight job. `!Send` is fine: everything stays on the reactor thread.
pub type JobFuture = Pin<Box<dyn Future<Output = JobDone>>>;

#[allow(clippy::struct_excessive_bools)]
pub struct App {
    pub(crate) config: Config,
    pub(crate) config_path: PathBuf,
    pub(crate) client: Rc<Client>,
    pub(crate) registry: Rc<Registry>,
    pub(crate) bus: Rc<Bus>,
    pub(crate) events: kanal::AsyncReceiver<AppEvent>,
    pub(crate) client_id: Option<String>,
    pub(crate) debug: bool,

    pub(crate) channels: HashMap<u64, ChannelDetail>,
    pub(crate) pending: Vec<String>,
    pub(crate) conn: ConnHealth,
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

    pub(crate) jobs: FuturesUnordered<JobFuture>,
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
            events,
            client_id,
            debug,
        } = init;
        let matcher = crate::hermes::build_matcher(&config.filtered_words);
        let conn = if debug {
            ConnHealth::Connected
        } else if config.channels.is_empty() {
            ConnHealth::NoChannels
        } else {
            ConnHealth::Connecting
        };
        Self {
            config,
            config_path,
            client,
            registry,
            bus,
            events,
            client_id,
            debug,
            channels: HashMap::new(),
            pending: Vec::new(),
            conn,
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
            jobs: FuturesUnordered::new(),
            pending_unsubscribes: Vec::new(),
            save_dirty: false,
            save_running: false,
        }
    }

    pub async fn run(mut self) {
        let mut tick = compio::time::interval(Duration::from_secs(1));
        loop {
            if !self.debug
                && self.socket.is_none()
                && !self.connecting
                && !self.config.channels.is_empty()
                && Instant::now() >= self.reconnect_at
            {
                self.start_connect();
            }
            if Instant::now() >= self.update_check_at {
                self.push_update_check();
            }

            let event = {
                let app_fut = self.events.recv().fuse();
                let socket_fut = async {
                    match self.socket.as_mut() {
                        Some(socket) => socket.next_text().await,
                        None => std::future::pending().await,
                    }
                }
                .fuse();
                let tick_fut = tick.tick().fuse();
                let job_fut = Self::next_job(&mut self.jobs).fuse();

                futures_util::pin_mut!(app_fut, socket_fut, tick_fut, job_fut);
                futures_util::select! {
                    event = app_fut => match event {
                        Ok(event) => Event::App(event),
                        Err(_) => break,
                    },
                    message = socket_fut => match message {
                        Ok(Some(text)) => Event::Text(text),
                        Ok(None) => {
                            tracing::info!(target: "hermes", "connection closed");
                            Event::SocketClosed
                        }
                        Err(error) => {
                            tracing::info!(target: "hermes", "read error: {error}");
                            Event::SocketClosed
                        }
                    },
                    _ = tick_fut => Event::Tick,
                    done = job_fut => match done {
                        Some(done) => Event::Job(Box::new(done)),
                        None => continue,
                    },
                }
            };
            match event {
                Event::App(AppEvent::Snapshot(reply)) => {
                    let _ = reply.try_send(self.snapshot());
                }
                Event::App(AppEvent::Request {
                    id,
                    request,
                    control,
                }) => self.handle_request(id, request, &control),
                Event::Text(text) => self.handle_message(&text).await,
                Event::SocketClosed => self.on_disconnect().await,
                Event::Tick => self.on_tick().await,
                Event::Job(done) => self.handle_job(*done).await,
            }
            self.flush_unsubscribes().await;
        }
    }

    async fn next_job(jobs: &mut FuturesUnordered<JobFuture>) -> Option<JobDone> {
        if jobs.is_empty() {
            std::future::pending::<()>().await;
            None
        } else {
            jobs.next().await
        }
    }

    async fn handle_job(&mut self, done: JobDone) {
        match done {
            JobDone::AddResolved(login, result) => {
                self.complete_add_login(login, result).await;
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
        }
    }

    fn handle_request(
        &mut self,
        id: u64,
        request: Request,
        control: &kanal::Sender<ServerMessage>,
    ) {
        let result = match request {
            Request::AddLogin(login) => {
                self.request_add_login(&login);
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
            Request::AddFilteredWord(word) => Ok(self.apply_add_filtered_word(&word)),
            Request::RemoveFilteredWord(index) => {
                self.apply_remove_filtered_word(usize::try_from(index).unwrap_or(usize::MAX))
            }
            Request::SetRecord { channel, record } => self.apply_set_record(channel, record),
            Request::ClearError => {
                self.clear_error();
                Ok(Response::Ack)
            }
            Request::DismissNotification(id) => self.dismiss_notification(id),
            Request::ApplyUpdate => {
                self.handle_apply_update();
                Ok(Response::Ack)
            }
            Request::PipelineStop(key) => {
                self.registry.stop_pipeline(&key);
                Ok(Response::Ack)
            }
            Request::PipelineRestart(key) => {
                self.registry.restart_pipeline(&key);
                Ok(Response::Ack)
            }
            Request::GetSnapshot => Ok(Response::Snapshot(Box::new(self.snapshot()))),
            Request::GetStreamDetail(key) => self
                .registry
                .detail(&key, 256)
                .map_or(Err(RpcError::NotFound), |detail| {
                    Ok(Response::StreamDetail(detail))
                }),
            Request::ListSegments { key, limit } => self
                .registry
                .segments(&key, usize::try_from(limit).unwrap_or(usize::MAX))
                .map_or(Err(RpcError::NotFound), |segments| {
                    Ok(Response::Segments(segments))
                }),
            Request::GetArena => Ok(Response::Arena(self.registry.arena_summary())),
            Request::GetDownloader => Ok(Response::Downloader(self.registry.downloader_summary())),
            Request::GetDisk(_) => Err(RpcError::Unsupported),
        };
        let _ = control.try_send(ServerMessage::Response { id, result });
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
                    profile_image_url: String::new(),
                    live: false,
                    title: None,
                    game: None,
                    viewers: None,
                    collaboration_viewers: None,
                    stream_start_ms: None,
                },
                ChannelDetail::view,
            );
            channels.push(view);
        }
        Snapshot {
            revision: self.bus.revision(),
            config: self.config.clone(),
            conn: self.conn,
            channels,
            pending: self.pending.clone(),
            pipelines: self.registry.views(),
            arena: self.registry.arena_summary(),
            downloader: self.registry.downloader_summary(),
            notifications: self.notifications.clone(),
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
        self.error = Some(message.clone());
        self.bus.publish(Update::Error(Some(message)));
    }

    pub fn clear_error(&mut self) {
        if self.error.take().is_some() {
            self.bus.publish(Update::Error(None));
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
        self.jobs.push(Box::pin(async move {
            JobDone::SaveFinished(config::save(&config, &path).await)
        }));
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
        self.bus.publish(Update::Notification(notification));

        let client = Rc::clone(&self.client);
        let sound = self.config.sound;
        self.jobs.push(Box::pin(async move {
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
        }));
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
    pub fn request_add_login(&mut self, login: &str) -> Option<String> {
        let login = login.trim().to_lowercase();
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
        self.jobs.push(Box::pin(async move {
            let result = gql::fetch_channel(&client, client_id.as_deref(), &job_login).await;
            JobDone::AddResolved(job_login, result)
        }));
        Some(login)
    }

    pub(crate) async fn complete_add_login(
        &mut self,
        login: String,
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
                self.config.channels.push(siphon_rpc::Channel {
                    login: user.login.clone(),
                    id,
                    display_name: Some(user.display_name.clone()),
                    record: false,
                });
                self.publish_config();
                self.track_channel(user);
                self.queue_save();
                self.ensure_subs(id);
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
        self.registry.monitor_clear(&removed.login);
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

    fn apply_remove_filtered_word(&mut self, index: usize) -> Result<Response, RpcError> {
        if index >= self.config.filtered_words.len() {
            return Err(RpcError::BadRequest(
                "filtered word out of range".to_owned(),
            ));
        }
        self.config.filtered_words.remove(index);
        self.config.version = Config::VERSION;
        self.matcher = crate::hermes::build_matcher(&self.config.filtered_words);
        self.publish_config();
        self.queue_save();
        Ok(Response::Ack)
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
        let login = entry.login.clone();
        self.publish_config();
        self.queue_save();
        if record {
            if self.registry.can_record() {
                self.registry.monitor_set(&login);
            } else {
                self.set_error("recording needs --output");
            }
        } else {
            self.registry.monitor_clear(&login);
        }
        Ok(Response::Ack)
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
            self.jobs.push(Box::pin(async move {
                JobDone::UpdateDownloadFinished(update::download_msi(&client, &url, &version).await)
            }));
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
        if self.debug || self.update_busy {
            return;
        }
        self.update_busy = true;
        let client = Rc::clone(&self.client);
        self.jobs.push(Box::pin(async move {
            JobDone::UpdateCheckFinished(update::fetch_latest(&client).await)
        }));
    }
}

fn update_view(release: &update::Release) -> UpdateView {
    UpdateView {
        version: release.version.to_string(),
        page_url: release.page_url.clone(),
        installer: release.msi_url.is_some(),
    }
}
