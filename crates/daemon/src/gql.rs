//! Twitch GQL: channel resolution, the live-state baseline and avatar fetches.
//!
//! This is Siphon's typed decoder, moved onto the daemon's own HTTP client and
//! `sonic_rs`, so the backend keeps one TLS/connection stack.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde::Deserializer;
use serde::de::Visitor;
use siphon_rpc::ChannelView;
use thiserror::Error;

use crate::constants::DEFAULT_CLIENT_ID;
use crate::constants::MAX_IMAGE;
use crate::constants::MAX_JSON;
use crate::http::Client;
use crate::http::StatusCode;
use crate::time::parse_iso_ms;

const GQL_URL: &str = "https://gql.twitch.tv/gql";
const GQL_TIMEOUT: Duration = Duration::from_secs(15);

const USERS_BY_IDS_QUERY: &str = "query UsersByIds($ids:[ID!]){users(ids:$ids){id login displayName profileImageURL(width:70) lastBroadcast{id title game{id name displayName}}stream{id createdAt viewersCount collaborationViewersCount}}}";
const USER_BY_LOGIN_QUERY: &str = "query UserByLogin($login:String!){user(login:$login){id login displayName profileImageURL(width:70) lastBroadcast{id title game{id name displayName}}stream{id createdAt viewersCount collaborationViewersCount}}}";

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
    pub profile_image_url: String,
    /// Local avatar file, once the app has cached the remote picture.
    pub avatar_path: Option<String>,
    /// Twitch's `BROADCAST-ID` from the last broadcast; 0 when absent.
    pub stream_id: u64,
    pub stream_title: Option<String>,
    /// Milliseconds since the unix epoch; `None` when offline.
    pub stream_start: Option<i64>,
    /// Raw `createdAt` from the stream, kept for logging; `None` offline.
    pub stream_created_at: Option<String>,
    pub viewers: Option<u32>,
    pub collaboration_viewers: Option<u32>,
    pub game: Option<StreamGame>,
    pub live: bool,
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
    pub fn view(&self, channel: Option<&siphon_rpc::Channel>) -> ChannelView {
        ChannelView {
            id: self.id,
            login: self.login.clone(),
            display_name: self.display_name.clone(),
            profile_image_url: self.profile_image_url.clone(),
            avatar_path: self.avatar_path.clone(),
            live: self.live,
            title: self.stream_title.clone(),
            game: self.game.as_ref().map(|game| game.display_name.clone()),
            game_id: self.game.as_ref().map(|game| game.id),
            viewers: self.viewers,
            collaboration_viewers: self.collaboration_viewers,
            stream_start_ms: self.stream_start,
            stream_created_at: self.stream_created_at.clone(),
            stream_id: self.stream_id,
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
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    #[serde(rename = "profileImageURL")]
    profile_image_url: Option<String>,
    #[serde(rename = "lastBroadcast")]
    broadcast: Option<GqlBroadcast>,
    stream: Option<GqlStream>,
}

#[derive(Debug, Deserialize)]
struct GqlBroadcast {
    #[serde(default, deserialize_with = "de_opt_u64")]
    id: Option<u64>,
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

impl GqlUser {
    fn into_channel(self) -> Option<ChannelDetail> {
        let broadcast = self.broadcast?;
        Some(ChannelDetail {
            id: self.id?,
            login: self.login.filter(|login| !login.is_empty())?,
            display_name: self.display_name.filter(|name| !name.is_empty())?,
            profile_image_url: self.profile_image_url.filter(|url| !url.is_empty())?,
            avatar_path: None,
            stream_id: broadcast.id?,
            stream_title: broadcast.title.filter(|title| !title.is_empty()),
            stream_start: self
                .stream
                .as_ref()
                .and_then(|stream| stream.created_at.as_deref())
                .and_then(parse_iso_ms),
            stream_created_at: self
                .stream
                .as_ref()
                .and_then(|stream| stream.created_at.clone()),
            game: broadcast.game.and_then(|game| {
                Some(StreamGame {
                    id: game.id?,
                    name: game.name?,
                    display_name: game.display_name?,
                })
            }),
            collaboration_viewers: self
                .stream
                .as_ref()
                .and_then(|stream| stream.collaboration_viewers_count),
            viewers: self.stream.as_ref().and_then(|stream| stream.viewers_count),
            live: self.stream.is_some(),
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
                 "lastBroadcast": {"id": "999", "title": "hi",
                    "game": {"id": "10", "name": "g", "displayName": "G"}},
                 "stream": {"id": "1", "createdAt": "2026-09-10T18:35:06Z",
                    "viewersCount": 583, "collaborationViewersCount": null}}
            ]}}"#;
        let users = decode_users(body);
        assert_eq!(users.len(), 1, "{users:?}");
        let user = &users[0];
        assert_eq!(user.id, 123);
        assert_eq!(user.login, "alice");
        assert_eq!(user.display_name, "Alice");
        assert_eq!(user.profile_image_url, "http://x/y.png");
        assert_eq!(user.stream_id, 999);
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
                 "lastBroadcast": {"id": 777, "title": "", "game": null},
                 "stream": null}
            ]}}"#;
        let users = decode_users(body);
        assert_eq!(users.len(), 1, "{users:?}");
        let user = &users[0];
        assert_eq!(user.id, 456);
        assert_eq!(user.stream_id, 777);
        assert!(user.stream_title.is_none());
        assert!(user.stream_start.is_none());
        assert!(user.viewers.is_none());
        assert!(user.collaboration_viewers.is_none());
        assert!(user.game.is_none());
        assert!(!user.live);
    }

    #[test]
    fn typed_decode_skips_bad_entries_but_keeps_batch() {
        let body = br#"{"data": {"users": [
                null,
                {"id": "bad", "login": "ghost",
                 "lastBroadcast": {"id": "1", "title": "t", "game": null},
                 "stream": null},
                {"id": "789", "login": "nobra",
                 "lastBroadcast": null, "stream": null},
                {"id": "123", "login": "alice", "displayName": "Alice",
                 "profileImageURL": "http://x/y.png",
                 "lastBroadcast": {"id": "999", "title": "hi",
                    "game": {"id": "10", "name": "g", "displayName": "G"}},
                 "stream": {"id": "1", "createdAt": "2026-09-10T18:35:06Z"}}
            ]}}"#;
        let users = decode_users(body);
        assert_eq!(users.len(), 1, "{users:?}");
        assert_eq!(users[0].id, 123);
    }

    #[test]
    fn typed_decode_tolerates_null_data_and_users() {
        assert!(decode_users(br#"{"data": null}"#).is_empty());
        assert!(decode_users(br#"{"data": {}}"#).is_empty());
        assert!(decode_users(br#"{"data": {"users": null}}"#).is_empty());
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
                     "lastBroadcast": null, "stream": null}
                }}"#
            )
            .is_none()
        );
    }
}
