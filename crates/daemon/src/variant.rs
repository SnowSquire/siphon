use std::borrow::Cow;

use sonic_rs::JsonContainerTrait;
use sonic_rs::JsonValueTrait;
use sonic_rs::PointerTree;
use sonic_rs::pointer;
use thiserror::Error;

use crate::constants::DEFAULT_CLIENT_ID;
use crate::constants::MAX_JSON;
use crate::constants::MAX_MANIFEST;
use crate::constants::PLAYER_VERSION;
use crate::http::Client;
use crate::utils::attribute;
use crate::utils::attributes;
use crate::utils::float_string_to_centis;
use crate::utils::query_string;
use crate::utils::random_id;

/// One variant a master withheld, from `com.amazon.ivs.unavailable-media`.
#[derive(Debug, PartialEq, Eq)]
pub struct Unavailable {
    pub id: String,
    pub groups: String,
    pub resolution: String,
    pub reasons: String,
}

impl Unavailable {
    /// Audio-only renditions are the expected entries: `ertmpaudio1` is
    /// Twitch's separate muted-audio track and `audio_only` is the one the VOD
    /// endpoint returns only when asked for. A withheld *video* variant means
    /// the request did not ask for something it could have had.
    fn is_audio(&self) -> bool {
        self.resolution.is_empty() || self.id == "ertmpaudio1" || self.id == "audio_only"
    }
}

/// The variants a master withheld. Twitch publishes them as base64 JSON in
/// `#EXT-X-SESSION-DATA:DATA-ID="com.amazon.ivs.unavailable-media"`:
///
/// ```json
/// [{"STABLE-VARIANT-ID":"1080p60-portrait","IVS-GROUPS":"portrait",
///   "RESOLUTION":"1080x1920","FILTER_REASONS":["FR_DUALFORMAT_INCOMPATIBLE"]}]
/// ```
///
/// Without `multigroup_video=true` a dual-format archive lists its whole
/// portrait group here; the live master normally lists only `ertmpaudio1`.
#[must_use]
pub fn unavailable_media(master: &str) -> Vec<Unavailable> {
    let Some(encoded) = master.lines().find_map(|line| {
        let attributes = line.strip_prefix("#EXT-X-SESSION-DATA:")?;
        (attribute(attributes, "DATA-ID")? == "com.amazon.ivs.unavailable-media")
            .then(|| attribute(attributes, "VALUE"))?
    }) else {
        return Vec::new();
    };
    let Some(json) =
        crate::utils::base64_decode(encoded).and_then(|bytes| String::from_utf8(bytes).ok())
    else {
        return Vec::new();
    };
    let Ok(value) = sonic_rs::from_str::<sonic_rs::Value>(&json) else {
        return Vec::new();
    };
    let Some(entries) = value.as_array() else {
        return Vec::new();
    };

    entries
        .iter()
        .filter_map(|entry| {
            let text = |key: &str| {
                entry
                    .pointer([key])
                    .and_then(JsonValueTrait::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let id = text("STABLE-VARIANT-ID");
            if id.is_empty() {
                return None;
            }
            let reasons = entry
                .pointer(["FILTER_REASONS"])
                .and_then(|reasons| reasons.as_array())
                .map_or_else(String::new, |reasons| {
                    reasons
                        .iter()
                        .filter_map(JsonValueTrait::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                });
            Some(Unavailable {
                id,
                groups: text("IVS-GROUPS"),
                resolution: text("RESOLUTION"),
                reasons,
            })
        })
        .collect()
}

/// Log what a master withheld. Only video variants are reported: a withheld
/// audio rendition is normal and would otherwise repeat on every fetch.
pub fn log_unavailable_media(master: &str, source: &str) {
    let entries = unavailable_media(master);
    if entries.is_empty() {
        return;
    }
    let unexpected: Vec<&Unavailable> = entries.iter().filter(|entry| !entry.is_audio()).collect();
    if unexpected.is_empty() {
        tracing::debug!(
            source,
            withheld = entries.len(),
            "master withheld audio-only variants"
        );
        return;
    }
    let listed = unexpected
        .iter()
        .map(|entry| {
            format!(
                "{} ({} [{}] groups={})",
                entry.id, entry.resolution, entry.reasons, entry.groups
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    tracing::warn!(
        source,
        withheld = %listed,
        "master withheld video variants; the request did not ask for them"
    );
}

#[derive(Debug, Error)]
pub enum VariantPlaylistError {
    #[error("http: {0}")]
    Request(#[from] crate::http::Error),

    #[error("token value is null or missing in response")]
    TokenMissing,

    #[error("signature is missing in response")]
    SignatureMissing,

    #[error("token value is not valid JSON: {0}")]
    TokenJsonInvalid(#[from] sonic_rs::Error),

    #[error("channel field is missing from token")]
    ChannelMissing,

    #[error("manifest is not valid utf-8")]
    Utf8,

    #[error("manifest request returned {0}")]
    Status(crate::http::StatusCode),

    #[error("no variants for {0}: is the channel live?")]
    NoVariants(String),

    #[error("no variant {quality} for {channel}")]
    UnknownQuality { channel: String, quality: String },
}

pub async fn fetch_variant_playlist(
    client: &Client,
    channel: &str,
    client_id: &str,
    auth_token: Option<&str>,
    low_latency: bool,
) -> Result<String, VariantPlaylistError> {
    let body = sonic_rs::to_vec(&sonic_rs::json!({
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
    }))?;

    let device_id = random_id();
    let authorization = auth_token.map(|token| format!("OAuth {token}"));
    let mut headers: Vec<(&str, &str)> = vec![("Device-ID", &device_id), ("Client-ID", client_id)];
    if let Some(authorization) = &authorization {
        headers.push(("Authorization", authorization));
    }

    let mut response = client
        .post(
            crate::constants::gql_endpoint(),
            "text/plain;charset=UTF-8",
            &body,
            &headers,
        )
        .await?;
    let payload = response.body.read_to_end(MAX_JSON).await?;

    let mut tree = PointerTree::new();
    tree.add_path(pointer!["data", "streamPlaybackAccessToken", "value"]);
    tree.add_path(pointer!["data", "streamPlaybackAccessToken", "signature"]);

    let parsed_json = sonic_rs::get_many(payload.as_slice(), &tree)?;

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

    let p_value = rand::random_range(0..9_999_999).to_string();
    let session_id = random_id();
    let params = [
        ("allow_source", "true"),
        ("allow_audio_only", "true"),
        ("browser_family", "chrome"),
        ("browser_version", "151.0"),
        ("cdm", "wv"),
        ("enable_score", "true"), // enables SCORE attribute in EXT-X-STREAM-INF
        ("fast_bread", if low_latency { "true" } else { "false" }),
        ("include_unavailable", "true"),
        ("lang", "en"),
        ("os_name", "Windows"),
        ("os_version", "NT 10.0"),
        ("p", p_value.as_str()),
        ("platform", "web"),
        ("play_session_id", session_id.as_str()),
        ("player_backend", "mediaplayer"),
        ("player_version", PLAYER_VERSION),
        ("playlist_include_framerate", "true"),
        ("reassignments_supported", "true"),
        ("sig", signature),
        ("supported_codecs", "av1,h265,h264"),
        ("token", token_value),
        ("transcode_mode", "cbr_v1"),
        ("multigroup_video", "true"), // enables vertical variants
    ];

    let url = format!(
        "{}/{token_channel}.m3u8?{}",
        crate::constants::hls_base(),
        query_string(&params)
    );

    let mut response = client.get(&url).await?;
    if !response.status.is_success() {
        return Err(VariantPlaylistError::Status(response.status));
    }
    let body = response.body.read_to_end(MAX_MANIFEST).await?;

    String::from_utf8(body).map_err(|_| VariantPlaylistError::Utf8)
}

#[derive(Clone, Debug, PartialEq, Eq)]
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

fn parse_kind(
    variant_id: &str,
    res: Option<(std::num::NonZeroU16, std::num::NonZeroU16)>,
) -> VariantKind {
    // `RESOLUTION` is the only stable signal: Twitch names a portrait source by
    // its long edge in the live master ("1920p" at 1080x1920) and by its short
    // edge with a suffix in the archive ("1080p-portrait"), so the id alone
    // classifies the same track differently on the two sides.
    if let Some((width, height)) = res {
        return if height > width {
            VariantKind::Portrait
        } else {
            VariantKind::Landscape
        };
    }
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
        kind: parse_kind(variant_id, res),
        codec,
        res,
        frame_rate,
    })
}
#[allow(dead_code)]
#[derive(Clone)]
pub struct Variant {
    pub score: Option<std::num::NonZeroU16>,
    pub url: Box<str>,
    pub id: Box<str>,
    pub kind: VariantKind,
    pub codec: Option<Codec>,
    pub res: Option<(std::num::NonZeroU16, std::num::NonZeroU16)>,
    pub frame_rate: Option<std::num::NonZeroU16>,
}

/// The highest-`SCORE` variant, which is what a recording takes when no id is
/// named. `SCORE` is Twitch's own ranking; `choose_variant` prefers the first
/// variant instead, which is what an unqualified RTSP request means.
#[must_use]
pub fn best_variant(variants: &[Variant]) -> Option<&Variant> {
    variants
        .iter()
        .max_by_key(|variant| variant.score.map_or(0, std::num::NonZeroU16::get))
}

/// The variant an explicit quality id names, or the first one when `None`.
#[must_use]
pub fn choose_variant<'a>(variants: &'a [Variant], quality: Option<&str>) -> Option<&'a Variant> {
    quality.map_or_else(
        || variants.first(),
        |quality| {
            variants
                .iter()
                .find(|variant| variant.id.as_ref() == quality)
        },
    )
}

/// The landscape variant debug mode pretends the mock server offers, on `base`.
///
/// The score is what `best_variant` ranks monitors by, and it must beat the
/// portrait track: a channel's primary recording is the landscape source, the
/// portrait one is the optional second recording.
///
/// Dev builds only (`test` keeps it for the release-profile test binary).
#[cfg(any(test, debug_assertions))]
#[must_use]
pub fn debug_variant(base: &str) -> Variant {
    Variant {
        score: std::num::NonZeroU16::new(6),
        url: format!("{base}/index.m3u8").into(),
        id: crate::constants::DEBUG_VARIANT_ID.into(),
        kind: VariantKind::Landscape,
        codec: None,
        res: std::num::NonZeroU16::new(1920).zip(std::num::NonZeroU16::new(1080)),
        frame_rate: None,
    }
}

/// The portrait track of a dual-format broadcast, which the mock offers as a
/// second group (`IVS-GROUPS="portrait"`).
///
/// Dev builds only (`test` keeps it for the release-profile test binary).
#[cfg(any(test, debug_assertions))]
#[must_use]
pub fn debug_portrait_variant(base: &str) -> Variant {
    Variant {
        score: std::num::NonZeroU16::new(5),
        url: format!("{base}/portrait.m3u8").into(),
        id: crate::constants::DEBUG_PORTRAIT_VARIANT_ID.into(),
        kind: VariantKind::Portrait,
        codec: None,
        res: std::num::NonZeroU16::new(1080).zip(std::num::NonZeroU16::new(1920)),
        frame_rate: None,
    }
}

fn parse_variant_playlist(buffer: &str) -> (Vec<Variant>, Option<u64>) {
    log_unavailable_media(buffer, "live");

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
            .map_or(0, std::num::NonZeroU16::get)
            .cmp(&a.score.map_or(0, std::num::NonZeroU16::get))
    });

    var_vec.shrink_to_fit();

    (var_vec, stream_id)
}

async fn choose_client_id<'a>(
    client: &Client,
    client_id: Option<&'a str>,
    auth_token: Option<&str>,
) -> Result<Cow<'a, str>, VariantPlaylistError> {
    match (client_id, auth_token) {
        (None, None) => Ok(DEFAULT_CLIENT_ID.into()),
        (Some(client_id), _) => Ok(client_id.into()),
        (None, Some(auth_token)) => {
            let authorization = format!("OAuth {auth_token}");
            let mut response = client
                .get_with(
                    crate::constants::public_endpoint(),
                    &[("Authorization", &authorization)],
                )
                .await?;
            let body = response.body.read_to_end(MAX_JSON).await?;
            Ok(sonic_rs::get(body.as_slice(), pointer!["client_id"])?
                .to_string()
                .into())
        }
    }
}

pub async fn get_variants(
    client: &Client,
    client_id: Option<&str>,
    auth_token: Option<&str>,
    channel: &str,
    low_latency: bool,
) -> Result<(Vec<Variant>, u64), VariantPlaylistError> {
    let client_id = choose_client_id(client, client_id, auth_token).await?;

    let variant_playlist =
        fetch_variant_playlist(client, channel, &client_id, auth_token, low_latency).await?;

    let (variants, stream_id) = parse_variant_playlist(&variant_playlist);
    if variants.is_empty() {
        return Err(VariantPlaylistError::NoVariants(channel.to_owned()));
    }

    let Some(stream_id) = stream_id else {
        // The broadcast id keys the on-disk files; a one-off session can live
        // without it, it just cannot resume.
        tracing::warn!("master playlist has no BROADCAST-ID, using a random stream id");
        return Ok((variants, rand::random::<u64>()));
    };

    Ok((variants, stream_id))
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
        assert_eq!(
            variant.frame_rate.map(std::num::NonZeroU16::get),
            Some(6000)
        );
        assert_eq!(variant.score.map(std::num::NonZeroU16::get), Some(950));
        assert!(matches!(variant.codec, Some(Codec::H264)));
    }

    #[test]
    fn reads_the_withheld_variants_tag() {
        use crate::utils::base64_encode;

        let json = r#"[{"STABLE-VARIANT-ID":"ertmpaudio1","IVS-GROUPS":"landscape,portrait","RESOLUTION":"","FILTER_REASONS":["FR_AUDIO_DISALLOWED"]},{"STABLE-VARIANT-ID":"1080p60-portrait","IVS-GROUPS":"portrait","RESOLUTION":"1080x1920","FILTER_REASONS":["FR_DUALFORMAT_INCOMPATIBLE"]}]"#;
        let master = format!(
            "#EXTM3U\n#EXT-X-SESSION-DATA:DATA-ID=\"com.amazon.ivs.unavailable-media\",VALUE=\"{}\"\n#EXT-X-STREAM-INF:BANDWIDTH=1,STABLE-VARIANT-ID=\"720p\"\nhttps://example/720p.m3u8\n",
            base64_encode(json.as_bytes())
        );

        let entries = unavailable_media(&master);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "ertmpaudio1");
        assert_eq!(entries[0].reasons, "FR_AUDIO_DISALLOWED");
        assert!(entries[0].is_audio());
        // The archive's usual entry is `audio_only` (FR_AUDIO_DISALLOWED), which
        // must not warn either.
        assert!(
            Unavailable {
                id: "audio_only".to_owned(),
                groups: String::new(),
                resolution: String::new(),
                reasons: "FR_AUDIO_DISALLOWED".to_owned(),
            }
            .is_audio()
        );
        assert!(!entries[1].is_audio());
        assert_eq!(entries[1].id, "1080p60-portrait");
        assert_eq!(entries[1].groups, "portrait");
        assert_eq!(entries[1].resolution, "1080x1920");
        assert_eq!(entries[1].reasons, "FR_DUALFORMAT_INCOMPATIBLE");
    }

    #[test]
    fn tolerates_a_missing_or_broken_withheld_tag() {
        assert_eq!(unavailable_media("#EXTM3U\n"), []);
        assert_eq!(
            unavailable_media(
                "#EXTM3U\n#EXT-X-SESSION-DATA:DATA-ID=\"com.amazon.ivs.unavailable-media\",VALUE=\"!!!\"\n"
            ),
            []
        );
        assert_eq!(
            unavailable_media(
                "#EXTM3U\n#EXT-X-SESSION-DATA:DATA-ID=\"com.amazon.ivs.unavailable-media\",VALUE=\"bm90IGpzb24=\"\n"
            ),
            []
        );
    }

    /// The same portrait source is named by its long edge live and by its short
    /// edge with a suffix in the archive; only `RESOLUTION` classifies both.
    #[test]
    fn classifies_portrait_by_resolution_not_by_id() {
        let live = "#EXT-X-STREAM-INF:BANDWIDTH=2691820,RESOLUTION=1080x1920,\
                    CODECS=\"avc1.4D4028,mp4a.40.2\",FRAME-RATE=30.000,\
                    STABLE-VARIANT-ID=\"1920p\"";
        let archive = "#EXT-X-STREAM-INF:BANDWIDTH=3228115,RESOLUTION=1080x1920,\
                    CODECS=\"avc1.4D4028,mp4a.40.2\",FRAME-RATE=30.000,\
                    STABLE-VARIANT-ID=\"1080p-portrait\"";

        assert_eq!(
            parse_stream_inf(live, "https://example/live.m3u8")
                .unwrap()
                .kind,
            VariantKind::Portrait
        );
        assert_eq!(
            parse_stream_inf(archive, "https://example/archive.m3u8")
                .unwrap()
                .kind,
            VariantKind::Portrait
        );
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
        assert_eq!(stream_id, Some(123_456_789));
    }
}
