//! DVR/VOD playlist discovery and parsing.
//!
//! The live playlist only contains the recent window; the whole broadcast is
//! exposed as a separate DVR playlist. Finding that playlist is a port of
//! twitch-dlp's `getM3u8.ts`:
//!
//! 1. `StreamMetadata` gives the channel id, broadcast id and start time.
//! 2. If the broadcast already has a public archive, `PlaybackAccessToken` plus
//!    usher's `/vod/v2/` endpoint yields a master playlist. Only its media
//!    playlist URLs are kept; the master's playback signature does not reach
//!    them. The variant is chosen later by matching the live
//!    `STABLE-VARIANT-ID`, so the master is fetched without waiting for the
//!    live variant resolution to finish.
//! 3. Otherwise Twitch's private DVR path is probed on each VOD domain:
//!    `sha1("{channel}_{stream_id}_{start}")[..20]_…/chunked/index-dvr.m3u8`.
//!
//! Discovery and refresh run in a background task, in parallel with the live
//! scheduler, so a seek never has to fetch a playlist first.

use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use std::time::Instant;

use sonic_rs::JsonValueTrait;
use sonic_rs::pointer;
use tracing::debug;

use crate::constants::DEFAULT_CLIENT_ID;
use crate::constants::MAX_JSON;
use crate::constants::MAX_VOD_MANIFEST;
use crate::constants::TWITCH_GQL_ENDPOINT;
use crate::http::Client;
use crate::playlist::Event;
use crate::playlist::Playlist;
use crate::sha1;
use crate::utils::attribute;
use crate::utils::base64_encode;
use crate::utils::parse_iso8601_millis;
use crate::utils::query_string;
use crate::utils::random_id;

const STREAM_METADATA_HASH: &str =
    "b57f9b910f8cd1a4659d894fe7550ccc81ec9052c01e438b290fd66a040b9b93";
const RECENT_BROADCASTS_HASH: &str =
    "a92c0ad34fdba9b3f5e6125dce4b0acc9373f2b2c06c74c92187749d12f73d4c";
const PLAYBACK_TOKEN_HASH: &str =
    "ed230aa1e33e07eebb8928504583da78a5173989fadfb1ac94be06a04f3cdbe9";

/// Twitch's VOD edge hosts. A private DVR lives on exactly one of them.
const VOD_DOMAINS: [&str; 12] = [
    "https://ds0h3roq6wcgc.cloudfront.net",
    "https://d2nvs31859zcd8.cloudfront.net",
    "https://d2aba1wr3818hz.cloudfront.net",
    "https://d3c27h4odz752x.cloudfront.net",
    "https://dgeft87wbj63p.cloudfront.net",
    "https://d1m7jfoe9zdc1j.cloudfront.net",
    "https://d3vd9lfkzbru3h.cloudfront.net",
    "https://ddacn6pr5v0tl.cloudfront.net",
    "https://d3aqoihi2n8ty8.cloudfront.net",
    "https://d3fi1amfgojobc.cloudfront.net",
    "https://d2vi6trrdongqn.cloudfront.net",
    "https://d3stzm2eumvgb4.cloudfront.net",
];

/// How often the DVR playlist is re-fetched once its URL is known. New segments
/// are published about every ten seconds, so refreshing faster only adds load.
const REFRESH: Duration = Duration::from_secs(10);
/// How long to wait before probing again when discovery found nothing.
const DISCOVERY_RETRY: Duration = Duration::from_secs(10);
/// How long the public archive's master may wait for the live variant choice.
/// The master itself is fetched in parallel; only picking one of its media
/// playlists depends on the scheduler's variant.
const VARIANT_WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VodSegment {
    pub start_ms: u64,
    pub duration_ms: u64,
    pub url: String,
}

#[derive(Clone, Debug, Default)]
pub struct VodTimeline {
    /// Sorted by `start_ms`.
    pub segments: Vec<VodSegment>,
    /// End of the newest segment, in broadcast milliseconds.
    pub end_ms: u64,
}

impl VodTimeline {
    /// Parse a media playlist. `base_url` resolves relative segment URIs.
    #[cfg(test)]
    #[must_use]
    pub fn parse(base_url: &str, text: &str) -> Self {
        let mut timeline = Self::default();
        timeline.reload(base_url, text);
        timeline
    }

    /// Replace the timeline with a freshly fetched playlist, reusing the vector.
    pub fn reload(&mut self, base_url: &str, text: &str) {
        self.segments.clear();
        self.end_ms = 0;

        for event in Playlist::new(text).flatten() {
            let Event::Media(media) = event else {
                continue;
            };
            let url = resolve_url(base_url, media.uri);
            self.end_ms = media.timestamp_ms.saturating_add(media.duration_ms);
            self.segments.push(VodSegment {
                start_ms: media.timestamp_ms,
                duration_ms: media.duration_ms,
                url,
            });
        }
    }

    /// Index of the segment containing `position_ms`.
    #[must_use]
    pub fn find(&self, position_ms: u64) -> Option<usize> {
        let index = self
            .segments
            .partition_point(|segment| segment.start_ms <= position_ms);
        let index = index.checked_sub(1)?;
        let segment = &self.segments[index];
        (position_ms < segment.start_ms.saturating_add(segment.duration_ms)).then_some(index)
    }
}

#[derive(Default)]
struct DvrState {
    url: Option<String>,
    timeline: Option<VodTimeline>,
    /// `STABLE-VARIANT-ID` of the variant the live scheduler picked. The public
    /// archive's master is fetched in parallel, but its media playlist can only
    /// be chosen once this is known.
    live_variant: Option<String>,
}

/// The discovered DVR playlist shared by the refresh task and the RTSP output.
pub struct Dvr {
    state: RefCell<DvrState>,
    /// Set when the daemon drops the stream: the refresh task exits instead of
    /// polling a playlist nobody consumes.
    stop: Cell<bool>,
}

impl Dvr {
    /// `url` is supplied in debug mode; otherwise the background task discovers
    /// it from the channel login.
    #[must_use]
    pub fn new(url: Option<String>) -> Rc<Self> {
        Rc::new(Self {
            state: RefCell::new(DvrState {
                url,
                timeline: None,
                live_variant: None,
            }),
            stop: Cell::new(false),
        })
    }

    /// Ask the discovery/refresh task to exit. It checks the flag between
    /// polls, so it may live for up to [`REFRESH`] longer.
    pub fn stop(&self) {
        self.stop.set(true);
    }

    const fn stopped(&self) -> bool {
        self.stop.get()
    }

    /// Record the live variant selected by the scheduler (its
    /// `STABLE-VARIANT-ID`).
    pub fn set_live_variant(&self, id: &str) {
        self.state.borrow_mut().live_variant = Some(id.to_owned());
    }

    fn live_variant(&self) -> Option<String> {
        self.state.borrow().live_variant.clone()
    }

    fn url(&self) -> Option<String> {
        self.state.borrow().url.clone()
    }

    fn set_url(&self, url: String) {
        self.state.borrow_mut().url = Some(url);
    }

    /// Parse `text` into the shared timeline, reusing the previous allocation.
    fn set_timeline(&self, base_url: &str, text: &str) {
        let mut state = self.state.borrow_mut();
        let was_empty = state
            .timeline
            .as_ref()
            .is_none_or(|timeline| timeline.segments.is_empty());
        let timeline = state.timeline.get_or_insert_with(VodTimeline::default);
        timeline.reload(base_url, text);
        if was_empty && !timeline.segments.is_empty() {
            debug!(segments = timeline.segments.len(), "dvr: playlist ready");
        }
    }

    /// URL, playlist start and duration of the segment at `position_ms`.
    ///
    /// The playlist is replaced by the refresh task while VOD is playing, so a
    /// positional index is not stable. Look the segment up by its broadcast
    /// time instead. If the position falls before the timeline (the DVR window
    /// moved past it) or in a gap, use the first segment starting after it;
    /// returning `None` means the live edge has been reached.
    #[must_use]
    pub fn segment_at(&self, position_ms: u64) -> Option<(String, u64, u64)> {
        let state = self.state.borrow();
        let timeline = state.timeline.as_ref()?;
        let index = timeline.find(position_ms).or_else(|| {
            let index = timeline
                .segments
                .partition_point(|segment| segment.start_ms < position_ms);
            (index < timeline.segments.len()).then_some(index)
        })?;
        let segment = timeline.segments.get(index)?;
        Some((segment.url.clone(), segment.start_ms, segment.duration_ms))
    }

    /// Whether no timeline has been published yet (discovery still running, or
    /// the DVR playlist was empty on every attempt).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.state
            .borrow()
            .timeline
            .as_ref()
            .is_none_or(|timeline| timeline.segments.is_empty())
    }

    /// End of the newest known DVR segment, in broadcast milliseconds.
    #[must_use]
    pub fn end_ms(&self) -> u64 {
        self.state
            .borrow()
            .timeline
            .as_ref()
            .map_or(0, |timeline| timeline.end_ms)
    }
}

/// Discover the DVR playlist and keep it fresh, independently of the live
/// scheduler. A failed discovery is retried; a failed refresh keeps the last
/// good timeline.
pub fn spawn(client: Rc<Client>, channel: Option<String>, dvr: Rc<Dvr>) {
    compio::runtime::spawn(async move {
        let mut url = dvr.url();
        loop {
            if dvr.stopped() {
                debug!("dvr: stopped");
                return;
            }
            if url.is_none() {
                let Some(channel) = channel.as_deref() else {
                    return;
                };
                let Some(discovered) = discover(&client, channel).await else {
                    debug!("dvr: no playlist found, retrying");
                    compio::time::sleep(DISCOVERY_RETRY).await;
                    continue;
                };
                let Some(discovered) = resolve_discovery(discovered, &dvr).await else {
                    debug!("dvr: no matching variant in the archive, retrying");
                    compio::time::sleep(DISCOVERY_RETRY).await;
                    continue;
                };
                debug!("dvr: playlist discovered");
                dvr.set_url(discovered.clone());
                url = Some(discovered);
            }

            let Some(current) = url.as_deref() else {
                continue;
            };
            if let Some(text) = fetch_text(&client, current, MAX_VOD_MANIFEST).await {
                dvr.set_timeline(current, &text);
            } else {
                debug!("dvr: playlist refresh failed");
            }

            compio::time::sleep(REFRESH).await;
        }
    })
    .detach();
}

enum Discovery {
    /// Private DVR: one media playlist, no variant choice involved.
    Media(String),
    /// Public archive: a master playlist plus its media playlists. The master
    /// URL carries the playback signature; the media playlist URLs do not.
    Master(Vec<VodVariant>),
}

struct VodVariant {
    /// The same `STABLE-VARIANT-ID` the live master publishes.
    id: Option<String>,
    url: String,
}

/// Pick the media playlist matching the live variant, or the first one when the
/// choice is unknown or absent from the archive.
async fn resolve_discovery(discovery: Discovery, dvr: &Dvr) -> Option<String> {
    match discovery {
        Discovery::Media(url) => Some(url),
        Discovery::Master(variants) => {
            let live = wait_live_variant(dvr).await;
            let chosen = choose_variant(&variants, live.as_deref())?;
            debug!(
                live_variant = live.as_deref().unwrap_or("<unknown>"),
                archive_variant = chosen.id.as_deref().unwrap_or("<unknown>"),
                "dvr: archive variant selected"
            );
            Some(chosen.url.clone())
        }
    }
}

/// Match the live `STABLE-VARIANT-ID` against the archive master, falling back
/// to the first media playlist when there is no match.
fn choose_variant<'a>(variants: &'a [VodVariant], live: Option<&str>) -> Option<&'a VodVariant> {
    live.and_then(|id| {
        variants
            .iter()
            .find(|variant| variant.id.as_deref() == Some(id))
    })
    .or_else(|| variants.first())
}

/// Wait briefly for the scheduler to finish picking its live variant. The
/// archive master was already fetched in parallel; only this choice is deferred.
async fn wait_live_variant(dvr: &Dvr) -> Option<String> {
    let deadline = Instant::now() + VARIANT_WAIT;
    loop {
        if let Some(id) = dvr.live_variant() {
            return Some(id);
        }
        if Instant::now() >= deadline {
            return None;
        }
        compio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn discover(client: &Client, channel: &str) -> Option<Discovery> {
    let data = gql(
        client,
        sonic_rs::json!({
            "operationName": "StreamMetadata",
            "variables": { "channelLogin": channel, "includeIsDJ": false },
            "extensions": {
                "persistedQuery": { "version": 1, "sha256Hash": STREAM_METADATA_HASH }
            }
        }),
    )
    .await?;

    let user = data.pointer(pointer!["user"])?;
    let user_id = user.pointer(pointer!["id"])?.as_str()?.to_owned();
    let stream = user.pointer(pointer!["stream"])?;
    if stream.is_null() {
        return None;
    }
    let stream_id = stream.pointer(pointer!["id"])?.as_str()?.to_owned();
    let created_at = stream.pointer(pointer!["createdAt"])?.as_str()?;
    let created_ms = parse_iso8601_millis(created_at)?;

    if let Some(master) = public_vod(client, channel, &user_id, created_ms).await {
        return Some(Discovery::Master(master));
    }

    private_dvr(client, channel, &stream_id, created_ms.div_euclid(1_000))
        .await
        .map(Discovery::Media)
}

/// Public VOD: the current broadcast's archive master, fetched through usher.
///
/// The whole master (and its signature) is only used to enumerate the media
/// playlists; their URLs are unsigned and stable.
async fn public_vod(
    client: &Client,
    channel: &str,
    user_id: &str,
    stream_created_ms: i64,
) -> Option<Vec<VodVariant>> {
    let data = gql(
        client,
        sonic_rs::json!({
            "operationName": "FFZ_RecentBroadcasts",
            "variables": { "id": user_id, "type": "ARCHIVE", "sort": "TIME", "limit": 1 },
            "extensions": {
                "persistedQuery": { "version": 1, "sha256Hash": RECENT_BROADCASTS_HASH }
            }
        }),
    )
    .await?;

    let broadcast = data.pointer(pointer!["user", "videos", "edges", 0, "node"])?;
    if broadcast.is_null() {
        return None;
    }
    let broadcast_id = broadcast.pointer(pointer!["id"])?.as_str()?.to_owned();
    let broadcast_created = broadcast.pointer(pointer!["createdAt"])?.as_str()?;
    if stream_created_ms > parse_iso8601_millis(broadcast_created)? {
        return None;
    }

    let data = gql(
        client,
        sonic_rs::json!({
            "operationName": "PlaybackAccessToken",
            "variables": {
                "isLive": false,
                "login": "",
                "isVod": true,
                "vodID": broadcast_id,
                "playerType": "site",
                "platform": "web"
            },
            "extensions": {
                "persistedQuery": { "version": 1, "sha256Hash": PLAYBACK_TOKEN_HASH }
            }
        }),
    )
    .await?;

    let token = data.pointer(pointer!["videoPlaybackAccessToken"])?;
    let value = token.pointer(pointer!["value"])?.as_str()?.to_owned();
    let signature = token.pointer(pointer!["signature"])?.as_str()?.to_owned();
    let session_id = random_id();
    let acmb = base64_encode(
        format!(
            "{{\"AppVersion\":\"5a991ac6-7509-43f9-a4cb-3cc978734936\",\"ClientApp\":\"twilight\",\"URL\":\"https://www.twitch.tv/{channel}\"}}"
        )
        .as_bytes(),
    );

    let params = [
        ("acmb", acmb.as_str()),
        ("allow_source", "true"),
        ("browser_family", "chrome"),
        ("browser_version", "157.0"),
        ("cdm", "wv"),
        ("enable_score", "true"),
        ("include_unavailable", "true"),
        ("lang", "en"),
        ("os_name", "Windows"),
        ("os_version", "NT 10.0"),
        ("p", "3642389"),
        ("platform", "web"),
        ("play_session_id", session_id.as_str()),
        ("player_backend", "mediaplayer"),
        ("player_version", "1.59.0-rc.1"),
        ("playlist_include_framerate", "true"),
        ("reassignments_supported", "true"),
        ("sig", signature.as_str()),
        ("supported_codecs", "av1,h265,h264"),
        ("token", value.as_str()),
        ("transcode_mode", "cbr_v1"),
    ];

    let url = format!(
        "https://usher.ttvnw.net/vod/v2/{broadcast_id}.m3u8?{}",
        query_string(&params)
    );
    let master = fetch_text(client, &url, MAX_VOD_MANIFEST).await?;
    let variants = parse_master(&master);
    (!variants.is_empty()).then_some(variants)
}

/// Parse the media playlists out of a VOD master. The v2 endpoint publishes the
/// same `STABLE-VARIANT-ID` as the live master, so the archive can be matched
/// by the scheduler's chosen variant.
fn parse_master(text: &str) -> Vec<VodVariant> {
    let mut variants = Vec::new();
    let mut stream_inf = None;

    for line in text.lines() {
        let line = line.trim();
        if let Some(attributes) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            stream_inf = Some(attributes);
        } else if !line.is_empty()
            && !line.starts_with('#')
            && let Some(attributes) = stream_inf.take()
        {
            variants.push(VodVariant {
                id: attribute(attributes, "STABLE-VARIANT-ID").map(str::to_owned),
                url: line.to_owned(),
            });
        }
    }

    variants
}

/// Private DVR: probe the SHA-1 path for a few start-time offsets on every
/// domain, exactly like `getM3u8.ts`.
async fn private_dvr(
    client: &Client,
    channel: &str,
    stream_id: &str,
    start_secs: i64,
) -> Option<String> {
    for offset in [0i64, -1, 1] {
        let (hash, vod_path) = dvr_path(channel, stream_id, start_secs, offset);
        for domain in VOD_DOMAINS {
            let url = format!("{domain}/{hash}_{vod_path}/chunked/index-dvr.m3u8");
            if let Ok(response) = client.head(&url).await
                && response.status.is_success()
            {
                return Some(url);
            }
        }
    }
    None
}

/// Twitch's private DVR object name: the first 20 hex digits of the SHA-1 of
/// `{channel}_{stream_id}_{start}`, plus the unhashed path.
fn dvr_path(channel: &str, stream_id: &str, start_secs: i64, offset: i64) -> (String, String) {
    let vod_path = format!("{channel}_{stream_id}_{}", start_secs + offset);
    let digest = sha1::hex(vod_path.as_bytes());
    (digest[..20].to_owned(), vod_path)
}

async fn gql(client: &Client, query: sonic_rs::Value) -> Option<sonic_rs::Value> {
    let body = sonic_rs::to_vec(&sonic_rs::json!([query])).ok()?;
    let mut response = client
        .post(
            TWITCH_GQL_ENDPOINT,
            "text/plain;charset=UTF-8",
            &body,
            &[("Client-Id", DEFAULT_CLIENT_ID)],
        )
        .await
        .ok()?;
    if !response.status.is_success() {
        return None;
    }

    let payload = response.body.read_to_end(MAX_JSON).await.ok()?;
    let value: sonic_rs::Value = sonic_rs::from_slice(&payload).ok()?;
    value.pointer(pointer![0, "data"]).cloned()
}

async fn fetch_text(client: &Client, url: &str, limit: usize) -> Option<String> {
    let mut response = client.get(url).await.ok()?;
    if !response.status.is_success() {
        return None;
    }
    let body = response.body.read_to_end(limit).await.ok()?;
    String::from_utf8(body).ok()
}

#[must_use]
pub fn resolve_url(base_url: &str, uri: &str) -> String {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return uri.to_owned();
    }

    let origin_len = base_url.find("://").map_or(0, |index| index + 3);
    let path_start = base_url[origin_len..]
        .find('/')
        .map_or(base_url.len(), |index| origin_len + index);
    let origin = &base_url[..path_start];

    if let Some(absolute) = uri.strip_prefix('/') {
        return format!("{origin}/{absolute}");
    }

    let path_end = base_url.find('?').unwrap_or(base_url.len());
    let directory = base_url[..path_end]
        .rfind('/')
        .map_or(origin, |slash| &base_url[..slash]);
    format!("{directory}/{uri}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DVR: &str = "\
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:12
#EXT-X-PLAYLIST-TYPE:EVENT
#EXT-X-MEDIA-SEQUENCE:0
#EXT-X-TWITCH-ELAPSED-SECS:0.000
#EXT-X-TWITCH-TOTAL-SECS:30.000
#EXTINF:10.000,
dvr/0.ts
#EXTINF:10.000,
dvr/1.ts
#EXTINF:10.000,
dvr/2.ts
";

    #[test]
    fn matches_the_live_variant_in_the_archive_master() {
        const MASTER: &str = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=9073552,STABLE-VARIANT-ID=\"1440p60\"
https://cdn/1440p60/index-dvr.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=4188640,STABLE-VARIANT-ID=\"chunked\"
https://cdn/chunked/index-dvr.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3100000,STABLE-VARIANT-ID=\"720p60\"
https://cdn/720p60/index-dvr.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=218080,STABLE-VARIANT-ID=\"audio_only\"
https://cdn/audio_only/index-dvr.m3u8
";

        let variants = parse_master(MASTER);
        assert_eq!(
            variants
                .iter()
                .map(|variant| variant.id.as_deref())
                .collect::<Vec<_>>(),
            [
                Some("1440p60"),
                Some("chunked"),
                Some("720p60"),
                Some("audio_only")
            ]
        );
        assert_eq!(
            choose_variant(&variants, Some("720p60")).map(|variant| variant.url.as_str()),
            Some("https://cdn/720p60/index-dvr.m3u8")
        );
        // Unknown or unavailable choices keep the archive usable.
        assert_eq!(
            choose_variant(&variants, Some("1080p60")).map(|variant| variant.id.as_deref()),
            Some(Some("1440p60"))
        );
        assert_eq!(
            choose_variant(&variants, None).map(|variant| variant.id.as_deref()),
            Some(Some("1440p60"))
        );
    }

    #[test]
    fn builds_the_private_dvr_path() {
        let (hash, path) = dvr_path("sunnys", "123", 1_794_119_852, 0);
        assert_eq!(hash, "22de0f875bc867cf4d8f");
        assert_eq!(path, "sunnys_123_1794119852");

        let (hash, path) = dvr_path("sunnys", "123", 1_794_119_852, -1);
        assert_eq!(path, "sunnys_123_1794119851");
        assert_eq!(hash, sha1::hex(path.as_bytes())[..20].to_owned());
    }

    #[test]
    fn parses_positions_and_resolves_urls() {
        let timeline = VodTimeline::parse("http://127.0.0.2:8000/index-dvr.m3u8", DVR);
        assert_eq!(timeline.segments.len(), 3);
        assert_eq!(timeline.end_ms, 30_000);
        assert_eq!(timeline.segments[1].start_ms, 10_000);
        assert_eq!(timeline.segments[1].url, "http://127.0.0.2:8000/dvr/1.ts");
    }

    #[test]
    fn reloads_in_place() {
        let mut timeline = VodTimeline::parse("http://host/index-dvr.m3u8", DVR);
        timeline.reload(
            "http://host/index-dvr.m3u8",
            "#EXTM3U\n#EXTINF:2.000,\nx.ts\n",
        );
        assert_eq!(timeline.segments.len(), 1);
        assert_eq!(timeline.end_ms, 2000);
        assert_eq!(timeline.segments[0].url, "http://host/x.ts");
    }

    #[test]
    fn finds_the_containing_and_next_segment() {
        let timeline = VodTimeline::parse("http://host/index-dvr.m3u8", DVR);
        assert_eq!(timeline.find(0), Some(0));
        assert_eq!(timeline.find(15_000), Some(1));
        assert_eq!(timeline.find(30_000), None);
    }

    #[test]
    fn segment_at_is_stable_across_playlist_refreshes() {
        let dvr = Dvr::new(None);
        dvr.set_timeline("http://127.0.0.2:8000/index-dvr.m3u8", DVR);

        let (url, start, duration) = dvr.segment_at(15_000).expect("segment");
        assert_eq!(url, "http://127.0.0.2:8000/dvr/1.ts");
        assert_eq!((start, duration), (10_000, 10_000));

        // Before the DVR window: start at its front rather than refusing.
        assert_eq!(dvr.segment_at(0).expect("front").1, 0);

        // The next lookup is by broadcast time, so a playlist replacement
        // cannot shift it onto a different segment.
        let (url, start, _) = dvr.segment_at(20_000).expect("next");
        assert_eq!(url, "http://127.0.0.2:8000/dvr/2.ts");
        assert_eq!(start, 20_000);

        // Past the published end: the live edge takes over.
        assert_eq!(dvr.segment_at(30_000), None);
    }

    #[test]
    fn resolves_absolute_and_root_relative_urls() {
        assert_eq!(
            resolve_url("http://host/a/index.m3u8", "https://cdn/x.ts"),
            "https://cdn/x.ts"
        );
        assert_eq!(
            resolve_url("http://host/a/index.m3u8", "/x.ts"),
            "http://host/x.ts"
        );
    }
}
