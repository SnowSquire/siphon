use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use kanal::Receiver;
use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tokio_tungstenite::{connect_async, tungstenite::Message, WebSocketStream};

use crate::gql::{self, Game, User};
use crate::logging::log;
use crate::notifier;
use crate::AppState;

const HERMES_URL: &str = "wss://hermes.twitch.tv/v1?clientId=kimne78kx3ncx6brgo4mv6wki5h1ko";
const WELCOME_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(15);
const SOCKET_POLL: Duration = Duration::from_millis(500);
const KEEPALIVE_GRACE_MULTIPLIER: u64 = 2;
const TOPIC_PREFIXES: [&str; 2] = ["broadcast-settings-update", "video-playback-by-id"];
const ID_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// A channel as configured. `id` is None when gql has not resolved the login
/// yet; those entries cannot be subscribed and show up as unresolved.
pub struct ChannelEntry {
    pub login: String,
    pub id: Option<u64>,
    pub display_name: Option<String>,
}

/// One imperative mutation, sent by the ui layer instead of diffing whole
/// settings snapshots against the session state.
pub enum Command {
    AddChannel(ChannelEntry),
    RemoveChannel(String),
    SetNotifyTitleChanges(bool),
    SetSound(bool),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SubState {
    Pending,
    Subscribed,
    Failed,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SubStatus {
    Pending,
    Connected,
    Failed,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelStatus {
    pub channel_id: u64,
    pub login: String,
    pub display_name: String,
    pub title_status: SubStatus,
    pub live_status: SubStatus,
}

/// Snapshot of everything the ui needs to render the channel list and the
/// connection state; pushed to the frontend on every state change via the
/// `hermes-status` event and also served through the `get_status` command.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusEvent {
    pub connected: bool,
    pub error: Option<String>,
    pub channels: Vec<ChannelStatus>,
    pub unresolved: Vec<String>,
}

struct Subscription {
    topic: String,
    state: SubState,
}

/// Placeholder user tracked from configuration until the first gql fetch
/// refreshes it.
fn user_for(entry: &ChannelEntry, id: u64) -> User {
    User {
        channel_id: id,
        channel_name: entry.login.clone(),
        channel_display_name: entry
            .display_name
            .clone()
            .unwrap_or_else(|| entry.login.clone()),
        profile_image_url: String::new(),
        stream_id: 0,
        stream_title: None,
        stream_start: None,
        game: None,
        live: false,
    }
}

type WsStream = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub fn spawn<R: Runtime>(app: AppHandle<R>, command_rx: Receiver<Command>) {
    tauri::async_runtime::spawn(async move {
        let mut session = Session::new(app, command_rx);
        session.main_loop().await;
    });
}

/// Notification preferences the ui can change at runtime.
struct Preferences {
    notify_title_changes: bool,
    sound: bool,
}

struct Session<R: Runtime> {
    app: AppHandle<R>,
    command_rx: Receiver<Command>,
    channels: Vec<ChannelEntry>,
    preferences: Preferences,
    infos: Vec<User>,
    subs: HashMap<String, Subscription>,
    socket: Option<WsStream>,
    current_url: String,
    recovery_url: Option<String>,
    got_welcome: bool,
    keepalive_secs: u64,
    last_message: Instant,
    welcome_deadline: Option<Instant>,
    reconnect_at: Instant,
    reconnect_attempt: u32,
    failed_recoveries: u32,
    resubscribe_on_welcome: bool,
    rng: u64,
}

impl<R: Runtime> Session<R> {
    fn new(app: AppHandle<R>, command_rx: Receiver<Command>) -> Self {
        let rng = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0x9E37_79B9_7F4A_7C15, |elapsed| {
                elapsed.subsec_nanos() as u64 ^ elapsed.as_secs()
            })
            | 1;
        Self {
            app,
            command_rx,
            channels: Vec::new(),
            preferences: Preferences {
                notify_title_changes: true,
                sound: true,
            },
            infos: Vec::new(),
            subs: HashMap::new(),
            socket: None,
            current_url: HERMES_URL.into(),
            recovery_url: None,
            got_welcome: false,
            keepalive_secs: 15,
            last_message: Instant::now(),
            welcome_deadline: None,
            reconnect_at: Instant::now(),
            reconnect_attempt: 0,
            failed_recoveries: 0,
            resubscribe_on_welcome: true,
            rng,
        }
    }

    async fn main_loop(&mut self) {
        log("hermes", "worker started");
        loop {
            self.poll_commands().await;
            if self.channels.is_empty() {
                // nothing to watch; a session with no active subscriptions
                // would just time out, so stay disconnected until the config
                // gains channels
                self.close_socket().await;
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            if self.socket.is_some() {
                self.pump_socket().await;
            } else if Instant::now() >= self.reconnect_at {
                self.establish_connection().await;
            }
            if self.socket.is_none() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    /// Applies the newest commands received from the ui. Additions are
    /// subscribed on the current connection and removals unsubscribed,
    /// without ever restarting the websocket session.
    async fn poll_commands(&mut self) {
        while let Ok(Some(command)) = self.command_rx.try_recv() {
            match command {
                Command::AddChannel(entry) => self.add_channel(entry).await,
                Command::RemoveChannel(login) => self.remove_channel(&login).await,
                Command::SetNotifyTitleChanges(value) => {
                    log("hermes", format!("notify_title_changes={value}"));
                    self.preferences.notify_title_changes = value;
                }
                Command::SetSound(value) => {
                    log("hermes", format!("sound={value}"));
                    self.preferences.sound = value;
                }
            }
        }
    }

    async fn add_channel(&mut self, entry: ChannelEntry) {
        if self
            .channels
            .iter()
            .any(|configured| configured.login.eq_ignore_ascii_case(&entry.login))
        {
            return;
        }
        log("hermes", format!("channel {} added", entry.login));
        self.channels.push(ChannelEntry {
            login: entry.login.clone(),
            id: entry.id,
            display_name: entry.display_name.clone(),
        });
        // one gql request resolves an unresolved login and fetches the new
        // channel's info in the same response; a resolved id fetches the
        // info directly. the two topics are subscribed right after.
        match entry.id {
            Some(id) => self.refresh(&[id], &[]).await,
            None => self.refresh(&[], std::slice::from_ref(&entry.login)).await,
        }
        if self.got_welcome {
            // only the new channel's topics are pending here
            self.send_pending().await;
        }
        self.emit_status(None);
    }

    async fn remove_channel(&mut self, login: &str) {
        let Some(index) = self
            .channels
            .iter()
            .position(|configured| configured.login.eq_ignore_ascii_case(login))
        else {
            return;
        };
        let entry = self.channels.remove(index);
        log("hermes", format!("channel {} removed", entry.login));
        if let Some(id) = entry.id {
            for prefix in TOPIC_PREFIXES {
                self.unsubscribe_topic(&format!("{prefix}.{id}")).await;
            }
            self.infos.retain(|info| info.channel_id != id);
        }
        self.emit_status(None);
    }

    async fn establish_connection(&mut self) {
        // one gql fetch resolves unknown logins and refreshes tracked info
        let ids: Vec<u64> = self
            .channels
            .iter()
            .filter_map(|entry| entry.id)
            .collect();
        let logins: Vec<String> = self
            .channels
            .iter()
            .filter(|entry| entry.id.is_none())
            .map(|entry| entry.login.clone())
            .collect();
        self.refresh(&ids, &logins).await;
        if self.infos.is_empty() {
            // connecting with no active subscriptions would just time out
            log(
                "hermes",
                format!(
                    "no channels resolved for {:?}, retrying (typo in a login?)",
                    self.channels
                        .iter()
                        .map(|entry| entry.login.as_str())
                        .collect::<Vec<_>>()
                ),
            );
            self.emit_status(Some("no channels found for the configured logins"));
            self.schedule_reconnect();
            return;
        }
        log("hermes", format!("connecting to {}", self.current_url));
        match connect_async(self.current_url.as_str()).await {
            Ok((socket, _)) => {
                log("hermes", "connected, waiting for welcome");
                self.socket = Some(socket);
                self.got_welcome = false;
                self.last_message = Instant::now();
                self.welcome_deadline = Some(Instant::now() + WELCOME_TIMEOUT);
            }
            Err(error) => {
                log(
                    "hermes",
                    format!("connect to {} failed: {error}", self.current_url),
                );
                self.on_disconnect().await;
            }
        }
    }

    /// One gql fetch serves resolution and info tracking at once: unresolved
    /// logins gain their id, tracked infos gain fresh title/game/avatar/live
    /// data. Ids and logins land in a single http request (two operations).
    async fn refresh(&mut self, ids: &[u64], logins: &[String]) {
        // local state is aligned first so a failed fetch still leaves every
        // known id tracked (and subscribed once the session welcomes)
        self.sync_infos_and_subs();
        if ids.is_empty() && logins.is_empty() {
            return;
        }
        match gql::fetch_users(ids, logins).await {
            Ok(users) => {
                for entry in &mut self.channels {
                    if entry.id.is_some() {
                        continue;
                    }
                    match users
                        .iter()
                        .find(|user| user.channel_name.eq_ignore_ascii_case(&entry.login))
                    {
                        Some(user) => {
                            log(
                                "hermes",
                                format!("resolved {} to id {}", entry.login, user.channel_id),
                            );
                            entry.login = user.channel_name.clone();
                            entry.id = Some(user.channel_id);
                            entry.display_name = Some(user.channel_display_name.clone());
                        }
                        None => {
                            log(
                                "hermes",
                                format!(
                                    "gql returned no channel named {}, is it a typo?",
                                    entry.login
                                ),
                            );
                        }
                    }
                }
                // newly resolved entries only exist as infos after this
                self.sync_infos_and_subs();
                for info in &mut self.infos {
                    if let Some(fresh) =
                        users.iter().find(|user| user.channel_id == info.channel_id)
                    {
                        *info = fresh.clone();
                    }
                }
            }
            Err(error) => log("hermes", format!("failed to resolve channels: {error}")),
        }
    }

    /// Makes sure runtime state (infos + subscription entries) exists for
    /// every resolved config channel; reconnects replay these.
    fn sync_infos_and_subs(&mut self) {
        let entries: Vec<(String, u64, Option<String>)> = self
            .channels
            .iter()
            .filter_map(|entry| Some((entry.login.clone(), entry.id?, entry.display_name.clone())))
            .collect();
        for (login, id, display_name) in entries {
            if let Some(info) = self.infos.iter_mut().find(|info| info.channel_id == id) {
                info.channel_name = login;
                if let Some(display_name) = display_name {
                    info.channel_display_name = display_name;
                }
            } else {
                log("hermes", format!("tracking {login}({id})"));
                self.infos.push(user_for(
                    &ChannelEntry {
                        login,
                        id: Some(id),
                        display_name,
                    },
                    id,
                ));
            }
            self.create_subs(id);
        }
    }

    /// Mirrors the TS client's onClose: prefer session recovery via the
    /// server-persisted subscription set, retry recovery once, otherwise
    /// reconnect fresh and replay all subscriptions locally.
    async fn on_disconnect(&mut self) {
        self.close_socket().await;
        self.welcome_deadline = None;
        // The server keeps the session's subscription set alive, so
        // reconnecting through the recovery URL restores subscriptions
        // without resubscribing. A fresh welcome (with a new recovery URL)
        // arrives after recovery; a recovery attempt that fails before
        // welcome gets exactly one retry before the session is abandoned.
        let use_recovery = if self.got_welcome {
            self.failed_recoveries = 0;
            true
        } else if self.current_url == HERMES_URL {
            false
        } else {
            self.failed_recoveries += 1;
            self.failed_recoveries < 2
        };
        if use_recovery {
            if let Some(url) = self.recovery_url.clone() {
                self.current_url = url;
            }
        } else {
            // no session to recover: start fresh and replay all subscriptions
            self.current_url = HERMES_URL.into();
            self.resubscribe_on_welcome = true;
        }
        self.emit_status(None);
        self.schedule_reconnect();
    }

    fn schedule_reconnect(&mut self) {
        self.reconnect_attempt += 1;
        let delay = Duration::from_millis(500)
            .saturating_mul(1 << (self.reconnect_attempt.min(10) - 1))
            .min(MAX_RECONNECT_DELAY);
        log(
            "hermes",
            format!(
                "disconnected, {} in {delay:?}",
                if self.resubscribe_on_welcome {
                    "reconnecting fresh"
                } else {
                    "recovering session"
                }
            ),
        );
        self.reconnect_at = Instant::now() + delay;
    }

    async fn close_socket(&mut self) {
        if let Some(mut socket) = self.socket.take() {
            let _ = socket.close(None).await;
        }
    }

    async fn pump_socket(&mut self) {
        let now = Instant::now();
        if let Some(deadline) = self.welcome_deadline {
            if now >= deadline {
                // the server never sent a welcome; treat the connection as dead
                log("hermes", "no welcome within 10s, dropping connection");
                self.on_disconnect().await;
                return;
            }
        }
        let Some(socket) = self.socket.as_mut() else {
            return;
        };
        let message = tokio::time::timeout(SOCKET_POLL, socket.next()).await;
        match message {
            Ok(Some(Ok(message))) => match message {
                Message::Close(_) => {
                    log("hermes", "server sent close frame");
                    self.on_disconnect().await;
                }
                // text and binary frames carry the same json payloads
                message => {
                    if let Ok(text) = message.into_text() {
                        self.handle_message(&text).await;
                    }
                }
            },
            Ok(Some(Err(error))) => {
                log("hermes", format!("read error: {error}"));
                self.on_disconnect().await;
            }
            Ok(None) => {
                log("hermes", "connection closed");
                self.on_disconnect().await;
            }
            Err(_) => {
                // idle tick; consider the connection dead after two missed
                // keepalives
                if self.got_welcome
                    && now.duration_since(self.last_message)
                        > Duration::from_secs(self.keepalive_secs * KEEPALIVE_GRACE_MULTIPLIER)
                {
                    log(
                        "hermes",
                        format!(
                            "no keepalive for {}s, dropping connection",
                            self.keepalive_secs * KEEPALIVE_GRACE_MULTIPLIER
                        ),
                    );
                    self.on_disconnect().await;
                }
            }
        }
    }

    async fn handle_message(&mut self, text: &str) {
        let Ok(message) = serde_json::from_str::<Value>(text) else {
            log("hermes", format!("unparseable message: {text}"));
            return;
        };
        self.last_message = Instant::now();
        match message["type"].as_str() {
            Some("welcome") => {
                self.welcome_deadline = None;
                self.got_welcome = true;
                self.reconnect_attempt = 0;
                self.keepalive_secs = message["welcome"]["keepaliveSec"].as_u64().unwrap_or(15);
                self.recovery_url = message["welcome"]["recoveryUrl"].as_str().map(String::from);
                log(
                    "hermes",
                    format!(
                        "welcome: keepalive={}s, recovery_url={:?}",
                        self.keepalive_secs, self.recovery_url
                    ),
                );
                self.emit_status(None);
                if self.resubscribe_on_welcome {
                    self.resubscribe_on_welcome = false;
                    self.resubscribe_all().await;
                } else {
                    // recovering: the server restored its own subscriptions;
                    // only topics subscribed while disconnected need sending
                    self.send_pending().await;
                }
            }
            Some("keepalive") => {}
            Some("subscribeResponse") => {
                let Some(subscription_id) =
                    message["subscribeResponse"]["subscription"]["id"].as_str()
                else {
                    return;
                };
                let ok = message["subscribeResponse"]["result"].as_str() == Some("ok");
                let Some(subscription) = self.subs.get_mut(subscription_id) else {
                    return;
                };
                log(
                    "hermes",
                    format!(
                        "subscription to {} {}",
                        subscription.topic,
                        if ok { "accepted" } else { "rejected" }
                    ),
                );
                subscription.state = if ok {
                    SubState::Subscribed
                } else {
                    SubState::Failed
                };
                self.emit_status(None);
            }
            Some("unsubscribeResponse") => log("hermes", format!("unsubscribe response: {text}")),
            Some("notification") => self.handle_notification(&message).await,
            _ => log("hermes", format!("unknown message: {text}")),
        }
    }

    async fn resubscribe_all(&mut self) {
        log("hermes", format!("replaying {} topic(s)", self.subs.len()));
        for subscription in self.subs.values_mut() {
            subscription.state = SubState::Pending;
        }
        self.send_subs(|_| true).await;
    }

    /// Sends subscribe messages for topics that are still pending, used after
    /// a session recovery where the server already restored everything it
    /// knew about.
    async fn send_pending(&mut self) {
        let pending = self
            .subs
            .values()
            .filter(|subscription| subscription.state == SubState::Pending)
            .count();
        if pending > 0 {
            log("hermes", format!("subscribing {pending} pending topic(s)"));
        }
        self.send_subs(|subscription| subscription.state == SubState::Pending)
            .await;
    }

    /// Sends subscribe messages for the subscriptions matching `filter`.
    async fn send_subs(&mut self, filter: impl Fn(&Subscription) -> bool) {
        let matching: Vec<(String, String)> = self
            .subs
            .iter()
            .filter(|(_, subscription)| filter(subscription))
            .map(|(id, subscription)| (id.clone(), subscription.topic.clone()))
            .collect();
        for (id, topic) in matching {
            self.send_subscribe(&id, &topic).await;
        }
    }

    /// Registers the two topics of a channel if not registered already.
    fn create_subs(&mut self, channel_id: u64) {
        for prefix in TOPIC_PREFIXES {
            let topic = format!("{prefix}.{channel_id}");
            if self.subs.values().any(|s| s.topic == topic) {
                continue;
            }
            let subscription_id = self.message_id();
            self.subs.insert(
                subscription_id,
                Subscription {
                    topic,
                    state: SubState::Pending,
                },
            );
        }
    }

    async fn unsubscribe_topic(&mut self, topic: &str) {
        let Some(subscription_id) = self
            .subs
            .iter()
            .find(|(_, subscription)| subscription.topic == topic)
            .map(|(id, _)| id.clone())
        else {
            return;
        };
        log("hermes", format!("unsubscribing from {topic}"));
        let id = self.message_id();
        self.send_message(&json!({
            "type": "unsubscribe",
            "id": id,
            "unsubscribe": { "id": subscription_id },
            "timestamp": crate::logging::timestamp(),
        }))
        .await;
        self.subs
            .retain(|_, subscription| subscription.topic != topic);
    }

    async fn send_subscribe(&mut self, subscription_id: &str, topic: &str) {
        log("hermes", format!("subscribing to {topic}"));
        let id = self.message_id();
        self.send_message(&json!({
            "type": "subscribe",
            "id": id,
            "subscribe": {
                "id": subscription_id,
                "type": "pubsub",
                "pubsub": { "topic": topic },
            },
            "timestamp": crate::logging::timestamp(),
        }))
        .await;
    }

    async fn send_message(&mut self, message: &Value) {
        let Some(socket) = self.socket.as_mut() else {
            return;
        };
        if let Err(error) = socket.send(Message::text(message.to_string())).await {
            log("hermes", format!("send failed: {error}"));
            self.on_disconnect().await;
        }
    }

    async fn handle_notification(&mut self, message: &Value) {
        let Some(subscription_id) = message["notification"]["subscription"]["id"].as_str() else {
            return;
        };
        let Some(subscription) = self.subs.get(subscription_id) else {
            return;
        };
        if subscription.state != SubState::Subscribed {
            return;
        }
        let Some((topic, channel_id)) = subscription.topic.split_once('.') else {
            return;
        };
        let Ok(channel_id) = channel_id.parse::<u64>() else {
            return;
        };
        let Ok(pubsub) =
            serde_json::from_str::<Value>(message["notification"]["pubsub"].as_str().unwrap_or(""))
        else {
            return;
        };
        log(
            "hermes",
            format!(
                "notification on {}: {}",
                subscription.topic,
                serde_json::to_string(&pubsub).unwrap_or_default()
            ),
        );
        match topic {
            "broadcast-settings-update" => self.on_broadcast_settings_update(channel_id, &pubsub).await,
            "video-playback-by-id" => self.on_video_playback(channel_id, &pubsub).await,
            _ => {}
        }
    }

    async fn on_broadcast_settings_update(&mut self, channel_id: u64, pubsub: &Value) {
        let status = pubsub["status"].as_str().unwrap_or_default().to_owned();
        let old_status = pubsub["old_status"].as_str().unwrap_or_default();
        let game = pubsub["game"]
            .as_str()
            .filter(|game| !game.is_empty())
            .map(String::from);
        let old_game = pubsub["old_game"].as_str().unwrap_or_default();
        let Some(info) = self
            .infos
            .iter_mut()
            .find(|info| info.channel_id == channel_id)
        else {
            return;
        };
        let title_changed = info.stream_title.as_deref() != Some(status.as_str());
        let game_changed = game.as_deref() != info.game.as_ref().map(|game| game.name.as_str());
        // a local because nursery's suspicious_operation_groupings misfires
        // when the two fields appear in one && chain
        let can_notify = self.preferences.notify_title_changes && !info.live;
        if can_notify && (title_changed || game_changed) {
            let mut lines = Vec::new();
            if title_changed {
                lines.push(format!("{old_status} → {status}"));
            }
            if game_changed {
                lines.push(format!("{old_game} → {}", game.as_deref().unwrap_or("")));
            }
            let what = [
                title_changed.then_some("title"),
                game_changed.then_some("game"),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" & ");
            notifier::notify(
                &format!("[{}] {what} changed", info.channel_display_name),
                &lines.join("\n"),
                self.preferences.sound,
                Some(info.profile_image_url.as_str()).filter(|url| !url.is_empty()),
            )
            .await;
        }
        // mirrors the TS client's state updates for these events
        info.stream_title = Some(status).filter(|title| !title.is_empty());
        let game_display_name = info.game.as_ref().map(|game| game.display_name.clone());
        info.game = game.zip(pubsub["game_id"].as_u64()).map(|(name, id)| Game {
            id,
            name,
            display_name: game_display_name.unwrap_or_default(),
        });
    }

    async fn on_video_playback(&mut self, channel_id: u64, pubsub: &Value) {
        match pubsub["type"].as_str() {
            Some("stream-up") => match gql::fetch_users(&[channel_id], &[]).await {
                Ok(users) => {
                    let Some(mut user) =
                        users.into_iter().find(|user| user.channel_id == channel_id)
                    else {
                        return;
                    };
                    user.live = true;
                    let body = match (&user.game, user.stream_title.as_deref()) {
                        (Some(game), Some(title)) => format!("{title} — {}", game.name),
                        (Some(game), None) => game.name.clone(),
                        (None, Some(title)) => title.into(),
                        (None, None) => String::new(),
                    };
                    notifier::notify(
                        &format!("[{}] is LIVE", user.channel_display_name),
                        &body,
                        self.preferences.sound,
                        Some(user.profile_image_url.as_str()).filter(|url| !url.is_empty()),
                    )
                    .await;
                    let info = user;
                    match self
                        .infos
                        .iter()
                        .position(|existing| existing.channel_id == channel_id)
                    {
                        Some(index) => self.infos[index] = info,
                        None => self.infos.push(info),
                    }
                    self.emit_status(None);
                }
                Err(error) => log("hermes", format!("stream-up refresh failed: {error}")),
            },
            Some("stream-down") => {
                if let Some(info) = self
                    .infos
                    .iter_mut()
                    .find(|info| info.channel_id == channel_id)
                {
                    info.live = false;
                }
            }
            _ => {}
        }
    }

    fn sub_status_for(&self, topic: &str) -> SubStatus {
        self.subs
            .values()
            .find(|subscription| subscription.topic == topic)
            .map_or(SubStatus::Pending, |subscription| {
                match subscription.state {
                    SubState::Pending => SubStatus::Pending,
                    SubState::Subscribed => SubStatus::Connected,
                    SubState::Failed => SubStatus::Failed,
                }
            })
    }

    fn emit_status(&self, error: Option<&str>) {
        let unresolved: Vec<String> = self
            .channels
            .iter()
            .filter(|entry| {
                !entry
                    .id
                    .is_some_and(|id| self.infos.iter().any(|info| info.channel_id == id))
            })
            .map(|entry| entry.login.clone())
            .collect();
        let channels = self
            .infos
            .iter()
            .map(|info| ChannelStatus {
                channel_id: info.channel_id,
                login: info.channel_name.clone(),
                display_name: info.channel_display_name.clone(),
                title_status: self
                    .sub_status_for(&format!("broadcast-settings-update.{}", info.channel_id)),
                live_status: self
                    .sub_status_for(&format!("video-playback-by-id.{}", info.channel_id)),
            })
            .collect();
        let status = StatusEvent {
            connected: self.socket.is_some() && self.got_welcome,
            error: error.map(String::from),
            channels,
            unresolved,
        };
        if let Some(state) = self.app.try_state::<AppState>() {
            *state.status.lock().expect("status lock poisoned") = Some(status.clone());
        }
        if let Err(send_error) = self.app.emit("hermes-status", &status) {
            log(
                "hermes",
                format!("failed to emit status event: {send_error}"),
            );
        }
    }

    fn message_id(&mut self) -> String {
        // xorshift64; the server only needs uniqueness per connection
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        let mut id = String::with_capacity(21);
        for _ in 0..21 {
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            id.push(ID_ALPHABET[(self.rng & 63) as usize] as char);
        }
        id
    }
}
