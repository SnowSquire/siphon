pub const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:154.0) Gecko/20100101 Firefox/154.0";

pub const PLAYER_VERSION: &str = "1.54.0-rc.3";

pub const TWITCH_GQL_ENDPOINT: &str = "https://gql.twitch.tv/gql";
pub const TWITCH_OAUTH_ENDPOINT: &str = "https://id.twitch.tv/oauth2/validate";
pub const TWITCH_HLS_BASE: &str = "https://usher.ttvnw.net/api/v2/channel/hls";

pub const KICK_CHANNELS_ENDPOINT: &str = "https://kick.com/api/v2/channels";

pub const DEFAULT_CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
pub const DEFAULT_CONFIG_PATH: &str = concat!(env!("CARGO_PKG_NAME"), "/config");

pub const MPEG_TS_PACKET_SIZE: u8 = 188;
pub const DOWNLOAD_THREADS: usize = 8;
pub const fn round_chunk_size(size: usize) -> usize {
    size / MPEG_TS_PACKET_SIZE as usize * MPEG_TS_PACKET_SIZE as usize
}

pub const DEFAULT_PORT: u16 = 7417;
// LCM(16384, 188) twitch sends each chunk in 16KiB, or at least ureq buffers to this size
pub const CHUNK_SIZE: usize = 770_048;
