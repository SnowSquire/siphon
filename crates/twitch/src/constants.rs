pub const PLAYER_VERSION: &str = "1.56.0-rc.1";

pub const TWITCH_GQL_ENDPOINT: &str = "https://gql.twitch.tv/gql";
pub const TWITCH_OAUTH_ENDPOINT: &str = "https://id.twitch.tv/oauth2/validate";
pub const TWITCH_HLS_BASE: &str = "https://usher.ttvnw.net/api/v2/channel/hls";

pub const DEFAULT_CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";

pub const DOWNLOAD_THREADS: usize = 8;

pub const DEFAULT_PORT: u16 = 7417;
// LCM(16384, 188) twitch sends each chunk in 16KiB, or at least ureq buffers to this size
pub const CHUNK_SIZE: usize = 770_048;
