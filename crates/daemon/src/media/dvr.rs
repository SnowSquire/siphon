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
//!    them. Which one to use is decided later, from the live variant: the two
//!    masters do not agree on `STABLE-VARIANT-ID` (a portrait source is `1920p`
//!    live and `1080p-portrait` archived), so `RESOLUTION` is the key. That
//!    lets the master be fetched without waiting for the live pick.
//! 3. Otherwise Twitch's private DVR path is probed on each VOD domain:
//!    `sha1("{channel}_{stream_id}_{start}")[..20]_…/chunked/index-dvr.m3u8`.
//!
//! Discovery and refresh run in a background task, in parallel with the live
//! scheduler, so a seek never has to fetch a playlist first.

// The seek helpers below serve the Riffle media lane; nothing seeks yet.
#![allow(dead_code)]

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
use crate::spool::ConsumerWake;
use crate::utils::attribute;
use crate::utils::base64_encode;
use crate::utils::parse_iso8601_millis;
use crate::utils::query_string;
use crate::utils::random_id;
use crate::variant::Variant;

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
/// When to look for the DVR playlist, from the moment the DVR is spawned. The
/// object is not ephemeral - it stays up after the broadcast, and the archive
/// master points at it - so there is nothing to poll: a broadcast that was
/// already live answers at once, and a fresh one needs its first chunk to close.
/// Chunks are whole live segments and at least `TARGETDURATION` long, so nothing
/// can exist for ~10 s; measured on a fresh broadcast, the playlist was
/// published 26.7 s after the broadcast started, hence 30 s and a 45 s retry.
const DISCOVERY_PROBES: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_secs(30),
    Duration::from_secs(45),
];
/// How long the public archive's master may wait for the live variant choice.
/// The master itself is fetched in parallel; only picking one of its media
/// playlists depends on the scheduler's variant.
const VARIANT_WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DvrSegment {
    pub start_ms: u64,
    pub duration_ms: u64,
    /// Absolute `EXT-X-PROGRAM-DATE-TIME` of the chunk, or `0` when the playlist
    /// publishes none.
    pub pdt_ms: u64,
    /// The playlist's own sequence number for this chunk.
    pub sequence: u64,
    pub url: String,
    /// Index into [`DvrTimeline::maps`] for the `EXT-X-MAP` in effect.
    pub map: Option<usize>,
}

/// One `EXT-X-MAP` object. `byterange` is `(offset, length)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DvrMap {
    pub uri: String,
    pub byterange: Option<(u64, u64)>,
}

#[derive(Clone, Debug, Default)]
pub struct DvrTimeline {
    /// Sorted by `start_ms`.
    pub segments: Vec<DvrSegment>,
    /// The distinct `EXT-X-MAP` objects referenced by the segments.
    pub maps: Vec<DvrMap>,
    /// End of the newest segment, in broadcast milliseconds.
    pub end_ms: u64,
    /// The playlist published `#EXT-X-ENDLIST`: no more chunks will appear.
    pub ended: bool,
}

impl DvrTimeline {
    /// Parse a media playlist. `base_url` resolves relative segment URIs.
    #[must_use]
    pub fn parse(base_url: &str, text: &str) -> Self {
        let mut timeline = Self::default();
        timeline.reload(base_url, text);
        timeline
    }

    /// Replace the timeline with a freshly fetched playlist.
    pub fn reload(&mut self, base_url: &str, text: &str) {
        self.segments.clear();
        self.maps.clear();
        self.end_ms = 0;
        self.ended = false;
        let mut current_map = None;

        for event in Playlist::new(text).flatten() {
            match event {
                Event::Ended => self.ended = true,
                Event::Init(init) => {
                    let uri = resolve_url(base_url, init.uri);
                    let index = self
                        .maps
                        .iter()
                        .position(|map| map.uri == uri && map.byterange == init.byterange)
                        .unwrap_or_else(|| {
                            self.maps.push(DvrMap {
                                uri,
                                byterange: init.byterange,
                            });
                            self.maps.len() - 1
                        });
                    current_map = Some(index);
                }
                Event::Media(media) => {
                    let url = resolve_url(base_url, media.uri);
                    self.end_ms = media.timestamp_ms.saturating_add(media.duration_ms);
                    self.segments.push(DvrSegment {
                        start_ms: media.timestamp_ms,
                        duration_ms: media.duration_ms,
                        pdt_ms: media.pdt_ms.unwrap_or(0),
                        sequence: media.sequence,
                        url,
                        map: current_map,
                    });
                }
                _ => {}
            }
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

    /// `pdt - elapsed` for the segment at `index`, the broadcast's wall-clock
    /// origin. Both sources should agree on this constant.
    #[must_use]
    pub fn origin_ms(&self, index: usize) -> Option<i64> {
        let segment = self.segments.get(index)?;
        if segment.pdt_ms == 0 {
            return None;
        }
        Some(i64::try_from(segment.pdt_ms).ok()? - i64::try_from(segment.start_ms).ok()?)
    }
}

#[derive(Default)]
/// The live variant the scheduler chose.
///
/// Its `STABLE-VARIANT-ID` is *not* what the archive master publishes: the same
/// portrait source is `1920p` live and `1080p-portrait` in the archive, and a
/// landscape source can be `chunked` there. `RESOLUTION` is the only field both
/// masters agree on, so it is what the archive is really matched by.
#[derive(Clone, Debug)]
struct LiveVariant {
    id: String,
    res: Option<(u16, u16)>,
}

#[derive(Default)]
struct DvrState {
    url: Option<String>,
    timeline: Option<Rc<DvrTimeline>>,
    /// The variant the live scheduler picked. The public archive's master is
    /// fetched in parallel, but its media playlist can only be chosen once this
    /// is known.
    live_variant: Option<LiveVariant>,
}

/// The discovered DVR playlist shared by the refresh task, the RTSP output and
/// the disk output.
pub struct Dvr {
    state: RefCell<DvrState>,
    /// Set when the daemon drops the stream: the refresh task exits instead of
    /// polling a playlist nobody consumes.
    stop: Cell<bool>,
    /// Bumped on every URL/timeline change; a watcher parks on `wake` and
    /// compares the generation.
    generation: Cell<u64>,
    wake: RefCell<Option<Rc<ConsumerWake>>>,
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
            generation: Cell::new(0),
            wake: RefCell::new(None),
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

    /// Subscribe to URL/timeline changes. Only the disk output needs this; the
    /// RTSP seek path reads the current snapshot on demand.
    pub fn subscribe(&self, wake: &Rc<ConsumerWake>) {
        *self.wake.borrow_mut() = Some(Rc::clone(wake));
    }

    /// Current change counter, for a watcher that does not need the value.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation.get()
    }

    fn changed(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if let Some(wake) = self.wake.borrow().as_ref() {
            wake.wake();
        }
    }

    /// Record the live variant selected by the scheduler.
    pub fn set_live_variant(&self, variant: &Variant) {
        self.state.borrow_mut().live_variant = Some(LiveVariant {
            id: variant.id.to_string(),
            res: variant
                .res
                .map(|(width, height)| (width.get(), height.get())),
        });
    }

    fn live_variant(&self) -> Option<LiveVariant> {
        self.state.borrow().live_variant.clone()
    }

    /// The discovered playlist URL, once discovery succeeded.
    #[must_use]
    pub fn url(&self) -> Option<String> {
        self.state.borrow().url.clone()
    }

    fn set_url(&self, url: String) {
        self.state.borrow_mut().url = Some(url);
        self.changed();
    }

    /// The current timeline snapshot. The refresh task replaces the `Rc` instead
    /// of mutating it, so a reader can hold one without blocking the refresh.
    #[must_use]
    pub fn snapshot(&self) -> Option<Rc<DvrTimeline>> {
        self.state.borrow().timeline.clone()
    }

    /// Parse `text` into the shared timeline, replacing the previous snapshot.
    fn set_timeline(&self, base_url: &str, text: &str) {
        let timeline = Rc::new(DvrTimeline::parse(base_url, text));
        if !timeline.segments.is_empty() {
            debug!(segments = timeline.segments.len(), "dvr: playlist ready");
        }
        self.state.borrow_mut().timeline = Some(timeline);
        self.changed();
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
        let spawned = Instant::now();
        let mut probes = DISCOVERY_PROBES.into_iter();
        loop {
            if dvr.stopped() {
                debug!("dvr: stopped");
                return;
            }
            if url.is_none() {
                let Some(channel) = channel.as_deref() else {
                    return;
                };
                let Some(probe) = probes.next() else {
                    debug!("dvr: no playlist after the last probe; giving up");
                    return;
                };
                // Waiting for a probe is not a retry: the outcome cannot change
                // before the DVR's first chunk exists.
                if let Some(wait) = (spawned + probe).checked_duration_since(Instant::now()) {
                    compio::time::sleep(wait).await;
                }
                let Some(discovered) = discover(&client, channel).await else {
                    debug!(?probe, "dvr: no playlist found");
                    continue;
                };
                let Some(discovered) = resolve_discovery(discovered, &dvr).await else {
                    debug!(?probe, "dvr: no matching variant in the archive");
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
    Master(Vec<DvrVariant>),
}

struct DvrVariant {
    /// The archive master's `STABLE-VARIANT-ID`, tried first but often different
    /// from the live one.
    id: Option<String>,
    /// `RESOLUTION`, which both masters publish identically.
    res: Option<(u16, u16)>,
    url: String,
}

/// Pick the media playlist matching the live variant, or the first one when the
/// choice is unknown or absent from the archive.
async fn resolve_discovery(discovery: Discovery, dvr: &Dvr) -> Option<String> {
    match discovery {
        Discovery::Media(url) => Some(url),
        Discovery::Master(variants) => {
            let live = wait_live_variant(dvr).await;
            let Some(chosen) = choose_variant(&variants, live.as_ref()) else {
                debug!(
                    live_variant = live.as_ref().map_or("<unknown>", |live| live.id.as_str()),
                    live_res = ?live.as_ref().and_then(|live| live.res),
                    "dvr: archive master has no variant of this orientation; not using it"
                );
                return None;
            };
            debug!(
                live_variant = live.as_ref().map_or("<unknown>", |live| live.id.as_str()),
                live_res = ?live.as_ref().and_then(|live| live.res),
                archive_variant = chosen.id.as_deref().unwrap_or("<unknown>"),
                archive_res = ?chosen.res,
                "dvr: archive variant selected"
            );
            Some(chosen.url.clone())
        }
    }
}

/// Match the live variant in the archive master: by `STABLE-VARIANT-ID` first,
/// then by `RESOLUTION`. `None` means the master has no variant of this
/// orientation, and then there is no archive to use.
///
/// There is deliberately no "first variant" fallback. On a dual-format
/// broadcast Twitch filters the whole portrait group out of the archive master
/// (`com.amazon.ivs.unavailable-media`, `FR_DUALFORMAT_INCOMPATIBLE`), so a
/// recording of the portrait stream would otherwise be handed the landscape
/// archive and splice it in.
///
/// The id alone is not enough. For the same broadcast Twitch published
/// `STABLE-VARIANT-ID="1920p"` (1080x1920) live and `"1080p-portrait"`
/// (1080x1920) in the archive, and the source appears as `"chunked"` there. A
/// dual-orientation broadcast has both orientations in the archive, so falling
/// back to the first variant would silently pick the wrong one; the resolution
/// is what the two masters agree on.
fn choose_variant<'a>(
    variants: &'a [DvrVariant],
    live: Option<&LiveVariant>,
) -> Option<&'a DvrVariant> {
    let by_id = live.and_then(|live| {
        variants
            .iter()
            .find(|variant| variant.id.as_deref() == Some(live.id.as_str()))
    });
    by_id.or_else(|| {
        live.and_then(|live| {
            live.res
                .and_then(|res| variants.iter().find(|variant| variant.res == Some(res)))
        })
    })
}

/// Wait briefly for the scheduler to finish picking its live variant. The
/// archive master was already fetched in parallel; only this choice is deferred.
async fn wait_live_variant(dvr: &Dvr) -> Option<LiveVariant> {
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
) -> Option<Vec<DvrVariant>> {
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
        // Without this the audio-only DVR is withheld (`FR_AUDIO_DISALLOWED` in
        // com.amazon.ivs.unavailable-media), so a recording of `-q audio_only`
        // would have no archive to fill from.
        ("allow_audio_only", "true"),
        ("allow_source", "true"),
        ("browser_family", "chrome"),
        ("browser_version", "157.0"),
        ("cdm", "wv"),
        ("enable_score", "true"),
        ("include_unavailable", "true"),
        ("lang", "en"),
        // Without this the archive master hides every second orientation: on a
        // dual-format broadcast the portrait variants are listed in
        // `com.amazon.ivs.unavailable-media` as `FR_DUALFORMAT_INCOMPATIBLE` and
        // nothing else in the master refers to them. With it the master is the
        // same set the live one publishes.
        ("multigroup_video", "true"),
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

/// Parse the media playlists out of a VOD master. The v2 endpoint publishes
/// `STABLE-VARIANT-ID` values that do **not** always match the live master's, so
/// the resolution is kept as the real matching key.
fn parse_master(text: &str) -> Vec<DvrVariant> {
    crate::variant::log_unavailable_media(text, "archive");

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
            variants.push(DvrVariant {
                id: attribute(attributes, "STABLE-VARIANT-ID").map(str::to_owned),
                res: attribute(attributes, "RESOLUTION").and_then(|res| {
                    let (width, height) = res.split_once('x')?;
                    Some((width.parse().ok()?, height.parse().ok()?))
                }),
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
        let by_id = LiveVariant {
            id: "720p60".to_owned(),
            res: None,
        };
        assert_eq!(
            choose_variant(&variants, Some(&by_id)).map(|variant| variant.url.as_str()),
            Some("https://cdn/720p60/index-dvr.m3u8")
        );
        // No id and no resolution: refuse rather than record another
        // orientation's archive.
        let unknown = LiveVariant {
            id: "480p30".to_owned(),
            res: None,
        };
        assert!(choose_variant(&variants, Some(&unknown)).is_none());
        assert!(choose_variant(&variants, None).is_none());

        // A live resolution the archive does not have is refused the same way.
        let absent = LiveVariant {
            id: "1080p60".to_owned(),
            res: Some((1080, 1920)),
        };
        assert!(choose_variant(&variants, Some(&absent)).is_none());
    }

    /// Twitch does not keep `STABLE-VARIANT-ID` in sync between the live master
    /// and the archive master: for the same broadcast the portrait source was
    /// `1920p` live and `1080p-portrait` in the archive. The archive master a
    /// dual-orientation broadcast publishes has both, so matching the id alone
    /// would fall through to the first (landscape) variant and quietly record
    /// the wrong track.
    #[test]
    fn matches_a_renamed_portrait_variant_by_resolution() {
        const MASTER: &str = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=6389220,RESOLUTION=1920x1080,STABLE-VARIANT-ID=\"chunked\"
https://cdn/chunked/index-dvr.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3228115,RESOLUTION=1080x1920,STABLE-VARIANT-ID=\"1080p-portrait\"
https://cdn/1080p-portrait/index-dvr.m3u8
";
        let variants = parse_master(MASTER);
        assert_eq!(variants[0].res, Some((1920, 1080)));
        assert_eq!(variants[1].res, Some((1080, 1920)));

        let live = LiveVariant {
            id: "1920p".to_owned(),
            res: Some((1080, 1920)),
        };
        assert_eq!(
            choose_variant(&variants, Some(&live)).map(|variant| variant.id.as_deref()),
            Some(Some("1080p-portrait"))
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
        let timeline = DvrTimeline::parse("http://127.0.0.2:8000/index-dvr.m3u8", DVR);
        assert_eq!(timeline.segments.len(), 3);
        assert_eq!(timeline.end_ms, 30_000);
        assert_eq!(timeline.segments[1].start_ms, 10_000);
        assert_eq!(timeline.segments[1].url, "http://127.0.0.2:8000/dvr/1.ts");
    }

    #[test]
    fn tracks_map_changes() {
        const MAPS: &str = "\
#EXTM3U
#EXT-X-MAP:URI=\"init-a.mp4\"
#EXTINF:10.000,
0.ts
#EXT-X-MAP:URI=\"init-b.mp4\",BYTERANGE=\"100@5\"
#EXTINF:10.000,
1.ts
";
        let timeline = DvrTimeline::parse("http://host/index-dvr.m3u8", MAPS);
        assert_eq!(timeline.maps.len(), 2);
        assert_eq!(timeline.maps[0].uri, "http://host/init-a.mp4");
        assert_eq!(timeline.maps[1].byterange, Some((5, 100)));
        assert_eq!(timeline.segments[0].map, Some(0));
        assert_eq!(timeline.segments[1].map, Some(1));
    }

    #[test]
    fn reloads_in_place() {
        let mut timeline = DvrTimeline::parse("http://host/index-dvr.m3u8", DVR);
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
        let timeline = DvrTimeline::parse("http://host/index-dvr.m3u8", DVR);
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
