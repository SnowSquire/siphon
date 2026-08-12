use crate::{
    ManifestError,
    constants::{PLAYER_VERSION, TWITCH_GQL_ENDPOINT, TWITCH_HLS_BASE},
};
use base64::engine::general_purpose::STANDARD;
use rand::{RngExt, distr::Alphanumeric};
use sonic_rs::{JsonInput, JsonValueTrait, PointerTree, pointer};
use std::ops::Range;
use tracing::info;
use ureq::{Agent, ResponseExt};

pub fn fetch_master_manifest(
    agent: &Agent,
    channel: &str,
    client_id: &str,
    auth_token: Option<&str>,
    low_latency: bool,
) -> Result<String, ManifestError> {
    //  Get Access Token --------------------------------------------------
    let req = agent
        .post(TWITCH_GQL_ENDPOINT)
        .content_type("text/plain;charset=UTF-8")
        .header("Device-ID", random_id())
        .header("Client-ID", client_id);

    let req = if let Some(auth_token) = auth_token {
        req.header("Authorization", format!("OAuth {auth_token}"))
    } else {
        req
    };

    let res = req.send(sonic_rs::to_vec(&sonic_rs::json!({
        "extensions": {
            "persistedQuery": {
                "sha256Hash": "ed230aa1e33e07eebb8928504583da78a5173989fadfb1ac94be06a04f3cdbe9",
                "version": 1
            }
        },
        "operationName": "PlaybackAccessToken",
        "variables": {
            "isLive": true,
            "isVod": false,
            "login": channel,
            "playerType": "site",
            "platform": "site",
            "vodID": ""
        }
    })).map_err(|_|ManifestError::IDK)?).map_err(ManifestError::AccessTokenRequest)?;
    info!("sent pat req");
    let res = res
        .into_body()
        .read_to_vec()
        .map_err(ManifestError::AccessTokenParse)?;

    // setup master playlist -------------------------------------------

    let mut tree = PointerTree::new();
    tree.add_path(pointer!["data", "streamPlaybackAccessToken", "value"]);
    tree.add_path(pointer!["data", "streamPlaybackAccessToken", "signature"]);

    info!("added paths");
    let parsed_json = sonic_rs::get_many(res.as_slice(), &tree)?;

    let token_value = parsed_json[0].as_str().ok_or(ManifestError::TokenMissing)?;

    let signature = parsed_json[1]
        .as_str()
        .ok_or(ManifestError::SignatureMissing)?;

    let token_channel = sonic_rs::get_from_str(token_value, pointer!["channel"])?;

    let token_channel = token_channel
        .as_str()
        .ok_or(ManifestError::ChannelMissing)?;

    let acmb = sonic_rs::json!({
        "AppVersion": "8faed90d-c8f5-46a7-9d7c-4934daaba82",
        "ClientApp": "twilight",
        "URL": format!("https://www.twitch.tv/{token_channel}"),
    });

    let acmb_str = sonic_rs::to_string(&acmb).map_err(|_| ManifestError::ChannelMissing)?;
    let acmb_b64 = base64::Engine::encode(&STANDARD, acmb_str.as_bytes());

    let p_value = rand::random_range(0..9_999_999);
    // info!(%acmb_str, %token_value_str, %signature);
    let manifest = agent
        .get(format!("{TWITCH_HLS_BASE}/{token_channel}.m3u8"))
        .query_pairs([
            ("acmb", acmb_b64.as_str()),
            ("allow_source", "true"),
            // ("allow_audio_only", "true"),
            ("browser_family", "chrome"),
            ("browser_version", "151.0"),
            ("cdm", "wv"),
            ("enable_score", "true"),
            (
                "fast_bread",
                low_latency.then_some("true").unwrap_or("false"),
            ),
            ("include_unavailable", "true"),
            ("lang", "en"),
            ("os_name", "Windows"),
            ("os_version", "NT 10.0"),
            ("p", &p_value.to_string()),
            ("platform", "web"),
            ("play_session_id", &random_id()),
            ("player_backend", "mediaplayer"),
            ("player_version", PLAYER_VERSION),
            ("playlist_include_framerate", "true"),
            ("reassignments_supported", "true"),
            ("sig", signature),
            ("supported_codecs", "av1,h265,h264"),
            ("token", token_value),
            ("transcode_mode", "cbr_v1"),
            // ("multigroup_video", "false"),
            // ("warp", low_latency.then_some("true").unwrap_or("false")),
        ]);

    manifest
        .call()
        .map_err(|e| ManifestError::ManifestRequest {
            source: e,
            url: "".into(),
        })?
        .into_body()
        .read_to_string()
        .map_err(ManifestError::ManifestRead)
}

fn random_id() -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

fn attributes(tag: &str) -> impl Iterator<Item = (&str, &str)> {
    let mut rest = tag.split_once(':').map_or("", |(_, attrs)| attrs);
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }

        let eq: usize = rest.find('=')?;
        let key = rest[..eq].trim();
        let after_eq = &rest[eq + 1..];

        let (value, tail) = if let Some(quoted) = after_eq.strip_prefix('"') {
            let close = quoted.find('"')?;
            let value = &quoted[..close];
            let tail = quoted[close + 1..].strip_prefix(',').unwrap_or("");
            (value, tail)
        } else {
            after_eq
                .split_once(',')
                .map_or((after_eq.trim(), ""), |(v, t)| (v.trim(), t))
        };

        rest = tail;
        Some((key, value))
    })
}

fn get_attr<'a>(tag: &'a str, key: &str) -> Option<&'a str> {
    attributes(tag).find(|(k, _)| *k == key).map(|(_, v)| v)
}

fn parse_name<'a>(media: Option<&'a str>, stream_inf: &'a str) -> Option<&'a str> {
    let name = media
        .and_then(|m| get_attr(m, "NAME"))
        .or_else(|| get_attr(stream_inf, "IVS-NAME"))?;
    Some(name.strip_suffix(" (source)").unwrap_or(name))
}

fn parse_resolution(stream_inf: &str) -> Option<(u16, u16)> {
    let (w, h) = get_attr(stream_inf, "RESOLUTION")?.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

struct Candidate<'a> {
    url_range: &'a str,
    area: u32,
}

// "track" isn't the best naming, but "stream" is already used in this project
// to mean a stream of bytes (from an HTTP source), and this combo has the best
// overall fit I can think of for now.
pub fn select_track(buffer: &str, preferred_prefix: Option<&str>) -> Option<Box<str>> {
    let mut media: Option<&str> = None;
    let mut stream_inf: Option<&str> = None;
    let mut best: Option<Candidate> = None;

    for line in buffer.lines() {
        if line.starts_with("#EXT-X-MEDIA") {
            media = Some(line);
        } else if line.starts_with("#EXT-X-STREAM-INF") {
            stream_inf = Some(line);
        } else if line.starts_with("http") {
            let Some(inf) = stream_inf.take() else {
                continue;
            };

            let Some(name) = parse_name(media.take(), inf) else {
                continue;
            };

            if let Some(prefix) = preferred_prefix {
                if name.starts_with(prefix) {
                    return Some(line.into());
                }
            }
            let area = parse_resolution(inf).map_or(0, |(w, h)| w as u32 * h as u32);
            if best.as_ref().is_none_or(|b| area > b.area) {
                best = Some(Candidate {
                    url_range: line,
                    area,
                });
            }
        }
    }

    best.map(|b| b.url_range.into())
}
