//! Hermes pubsub: the batched baseline, the websocket lifecycle (welcome,
//! keepalives, reconnect backoff), subscription replay and live/title
//! notifications.

use std::collections::HashSet;
use std::time::Duration;
use std::time::Instant;

use siphon_rpc::LogLevel;
use siphon_rpc::Update;
use sonic_rs::JsonValueTrait as _;
use sonic_rs::Value;
use sonic_rs::json;

use crate::app::App;
use crate::app::JobDone;
use crate::balesh::NanoId;
use crate::gql;
use crate::gql::ChannelDetail;
use crate::gql::GqlError;
use crate::gql::StreamGame;
use crate::http::ws::WsStream;
use crate::matcher::Matcher;
use crate::time::now_iso;

const HERMES_URL: &str = "wss://hermes.twitch.tv/v1?clientId=kimne78kx3ncx6brgo4mv6wki5h1ko";
const WELCOME_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(15);
const KEEPALIVE_MISSED_LIMIT: u64 = 2;
/// How long after a `stream-down` a channel's toasts stay muted. Restreams
/// within this window update state silently instead of notifying again.
const OFFLINE_QUIET: Duration = Duration::from_mins(10);

/// One Hermes pubsub subscription: title/game updates or live/viewer updates
/// for a channel. The subscription id stays in the app; every welcome replays
/// the whole map.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Topic {
    BroadcastSettingsUpdate(u64),
    VideoPlaybackById(u64),
}

impl Topic {
    pub const fn for_channel(id: u64) -> [Self; 2] {
        [
            Self::BroadcastSettingsUpdate(id),
            Self::VideoPlaybackById(id),
        ]
    }
}

impl core::fmt::Display for Topic {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BroadcastSettingsUpdate(id) => write!(f, "broadcast-settings-update.{id}"),
            Self::VideoPlaybackById(id) => write!(f, "video-playback-by-id.{id}"),
        }
    }
}

/// One Hermes pubsub registration: the server-issued id, which send attempt it
/// belongs to, and whether the server confirmed it. Attempt 0 means never
/// sent, which no timeout can match.
#[derive(Clone, Copy)]
pub struct SubEntry {
    pub sub_id: NanoId,
    pub attempt: u64,
    pub state: SubState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SubState {
    Pending,
    Accepted,
    Rejected,
}

/// Outcome of one connection attempt: the fresh baseline (with the ids that no
/// longer resolve) plus the live socket, if the handshake succeeded.
pub struct ConnectOutcome {
    pub channels: Vec<ChannelDetail>,
    pub missing: Vec<u64>,
    pub socket: Option<WsStream>,
}

/// Builds the matcher over `words`. Called once at startup and again on every
/// filter change, so the live matcher always reflects the persisted list.
pub fn build_matcher(words: &[String]) -> Matcher {
    Matcher::new(
        &words
            .iter()
            .filter(|word| !word.is_empty())
            .cloned()
            .collect::<Vec<_>>(),
    )
}

impl App {
    /// Whether a channel's toasts are muted by a recent `stream-down`.
    fn quiet(&mut self, id: u64) -> bool {
        match self.quiet_until.get(&id) {
            Some(until) if Instant::now() < *until => true,
            Some(_) => {
                self.quiet_until.remove(&id);
                false
            }
            None => false,
        }
    }

    pub fn start_connect(&mut self) {
        let mut ids: Vec<u64> = self
            .config
            .channels
            .iter()
            .map(|channel| channel.id)
            .collect();
        for id in self.channels.keys() {
            if !ids.contains(id) {
                ids.push(*id);
            }
        }
        if ids.is_empty() {
            return;
        }
        self.connecting = true;
        let client = std::rc::Rc::clone(&self.client);
        let client_id = self.client_id.clone();
        self.spawn_job(async move {
            let fetched = gql::fetch_channels(&client, client_id.as_deref(), &ids).await;
            let ok = fetched.is_ok();
            let (channels, missing) = match fetched {
                Ok(fetched) => {
                    let seen: HashSet<u64> = fetched.iter().map(|channel| channel.id).collect();
                    let missing: Vec<u64> = ids
                        .iter()
                        .copied()
                        .filter(|id| !seen.contains(id))
                        .collect();
                    (fetched, missing)
                }
                Err(error) => {
                    tracing::info!(target: "hermes", "channel refresh failed: {error}");
                    (Vec::new(), Vec::new())
                }
            };
            // Nothing resolved and nothing stale could either: connecting
            // would just time out waiting for a welcome with no topics.
            if ok && channels.is_empty() {
                return JobDone::ConnectFinished(Box::new(ConnectOutcome {
                    channels: Vec::new(),
                    missing,
                    socket: None,
                }));
            }

            tracing::info!(target: "hermes", "connecting to {HERMES_URL}");
            let socket = match WsStream::connect(HERMES_URL).await {
                Ok(socket) => {
                    tracing::info!(target: "hermes", "connected, waiting for welcome");
                    Some(socket)
                }
                Err(error) => {
                    tracing::info!(target: "hermes", "connect failed: {error}");
                    None
                }
            };

            JobDone::ConnectFinished(Box::new(ConnectOutcome {
                channels,
                missing,
                socket,
            }))
        });
    }

    /// Applies a finished connection attempt. Users for channels removed
    /// mid-flight are dropped; ids that resolve to nothing are pruned from the
    /// persisted list. An empty baseline or a failed handshake backs off; only a
    /// live socket with something to watch is kept.
    pub(crate) async fn complete_connect(&mut self, outcome: ConnectOutcome) {
        self.connecting = false;
        let ConnectOutcome {
            channels,
            missing,
            socket,
        } = outcome;
        let wanted: HashSet<u64> = self
            .config
            .channels
            .iter()
            .map(|channel| channel.id)
            .collect();
        let fresh: Vec<ChannelDetail> = channels
            .into_iter()
            .filter(|channel| wanted.contains(&channel.id))
            .collect();
        for channel in &fresh {
            let old = self.channels.get(&channel.id);
            if should_log_stream(old, channel) {
                tracing::info!(
                    target: "hermes",
                    "stream {}({}): stream_id={} started={}",
                    channel.login,
                    channel.id,
                    channel.stream_id,
                    channel
                        .stream_start
                        .map_or("unknown".to_owned(), crate::time::format_iso_ms),
                );
            }
        }
        for channel in fresh {
            let id = channel.id;
            self.track_channel(channel);
            self.ensure_subs(id);
        }
        let missing: Vec<u64> = missing
            .into_iter()
            .filter(|id| wanted.contains(id))
            .collect();
        if !missing.is_empty() {
            self.apply_prune_ids(&missing);
        }
        let mut socket = socket;
        if self.channels.is_empty() {
            if let Some(mut stray) = socket.take() {
                let _ = stray.close(1000, "").await;
            }
            tracing::info!(
                target: "hermes",
                "no channels resolved, retrying (typo in a login?)"
            );
            self.schedule_reconnect();
            self.publish_no_channels();
            return;
        }
        let Some(live) = socket else {
            self.schedule_reconnect();
            self.publish_conn_health();
            return;
        };
        self.socket = Some(live);
        self.welcomed = false;
        self.last_message = Instant::now();
        // A channel that is already live when the baseline lands starts its
        // recording monitor now; Hermes will not replay a `stream-up` for it.
        let live_logins: Vec<String> = self
            .config
            .channels
            .iter()
            .filter(|channel| channel.record)
            .filter(|channel| {
                self.channels
                    .get(&channel.id)
                    .is_some_and(|detail| detail.live)
            })
            .map(|channel| channel.login.clone())
            .collect();
        for login in live_logins {
            self.registry.monitor_set(&login);
        }
    }

    /// The persisted follow entry for one channel, if there is one.
    ///
    /// Cloned because callers hold a mutable borrow of `channels` when they
    /// build the view, and a view needs both sides.
    fn channel_config(&self, id: u64) -> Option<siphon_rpc::Channel> {
        self.config
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .cloned()
    }

    /// Starts one avatar download, if this channel does not have one.
    fn ensure_avatar(&mut self, id: u64, url: &str) {
        if url.is_empty() || !self.avatars_pending.insert(id) {
            return;
        }
        let client = std::rc::Rc::clone(&self.client);
        let url = url.to_owned();
        self.spawn_job(async move {
            JobDone::AvatarReady(id, crate::avatar::ensure(&client, &url).await)
        });
    }

    /// Tracks a fresh resolve, logging stream identity when it moved.
    pub fn track_channel(&mut self, mut user: ChannelDetail) {
        let id = user.id;
        // A refresh replaces the whole detail; the cached picture is still the
        // same person, so do not make the row fall back to initials for it.
        if user.avatar_path.is_none() {
            user.avatar_path = self
                .channels
                .get(&id)
                .and_then(|old| old.avatar_path.clone());
        }
        if should_log_stream(self.channels.get(&id), &user) {
            let started = user
                .stream_start
                .map_or("an unknown time".to_owned(), crate::time::format_iso_ms);
            tracing::info!(
                target: "hermes",
                "stream {}({}): stream_id={} started={started}",
                user.login,
                user.id,
                user.stream_id,
            );
            self.bus.log_channel(
                LogLevel::Info,
                "hermes",
                format!("{} stream {} started at {started}", user.login, user.stream_id),
                Some(user.login.clone()),
                Some(user.stream_id),
            );
        }
        let config = self.channel_config(id);
        let avatar_url = user.profile_image_url.clone();
        let needs_avatar = user.avatar_path.is_none();
        self.bus
            .publish(Update::ChannelUpsert(user.view(config.as_ref())));
        self.channels.insert(id, user);
        if needs_avatar {
            self.ensure_avatar(id, &avatar_url);
        }
    }

    /// Registers a channel's two topics if not registered already.
    pub fn ensure_subs(&mut self, id: u64) {
        for topic in Topic::for_channel(id) {
            if !self.sub_ids.contains_key(&topic) {
                self.sub_ids.insert(
                    topic,
                    SubEntry {
                        sub_id: self.rng.nano_id(),
                        attempt: 0,
                        state: SubState::Pending,
                    },
                );
            }
        }
    }

    /// Drops one id's row and topics, returning the removed subscriptions for
    /// unsubscribing.
    pub fn drop_channel(&mut self, id: u64) -> Vec<(NanoId, Topic)> {
        self.quiet_until.remove(&id);
        let mut removed = Vec::new();
        for topic in Topic::for_channel(id) {
            if let Some(entry) = self.sub_ids.remove(&topic) {
                removed.push((entry.sub_id, topic));
            }
        }
        self.channels.remove(&id);
        removed
    }

    /// Drops the socket and backs off. Reconnects always start fresh and
    /// replay all subscriptions.
    pub(crate) async fn on_disconnect(&mut self) {
        self.close_socket().await;
        self.welcomed = false;
        self.bus.log(
            LogLevel::Error,
            "hermes",
            "disconnected from Twitch, reconnecting",
        );
        self.publish_conn_health();
        self.schedule_reconnect();
    }

    fn schedule_reconnect(&mut self) {
        self.reconnect_attempt += 1;
        let delay = Duration::from_millis(500)
            .saturating_mul(1 << (self.reconnect_attempt.min(10) - 1))
            .min(MAX_RECONNECT_DELAY);
        tracing::info!(target: "hermes", "disconnected, reconnecting in {delay:?}");
        self.reconnect_at = Instant::now() + delay;
    }

    async fn close_socket(&mut self) {
        if let Some(mut socket) = self.socket.take() {
            let _ = socket.close(1000, "").await;
        }
    }

    /// Fires once a second: enforces the welcome timeout and drops the
    /// connection after missed keepalives.
    pub(crate) async fn on_tick(&mut self) {
        // Once a second, and before the socket check: a UI watching recordings
        // must keep getting fresh counters even while Hermes is reconnecting.
        self.publish_telemetry();
        if self.socket.is_none() {
            return;
        }
        if !self.welcomed {
            if self.last_message.elapsed() > WELCOME_TIMEOUT {
                tracing::info!(target: "hermes", "no welcome within 10s, dropping connection");
                self.on_disconnect().await;
            }
        } else if self.last_message.elapsed()
            > Duration::from_secs(self.keepalive_secs * KEEPALIVE_MISSED_LIMIT)
        {
            tracing::info!(
                target: "hermes",
                "no keepalive for {}s, dropping connection",
                self.keepalive_secs * KEEPALIVE_MISSED_LIMIT
            );
            self.on_disconnect().await;
        }
    }

    pub(crate) async fn handle_message(&mut self, text: &str) {
        // Any frame at all is proof the connection is alive, even a blank one.
        self.last_message = Instant::now();
        if text.trim().is_empty() {
            return;
        }
        let Ok(message) = sonic_rs::from_str::<Value>(text) else {
            tracing::info!(target: "hermes", "unparseable message: {text}");
            return;
        };
        match message["type"].as_str() {
            Some("welcome") => {
                let summary = if self.reconnect_attempt == 0 {
                    "Hermes connected"
                } else {
                    "Hermes reconnected"
                };
                self.welcomed = true;
                self.reconnect_attempt = 0;
                self.keepalive_secs = message["welcome"]["keepaliveSec"].as_u64().unwrap_or(15);
                tracing::info!(
                    target: "hermes",
                    "welcome: keepalive={}s",
                    self.keepalive_secs
                );
                self.publish_conn_health();
                self.queue_notification(
                    summary.to_owned(),
                    format!("watching {} channel(s)", self.channels.len()),
                    None,
                    None,
                );
                self.resubscribe_all().await;
            }
            Some("keepalive") => {}
            Some("subscribeResponse") => {
                let Some(target) = message["subscribeResponse"]["subscription"]["id"]
                    .as_str()
                    .and_then(NanoId::parse)
                else {
                    return;
                };
                let ok = message["subscribeResponse"]["result"].as_str() == Some("ok");
                let Some(topic) = self
                    .sub_ids
                    .iter()
                    .find_map(|(topic, entry)| (entry.sub_id == target).then_some(*topic))
                else {
                    return;
                };
                if let Some(entry) = self.sub_ids.get_mut(&topic) {
                    entry.state = if ok {
                        SubState::Accepted
                    } else {
                        SubState::Rejected
                    };
                }
                if ok {
                    tracing::info!(target: "hermes", "subscription to {topic} accepted");
                } else {
                    tracing::info!(target: "hermes", "subscription to {topic} rejected");
                    self.set_error(format!("subscription to {topic} rejected"));
                }
                self.publish_conn_health();
            }
            Some("unsubscribeResponse") => {
                tracing::info!(target: "hermes", "unsubscribe response: {text}");
            }
            Some("notification") => self.handle_notification(&message),
            _ => tracing::info!(target: "hermes", "unknown message: {text}"),
        }
    }

    async fn resubscribe_all(&mut self) {
        let pairs: Vec<(NanoId, Topic)> = self
            .sub_ids
            .iter()
            .map(|(topic, entry)| (entry.sub_id, *topic))
            .collect();
        self.subscribe(&pairs).await;
    }

    /// Subscribes a single channel's topics; used when a channel is added to an
    /// already-live session.
    pub(crate) async fn subscribe_channel(&mut self, id: u64) {
        let pairs: Vec<(NanoId, Topic)> = Topic::for_channel(id)
            .into_iter()
            .filter_map(|topic| self.sub_ids.get(&topic).map(|entry| (entry.sub_id, topic)))
            .collect();
        self.subscribe(&pairs).await;
    }

    /// Sends subscribes for already-registered topics and arms one confirmation
    /// deadline per topic.
    async fn subscribe(&mut self, pairs: &[(NanoId, Topic)]) {
        if !pairs.is_empty() {
            tracing::info!(
                target: "hermes",
                "subscribing to {} topic(s)",
                pairs.len()
            );
        }
        for &(sub_id, topic) in pairs {
            self.sub_gen += 1;
            let attempt = self.sub_gen;
            if let Some(entry) = self.sub_ids.get_mut(&topic) {
                entry.attempt = attempt;
                entry.state = SubState::Pending;
            }
            self.send_subscribe(sub_id, topic).await;
            let timeout = async move {
                compio::time::sleep(WELCOME_TIMEOUT).await;
                JobDone::SubscribeCheckDue(topic, sub_id, attempt)
            };
            self.spawn_job(timeout);
        }
    }

    async fn send_subscribe(&mut self, subscription_id: NanoId, topic: Topic) {
        tracing::info!(target: "hermes", "subscribing to {topic}");
        let id = self.rng.nano_id();
        let timestamp = now_iso();
        let message = json!({
            "type": "subscribe",
            "id": id,
            "subscribe": {
                "id": subscription_id,
                "type": "pubsub",
                "pubsub": { "topic": topic.to_string() },
            },
            "timestamp": timestamp,
        });
        self.send_message(&message).await;
    }

    pub(crate) async fn send_message(&mut self, message: &Value) {
        let Some(socket) = self.socket.as_mut() else {
            return;
        };
        let text = sonic_rs::to_string(message).unwrap_or_default();
        if let Err(error) = socket.send_text(&text).await {
            tracing::info!(target: "hermes", "send failed: {error}");
            self.on_disconnect().await;
        }
    }

    /// Sends the queued unsubscribe frames now that a socket is available.
    pub(crate) async fn flush_unsubscribes(&mut self) {
        if self.socket.is_none() || self.pending_unsubscribes.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_unsubscribes);
        for (sub_id, topic) in pending {
            tracing::info!(target: "hermes", "unsubscribing from {topic}");
            let id = self.rng.nano_id();
            let timestamp = now_iso();
            let message = json!({
                "type": "unsubscribe",
                "id": id,
                "unsubscribe": { "id": sub_id },
                "timestamp": timestamp,
            });
            self.send_message(&message).await;
        }
    }

    fn handle_notification(&mut self, message: &Value) {
        let Some(target) = message["notification"]["subscription"]["id"]
            .as_str()
            .and_then(NanoId::parse)
        else {
            return;
        };
        let Some(topic) = self
            .sub_ids
            .iter()
            .find_map(|(topic, entry)| (entry.sub_id == target).then_some(*topic))
        else {
            return;
        };
        // Only confirmed subscriptions notify: welcomes replay everything, and
        // confirmations arrive after.
        let confirmed = matches!(
            self.sub_ids.get(&topic),
            Some(entry) if entry.state == SubState::Accepted
        );
        if !confirmed {
            return;
        }
        let Ok(pubsub) =
            sonic_rs::from_str::<Value>(message["notification"]["pubsub"].as_str().unwrap_or(""))
        else {
            return;
        };
        tracing::info!(
            target: "hermes",
            "notification on {topic}: {}",
            sonic_rs::to_string(&pubsub).unwrap_or_default()
        );
        match topic {
            Topic::BroadcastSettingsUpdate(id) => {
                self.on_broadcast_settings_update(id, &pubsub);
            }
            Topic::VideoPlaybackById(id) => {
                self.on_video_playback(id, &pubsub);
            }
        }
    }

    fn on_broadcast_settings_update(&mut self, id: u64, pubsub: &Value) {
        let status = pubsub["status"].as_str().unwrap_or_default().to_owned();
        let old_status = pubsub["old_status"].as_str().unwrap_or_default();
        let game = pubsub["game"]
            .as_str()
            .filter(|game| !game.is_empty())
            .map(str::to_owned);
        let old_game = pubsub["old_game"].as_str().unwrap_or_default();
        let title_filtered = self.matcher.is_match(&status);
        let notify_titles = self.config.notify_title_changes;
        let channel = self.channel_config(id);
        let channel_notifies = channel.as_ref().is_none_or(|channel| channel.notify_titles);
        let quiet = self.quiet(id);
        let Some(user) = self.channels.get_mut(&id) else {
            return;
        };
        let title_changed = user.stream_title.as_deref() != Some(status.as_str());
        let game_changed = game.as_deref() != user.game.as_ref().map(|game| game.name.as_str());
        let can_notify = notify_titles && channel_notifies && !user.live && !quiet;
        let filtered = title_changed && title_filtered;
        let toast = if can_notify && (title_changed || game_changed) && !filtered {
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
            Some((
                format!("[{}] {what} changed", user.display_name),
                lines.join("\n"),
                user.profile_image_url.clone(),
                user.login.clone(),
            ))
        } else {
            None
        };
        let mut changes = Vec::new();
        if title_changed {
            changes.push(format!("title → {status}"));
        }
        if game_changed {
            changes.push(format!("game → {}", game.as_deref().unwrap_or("none")));
        }
        let login = user.login.clone();
        let stream_id = user.stream_id;
        user.stream_title = Some(status).filter(|title| !title.is_empty());
        let game_display_name = user.game.as_ref().map(|game| game.display_name.clone());
        user.game = game
            .zip(pubsub["game_id"].as_u64())
            .map(|(name, game_id)| StreamGame {
                id: game_id,
                name,
                display_name: game_display_name.unwrap_or_default(),
            });
        let view = user.view(channel.as_ref());
        self.bus.publish(Update::ChannelUpsert(view));
        if !changes.is_empty() {
            self.bus.log_channel(
                LogLevel::Info,
                "hermes",
                changes.join(", "),
                Some(login),
                Some(stream_id),
            );
        }
        if let Some((summary, body, avatar, login)) = toast {
            self.queue_notification(
                summary,
                body,
                Some(avatar).filter(|url| !url.is_empty()),
                Some(login),
            );
        }
    }

    /// `video-playback-by-id`: stream-up/down, viewcounts and collaborations.
    fn on_video_playback(&mut self, id: u64, pubsub: &Value) {
        match pubsub["type"].as_str() {
            Some("viewcount") => {
                let channel = self.channel_config(id);
                let Some(user) = self.channels.get_mut(&id) else {
                    return;
                };
                user.viewers = pubsub["viewers"]
                    .as_u64()
                    .and_then(|count| u32::try_from(count).ok());
                user.collaboration_viewers = None;
                if pubsub["collaboration_status"].as_str() == Some("in_collaboration") {
                    user.collaboration_viewers = pubsub["collaboration_viewers"]
                        .as_u64()
                        .and_then(|count| u32::try_from(count).ok());
                }
                let view = user.view(channel.as_ref());
                self.bus.publish(Update::ChannelUpsert(view));
            }
            Some("stream-up") => {
                let quiet = self.quiet(id);
                let channel = self.channel_config(id);
                let Some(user) = self.channels.get(&id) else {
                    return;
                };
                let login = user.login.clone();
                let display_name = user.display_name.clone();
                let game = user.game.clone();
                let title = user.stream_title.clone();
                let avatar = user.profile_image_url.clone();
                let stream_id = user.stream_id;
                let Some(user) = self.channels.get_mut(&id) else {
                    return;
                };
                user.live = true;
                let view = user.view(channel.as_ref());
                self.bus.publish(Update::ChannelUpsert(view));
                self.publish_conn_health();
                self.bus.log_channel(
                    LogLevel::Info,
                    "hermes",
                    "is live",
                    Some(login.clone()),
                    Some(stream_id),
                );
                if quiet {
                    tracing::info!(
                        target: "hermes",
                        "stream-up for {id} inside offline window, staying silent"
                    );
                } else if channel.as_ref().is_none_or(|channel| channel.notify_live) {
                    self.notify_live(
                        &display_name,
                        game.as_ref(),
                        title.as_deref(),
                        &avatar,
                        Some(&login),
                    );
                } else {
                    tracing::info!(target: "hermes", "stream-up for {id} muted by channel");
                }
                if channel.as_ref().is_some_and(|channel| channel.record) {
                    self.registry.monitor_set(&login);
                }
                let client = std::rc::Rc::clone(&self.client);
                let client_id = self.client_id.clone();
                self.spawn_job(async move {
                    let result = gql::fetch_channels(&client, client_id.as_deref(), &[id]).await;
                    JobDone::LiveRefreshed(id, result)
                });
            }
            Some("stream-down") => {
                let channel = self.channel_config(id);
                let Some(user) = self.channels.get(&id) else {
                    return;
                };
                let login = user.login.clone();
                let stream_id = user.stream_id;
                self.quiet_until.insert(id, Instant::now() + OFFLINE_QUIET);
                if let Some(user) = self.channels.get_mut(&id) {
                    user.live = false;
                    user.collaboration_viewers = None;
                    user.viewers = None;
                    let view = user.view(channel.as_ref());
                    self.bus.publish(Update::ChannelUpsert(view));
                }
                self.bus.log_channel(
                    LogLevel::Info,
                    "hermes",
                    "went offline",
                    Some(login.clone()),
                    Some(stream_id),
                );
                self.registry.stream_down(&login);
            }
            _ => {}
        }
    }

    /// Applies a finished `stream-up` refresh: the fresh user replaces the
    /// optimistic baseline.
    pub fn complete_live_refresh(&mut self, id: u64, result: Result<Vec<ChannelDetail>, GqlError>) {
        let wanted = self.channels.contains_key(&id)
            || self.config.channels.iter().any(|channel| channel.id == id);
        if !wanted {
            return;
        }
        match result {
            Ok(users) => {
                let Some(mut user) = users.into_iter().find(|user| user.id == id) else {
                    return;
                };
                user.live = true;
                let known = self.channels.contains_key(&id);
                let notifies = self
                    .channel_config(id)
                    .is_none_or(|channel| channel.notify_live);
                if !known && !self.quiet(id) && notifies {
                    let display_name = user.display_name.clone();
                    let game = user.game.clone();
                    let title = user.stream_title.clone();
                    let avatar = user.profile_image_url.clone();
                    self.notify_live(
                        &display_name,
                        game.as_ref(),
                        title.as_deref(),
                        &avatar,
                        Some(user.login.as_str()),
                    );
                }
                self.track_channel(user);
            }
            Err(error) => {
                tracing::info!(target: "hermes", "stream-up refresh failed: {error}");
            }
        }
    }

    /// "is LIVE" notification shared by the optimistic and fallback paths.
    fn notify_live(
        &mut self,
        display_name: &str,
        game: Option<&StreamGame>,
        title: Option<&str>,
        avatar: &str,
        login: Option<&str>,
    ) {
        let body = match (game, title) {
            (Some(game), Some(title)) => format!("{title} — {}", game.name),
            (Some(game), None) => game.name.clone(),
            (None, Some(title)) => title.to_owned(),
            (None, None) => String::new(),
        };
        self.queue_notification(
            format!("[{display_name}] is LIVE"),
            body,
            Some(avatar.to_owned()).filter(|url| !url.is_empty()),
            login.map(str::to_owned),
        );
    }
}

/// Whether a fresh resolve deserves a stream log: first sight logs only when
/// the request carried stream data, later resolves log only when the stream
/// identity moved.
fn should_log_stream(old: Option<&ChannelDetail>, new: &ChannelDetail) -> bool {
    old.map_or_else(
        || new.stream_start.is_some(),
        |prev| prev.stream_id != new.stream_id || prev.stream_start != new.stream_start,
    )
}
