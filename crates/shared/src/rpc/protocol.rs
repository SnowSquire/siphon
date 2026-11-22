//! Serializable state shared by the Siphon backend and its UI.
//!
//! Every type in this module crosses the RPC, so the two sides cannot drift:
//! the daemon builds these values and the UI renders them. Behaviour (config
//! files, GQL, Hermes, pipelines) stays in the daemon; this crate only owns
//! the shape and the frame codec.

use std::num::NonZeroU64;

use serde::Deserialize;
use serde::Serialize;

/// Monotonic protocol revision. Both sides refuse a mismatch before decoding
/// anything else, so a stale UI gets a typed error instead of a panic.
pub const PROTOCOL_VERSION: u16 = 5;

/// First field of `Hello`; a cheap guard against a client that connected to
/// the RTSP port or some unrelated loopback service.
pub const MAGIC: u32 = 0x5349_5048; // "SIPH"

/// Largest accepted frame. Rejected before allocating the payload.
pub const MAX_FRAME: usize = 1024 * 1024;

/// Identity of one pipeline: a Twitch channel id and an optional
/// `STABLE-VARIANT-ID`. `None` means the monitor's best variant.
///
/// The id, not the login: the daemon already holds `config.channels[].id`, and
/// a login is a name the RTSP URI and the Channels list can resolve, not an
/// identity. The UI resolves the login for display from its channel list.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamKey {
    /// Twitch channel id.
    pub channel: u64,
    /// `STABLE-VARIANT-ID` (`chunked`, `720p60`, `audio_only`, ...).
    pub quality: Option<String>,
}

impl StreamKey {
    #[must_use]
    pub const fn new(channel: u64, quality: Option<String>) -> Self {
        Self { channel, quality }
    }
}

/// One followed channel, with the three switches the Channels tab draws.
///
/// `record` is the per-channel monitor: true starts a recording pipeline on
/// `stream-up`. The two notify switches are narrower than the global ones: a
/// channel can stay followed and still be silent for just one kind of event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Channel {
    pub login: String,
    pub id: u64,
    pub display_name: Option<String>,
    pub record: bool,
    pub notify_live: bool,
    pub notify_titles: bool,
}

impl Default for Channel {
    /// A channel that predates the per-channel switches is a channel the user
    /// followed to hear about: both start on, which is what the global settings
    /// did for it before.
    fn default() -> Self {
        Self {
            login: String::new(),
            id: 0,
            display_name: None,
            record: false,
            notify_live: true,
            notify_titles: true,
        }
    }
}

/// The three switches a follow carries, as a caller may want them set when the
/// channel is added.
///
/// This is what makes removing a channel undoable: deleting it drops the entry
/// and its switches, and re-adding it with these restores both. `None` in
/// [`Request::AddLogin`] means the ordinary defaults (both notifications on,
/// recording off), which is what adding a channel by hand means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelSwitches {
    pub notify_live: bool,
    pub notify_titles: bool,
    pub record: bool,
}

impl Default for ChannelSwitches {
    fn default() -> Self {
        Self {
            notify_live: true,
            notify_titles: true,
            record: false,
        }
    }
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
    /// Where complete broadcasts are recorded, one directory per broadcast.
    /// `None` (or empty) disables recording; the daemon otherwise reports that
    /// recording needs a directory.
    pub record_dir: Option<String>,
    /// Record both orientations of a dual-format broadcast. A single-format
    /// broadcast is always recorded, whichever orientation it carries, so this
    /// only ever adds a second recording. It is a global preference, not a
    /// per-channel one: it never changes *what* is recorded as the primary.
    pub record_dual_format: bool,
}

impl Config {
    /// Current on-disk schema.
    pub const VERSION: u64 = 5;
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: Self::VERSION,
            channels: Vec::new(),
            notify_title_changes: true,
            sound: true,
            filtered_words: vec!["offline".to_owned()],
            record_dir: None,
            record_dual_format: false,
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

/// The colour a channel's switches and chevron take.
///
/// Two producers, one value: Twitch's `chatColor` is `#RRGGBB`, and a channel
/// that never picked one gets a generated colour. The generated one is authored
/// in OKLCH because the palette is — its chromas reach outside sRGB, so packing
/// it down to RGB would change the colour the design asks for. Either way the
/// UI only needs the CSS text, which `Display` is; a string on the wire would
/// put an unvalidated value straight into a style property, and cost more than
/// this does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChatColor {
    /// Twitch's `chatColor`, packed as `0xRRGGBB`.
    Twitch(u32),
    /// The generated fallback, in thousandths so the type stays `Eq`.
    Generated {
        lightness_milli: u16,
        chroma_milli: u16,
        hue: u16,
    },
}

impl std::fmt::Display for ChatColor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Twitch(rgb) => write!(formatter, "#{:06x}", rgb & 0x00ff_ffff),
            Self::Generated {
                lightness_milli,
                chroma_milli,
                hue,
            } => write!(
                formatter,
                "oklch(0.{lightness_milli:03} 0.{chroma_milli:03} {hue})"
            ),
        }
    }
}

/// One channel row, as the UI renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(
    clippy::struct_excessive_bools,
    reason = "live plus three independent per-channel switches"
)]
pub struct ChannelView {
    pub id: u64,
    pub login: String,
    pub display_name: String,
    /// Twitch's chat colour for the channel's switches, with a generated
    /// fallback when Twitch reports none.
    pub chat_color: ChatColor,
    /// A local file the daemon cached for the avatar; `None` until it exists.
    pub avatar_path: Option<String>,
    pub live: bool,
    pub title: Option<String>,
    pub game: Option<String>,
    /// Twitch's game id; `None` when no game was ever resolved.
    pub game_id: Option<u64>,
    pub viewers: Option<u32>,
    pub collaboration_viewers: Option<u32>,
    /// Milliseconds since the unix epoch; `None` while offline.
    pub stream_start_ms: Option<i64>,
    /// When this channel was last seen live: Twitch's `lastBroadcast`
    /// `startedAt` while offline (which is the *end* of that broadcast), or
    /// the moment `stream-down` landed until that poll corrects it.
    pub last_live_ms: Option<i64>,
    /// Twitch's `BROADCAST-ID` for the active stream; `None` while offline.
    pub stream_id: Option<NonZeroU64>,
    /// The last failure this channel itself hit (a GQL refresh, not the
    /// connection): the row draws a warning when it is set. Pipeline and disk
    /// failures are carried by their own entities and merged in the UI.
    pub error: Option<String>,
    pub notify_live: bool,
    pub notify_titles: bool,
    pub record: bool,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// The recorder behind this pipeline, when one is attached.
    pub disk: Option<DiskView>,
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloaderSummary {
    pub queued: u32,
    pub capacity: u32,
    pub in_flight: u32,
    pub retries: u64,
}

/// Live counters from the recorder behind one pipeline.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskView {
    pub active: bool,
    pub runs: u32,
    pub segments: u64,
    pub bytes: u64,
    pub position_ms: u64,
    pub last_error: Option<String>,
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

/// Severity of one event in the daemon's log ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LogLevel {
    Info,
    Error,
}

/// One line of the daemon's event log.
///
/// The stream identity is carried beside the message rather than folded into
/// it, so the Logs tab can show and filter it: a stream event without the
/// broadcast id and Twitch's `createdAt` is a line nobody can correlate with
/// the recording on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    /// Monotonic within one daemon process; the UI drops anything it has seen.
    pub seq: u64,
    /// Milliseconds since the unix epoch.
    pub at_ms: i64,
    pub level: LogLevel,
    /// Subsystem that produced the line (`hermes`, `recording`, `notify`, ...).
    pub target: String,
    pub message: String,
    pub login: Option<String>,
    pub stream_id: Option<NonZeroU64>,
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub config: Config,
    pub conn: ConnHealth,
    pub channels: Vec<ChannelView>,
    pub pending: Vec<String>,
    pub pipelines: Vec<PipelineView>,
    pub arena: ArenaSummary,
    pub downloader: DownloaderSummary,
    pub notifications: Vec<NotificationView>,
    pub logs: Vec<LogEntry>,
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
    ChannelUpsert(Box<ChannelView>),
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
    Log(LogEntry),
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
    /// Follow a login. `switches` restores the per-channel settings of a
    /// channel that was just removed; `None` is a fresh follow with defaults.
    AddLogin {
        login: String,
        switches: Option<ChannelSwitches>,
    },
    RemoveChannel(u64),
    SetNotifyTitleChanges(bool),
    SetSound(bool),
    /// Point new recordings at this directory; `None` disables recording.
    /// Recordings already running keep writing where they started.
    SetRecordDir(Option<String>),
    /// Record both orientations of dual-format broadcasts.
    SetRecordDualFormat(bool),
    AddFilteredWord(String),
    RemoveFilteredWord(String),
    /// Turn the per-channel recording monitor on or off.
    SetRecord {
        channel: u64,
        record: bool,
    },
    /// Notify when this channel goes live.
    SetNotifyLive {
        channel: u64,
        notify: bool,
    },
    /// Notify when this channel's title or game changes.
    SetNotifyTitles {
        channel: u64,
        notify: bool,
    },
    ClearError,
    DismissNotification(u64),
    ApplyUpdate,
    /// Stop serving; `force` also disconnects the viewers. The recording
    /// monitor (if any) stays desired.
    PipelineStop {
        key: StreamKey,
        force: bool,
    },
    PipelineRestart(StreamKey),
    /// Disconnect every RTSP session without touching the pipeline.
    DisconnectViewers(StreamKey),
    /// Record this stream only; nothing is persisted and it ends at
    /// `stream-down`.
    RecordOnce {
        login: String,
        quality: Option<String>,
    },
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
    StreamDetail(Box<StreamDetail>),
    Segments(Vec<SegmentRecord>),
    Arena(ArenaSummary),
    Downloader(DownloaderSummary),
    Disk(DiskView),
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
    Update(Update),
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
        assert_eq!(Config::VERSION, 5);
    }

    #[test]
    fn a_channel_defaults_to_notify_only() {
        let channel = Channel::default();
        assert!(!channel.record);
        assert!(channel.notify_live);
        assert!(channel.notify_titles);
    }
}
