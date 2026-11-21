//! Twitch GQL: channel resolution, the live-state baseline and avatar fetches.
//!
//! This is Siphon's typed decoder, moved onto the daemon's own HTTP client and
//! `sonic_rs`, so the backend keeps one TLS/connection stack.

use std::num::NonZeroU64;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde::Deserializer;
use serde::de::Visitor;
use siphon_shared::rpc::ChannelView;
use siphon_shared::rpc::ChatColor;
use thiserror::Error;

use crate::constants::DEFAULT_CLIENT_ID;
use crate::constants::MAX_IMAGE;
use crate::constants::MAX_JSON;
use crate::http::Client;
use crate::http::StatusCode;
use crate::time::parse_iso_ms;

const GQL_URL: &str = "https://gql.twitch.tv/gql";
const GQL_TIMEOUT: Duration = Duration::from_secs(15);

const USERS_BY_IDS_QUERY: &str = "query UsersByIds($ids:[ID!]){users(ids:$ids){id login chatColor displayName profileImageURL(width:70) broadcastSettings{title game{id name displayName}}stream{viewersCount collaborationViewersCount id createdAt}lastBroadcast{startedAt}}}";
const USER_BY_LOGIN_QUERY: &str = "query UserByLogin($login:String!){user(login:$login){id login chatColor displayName profileImageURL(width:70) broadcastSettings{title game{id name displayName}}stream{viewersCount collaborationViewersCount id createdAt}lastBroadcast{startedAt}}}";

#[derive(Debug, Error)]
pub enum GqlError {
    #[error("http: {0}")]
    Http(#[from] crate::http::Error),
    #[error("json: {0}")]
    Json(#[from] sonic_rs::Error),
    #[error("gql returned status {0}")]
    Status(StatusCode),
    #[error("request timed out")]
    Timeout,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// One resolved channel, including the stream identity the disk output needs.
#[derive(Debug, Clone)]
pub struct ChannelDetail {
    pub id: u64,
    pub login: String,
    pub display_name: String,
    /// CSS colour for this channel's switches: Twitch's chat colour, or a
    /// stable generated one when the user has not picked a chat colour.
    pub chat_color: ChatColor,
    pub profile_image_url: String,
    /// Local avatar file, once the app has cached the remote picture.
    pub avatar_path: Option<String>,
    /// Twitch's `BROADCAST-ID` from the active stream; `None` when offline.
    pub stream_id: Option<NonZeroU64>,
    pub stream_title: Option<String>,
    /// Milliseconds since the unix epoch; `None` when offline.
    pub stream_start: Option<i64>,
    /// The `startedAt` of the channel's most recent broadcast. While live that
    /// is the current broadcast's start; while offline Twitch reports the
    /// *end* of the last one under the same field. `stream-down` writes the
    /// current time first so the row is right before the next poll corrects it.
    pub last_live: Option<i64>,
    pub viewers: Option<u32>,
    pub collaboration_viewers: Option<u32>,
    pub game: Option<StreamGame>,
    pub live: bool,
    /// The last per-channel failure this detail carries, cleared by the next
    /// successful refresh. Not part of the GQL payload: it is set by the app
    /// when a refresh for this id fails.
    pub error: Option<String>,
}

/// The game a broadcast is categorised under.
#[derive(Debug, Clone)]
pub struct StreamGame {
    /// Kept for game-change matching; the UI renders the display name.
    #[allow(dead_code)]
    pub id: u64,
    pub name: String,
    pub display_name: String,
}

impl ChannelDetail {
    /// The UI entity for this channel.
    ///
    /// `channel` is the persisted follow entry, when there is one: it carries
    /// the three switches the row draws. A resolve with no entry is a pending
    /// add, which the row does not show yet.
    #[must_use]
    pub fn view(&self, channel: Option<&siphon_shared::rpc::Channel>) -> ChannelView {
        ChannelView {
            id: self.id,
            login: self.login.clone(),
            display_name: self.display_name.clone(),
            chat_color: self.chat_color,
            avatar_path: self.avatar_path.clone(),
            live: self.live,
            title: self.stream_title.clone(),
            game: self.game.as_ref().map(|game| game.display_name.clone()),
            game_id: self.game.as_ref().map(|game| game.id),
            viewers: self.viewers,
            collaboration_viewers: self.collaboration_viewers,
            stream_start_ms: self.stream_start,
            last_live_ms: self.last_live,
            stream_id: self.stream_id,
            error: self.error.clone(),
            notify_live: channel.is_none_or(|channel| channel.notify_live),
            notify_titles: channel.is_none_or(|channel| channel.notify_titles),
            record: channel.is_some_and(|channel| channel.record),
        }
    }
}

// Typed mirrors of the GQL responses. Decoding straight into these skips the
// generic value map; every nullable layer stays `Option` so one missing user
// or a null `data` collapses to `None` instead of failing the whole batch.
#[derive(Debug, Deserialize)]
struct GqlIdsResponse {
    data: Option<GqlIdsData>,
}

#[derive(Debug, Deserialize)]
struct GqlLoginResponse {
    data: Option<GqlLoginData>,
}

#[derive(Debug, Deserialize)]
struct GqlLoginData {
    #[serde(default)]
    user: Option<GqlUser>,
}

#[derive(Debug, Deserialize)]
struct GqlIdsData {
    #[serde(default)]
    users: Option<Vec<Option<GqlUser>>>,
}

#[derive(Debug, Deserialize)]
struct GqlUser {
    #[serde(default, deserialize_with = "de_opt_u64")]
    id: Option<u64>,
    login: Option<String>,
    /// Null until the user picks a chat colour in Twitch's settings.
    #[serde(rename = "chatColor")]
    chat_color: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    #[serde(rename = "profileImageURL")]
    profile_image_url: Option<String>,
    #[serde(rename = "broadcastSettings")]
    broadcast_settings: Option<GqlBroadcastSettings>,
    stream: Option<GqlStream>,
    /// The most recent broadcast. Twitch calls its `startedAt` a start time,
    /// but while the channel is offline it is the end of that broadcast.
    #[serde(rename = "lastBroadcast")]
    last_broadcast: Option<GqlLastBroadcast>,
}

#[derive(Debug, Deserialize)]
struct GqlLastBroadcast {
    #[serde(rename = "startedAt")]
    started_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GqlBroadcastSettings {
    title: Option<String>,
    game: Option<GqlGame>,
}

#[derive(Debug, Deserialize)]
struct GqlGame {
    #[serde(default, deserialize_with = "de_opt_u64")]
    id: Option<u64>,
    name: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GqlStream {
    #[serde(default, deserialize_with = "de_opt_u64")]
    id: Option<u64>,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
    #[serde(rename = "viewersCount")]
    viewers_count: Option<u32>,
    #[serde(rename = "collaborationViewersCount")]
    collaboration_viewers_count: Option<u32>,
}

/// GQL `ID` scalars arrive as strings ("12345") but may serialize as numbers;
/// both are accepted and anything else collapses to `None`.
fn de_opt_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    struct IdVisitor;

    impl<'de> Visitor<'de> for IdVisitor {
        type Value = Option<u64>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an id string, an id number or null")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D: Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> Result<Self::Value, D::Error> {
            // Re-enter the same visitor on the inner value so a number and a
            // numeric string both land here, and an unparseable id is None.
            deserializer.deserialize_any(self)
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
            Ok(Some(value))
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
            Ok(u64::try_from(value).ok())
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
            Ok(value.parse().ok())
        }
    }

    deserializer.deserialize_option(IdVisitor)
}

/// A stable stand-in for users with no `chatColor` (Twitch leaves it null until
/// they pick one). The channel id walked by the golden angle spreads hues
/// across the wheel while keeping each channel's colour the same across
/// refreshes and restarts, rather than flickering on every snapshot.
pub fn fallback_color(id: u64) -> ChatColor {
    const GOLDEN_ANGLE: f64 = 137.507_764_050_918;
    // Ids are only a hue seed here: the high bits survive, and a collision in
    // the low 12 would pick the same colour anyway after `% 360`.
    #[allow(
        clippy::cast_precision_loss,
        reason = "a channel id is a hue seed, not a quantity"
    )]
    let seed = id as f64;
    let hue = (seed * GOLDEN_ANGLE) % 360.0;
    let lightness = f64::from((id % 2) as u8).mul_add(0.03, 0.70);
    let chroma = f64::from((id % 3) as u8).mul_add(0.01, 0.13);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "0..=360 degrees and 0.70..=0.73 / 0.13..=0.15 in thousandths all fit u16"
    )]
    ChatColor::Generated {
        lightness_milli: (lightness * 1000.0).round() as u16,
        chroma_milli: (chroma * 1000.0).round() as u16,
        hue: hue.round() as u16,
    }
}
/// Twitch's `chatColor` is `#RRGGBB`; anything else is not a colour and gets
/// the generated fallback rather than a style property with junk in it.
fn parse_twitch_color(raw: &str) -> Option<ChatColor> {
    let hex = raw.strip_prefix('#').unwrap_or(raw);
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok().map(ChatColor::Twitch)
}

impl GqlUser {
    fn into_channel(self) -> Option<ChannelDetail> {
        let settings = self.broadcast_settings?;
        let id = self.id?;
        let chat_color = self
            .chat_color
            .as_deref()
            .and_then(parse_twitch_color)
            .unwrap_or_else(|| fallback_color(id));
        // `stream` is the single source of truth for liveness and the broadcast
        // identity: it is non-null only while the channel is live, and its `id`
        // is the `BROADCAST-ID` the rest of the pipeline keys off. `title` and
        // `game` live on `broadcastSettings`, which is present even offline.
        let stream_id = self
            .stream
            .as_ref()
            .and_then(|stream| stream.id)
            .and_then(NonZeroU64::new);
        let viewers = self.stream.as_ref().and_then(|stream| stream.viewers_count);
        let collaboration_viewers = self
            .stream
            .as_ref()
            .and_then(|stream| stream.collaboration_viewers_count);
        let live = self.stream.is_some();
        // `createdAt` is only useful as a parsed instant. If Twitch ever sends
        // a string we cannot parse, say so in the error path rather than
        // carrying the raw value through the happy path as redundant state.
        let created_at = self
            .stream
            .as_ref()
            .and_then(|stream| stream.created_at.as_deref());
        let stream_start = created_at.and_then(parse_iso_ms);
        if created_at.is_some() && stream_start.is_none() {
            tracing::warn!(
                target: "gql",
                login = %self.login.as_deref().unwrap_or(""),
                created_at = %created_at.unwrap_or(""),
                "could not parse Twitch stream createdAt"
            );
        }
        let last_at = self
            .last_broadcast
            .as_ref()
            .and_then(|broadcast| broadcast.started_at.as_deref());
        let last_live = last_at.and_then(parse_iso_ms);
        if last_at.is_some() && last_live.is_none() {
            tracing::warn!(
                target: "gql",
                login = %self.login.as_deref().unwrap_or(""),
                started_at = %last_at.unwrap_or(""),
                "could not parse Twitch lastBroadcast startedAt"
            );
        }
        Some(ChannelDetail {
            id,
            chat_color,
            login: self.login.filter(|login| !login.is_empty())?,
            display_name: self.display_name.filter(|name| !name.is_empty())?,
            profile_image_url: self.profile_image_url.filter(|url| !url.is_empty())?,
            avatar_path: None,
            stream_id,
            stream_title: settings.title.filter(|title| !title.is_empty()),
            stream_start,
            last_live,
            game: settings.game.and_then(|game| {
                Some(StreamGame {
                    id: game.id?,
                    name: game.name?,
                    display_name: game.display_name?,
                })
            }),
            collaboration_viewers,
            viewers,
            live,
            error: None,
        })
    }
}

fn configured_client_id(configured: Option<&str>) -> &str {
    configured.unwrap_or(DEFAULT_CLIENT_ID)
}

async fn post(client: &Client, client_id: &str, body: &[u8]) -> Result<Vec<u8>, GqlError> {
    let mut response = crate::http::timeout(
        GQL_TIMEOUT,
        client.post(
            GQL_URL,
            "application/json",
            body,
            &[("Client-ID", client_id)],
        ),
    )
    .await
    .map_err(|_| GqlError::Timeout)??;
    if !response.status.is_success() {
        return Err(GqlError::Status(response.status));
    }
    let body = crate::http::timeout(GQL_TIMEOUT, response.body.read_to_end(MAX_JSON))
        .await
        .map_err(|_| GqlError::Timeout)??;
    Ok(body)
}

/// Resolves one login. `Ok(None)` means the login does not resolve (unknown
/// login or an unparseable user); `Err` is a transport or decode failure.
pub async fn fetch_channel(
    client: &Client,
    client_id: Option<&str>,
    login: &str,
) -> Result<Option<ChannelDetail>, GqlError> {
    let body = sonic_rs::to_vec(&sonic_rs::json!({
        "query": USER_BY_LOGIN_QUERY,
        "variables": { "login": login },
        "operationName": "UserByLogin",
    }))?;
    tracing::info!(target: "gql", "request: login={login:?}");
    let payload = post(client, configured_client_id(client_id), &body).await?;
    let response: GqlLoginResponse = sonic_rs::from_slice(&payload)?;
    let user = response
        .data
        .and_then(|data| data.user)
        .and_then(GqlUser::into_channel)
        .filter(|user| user.login.eq_ignore_ascii_case(login));
    if let Some(user) = &user {
        tracing::info!(
            target: "gql",
            "parsed user {}({}){}",
            user.login,
            user.id,
            if user.live { " live" } else { "" }
        );
    } else {
        tracing::info!(target: "gql", "no user resolved for login={login:?}");
    }
    Ok(user)
}

/// Resolves a batch of channel ids for the Hermes baseline and `stream-up`
/// refreshes.
pub async fn fetch_channels(
    client: &Client,
    client_id: Option<&str>,
    ids: &[u64],
) -> Result<Vec<ChannelDetail>, GqlError> {
    let body = sonic_rs::to_vec(&sonic_rs::json!({
        "query": USERS_BY_IDS_QUERY,
        "variables": { "ids": ids.iter().map(ToString::to_string).collect::<Vec<_>>() },
        "operationName": "UsersByIds",
    }))?;
    tracing::info!(target: "gql", "request: ids={ids:?}");
    let payload = post(client, configured_client_id(client_id), &body).await?;
    let response: GqlIdsResponse = sonic_rs::from_slice(&payload)?;
    let users = response
        .data
        .and_then(|data| data.users)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|raw| raw.and_then(GqlUser::into_channel))
        .collect::<Vec<_>>();
    tracing::info!(
        target: "gql",
        "parsed {} user(s): {}",
        users.len(),
        users
            .iter()
            .map(|user| format!(
                "{}({}){}",
                user.login,
                user.id,
                if user.live { " live" } else { "" }
            ))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(users)
}

/// Downloads one small asset (a notification avatar) to `path`. The body is
/// read before the file is created, so a truncated transfer leaves no file.
pub async fn fetch_file(client: &Client, url: &str, path: &Path) -> Result<(), GqlError> {
    let mut response = crate::http::timeout(GQL_TIMEOUT, client.get(url))
        .await
        .map_err(|_| GqlError::Timeout)??;
    if !response.status.is_success() {
        return Err(GqlError::Status(response.status));
    }
    let body = crate::http::timeout(GQL_TIMEOUT, response.body.read_to_end(MAX_IMAGE))
        .await
        .map_err(|_| GqlError::Timeout)??;
    if let Some(parent) = path.parent() {
        compio::fs::create_dir_all(parent).await?;
    }
    compio::fs::write(path, body).await.0?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_users(body: &[u8]) -> Vec<ChannelDetail> {
        let response: GqlIdsResponse = sonic_rs::from_slice(body).unwrap();
        response
            .data
            .and_then(|data| data.users)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|raw| raw.and_then(GqlUser::into_channel))
            .collect()
    }

    fn decode_login(body: &[u8]) -> Option<ChannelDetail> {
        let response: GqlLoginResponse = sonic_rs::from_slice(body).unwrap();
        response
            .data
            .and_then(|data| data.user)
            .and_then(GqlUser::into_channel)
    }

    #[test]
    fn typed_decode_keeps_live_user_with_game() {
        let body = br#"{"data": {"users": [
                {"id": "123", "login": "alice", "displayName": "Alice",
                 "profileImageURL": "http://x/y.png",
                 "broadcastSettings": {"title": "hi",
                    "game": {"id": "10", "name": "g", "displayName": "G"}},
                 "stream": {"viewersCount": 583, "collaborationViewersCount": null,
                    "id": "42", "createdAt": "2026-09-10T18:35:06Z"}}
            ]}}"#;
        let users = decode_users(body);
        assert_eq!(users.len(), 1, "{users:?}");
        let user = &users[0];
        assert_eq!(user.id, 123);
        assert_eq!(user.login, "alice");
        assert_eq!(user.display_name, "Alice");
        assert_eq!(user.profile_image_url, "http://x/y.png");
        assert_eq!(user.stream_id, NonZeroU64::new(42));
        assert_eq!(user.stream_title.as_deref(), Some("hi"));
        assert_eq!(user.stream_start, parse_iso_ms("2026-09-10T18:35:06Z"));
        let game = user.game.as_ref().expect("game should decode");
        assert_eq!(game.id, 10);
        assert_eq!(game.name, "g");
        assert_eq!(game.display_name, "G");
        assert_eq!(user.viewers, Some(583));
        assert_eq!(user.collaboration_viewers, None);
        assert!(user.live);
    }

    #[test]
    fn typed_decode_accepts_numeric_ids_and_offline_user() {
        let body = br#"{"data": {"users": [
                {"id": 456, "login": "bob", "displayName": "Bob",
                 "profileImageURL": "http://x/z.png",
                 "broadcastSettings": {"title": "last night",
                    "game": {"id": 7, "name": "gg", "displayName": "GG"}},
                 "stream": null}
            ]}}"#;
        let users = decode_users(body);
        assert_eq!(users.len(), 1, "{users:?}");
        let user = &users[0];
        assert_eq!(user.id, 456);
        // Offline: `stream` is null, so no broadcast identity — but the title
        // and game still come through from `broadcastSettings`.
        assert!(user.stream_id.is_none());
        assert_eq!(user.stream_title.as_deref(), Some("last night"));
        assert_eq!(
            user.game.as_ref().map(|game| game.display_name.as_str()),
            Some("GG")
        );
        assert!(user.stream_start.is_none());
        assert!(user.viewers.is_none());
        assert!(user.collaboration_viewers.is_none());
        assert!(!user.live);
    }

    #[test]
    fn typed_decode_carries_the_last_broadcast_time() {
        let body = br#"{"data": {"users": [
                {"id": "1", "login": "alice", "displayName": "Alice",
                 "profileImageURL": "http://x/y.png",
                 "broadcastSettings": {"title": "last night", "game": null},
                 "stream": null,
                 "lastBroadcast": {"startedAt": "2026-11-20T13:41:07.123Z"}}
            ]}}"#;
        let users = decode_users(body);
        assert_eq!(users.len(), 1, "{users:?}");
        assert_eq!(users[0].last_live, parse_iso_ms("2026-11-20T13:41:07.123Z"));
        assert!(!users[0].live);
    }

    #[test]
    fn typed_decode_without_a_last_broadcast_has_no_last_live() {
        let body = br#"{"data": {"users": [
                {"id": "1", "login": "alice", "displayName": "Alice",
                 "profileImageURL": "http://x/y.png",
                 "broadcastSettings": {"title": "t", "game": null},
                 "stream": null}
            ]}}"#;
        let users = decode_users(body);
        assert_eq!(users.len(), 1, "{users:?}");
        assert!(users[0].last_live.is_none());
    }

    #[test]
    fn typed_decode_skips_bad_entries_but_keeps_batch() {
        let body = br#"{"data": {"users": [
                null,
                {"id": "bad", "login": "ghost",
                 "broadcastSettings": {"title": "t", "game": null},
                 "stream": null},
                {"id": "789", "login": "nobra",
                 "broadcastSettings": null, "stream": null},
                {"id": "123", "login": "alice", "displayName": "Alice",
                 "profileImageURL": "http://x/y.png",
                 "broadcastSettings": {"title": "hi",
                    "game": {"id": "10", "name": "g", "displayName": "G"}},
                 "stream": {"id": "1", "createdAt": "2026-09-10T18:35:06Z"}}
            ]}}"#;
        let users = decode_users(body);
        assert_eq!(users.len(), 1, "{users:?}");
        assert_eq!(users[0].id, 123);
        assert_eq!(users[0].stream_id, NonZeroU64::new(1));
    }

    #[test]
    fn typed_decode_tolerates_null_data_and_users() {
        assert!(decode_users(br#"{"data": null}"#).is_empty());
        assert!(decode_users(br#"{"data": {}}"#).is_empty());
        assert!(decode_users(br#"{"data": {"users": null}}"#).is_empty());
    }

    #[test]
    fn chat_color_is_used_when_present_and_generated_when_null() {
        let with = br##"{"data": {"users": [
            {"id": "123", "login": "alice", "chatColor": "#FF0000", "displayName": "Alice",
             "profileImageURL": "http://x/y.png",
             "broadcastSettings": {"title": "hi", "game": null}, "stream": null}
        ]}}"##;
        let user = &decode_users(with)[0];
        assert_eq!(user.chat_color, ChatColor::Twitch(0x00ff_0000));
        assert_eq!(user.chat_color.to_string(), "#ff0000");

        let without = br#"{"data": {"users": [
            {"id": "123", "login": "alice", "chatColor": null, "displayName": "Alice",
             "profileImageURL": "http://x/y.png",
             "broadcastSettings": {"title": "hi", "game": null}, "stream": null}
        ]}}"#;
        let user = &decode_users(without)[0];
        assert!(matches!(user.chat_color, ChatColor::Generated { .. }));
        assert_eq!(user.chat_color, fallback_color(123));
        assert_eq!(fallback_color(123), fallback_color(123));
        assert_ne!(fallback_color(123), fallback_color(456));
        assert!(fallback_color(123).to_string().starts_with("oklch("));
    }

    #[test]
    fn a_malformed_chat_color_falls_back_instead_of_reaching_css() {
        assert_eq!(
            parse_twitch_color("#ff0000"),
            Some(ChatColor::Twitch(0x00ff_0000))
        );
        assert_eq!(
            parse_twitch_color("9146ff"),
            Some(ChatColor::Twitch(0x0091_46ff))
        );
        assert_eq!(parse_twitch_color("#ff000"), None);
        assert_eq!(parse_twitch_color("#zzzzzz"), None);
        assert_eq!(parse_twitch_color(""), None);
    }

    #[test]
    fn login_decode_maps_unknown_or_bad_user_to_none() {
        assert!(decode_login(br#"{"data": {"user": null}}"#).is_none());
        assert!(decode_login(br#"{"data": null}"#).is_none());
        assert!(
            decode_login(
                br#"{"data": {"user":
                    {"id": "123", "login": "alice", "displayName": "Alice",
                     "profileImageURL": "http://x/y.png",
                     "broadcastSettings": null, "stream": null}
                }}"#
            )
            .is_none()
        );
    }
}
