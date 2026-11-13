// Debug/stat accessors are exercised by the arena tests and future policies.
#[allow(dead_code, clippy::future_not_send)]
mod arena;
#[allow(clippy::future_not_send)]
mod constants;
// Disk output is disabled for now: RTSP/RTP is the only sink. The module is kept
// (and mirrors index.hexpat) for when the DVR output comes back.
// #[allow(clippy::future_not_send)]
// mod disk;
#[allow(clippy::future_not_send)]
mod daemon;
#[allow(clippy::future_not_send)]
mod downloader;
#[allow(clippy::future_not_send)]
mod spool;
// compio tasks are deliberately !Send (one thread-per-core reactor).
// `dead_code`: the client exposes a slightly wider surface than this binary uses.
#[allow(dead_code, clippy::future_not_send)]
mod http;
#[allow(clippy::future_not_send)]
mod playlist;
#[allow(clippy::future_not_send)]
mod rtsp;
#[allow(clippy::future_not_send)]
mod scheduler;
#[allow(clippy::future_not_send)]
mod sha1;
#[allow(clippy::future_not_send)]
mod utils;
#[allow(clippy::future_not_send)]
mod variant;

use std::cell::RefCell;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::rc::Rc;

use clap::Args;
use clap::Parser;
use clap::Subcommand;
#[cfg(not(miri))]
use mimalloc::MiMalloc;
use tracing::error;
use tracing_subscriber::EnvFilter;

use crate::arena::ArenaSet;
use crate::constants::DEBUG_CHANNEL_TITLE;
use crate::constants::DEBUG_DVR_URL;
use crate::constants::DEFAULT_RTSP_PORT;
use crate::http::Client;
use crate::spool::ConsumerWake;
use crate::spool::WakeSet;
use crate::variant::VariantPlaylistError;
use crate::variant::choose_variant;
use crate::variant::get_variants;

// Miri cannot execute a foreign allocator, so it runs on the system allocator.
#[cfg(not(miri))]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Parser, Debug)]
#[command(author, version, about = "Twitch HLS client")]
struct Config {
    /// Twitch client id used for GQL requests
    #[arg(long, global = true)]
    client_id: Option<String>,

    /// Optional OAuth authorization token
    #[arg(long, global = true)]
    auth_token: Option<String>,

    /// Enable low latency mode flags
    #[arg(short, long, global = true, default_value_t = true)]
    low_latency: bool,

    /// Replay the mock server instead of fetching Twitch
    #[arg(short, long, global = true)]
    debug: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Serve one channel on the loopback RTSP addresses
    Play(Play),
    /// Serve any channel through `rtsp://host/<channel>/<quality>/`
    Daemon(Daemon),
}

#[derive(Args, Debug)]
struct Play {
    /// Twitch channel name to fetch
    #[arg(short, long)]
    channel: String,

    /// STABLE-VARIANT-ID (`720p60`, `chunked`, ...); defaults to the first
    #[arg(short, long)]
    quality: Option<String>,
}

#[derive(Args, Debug)]
struct Daemon {
    /// Address the RTSP listener binds
    #[arg(long, default_value_t = default_bind())]
    bind: SocketAddr,

    /// Streams served at once
    #[arg(long, default_value_t = crate::constants::MAX_STREAMS)]
    max_streams: usize,
}

fn default_bind() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, DEFAULT_RTSP_PORT))
}

/// Every panic is fatal, in every build.
///
/// Tasks are detached, so a panicking task would otherwise just disappear: the
/// RTSP control plane keeps answering while no media is ever produced again,
/// and the scheduler keeps running with a dead consumer. Release builds already
/// get this from `panic = "abort"`; this makes `cargo run` behave the same way
/// instead of freezing forty minutes into a stream.
fn abort_on_panic() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        std::process::abort();
    }));
}

#[compio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    abort_on_panic();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cfg = Config::parse();
    match &cfg.command {
        Command::Play(args) => play(&cfg, args).await?,
        Command::Daemon(args) => daemon(&cfg, args).await?,
    }
    Ok(())
}

/// One channel, served at the root of every loopback RTSP address.
#[allow(clippy::future_not_send)]
async fn play(cfg: &Config, args: &Play) -> Result<(), VariantPlaylistError> {
    let title = if cfg.debug {
        DEBUG_CHANNEL_TITLE.to_owned()
    } else {
        args.channel.clone()
    };

    let client = Rc::new(Client::new());
    let arena = Rc::new(ArenaSet::new());
    let wakes = WakeSet::new();

    // Debug mode replays the mock's DVR playlist. Otherwise discovery (GQL plus
    // domain probing) and the periodic playlist refresh run in the background,
    // concurrently with the live variant and manifest fetches.
    let dvr = rtsp::vod::Dvr::new(cfg.debug.then(|| DEBUG_DVR_URL.to_owned()));
    rtsp::vod::spawn(
        Rc::clone(&client),
        (!cfg.debug).then(|| args.channel.clone()),
        Rc::clone(&dvr),
    );

    let variants = if cfg.debug {
        vec![variant::debug_variant()]
    } else {
        // A manifest failure is reported and turned into a non-zero exit; it is
        // not a reason to kill the process from deep inside the request path.
        match get_variants(
            &client,
            cfg.client_id.as_deref(),
            cfg.auth_token.as_deref(),
            &args.channel,
            cfg.low_latency,
        )
        .await
        {
            Ok((variants, _stream_id)) => variants,
            Err(error) => {
                error!(%error, channel = %args.channel, "failed to resolve variants");
                return Err(error);
            }
        }
    };

    let variant = choose_variant(&variants, args.quality.as_deref()).ok_or_else(|| {
        VariantPlaylistError::UnknownQuality {
            channel: args.channel.clone(),
            quality: args.quality.clone().unwrap_or_default(),
        }
    })?;
    // The public archive's master is fetched in parallel; picking the media
    // playlist with the same stable-variant-id as the live stream is deferred
    // until now.
    dvr.set_live_variant(&variant.id);

    let key = rtsp::StreamKey {
        login: args.channel.to_ascii_lowercase(),
        quality: args.quality.clone(),
    };
    let stream = Rc::new(rtsp::Stream::new(key, title));

    let (downloaders_tx, downloaders_rx) = kanal::bounded(32);
    let (registration_tx, registration_rx) = kanal::unbounded();
    // The scheduler publishes its current live playlist window here. When VOD
    // playback reaches the end of the DVR timeline, the RTSP engine replays the
    // live segments in this window up to the live edge, bridging the DVR lag.
    let live_window = Rc::new(RefCell::new(Vec::new()));
    let reader_wake = ConsumerWake::new();

    downloader::spawn_downloaders::<{ constants::DOWNLOAD_TASKS }>(&client, &downloaders_rx);
    rtsp::spawn_media(
        Rc::clone(&stream),
        Rc::clone(&client),
        Rc::clone(&dvr),
        &wakes,
        &registration_tx,
        Rc::clone(&live_window),
        &reader_wake,
    );
    // `play` keeps serving after the last player leaves: a reconnect starts at
    // the live edge of the same stream.
    rtsp::spawn_single(Rc::clone(&stream));

    let scheduler = scheduler::StreamScheduler::new(
        client,
        arena,
        wakes,
        variant.url.clone(),
        downloaders_tx.to_async(),
        registration_rx,
        reader_wake,
    )
    .with_live_window(live_window);

    scheduler.main().await;

    // Dropping the runtime cancels the remaining tasks.
    drop(registration_tx);
    drop(downloaders_rx);
    Ok(())
}

/// A long-lived RTSP server: streams are created by `DESCRIBE` and dropped when
/// their last viewer disconnects.
#[allow(clippy::future_not_send)]
async fn daemon(cfg: &Config, args: &Daemon) -> Result<(), Box<dyn std::error::Error>> {
    let client = Rc::new(Client::new());
    let arena = Rc::new(ArenaSet::new());

    // Global: one client, one slot pool, one downloader queue for every stream.
    let (downloaders_tx, downloaders_rx) = kanal::bounded(32);
    downloader::spawn_downloaders::<{ constants::DOWNLOAD_TASKS }>(&client, &downloaders_rx);

    let registry = Rc::new(daemon::Registry::new(
        client,
        arena,
        downloaders_tx.to_async(),
        daemon::Options {
            client_id: cfg.client_id.clone(),
            auth_token: cfg.auth_token.clone(),
            low_latency: cfg.low_latency,
            debug: cfg.debug,
            max_streams: args.max_streams,
        },
    ));

    // Only returns on a bind error: the accept loop runs for the process's life.
    rtsp::serve(registry, args.bind).await?;

    drop(downloaders_rx);
    Ok(())
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::variant::Variant;
    use crate::variant::debug_variant;

    #[test]
    fn quality_defaults_to_the_first_variant() {
        let variants = vec![
            debug_variant(),
            Variant {
                id: "audio_only".into(),
                ..debug_variant()
            },
        ];
        assert_eq!(
            choose_variant(&variants, None).map(|variant| variant.id.as_ref()),
            Some("1080p")
        );
        assert_eq!(
            choose_variant(&variants, Some("audio_only")).map(|variant| variant.id.as_ref()),
            Some("audio_only")
        );
        assert!(choose_variant(&variants, Some("720p60")).is_none());
    }

    #[test]
    fn cli_parses_both_modes() {
        let play =
            Config::try_parse_from(["app", "play", "-c", "sunnys", "-q", "720p60"]).expect("play");
        let Command::Play(args) = play.command else {
            panic!("expected play");
        };
        assert_eq!(args.channel, "sunnys");
        assert_eq!(args.quality.as_deref(), Some("720p60"));

        let daemon =
            Config::try_parse_from(["app", "daemon", "--bind", "0.0.0.0:8554"]).expect("daemon");
        let Command::Daemon(args) = daemon.command else {
            panic!("expected daemon");
        };
        assert_eq!(args.bind.port(), 8554);
        assert_eq!(args.max_streams, constants::MAX_STREAMS);

        // `-c` is not a daemon option.
        assert!(Config::try_parse_from(["app", "daemon", "-c", "sunnys"]).is_err());
    }
}
