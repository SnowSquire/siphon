//! Serializable state shared by the Siphon backend and its UI.
//!
//! Every type in this module crosses the RPC, so the two sides cannot drift:
//! the daemon builds these values and the UI renders them. Behaviour (config
//! files, GQL, Hermes, pipelines) stays in the daemon; this crate only owns
//! the shape and the frame codec.

use serde::Deserialize;
use serde::Serialize;

/// Monotonic protocol revision. Both sides refuse a mismatch before decoding
/// anything else, so a stale UI gets a typed error instead of a panic.
pub const PROTOCOL_VERSION: u16 = 1;

/// First field of `Hello`; a cheap guard against a client that connected to
/// the RTSP port or some unrelated loopback service.
pub const MAGIC: u32 = 0x5349_5048; // "SIPH"

/// Largest accepted frame. Rejected before allocating the payload.
pub const MAX_FRAME: usize = 1024 * 1024;

/// Identity of one pipeline: a channel login and an optional
/// `STABLE-VARIANT-ID`. `None` means the monitor's best variant.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamKey {
    /// Twitch login, lowercased.
    pub login: String,
    /// `STABLE-VARIANT-ID` (`chunked`, `720p60`, `audio_only`, ...).
    pub quality: Option<String>,
}

impl StreamKey {
    #[must_use]
    pub fn new(login: impl Into<String>, quality: Option<String>) -> Self {
        Self {
            login: login.into(),
            quality,
        }
    }
}

/// One followed channel. `record` is the per-channel monitor: false means
/// notify-only, true starts a recording pipeline on `stream-up`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Channel {
    pub login: String,
    pub id: u64,
    pub display_name: Option<String>,
    pub record: bool,
}

/// The persisted app configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub version: u64,
    pub channels: Vec<Channel>,
    pub notify_title_changes: bool,
    pub sound: bool,
    pub filtered_words: Vec<String>,
}

impl Config {
    /// Current on-disk schema. `2` is Siphon's last release; `3` adds
    /// `channels[].record`.
    pub const VERSION: u64 = 3;
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: Self::VERSION,
            channels: Vec::new(),
            notify_title_changes: true,
            sound: true,
            filtered_words: vec!["offline".to_owned()],
        }
    }
}

/// Hermes connection health as the UI header renders it. Three static states,
/// so the UI picks a string off the enum without an allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConnHealth {
    Connected,
    #[default]
    Connecting,
    NoChannels,
}

/// One channel row, as the UI renders it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelView {
    pub id: u64,
    pub login: String,
    pub display_name: String,
    pub profile_image_url: String,
    pub live: bool,
    pub title: Option<String>,
    pub game: Option<String>,
    pub viewers: Option<u32>,
    pub collaboration_viewers: Option<u32>,
    /// Milliseconds since the unix epoch; `None` while offline.
    pub stream_start_ms: Option<i64>,
}

/// Lifecycle of a pipeline's scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PipelineState {
    Starting,
    Live,
    Offline,
    Stopped,
    Failed,
}

/// One running (or last-known) pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineView {
    pub key: StreamKey,
    pub title: String,
    pub variant: Option<String>,
    pub state: PipelineState,
    /// Desired by configuration: never evicted for capacity.
    pub pinned: bool,
    pub recording: bool,
    pub sessions: u32,
    pub position_ms: u64,
    pub live_edge_ms: u64,
    pub segment_tail: u64,
    pub prefetch_tail: u64,
    pub window: u32,
    pub polls: u64,
    pub poll_failures: u64,
    pub last_error: Option<String>,
}

/// Terminal outcome of one segment fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FetchOutcome {
    Complete,
    Failed,
}

/// One segment fetch, scalar-only: the ring must never pin an arena slot.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentRecord {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub duration_ms: u64,
    pub pdt_ms: u64,
    pub bytes: u64,
    pub elapsed_ms: u64,
    pub retries: u32,
    pub outcome: FetchOutcome,
}

/// Arena occupancy summary.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArenaSummary {
    pub live_slots: u32,
    pub arenas: u32,
    pub high_water: u32,
    pub alloc_failures: u64,
    pub spool_cap_failures: u64,
    pub backoffs: u64,
}

/// Downloader queue summary.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloaderSummary {
    pub queued: u32,
    pub capacity: u32,
    pub in_flight: u32,
    pub retries: u64,
}

/// One entry of the daemon-owned notification center.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationView {
    pub id: u64,
    /// Milliseconds since the unix epoch.
    pub at_ms: i64,
    pub summary: String,
    pub body: String,
    pub login: Option<String>,
    pub image: Option<String>,
    pub sound: bool,
}

/// An update offer rendered by the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateView {
    pub version: String,
    /// Release page, for builds with no installer.
    pub page_url: String,
    /// True when `ApplyUpdate` downloads and installs an MSI.
    pub installer: bool,
}

/// Everything the UI needs for one frame. Built on demand from app state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub revision: u64,
    pub config: Config,
    pub conn: ConnHealth,
    pub channels: Vec<ChannelView>,
    pub pending: Vec<String>,
    pub pipelines: Vec<PipelineView>,
    pub arena: ArenaSummary,
    pub downloader: DownloaderSummary,
    pub notifications: Vec<NotificationView>,
    pub error: Option<String>,
    pub update: Option<UpdateView>,
}

/// One entity-level change after a snapshot. Values are complete entities,
/// so applying an update twice is harmless and coalescing by entity is safe.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Update {
    Config(Config),
    Conn(ConnHealth),
    ChannelUpsert(ChannelView),
    ChannelRemove(u64),
    Pending(Vec<String>),
    PipelineUpsert(PipelineView),
    PipelineRemove(StreamKey),
    Segment {
        key: StreamKey,
        record: SegmentRecord,
    },
    Arena(ArenaSummary),
    Downloader(DownloaderSummary),
    Notification(NotificationView),
    NotificationDismissed(u64),
    Error(Option<String>),
    Update(Option<UpdateView>),
}

/// Detail for one pipeline, including its recent segment ring.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamDetail {
    pub pipeline: PipelineView,
    pub segments: Vec<SegmentRecord>,
}

/// A UI request. Intents return an ack; the resulting state arrives as an
/// `Update`. Queries return their result in the `Response`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Request {
    AddLogin(String),
    RemoveChannel(u64),
    SetNotifyTitleChanges(bool),
    SetSound(bool),
    AddFilteredWord(String),
    RemoveFilteredWord(u32),
    /// Turn the per-channel recording monitor on or off.
    SetRecord {
        channel: u64,
        record: bool,
    },
    ClearError,
    DismissNotification(u64),
    ApplyUpdate,
    PipelineStop(StreamKey),
    PipelineRestart(StreamKey),
    GetSnapshot,
    GetStreamDetail(StreamKey),
    ListSegments {
        key: StreamKey,
        limit: u32,
    },
    GetArena,
    GetDownloader,
    GetDisk(StreamKey),
}

/// A successful query result. Intents answer `Ack`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Response {
    Ack,
    Snapshot(Box<Snapshot>),
    StreamDetail(StreamDetail),
    Segments(Vec<SegmentRecord>),
    Arena(ArenaSummary),
    Downloader(DownloaderSummary),
}

/// Typed failure. The UI renders the message; the daemon never sends a raw
/// panic or a decode error through the frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RpcError {
    /// The request could not be applied (bad login, unknown channel, ...).
    BadRequest(String),
    /// No such channel/pipeline.
    NotFound,
    /// The backend does not implement this query yet.
    Unsupported,
    /// A new pipeline would exceed `--max-streams`.
    Capacity,
    /// The backend hit an expected failure (config save, update download, ...).
    Failed(String),
    /// The connection ended before the request completed.
    Disconnected,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(message) | Self::Failed(message) => write!(formatter, "{message}"),
            Self::NotFound => formatter.write_str("not found"),
            Self::Unsupported => formatter.write_str("unsupported"),
            Self::Capacity => formatter.write_str("stream capacity reached"),
            Self::Disconnected => formatter.write_str("backend disconnected"),
        }
    }
}

impl std::error::Error for RpcError {}

/// The first exchange on a connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ClientMessage {
    Hello {
        magic: u32,
        protocol: u16,
        app_version: String,
    },
    Request {
        id: u64,
        request: Request,
    },
}

/// Everything the backend sends.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ServerMessage {
    Welcome {
        protocol: u16,
        server_version: String,
    },
    /// The hello was refused before any state was sent. The client reports
    /// the reason instead of guessing at a decode error.
    Rejected(String),
    /// The full state, sent once after `Welcome` and on `GetSnapshot`.
    Snapshot(Box<Snapshot>),
    Update {
        revision: u64,
        update: Update,
    },
    Response {
        id: u64,
        result: Result<Response, RpcError>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_to_the_current_version() {
        assert_eq!(Config::default().version, Config::VERSION);
        assert_eq!(Config::VERSION, 3);
    }

    #[test]
    fn a_channel_defaults_to_notify_only() {
        assert!(!Channel::default().record);
    }
}
