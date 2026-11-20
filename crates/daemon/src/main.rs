// Debug/stat accessors are exercised by the arena tests and future policies.
#[allow(dead_code, clippy::future_not_send)]
mod app;
#[allow(dead_code, clippy::future_not_send)]
mod arena;
#[allow(clippy::future_not_send)]
mod avatar;
#[allow(clippy::future_not_send)]
mod balesh;
#[allow(dead_code, clippy::future_not_send)]
mod bus;
#[allow(clippy::future_not_send)]
mod config;
#[allow(clippy::future_not_send)]
mod constants;
#[allow(clippy::future_not_send)]
mod disk;
#[allow(clippy::future_not_send)]
mod downloader;
#[allow(clippy::future_not_send)]
mod gql;
#[allow(clippy::future_not_send)]
mod hermes;
#[allow(clippy::future_not_send)]
mod logging;

mod matcher;
#[allow(dead_code, clippy::future_not_send)]
mod metrics;
#[allow(clippy::future_not_send)]
mod notifier;
#[allow(clippy::future_not_send)]
mod notify;
#[allow(clippy::future_not_send)]
mod pid;
#[allow(clippy::future_not_send)]
mod registry;
#[allow(clippy::future_not_send)]
mod rpc;
#[allow(clippy::future_not_send)]
mod spool;
#[allow(clippy::future_not_send)]
mod time;
#[allow(clippy::future_not_send)]
mod update;
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

use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::rc::Rc;

use clap::Args;
use clap::Parser;
use clap::Subcommand;
use futures_util::future::Either;
use futures_util::future::select;
use futures_util::pin_mut;
#[cfg(not(miri))]
use mimalloc::MiMalloc;
// Cargo's unused-dependency lint only sees the crate root for target-gated
// crates; the real use is in `notifier`.
#[cfg(windows)]
use tauri_winrt_notification as _;
use tracing::error;

use crate::app::App;
use crate::app::AppHandle;
use crate::app::AppInit;
use crate::arena::ArenaSet;
use crate::bus::Bus;
use crate::constants::DEBUG_CHANNEL_TITLE;
use crate::constants::DEBUG_DVR_URL;
use crate::constants::DEFAULT_RTSP_PORT;
use crate::http::Client;
use crate::metrics::DiskMetrics;
use crate::metrics::DownloaderMetrics;
use crate::registry::Options as RegistryOptions;
use crate::registry::Registry;
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
#[command(author, version, about = "Siphon backend")]
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

    /// Record the complete broadcast into this directory
    #[arg(long)]
    output: Option<std::path::PathBuf>,

    /// Webhook to notify about skipped DVR chunks and alignment problems
    #[arg(long)]
    notify: Option<String>,

    /// Keep `live.NNN` run pairs after their holes are filled
    #[arg(long)]
    keep_runs: bool,
}

#[derive(Args, Debug)]
struct Daemon {
    /// Address the RTSP listener binds
    #[arg(long, default_value_t = default_bind())]
    bind: SocketAddr,

    /// Admin RPC endpoint: a named pipe on Windows, a socket path on Unix.
    /// Defaults to the per-app endpoint derived from the app id.
    #[arg(long)]
    admin: Option<String>,

    /// Do not listen for the UI
    #[arg(long)]
    no_admin: bool,

    /// Streams served at once
    #[arg(long, default_value_t = crate::constants::MAX_STREAMS)]
    max_streams: usize,

    /// Record monitored streams into this directory
    #[arg(long)]
    output: Option<std::path::PathBuf>,

    /// Webhook to notify about skipped DVR chunks and alignment problems
    #[arg(long)]
    notify: Option<String>,

    /// Keep `live.NNN` run pairs after their holes are filled
    #[arg(long)]
    keep_runs: bool,
}

fn default_bind() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, DEFAULT_RTSP_PORT))
}

#[compio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    logging::init(config::APP_ID);

    let cfg = Config::parse();
    match &cfg.command {
        Command::Play(args) => Box::pin(play(&cfg, args)).await?,
        Command::Daemon(args) => daemon(&cfg, args).await?,
    }
    Ok(())
}

/// One channel, served at the root of every loopback RTSP address.
#[allow(clippy::future_not_send)]
async fn play(cfg: &Config, args: &Play) -> Result<(), Box<dyn std::error::Error>> {
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
    // concurrently with the live variant and manifest fetches. `NO_DVR` turns
    // the mock's DVR playlist off so the no-URL promotion path can be tested.
    let debug_dvr = (std::env::var_os("NO_DVR").is_none()).then(|| DEBUG_DVR_URL.to_owned());
    let dvr = rtsp::dvr::Dvr::new(if cfg.debug { debug_dvr } else { None });
    rtsp::dvr::spawn(
        Rc::clone(&client),
        (!cfg.debug).then(|| args.channel.clone()),
        Rc::clone(&dvr),
    );

    let (variants, broadcast_id) = if cfg.debug {
        (vec![variant::debug_variant()], 0)
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
            Ok(resolved) => resolved,
            Err(error) => {
                error!(%error, channel = %args.channel, "failed to resolve variants");
                return Err(error.into());
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
    let downloader_metrics = Rc::new(DownloaderMetrics::new(32));
    let (registration_tx, registration_rx) = kanal::unbounded();
    // The scheduler publishes its current live playlist window here. When VOD
    // playback reaches the end of the DVR timeline, the RTSP engine replays the
    // live segments in this window up to the live edge, bridging the DVR lag.
    let live_window = Rc::new(std::cell::RefCell::new(Vec::new()));
    let reader_wake = ConsumerWake::new();

    let notifier = args
        .notify
        .as_ref()
        .map(|url| notify::Notifier::new(Rc::clone(&client), url.clone()));
    let disk = args.output.as_ref().map(|output| {
        disk::spawn_disk_output(
            disk::DiskOptions {
                output: output.clone(),
                stream_id: if cfg.debug { 0 } else { broadcast_id },
                keep_runs: args.keep_runs,
                notifier,
                dvr: Rc::clone(&dvr),
                client: Rc::clone(&client),
                metrics: Rc::new(DiskMetrics::new()),
            },
            &wakes,
            &downloaders_tx.clone().to_async(),
            &registration_tx,
        )
    });

    downloader::spawn_downloaders::<{ constants::DOWNLOAD_TASKS }>(
        &client,
        &downloaders_rx,
        &downloader_metrics,
    );
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

    let scheduler_future = scheduler.main();
    pin_mut!(scheduler_future);
    if let Some(disk) = disk {
        let disk_future = disk.wait();
        pin_mut!(disk_future);
        match select(scheduler_future.as_mut(), disk_future.as_mut()).await {
            Either::Left(((), _)) => {
                // The scheduler ended (offline): let the DVR finish its tail.
                if let Some(error) = disk_future.await {
                    return Err(error.into());
                }
            }
            Either::Right((fatal, _)) => {
                if let Some(error) = fatal {
                    return Err(error.into());
                }
                scheduler_future.await;
            }
        }
    } else {
        scheduler_future.await;
    }

    // Dropping the runtime cancels the remaining tasks.
    drop(registration_tx);
    drop(downloaders_rx);
    Ok(())
}

/// A long-lived RTSP server plus the Siphon backend: config, Hermes, monitors,
/// disk, notifications and the admin RPC.
#[allow(clippy::future_not_send)]
async fn daemon(cfg: &Config, args: &Daemon) -> Result<(), Box<dyn std::error::Error>> {
    let client = Rc::new(Client::new());
    let arena = Rc::new(ArenaSet::new());

    // Global: one client, one slot pool, one downloader queue for every stream.
    let (downloaders_tx, downloaders_rx) = kanal::bounded(32);
    let downloader_metrics = Rc::new(DownloaderMetrics::new(32));
    downloader::spawn_downloaders::<{ constants::DOWNLOAD_TASKS }>(
        &client,
        &downloaders_rx,
        &downloader_metrics,
    );

    let notifier = args
        .notify
        .as_ref()
        .map(|url| notify::Notifier::new(Rc::clone(&client), url.clone()));

    let bus = Bus::new();
    let registry = Rc::new(Registry::new(
        Rc::clone(&client),
        arena,
        downloaders_tx.to_async(),
        Rc::clone(&downloader_metrics),
        RegistryOptions {
            client_id: cfg.client_id.clone(),
            auth_token: cfg.auth_token.clone(),
            low_latency: cfg.low_latency,
            debug: cfg.debug,
            max_streams: args.max_streams,
            output: args.output.clone(),
            notify: notifier,
            keep_runs: args.keep_runs,
        },
        Rc::clone(&bus),
    ));
    registry.watch_finished();

    let config_path = config::default_path(config::APP_ID);
    let config = config::load(&config_path);
    if let Err(error) = pid::write(config::APP_ID) {
        error!(%error, "failed to write the daemon pid file");
    }
    notifier::register_aumid();

    let (events_tx, events_rx) = kanal::unbounded();
    let wake = ConsumerWake::new();
    let sender = app::EventSender::new(events_tx, Rc::clone(&wake));
    let handle = AppHandle::new(sender.clone(), Rc::clone(&bus));
    let app = App::new(AppInit {
        config,
        config_path,
        client: Rc::clone(&client),
        registry: Rc::clone(&registry),
        bus,
        events: events_rx,
        sender,
        wake,
        client_id: cfg.client_id.clone(),
        debug: cfg.debug,
    });

    if !args.no_admin {
        let handle = handle.clone();
        let endpoint = args
            .admin
            .clone()
            .unwrap_or_else(|| siphon_shared::rpc::admin_endpoint(config::APP_ID));
        compio::runtime::spawn(async move {
            if let Err(error) = rpc::serve(endpoint, handle).await {
                error!(%error, "rpc: listener failed");
            }
        })
        .detach();
    }
    compio::runtime::spawn(app.run()).detach();

    // Only returns on a bind error: the RTSP accept loop runs for the process's
    // life.
    rtsp::serve(registry, args.bind).await?;
    pid::remove(config::APP_ID);

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
        assert!(args.admin.is_none());
        assert!(!args.no_admin);
        assert_eq!(args.max_streams, constants::MAX_STREAMS);

        // `-c` is not a daemon option.
        assert!(Config::try_parse_from(["app", "daemon", "-c", "sunnys"]).is_err());
    }
}
