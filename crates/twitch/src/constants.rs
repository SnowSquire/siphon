//! Every tunable and architectural constant, in one place.
//!
//! Format-local values (`INDEX_MAGIC`, `INDEX_VERSION`) stay in `disk.rs` because
//! they describe the on-disk format, which mirrors `index.hexpat`.

use std::time::Duration;

// ---------------------------------------------------------------- twitch --

pub const PLAYER_VERSION: &str = "1.58.0-rc.2";

pub const TWITCH_GQL_ENDPOINT: &str = "https://gql.twitch.tv/gql";
pub const TWITCH_OAUTH_ENDPOINT: &str = "https://id.twitch.tv/oauth2/validate";
pub const TWITCH_HLS_BASE: &str = "https://usher.ttvnw.net/api/v2/channel/hls";

pub const DEFAULT_CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";

// ----------------------------------------------------------- local server --

/// Preferred port for the raw TS output; ephemeral if taken.
pub const DEFAULT_PORT: u16 = 7417;

/// RTSP control port; RTP/RTCP use ephemeral ports or interleaved TCP.
pub const DEFAULT_RTSP_PORT: u16 = 8554;

/// RTP payload budget, matching gortsplib's default:
/// 1500 (UDP MTU) - 20 (IP) - 8 (UDP) - 12 (RTP) - 10 (SRTP headroom).
pub const RTP_MAX_PAYLOAD: usize = 1450;

/// Downloader tasks pulling from the fetch queue: one per segment in flight.
pub const DOWNLOAD_TASKS: usize = 8;

/// A consumer whose queue holds this many segments is more than ~40 s behind the
/// live edge and gets dropped. Late or rate-limited viewers are meant to be served
/// by the RTSP/RTP output instead (see notes/es-rtp-rtsp.md).
pub const MAX_CONSUMER_LAG: usize = 20;

// ----------------------------------------------------------------- arena --

/// Bytes per slot: `188 * 1024`.
///
/// 188 is the mpegts packet size, so a slot boundary never splits a packet; 1024
/// packets also makes it 4 KiB page aligned (`47` pages), so the one-time zeroing
/// maps to whole pages.
pub const SLOT_SIZE: usize = 192_512;

/// Slots per arena (one `u32` free bitmap).
pub const SLOTS_PER_ARENA: usize = 32;

/// Alignment of an arena's payload.
pub const ARENA_ALIGN: usize = 4096;

/// Payload bytes of one arena.
pub const ARENA_BYTES: usize = SLOTS_PER_ARENA * SLOT_SIZE;

const ARENAS_PER_BROADCAST: usize = 6;

/// Slots one spool may hold at once: six arenas, ~37 MB, i.e. 30 s of an
/// ~9.8 Mbps stream. Twitch lists 30 s of segments in the live playlist, and the
/// scheduler's replay cursors pin that whole window.
pub const SLOT_BUDGET: usize = ARENAS_PER_BROADCAST * SLOTS_PER_ARENA;

/// Broadcasts the daemon may serve concurrently.
pub const MAX_BROADCASTS: usize = 4;

/// Hard ceiling across all broadcasts. Arenas are allocated lazily, so this is a
/// ceiling and not a reservation.
pub const MAX_ARENAS: usize = ARENAS_PER_BROADCAST * MAX_BROADCASTS;

/// Total slots across all broadcasts, and therefore the number of spool records
/// that can ever be live.
pub const MAX_SLOTS: usize = MAX_ARENAS * SLOTS_PER_ARENA;

// ------------------------------------------------------------ downloader --

/// Attempts per segment before giving up.
pub const DOWNLOAD_RETRIES: u32 = 4;

/// How long a downloader waits for a lagging consumer to release slots.
pub const ARENA_BACKOFF: Duration = Duration::from_millis(50);

/// Consecutive slot-arena waits before the segment is abandoned.
pub const ARENA_RETRIES: u32 = 40;

// --------------------------------------------------------------- http --

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

// -------------------------------------------------------------- bodies --

/// Cap on a media playlist body.
pub const MAX_MANIFEST: usize = 1024 * 1024;

/// Cap on a GQL / OAuth JSON body.
pub const MAX_JSON: usize = 256 * 1024;
