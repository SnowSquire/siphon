//! Every tunable and architectural constant, in one place.
//!
//! Format-local values (`INDEX_MAGIC`, `INDEX_VERSION`) stay in `disk.rs` because
//! they describe the on-disk format, which mirrors `index.hexpat`.

// The endpoint accessors read their `OnceLock` in a dev build, so they cannot be
// `const fn`; in a release build their body is only the constant.
#![allow(clippy::missing_const_for_fn)]

use std::time::Duration;

pub const PLAYER_VERSION: &str = "1.58.0-rc.2";

pub const TWITCH_GQL_ENDPOINT: &str = "https://gql.twitch.tv/gql";
pub const TWITCH_OAUTH_ENDPOINT: &str = "https://id.twitch.tv/oauth2/validate";
pub const TWITCH_HLS_BASE: &str = "https://usher.ttvnw.net/api/v2/channel/hls";
/// Hermes pubsub. `SIPHON_HERMES_URL` overrides it; `--debug` does not, because
/// the mock's socket is on its own port.
pub const HERMES_URL: &str = "wss://hermes.twitch.tv/v1?clientId=kimne78kx3ncx6brgo4mv6wki5h1ko";

pub const DEFAULT_CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";

/// RTSP control port; RTP/RTCP use ephemeral ports or interleaved TCP.
pub const DEFAULT_RTSP_PORT: u16 = 7417;

// `--debug <base>` replays the mock edge instead of Twitch. Dev builds only: a
// release binary has no such mode, and the addresses are the flag's, not these.
// Only the names the mock pretends to serve live here.

/// The mock's segments were captured from this channel.
#[cfg(debug_assertions)]
pub const DEBUG_CHANNEL_TITLE: &str = "sunnys";
/// The variants the debug channel pretends the mock offers. `test` keeps them
/// for the release-profile test binary.
#[cfg(any(test, debug_assertions))]
pub const DEBUG_VARIANT_ID: &str = "1080p";
#[cfg(any(test, debug_assertions))]
pub const DEBUG_PORTRAIT_VARIANT_ID: &str = "1080p-portrait";

/// RTP payload budget, matching gortsplib's default:
/// 1500 (UDP MTU) - 20 (IP) - 8 (UDP) - 12 (RTP) - 10 (SRTP headroom).
pub const RTP_MAX_PAYLOAD: usize = 1450;

/// Downloader tasks pulling from the fetch queue: one per segment in flight.
pub const DOWNLOAD_TASKS: usize = 8;

/// A consumer whose queue holds this many segments is more than ~40 s behind the
/// live edge and gets dropped. The RTSP consumer registers as lossy and drops the
/// individual message instead, so this only bites a strict consumer.
pub const MAX_CONSUMER_LAG: usize = 20;

/// Bytes per slot: `188 * 1024`.
///
/// 188 is the mpegts packet size, so a slot boundary never splits a packet; 1024
/// packets also makes it 4 KiB page aligned (`47` pages), so the one-time zeroing
/// maps to whole pages.
pub const SLOT_SIZE: usize = 192_512;

/// Slots per arena (one `u32` free bitmap).
pub const SLOTS_PER_ARENA: usize = 32;

pub const ARENA_ALIGN: usize = 4096;

pub const ARENA_BYTES: usize = SLOTS_PER_ARENA * SLOT_SIZE;

/// Slots one spool — one segment — may hold.
///
/// Twitch caps a source at 8 Mbps, so a 10 s DVR segment is ~10.6 MB and these 64
/// slots are 12.3 MB: room for a spike, but small enough that one lagging consumer
/// cannot pin the pool. Arenas themselves are allocated on demand with no cap, so
/// this is the only way a spool write fails.
pub const SEGMENT_SLOTS_MAX: usize = 64;

/// Slots one DVR chunk spool may hold.
///
/// A 12 Mbps source makes a 10 s DVR chunk ~15 MB, which does not fit the live
/// cap. The disk output gives these spools their own arena, so this cap only
/// bounds one chunk and never competes with live segments.
pub const DVR_SLOTS_MAX: usize = 96;

/// Attempts per segment before giving up.
pub const DOWNLOAD_RETRIES: u32 = 4;

/// How long a downloader waits for a lagging consumer to release slots.
pub const ARENA_BACKOFF: Duration = Duration::from_millis(50);

/// Consecutive slot-arena waits before the segment is abandoned.
pub const ARENA_RETRIES: u32 = 40;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const IO_TIMEOUT: Duration = Duration::from_secs(15);

/// Socket read staging size: bigger than the socket queue buys no syscalls, and
/// this lands in the measured 64–128 KiB sweet spot.
pub const READ_BUFFER: usize = 64 * 1024;

/// Plaintext bytes encrypted per TLS record.
pub const TLS_WRITE_CHUNK: usize = 16 * 1024;

/// Scratch for one encoded/encrypted record: plaintext plus record overhead.
pub const TLS_OUT_BUFFER: usize = TLS_WRITE_CHUNK + 2 * 1024;

/// Cap on a response head before the request is considered broken.
pub const MAX_HEAD_BYTES: usize = 64 * 1024;

pub const MAX_REDIRECTS: usize = 3;

/// Response headers parsed per response.
pub const MAX_HEADERS: usize = 64;

/// Longest accepted chunk-size / trailer line.
pub const MAX_CHUNK_LINE: usize = 256;

pub const POOL_MAX_IDLE: usize = 8;
pub const POOL_MAX_IDLE_PER_HOST: usize = 4;
pub const POOL_IDLE_AGE: Duration = Duration::from_secs(30);

/// Cap on a media playlist body.
pub const MAX_MANIFEST: usize = 1024 * 1024;

/// Cap on a VOD/DVR playlist body. These cover the whole broadcast, so a long
/// stream can be several megabytes.
pub const MAX_VOD_MANIFEST: usize = 8 * 1024 * 1024;

/// Cap on a GQL / OAuth JSON body.
pub const MAX_JSON: usize = 256 * 1024;

/// Cap on one notification avatar.
pub const MAX_IMAGE: usize = 4 * 1024 * 1024;

/// Cap on one downloaded installer.
pub const MAX_UPDATE: usize = 64 * 1024 * 1024;

/// Endpoint overrides, set once at startup from `--debug <base>`. Dev builds
/// only: a release binary has no such flag, so the statics are not there and
/// the accessors below are their Twitch constants.
#[cfg(debug_assertions)]
static GQL_ENDPOINT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
#[cfg(debug_assertions)]
static HLS_BASE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
#[cfg(debug_assertions)]
static PUBLIC_ENDPOINT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
#[cfg(debug_assertions)]
static MOCK_BASE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

#[must_use]
pub fn gql_endpoint() -> &'static str {
    #[cfg(debug_assertions)]
    {
        if let Some(endpoint) = GQL_ENDPOINT.get() {
            return endpoint;
        }
    }
    TWITCH_GQL_ENDPOINT
}

#[must_use]
pub fn hls_base() -> &'static str {
    #[cfg(debug_assertions)]
    {
        if let Some(base) = HLS_BASE.get() {
            return base;
        }
    }
    TWITCH_HLS_BASE
}

#[must_use]
pub fn public_endpoint() -> &'static str {
    #[cfg(debug_assertions)]
    {
        if let Some(endpoint) = PUBLIC_ENDPOINT.get() {
            return endpoint;
        }
    }
    TWITCH_OAUTH_ENDPOINT
}

/// The base `--debug` named, for the paths that are not one endpoint: the mock's
/// playlists and the archive route the registry recognises. `None` when this
/// build has no `--debug` or the flag was not given.
#[cfg(debug_assertions)]
#[must_use]
pub fn mock_base() -> Option<&'static str> {
    MOCK_BASE.get().map(String::as_str)
}

/// Points GQL, usher and OAuth at `base`. Called once, from `main`, and
/// compiled into dev builds only. Hermes is not derived from it: the mock's
/// socket is on its own port, so `SIPHON_HERMES_URL` names that.
#[cfg(debug_assertions)]
pub fn use_edge(base: &str) {
    let base = base.trim_end_matches('/');
    let _ = MOCK_BASE.set(base.to_owned());
    let _ = GQL_ENDPOINT.set(format!("{base}/gql"));
    let _ = HLS_BASE.set(format!("{base}/api/v2/channel/hls"));
    let _ = PUBLIC_ENDPOINT.set(format!("{base}/oauth2/validate"));
}

/// Segments kept per pipeline for `ListSegments` and the UI ring.
pub const SEGMENT_RING: usize = 64;

/// Event-log lines a snapshot carries.
pub const LOG_CAP: usize = 512;

/// Manifest poll bounds while the playlist is at the live edge.
pub const EDGE_POLL_MIN: Duration = Duration::from_millis(200);
pub const EDGE_POLL_MAX: Duration = Duration::from_secs(5);
pub const EDGE_CATCHUP_POLL: Duration = Duration::from_millis(100);

/// Scheduler restart backoff after a failed poll.
pub const SCHEDULER_BACKOFF_MIN: Duration = Duration::from_millis(200);
pub const SCHEDULER_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// How long a Hermes connection may take to say `welcome`.
pub const HERMES_WELCOME_TIMEOUT: Duration = Duration::from_secs(10);
/// Missed keepalives before the socket is considered dead.
pub const HERMES_KEEPALIVE_MISSES: u64 = 2;
/// Longest reconnect backoff.
pub const HERMES_MAX_RECONNECT_DELAY: Duration = Duration::from_secs(15);

/// Update check cadence, request timeout and the release feed.
pub const UPDATE_CHECK_INTERVAL: Duration = Duration::from_hours(6);
pub const UPDATE_TIMEOUT: Duration = Duration::from_secs(15);
pub const RELEASES_URL: &str = "https://api.github.com/repos/SnowSquire/siphon/releases/latest";
