use std::borrow::Cow;
use std::num::NonZeroU16;

use rand::RngExt;
use rand::distr::Alphanumeric;
use sonic_rs::JsonValueTrait;
use sonic_rs::PointerTree;
use sonic_rs::pointer;
use thiserror::Error;
use tracing::info;
use ureq::Agent;
use ureq::http::header::AUTHORIZATION;

use crate::constants::DEFAULT_CLIENT_ID;
use crate::constants::PLAYER_VERSION;
use crate::constants::TWITCH_GQL_ENDPOINT;
use crate::constants::TWITCH_HLS_BASE;
use crate::constants::TWITCH_OAUTH_ENDPOINT;
use crate::utils::attribute;
use crate::utils::attributes;
use crate::utils::float_string_to_centis;

#[derive(Debug, Error)]
pub enum VariantPlaylistError {
    #[error("failed to get playback access token: {0}")]
    AccessTokenRequest(#[source] ureq::Error),

    #[error("failed to parse access token response: {0}")]
    AccessTokenParse(#[source] ureq::Error),

    #[error("token value is null or missing in response")]
    TokenMissing,

    #[error("signature is missing in response")]
    SignatureMissing,

    #[error("token value is not valid JSON: {0}")]
    TokenJsonInvalid(#[from] sonic_rs::Error),

    #[error("channel field is missing from token")]
    ChannelMissing,

    #[error("failed to fetch HLS manifest: {source}, {url}")]
    ManifestRequest {
        #[source]
        source: ureq::Error,
        url: String,
    },

    #[error("failed to read manifest body: {0}")]
    ManifestRead(#[source] ureq::Error),
}

pub fn fetch_variant_playist(
    agent: &Agent,
    channel: &str,
    client_id: &str,
    auth_token: Option<&str>,
    low_latency: bool,
) -> Result<String, VariantPlaylistError> {
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
    }))?).map_err(VariantPlaylistError::AccessTokenRequest)?;
    info!("sent pat req");
    let res = res
        .into_body()
        .read_to_vec()
        .map_err(VariantPlaylistError::AccessTokenParse)?;

    // setup master playlist -------------------------------------------

    let mut tree = PointerTree::new();
    tree.add_path(pointer!["data", "streamPlaybackAccessToken", "value"]);
    tree.add_path(pointer!["data", "streamPlaybackAccessToken", "signature"]);

    info!("added paths");
    let parsed_json = sonic_rs::get_many(res.as_slice(), &tree)?;

    let token_value = parsed_json[0]
        .as_str()
        .ok_or(VariantPlaylistError::TokenMissing)?;

    let signature = parsed_json[1]
        .as_str()
        .ok_or(VariantPlaylistError::SignatureMissing)?;

    let token_channel = sonic_rs::get_from_str(token_value, pointer!["channel"])?;

    let token_channel = token_channel
        .as_str()
        .ok_or(VariantPlaylistError::ChannelMissing)?;

    // let acmb = sonic_rs::json!({
    //     "AppVersion": "8faed90d-c8f5-46a7-9d7c-4934daaba82",
    //     "ClientApp": "twilight",
    //     "URL": format!("https://www.twitch.tv/{token_channel}"),
    // });

    // let acmb_str = sonic_rs::to_string(&acmb).map_err(|_| ManifestError::ChannelMissing)?;
    // let acmb_b64 = base64::Engine::encode(&STANDARD, acmb_str.as_bytes());

    let p_value = rand::random_range(0..9_999_999);
    // info!(%acmb_str, %token_value_str, %signature);
    let manifest = agent
        .get(format!("{TWITCH_HLS_BASE}/{token_channel}.m3u8"))
        .query_pairs([
            // ("acmb", acmb_b64.as_str()),
            ("allow_source", "true"),     // enables source variant
            ("allow_audio_only", "true"), // enables audio only variant
            ("allow_spectre", "true"), // idk what this adds but twtich-dlp has it, to inspect, maybe the fabled ertmpaudio1?
            ("browser_family", "chrome"),
            ("browser_version", "151.0"),
            ("cdm", "wv"),
            ("enable_score", "true"), // enables SCORE attribute in EXT-X-STREAM-INF
            ("fast_bread", if low_latency { "true" } else { "false" }),
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
            ("multigroup_video", "true"), // enables vertical  variants
        ]);

    manifest
        .call()
        .map_err(|e| VariantPlaylistError::ManifestRequest {
            source: e,
            url: String::new(),
        })?
        .into_body()
        .read_to_string()
        .map_err(VariantPlaylistError::ManifestRead)
}

fn random_id() -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

#[derive(Clone)]
pub enum VariantKind {
    Landscape,
    Portrait,
    Audio,
    ERmtp, // ertmpaudio1, don't know how to access it as it's always unavailable
}
#[derive(Clone)]
pub enum Codec {
    H265,
    H264,
    Av1,
    Aac,
}

fn parse_kind(variant_id: &str) -> VariantKind {
    match variant_id {
        "ertmpaudio1" => VariantKind::ERmtp,
        s if s.ends_with("-portrait") => VariantKind::Portrait,
        "audio_only" => VariantKind::Audio,
        _ => VariantKind::Landscape,
    }
}

fn parse_stream_inf(line: &str, url: &str) -> Option<Variant> {
    let mut variant_id = None;
    let mut score = None;
    let mut frame_rate = None;
    let mut codec = None;
    let mut res = None;

    let attrs = line.split_once(':').map_or("", |(_, attrs)| attrs);
    for (key, value) in attributes(attrs) {
        match key {
            "STABLE-VARIANT-ID" => variant_id = Some(value),
            "SCORE" => score = float_string_to_centis(value),
            "FRAME-RATE" => frame_rate = float_string_to_centis(value),
            "CODECS" => {
                codec = match value.get(..4)? {
                    "avc1" => Some(Codec::H264),
                    "mp4a" => Some(Codec::Aac),
                    "av01" => Some(Codec::Av1),
                    "hvc1" | "hev1" => Some(Codec::H265),
                    _ => None,
                }
            }
            "RESOLUTION" => {
                if let Some((w, h)) = value.split_once('x') {
                    res = w.parse().ok().zip(h.parse().ok());
                }
            }
            _ => {}
        }
    }

    let variant_id = variant_id?;
    Some(Variant {
        score,
        url: url.into(),
        id: variant_id.into(),
        kind: parse_kind(variant_id),
        codec,
        res,
        frame_rate,
    })
}
#[allow(dead_code)]
#[derive(Clone)]
pub struct Variant {
    pub score: Option<NonZeroU16>,
    pub url: Box<str>,
    pub id: Box<str>,
    pub kind: VariantKind,
    pub codec: Option<Codec>,
    pub res: Option<(NonZeroU16, NonZeroU16)>,
    pub frame_rate: Option<NonZeroU16>,
}

fn parse_variant_playlist(buffer: &str) -> (Vec<Variant>, u64) {
    let mut stream_inf: Option<&str> = None;

    let mut var_vec = Vec::with_capacity(2); // always at least 2 tracks (source and audio_only)
    let mut stream_id = None;
    for line in buffer.lines() {
        if line.starts_with("#EXT-X-STREAM-INF") {
            stream_inf = Some(line);
        } else if line.starts_with("http") {
            let Some(stream_inf) = stream_inf.take() else {
                continue;
            };

            if let Some(variant) = parse_stream_inf(stream_inf, line) {
                var_vec.push(variant);
            }
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-SESSION-DATA:")
            && attribute(attrs, "DATA-ID") == Some("BROADCAST-ID")
            && let Some(value) = attribute(attrs, "VALUE")
        {
            stream_id = value.parse::<u64>().ok();
        }
    }

    var_vec.sort_by(|a, b| {
        b.score
            .map_or(0, NonZeroU16::get)
            .cmp(&a.score.map_or(0, NonZeroU16::get))
    });

    var_vec.shrink_to_fit();

    (var_vec, stream_id.unwrap())
}

fn choose_client_id<'a>(
    agent: &Agent,
    client_id: Option<&'a str>,
    auth_token: Option<&str>,
) -> Result<Cow<'a, str>, Box<dyn std::error::Error>> {
    match (client_id, auth_token) {
        (None, None) => Ok(DEFAULT_CLIENT_ID.into()),
        (Some(client_id), _) => Ok(client_id.into()),
        (None, Some(auth_token)) => {
            let response = agent
                .get(TWITCH_OAUTH_ENDPOINT)
                .header(AUTHORIZATION, format!("OAuth {auth_token}"))
                .call()?
                .into_body()
                .read_to_vec()?;
            Ok(sonic_rs::get(response.as_slice(), pointer!["client_id"])?
                .to_string()
                .into())
        }
    }
}

pub fn get_variants(
    agent: &Agent,
    client_id: Option<&str>,
    auth_token: Option<&str>,
    channel: &str,
    low_latency: bool,
) -> (Vec<Variant>, u64) {
    let client_id = choose_client_id(agent, client_id, auth_token).unwrap();

    let master_manifest =
        fetch_variant_playist(agent, channel, &client_id, auth_token, low_latency).unwrap();

    parse_variant_playlist(&master_manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stream_inf_with_quoted_codecs() {
        let line = "#EXT-X-STREAM-INF:BANDWIDTH=8000000,CODECS=\"avc1.4d401f,mp4a.40.2\",\
                    RESOLUTION=1920x1080,FRAME-RATE=60.000,SCORE=9.5,\
                    STABLE-VARIANT-ID=\"720p60\"";

        let variant = parse_stream_inf(line, "https://example/720p60.m3u8").unwrap();

        assert_eq!(variant.id.as_ref(), "720p60");
        assert_eq!(
            variant
                .res
                .map(|(width, height)| (width.get(), height.get())),
            Some((1920, 1080))
        );
        assert_eq!(variant.frame_rate.map(NonZeroU16::get), Some(6000));
        assert_eq!(variant.score.map(NonZeroU16::get), Some(950));
        assert!(matches!(variant.codec, Some(Codec::H264)));
    }

    #[test]
    fn reads_broadcast_id_from_session_data() {
        let playlist = "\
#EXTM3U
#EXT-X-SESSION-DATA:DATA-ID=\"BROADCAST-ID\",VALUE=\"123456789\"
#EXT-X-STREAM-INF:BANDWIDTH=8000000,STABLE-VARIANT-ID=\"720p60\"
https://example/720p60.m3u8
";
        let (variants, stream_id) = parse_variant_playlist(playlist);

        assert_eq!(variants.len(), 1);
        assert_eq!(stream_id, 123_456_789);
    }
}
