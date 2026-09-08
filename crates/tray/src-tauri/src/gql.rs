use std::time::Duration;

use serde_json::{json, Value};

use crate::logging::log;

const GQL_URL: &str = "https://gql.twitch.tv/gql";
const CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const GQL_TIMEOUT: Duration = Duration::from_secs(15);

const USERS_BY_IDS_QUERY: &str = "query UsersByIds($ids:[ID!]){users(ids:$ids){id login displayName broadcastSettings{id title game{id name displayName}}stream{id createdAt}}}";
const USERS_BY_LOGINS_QUERY: &str = "query UsersByLogins($logins:[String!]){users(logins:$logins){id login displayName broadcastSettings{id title game{id name displayName}}stream{id createdAt}}}";

// Mirrors the original hermesnotifier.ts `StreamInfo`: every field the TS
// client tracked is kept here even though only a subset is read.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct User {
    pub channel_id: u64,
    pub channel_name: String,
    pub channel_display_name: String,
    pub stream_id: Option<u64>,
    pub stream_title: String,
    pub stream_start: Option<String>,
    pub game_id: Option<u64>,
    pub game_name: Option<String>,
    pub game_display_name: Option<String>,
    pub live: bool,
}

pub fn fetch_users(ids: &[u64], logins: &[String]) -> Result<Vec<User>, ureq::Error> {
    let mut operations = Vec::with_capacity(2);
    if !ids.is_empty() {
        operations.push(json!({
            "query": USERS_BY_IDS_QUERY,
            "variables": {
                "ids": ids.iter().map(ToString::to_string).collect::<Vec<_>>()
            },
            "operationName": "UsersByIds",
        }));
    }
    if !logins.is_empty() {
        operations.push(json!({
            "query": USERS_BY_LOGINS_QUERY,
            "variables": { "logins": logins },
            "operationName": "UsersByLogins",
        }));
    }

    log(
        "gql",
        format!("request: ids={ids:?} logins={logins:?} ({GQL_URL})"),
    );
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(GQL_TIMEOUT))
            .build(),
    );
    let response: Value = agent
        .post(GQL_URL)
        .header("Client-ID", CLIENT_ID)
        .send_json(Value::Array(operations))?
        .body_mut()
        .read_json()?;
    log("gql", format!("response: {response}"));

    let Some(responses) = response.as_array() else {
        log("gql", "unexpected response shape, expected an array");
        return Ok(Vec::new());
    };
    let users = responses
        .iter()
        .flat_map(|response| {
            response["data"]["users"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|raw| {
            let channel_id = raw["id"].as_str()?.parse().ok()?;
            let broadcast = &raw["broadcastSettings"];
            Some(User {
                channel_id,
                channel_name: raw["login"].as_str().unwrap_or_default().into(),
                channel_display_name: raw["displayName"].as_str().unwrap_or_default().into(),
                stream_id: raw["stream"]["id"]
                    .as_str()
                    .and_then(|id| id.parse().ok())
                    .or_else(|| broadcast["id"].as_str().and_then(|id| id.parse().ok())),
                stream_title: broadcast["title"].as_str().unwrap_or_default().into(),
                stream_start: raw["stream"]["createdAt"].as_str().map(String::from),
                game_id: broadcast["game"]["id"]
                    .as_str()
                    .and_then(|id| id.parse().ok()),
                game_name: broadcast["game"]["name"]
                    .as_str()
                    .filter(|game| !game.is_empty())
                    .map(Into::into),
                game_display_name: broadcast["game"]["displayName"]
                    .as_str()
                    .filter(|game| !game.is_empty())
                    .map(Into::into),
                live: raw["stream"].is_object(),
            })
        })
        .collect::<Vec<_>>();
    log(
        "gql",
        format!(
            "parsed {} user(s): {}",
            users.len(),
            users
                .iter()
                .map(|user| format!(
                    "{}({}){}",
                    user.channel_name,
                    user.channel_id,
                    if user.live { " live" } else { "" }
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );
    Ok(users)
}
