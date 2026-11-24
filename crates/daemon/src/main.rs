// Debug/stat accessors are exercised by the arena tests.
#[allow(dead_code, clippy::future_not_send)]
mod app;
#[allow(dead_code, clippy::future_not_send)]
mod arena;
#[allow(clippy::future_not_send)]
mod auth;
#[allow(clippy::future_not_send)]
mod avatar;
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
#[allow(dead_code, clippy::future_not_send)]
mod lane;
#[allow(clippy::future_not_send)]
mod logging;

#[allow(dead_code, clippy::future_not_send)]
mod metrics;
#[allow(clippy::future_not_send)]
mod notify;
#[allow(clippy::future_not_send)]
mod pid;
#[allow(clippy::future_not_send)]
mod registry;
#[allow(clippy::future_not_send)]
mod riffle;
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
mod media;
#[allow(clippy::future_not_send)]
mod playlist;
#[allow(clippy::future_not_send)]
mod scheduler;
#[allow(clippy::future_not_send)]
mod sha1;
#[allow(clippy::future_not_send)]
mod utils;
#[allow(clippy::future_not_send)]
mod variant;

use std::rc::Rc;

use clap::Args;
use clap::CommandFactory;
use clap::FromArgMatches;
use clap::Parser;
use clap::Subcommand;
#[cfg(not(miri))]
use mimalloc::MiMalloc;
use sonic_rs::JsonValueTrait as _;
use tracing::error;

use crate::app::App;
use crate::app::AppInit;
use crate::arena::ArenaSet;
use crate::bus::Bus;
#[cfg(debug_assertions)]
use crate::constants::DEBUG_CHANNEL_TITLE;
use crate::constants::HERMES_WELCOME_TIMEOUT;
use crate::http::Client;
use crate::http::timeout;
use crate::http::ws::WsStream;
use crate::metrics::DiskMetrics;
use crate::metrics::DownloaderMetrics;
use crate::registry::Options as RegistryOptions;
use crate::registry::Recording as RegistryRecording;
use crate::registry::Registry;
use crate::spool::ConsumerWake;
use crate::spool::WakeSet;
use crate::variant::Variant;
use crate::variant::VariantKind;
use crate::variant::VariantPlaylistError;
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

    /// Enable low latency mode flags
    #[arg(short, long, global = true, default_value_t = true)]
    low_latency: bool,

    /// Replay a mock edge at this base URL instead of fetching Twitch
    /// (dev builds only)
    #[cfg(debug_assertions)]
    #[arg(short, long, global = true, value_name = "BASE")]
    debug: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Record one channel to disk, exit when its broadcast ends
    Record(Record),
    /// Run the backend: pipelines, the stream registry and the Riffle listener
    Daemon(Daemon),
    /// Open a window in which one remote UI can pair, and approve it by code
    Pair(Pair),
}

/// One channel, no UI: wait for it to go live, record it, exit when it ends.
/// Which side of a broadcast a recording takes.
#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
enum RecordMode {
    /// Live and DVR: the complete archive, as the daemon makes it.
    #[default]
    Both,
    /// The live playlist only, with no DVR backfill.
    Live,
    /// The DVR playlist only, with no live consumer.
    Dvr,
}

#[derive(Args, Debug)]
struct Record {
    /// Channel login to record
    channel: String,

    /// STABLE-VARIANT-ID (`720p60`, `chunked`, `1920p`, ...); defaults to the
    /// highest-scoring variant
    variant: Option<String>,

    /// live, dvr, or both
    #[arg(long, value_enum, default_value_t)]
    mode: RecordMode,

    /// List the channel's variants and exit (the channel must be live)
    #[arg(long)]
    list: bool,

    /// Write to <dir>/<broadcast id>/<variant>/
    #[arg(long)]
    output: Option<std::path::PathBuf>,

    /// Webhook to notify about skipped DVR chunks and alignment problems
    #[arg(long)]
    notify: Option<String>,

    /// Keep `live.NNN` run pairs after their holes are filled
    #[arg(long)]
    keep_runs: bool,

    /// Twitch token used for GQL and usher; also stored in `auth.json` so a
    /// later daemon start does not need it again
    #[arg(long)]
    auth_token: Option<String>,
}

#[derive(Args, Debug)]
struct Daemon {
    /// Admin RPC endpoint: a named pipe on Windows, a socket path on Unix.
    /// Defaults to the per-app endpoint derived from the app id.
    #[arg(long)]
    admin: Option<String>,

    /// Do not listen for the UI
    #[arg(long)]
    no_admin: bool,

    /// Record into this directory instead of the configured recordDir (dev override)
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
struct Pair {
    /// How long the window stays open, in seconds
    #[arg(long, default_value_t = 120)]
    timeout: u64,
}

#[compio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    logging::init(config::APP_ID);

    let command = Config::command().after_help(format!(
        "Build: {}",
        siphon_shared::build::version_with_commit()
    ));
    let matches = command.get_matches();
    let cfg = Config::from_arg_matches(&matches)?;
    // `--debug <base>` is only "the endpoints are the mock edge": no subsystem
    // below this point knows the difference.
    #[cfg(debug_assertions)]
    if let Some(base) = cfg.debug.as_deref() {
        constants::use_edge(base);
    }

    match &cfg.command {
        Command::Record(args) => record(&cfg, args).await?,
        Command::Daemon(args) => daemon(&cfg, args).await?,
        Command::Pair(args) => pair(args)?,
    }
    Ok(())
}

/// Whether `--debug` named a mock edge. A release build has no such flag, so
/// this is `false` there and the branches below fold away.
#[cfg(debug_assertions)]
const fn debug_mode(cfg: &Config) -> bool {
    cfg.debug.is_some()
}

#[cfg(not(debug_assertions))]
const fn debug_mode(_: &Config) -> bool {
    false
}

/// The channel `--debug` pretends to serve: the mock's title, and the live and
/// portrait variant groups it offers under broadcast id `0`.
#[cfg(debug_assertions)]
fn debug_channel(cfg: &Config) -> Option<(String, Vec<Variant>)> {
    let base = cfg.debug.as_deref()?.trim_end_matches('/');
    Some((
        DEBUG_CHANNEL_TITLE.to_owned(),
        vec![
            variant::debug_variant(base),
            variant::debug_portrait_variant(base),
        ],
    ))
}

#[cfg(not(debug_assertions))]
const fn debug_channel(_: &Config) -> Option<(String, Vec<Variant>)> {
    None
}

/// The mock's DVR playlist for a variant group. `NO_DVR` turns it off so the
/// no-URL promotion path can be tested.
#[cfg(debug_assertions)]
fn debug_dvr_url(cfg: &Config, variant: &Variant) -> Option<String> {
    let base = cfg.debug.as_deref()?.trim_end_matches('/');
    if std::env::var_os("NO_DVR").is_some() {
        return None;
    }
    let file = if variant.kind == VariantKind::Portrait {
        "portrait-dvr.m3u8"
    } else {
        "index-dvr.m3u8"
    };
    Some(format!("{base}/{file}"))
}

#[cfg(not(debug_assertions))]
const fn debug_dvr_url(_: &Config, _: &Variant) -> Option<String> {
    None
}

/// One channel, recorded to disk with no control plane and no viewer.
#[allow(clippy::future_not_send)]
async fn record(cfg: &Config, args: &Record) -> Result<(), Box<dyn std::error::Error>> {
    let login = args.channel.to_ascii_lowercase();
    let client = Rc::new(Client::new());
    let debug = debug_mode(cfg);
    // The flag is the ad-hoc mode's token; it is also written to `auth.json`,
    // which is how a daemon start learns it without a command-line secret.
    let auth_token = auth::resolve(args.auth_token.clone());

    let Some((variants, broadcast_id)) =
        resolve_record_variants(cfg, auth_token.as_deref(), &client, &login, args).await?
    else {
        return Ok(());
    };
    if args.list {
        list_variants(&login, &variants);
        return Ok(());
    }

    let Some(output) = args.output.clone() else {
        return Err("record needs --output".into());
    };
    let variant = match &args.variant {
        Some(id) => variants
            .iter()
            .find(|variant| variant.id.as_ref() == id.as_str())
            .ok_or_else(|| format!("no variant {id}; --list shows the live master's"))?,
        None => variant::best_variant(&variants).ok_or("the live master has no variants")?,
    };

    let want_live = !matches!(args.mode, RecordMode::Dvr);
    let want_dvr = !matches!(args.mode, RecordMode::Live);

    // The DVR side: discovery when recording one, otherwise no URL at all (the
    // disk then promotes the live recording straight into the canonical file).
    let dvr = if want_dvr {
        let dvr = media::dvr::Dvr::new(debug_dvr_url(cfg, variant));
        media::dvr::spawn(
            Rc::clone(&client),
            (!debug).then(|| login.clone()),
            Rc::clone(&dvr),
        );
        dvr
    } else {
        media::dvr::Dvr::new(None)
    };
    dvr.set_live_variant(variant);

    let arena = Rc::new(ArenaSet::new());
    let wakes = WakeSet::new();
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
    let (registration_tx, registration_rx) = kanal::unbounded();
    let dir = output
        .join(if debug { 0 } else { broadcast_id }.to_string())
        .join(&*variant.id);
    let disk = disk::spawn_disk_output(
        disk::DiskOptions {
            output: output.clone(),
            stream_id: if debug { 0 } else { broadcast_id },
            variant: variant.id.clone(),
            keep_runs: args.keep_runs,
            notifier,
            dvr,
            client: Rc::clone(&client),
            metrics: Rc::new(DiskMetrics::new()),
            live: want_live,
        },
        &wakes,
        &downloaders_tx.clone().to_async(),
        want_live.then_some(&registration_tx),
    );

    if want_live {
        let live_window = Rc::new(std::cell::RefCell::new(Vec::new()));
        let reader_wake = ConsumerWake::new();
        let scheduler = scheduler::StreamScheduler::new(
            Rc::clone(&client),
            arena,
            wakes,
            variant.url.clone(),
            downloaders_tx.to_async(),
            registration_rx,
            reader_wake,
        )
        .with_live_window(live_window);
        compio::runtime::spawn(scheduler.main()).detach();
    } else {
        drop(registration_rx);
    }
    // The sender stays alive for as long as the recording runs: the scheduler
    // treats a closed registration channel as "no consumers left". The disk's
    // live channel closes when the scheduler drops the registration it adopted.

    println!(
        "record: {login} {} ({}) -> {}",
        variant.id,
        match args.mode {
            RecordMode::Both => "live+dvr",
            RecordMode::Live => "live",
            RecordMode::Dvr => "dvr",
        },
        dir.display()
    );

    if let Some(error) = disk.wait().await {
        return Err(error.into());
    }
    // An empty canonical (header only) means the broadcast ended before the
    // recording could take anything: `--mode live` against an ended playlist.
    let written = std::fs::metadata(dir.join("index")).map_or(0, |file| file.len());
    if written <= 20 {
        return Err(format!("nothing was recorded into {}", dir.display()).into());
    }
    println!("record: {login} finished");
    Ok(())
}

/// Resolve the live master, waiting for `stream-up` when the channel is offline.
#[allow(clippy::future_not_send)]
/// `None` means the caller should stop (`--list` of an offline channel is an
/// error rather than a wait).
async fn resolve_record_variants(
    cfg: &Config,
    auth_token: Option<&str>,
    client: &Rc<Client>,
    login: &str,
    args: &Record,
) -> Result<Option<(Vec<Variant>, u64)>, Box<dyn std::error::Error>> {
    if let Some((_, variants)) = debug_channel(cfg) {
        return Ok(Some((variants, 0)));
    }
    match get_variants(
        client,
        cfg.client_id.as_deref(),
        auth_token,
        login,
        cfg.low_latency,
    )
    .await
    {
        Ok(resolved) => Ok(Some(resolved)),
        Err(VariantPlaylistError::NoVariants(_)) => {
            if args.list {
                return Err(format!("{login} is not live").into());
            }
            let detail = gql::fetch_channel(client, cfg.client_id.as_deref(), login)
                .await?
                .ok_or_else(|| format!("no channel named {login}"))?;
            println!("record: waiting for {login} to go live");
            wait_for_stream_up(login, detail.id).await?;
            // StreamMetadata lags the notification; retry until the master has
            // the first variant.
            for _ in 0..30 {
                match get_variants(
                    client,
                    cfg.client_id.as_deref(),
                    auth_token,
                    login,
                    cfg.low_latency,
                )
                .await
                {
                    Ok(resolved) => return Ok(Some(resolved)),
                    Err(_) => compio::time::sleep(std::time::Duration::from_secs(1)).await,
                }
            }
            Err(format!("{login} went live but its master never resolved").into())
        }
        Err(error) => Err(error.into()),
    }
}

/// Wait for `stream-up` on the channel's `video-playback-by-id` topic. Only the
/// notification matters: the master is fetched again afterward.
#[allow(clippy::future_not_send)]
async fn wait_for_stream_up(login: &str, id: u64) -> Result<(), Box<dyn std::error::Error>> {
    let mut socket = WsStream::connect(&hermes::url()).await?;
    // Welcome first: subscribing before it is accepted would be dropped. The
    // socket's reads have no deadline (a live channel can stay quiet for a
    // whole keepalive interval), so the welcome gets its own.
    loop {
        let Some(message) = timeout(HERMES_WELCOME_TIMEOUT, socket.next_text()).await?? else {
            return Err("hermes closed before the welcome".into());
        };
        if sonic_rs::from_str::<sonic_rs::Value>(&message)?["type"].as_str() == Some("welcome") {
            break;
        }
    }
    let topic = format!("video-playback-by-id.{id}");
    let subscription = format!("record-{id}");
    let subscribe = sonic_rs::json!({
        "type": "subscribe",
        "id": subscription,
        "subscribe": {
            "id": subscription,
            "type": "pubsub",
            "pubsub": { "topic": topic },
        },
        "timestamp": "1970-01-01T00:00:00Z",
    });
    socket.send_text(&sonic_rs::to_string(&subscribe)?).await?;
    loop {
        let Some(message) = socket.next_text().await? else {
            return Err("hermes closed while waiting for stream-up".into());
        };
        let Ok(value) = sonic_rs::from_str::<sonic_rs::Value>(&message) else {
            continue;
        };
        match value["type"].as_str() {
            Some("notification") => {
                let Some(pubsub) = value["notification"]["pubsub"].as_str() else {
                    continue;
                };
                let Ok(payload) = sonic_rs::from_str::<sonic_rs::Value>(pubsub) else {
                    continue;
                };
                if payload["type"].as_str() == Some("stream-up") {
                    tracing::info!(target: "record", %login, "stream-up");
                    return Ok(());
                }
            }
            Some("subscribeResponse")
                if value["subscribeResponse"]["result"].as_str() != Some("ok") =>
            {
                return Err(format!("hermes refused the {login} subscription").into());
            }
            _ => {}
        }
    }
}

/// One line per variant: id, what it is, and the score a recording ranks by.
fn list_variants(login: &str, variants: &[Variant]) {
    let best = variant::best_variant(variants).map(|variant| variant.id.as_ref());
    println!("{login}: {} variant(s)", variants.len());
    for variant in variants {
        let resolution = variant
            .res
            .map_or_else(|| "?".to_owned(), |(w, h)| format!("{w}x{h}"));
        let score = variant.score.map_or(0, std::num::NonZeroU16::get);
        let kind = match variant.kind {
            VariantKind::Landscape => "landscape",
            VariantKind::Portrait => "portrait",
            VariantKind::Audio => "audio",
            VariantKind::ERmtp => "ertmp",
        };
        println!(
            "  {:<20} {:<10} {:<9} score {:>2}{}",
            variant.id,
            resolution,
            kind,
            score,
            if Some(variant.id.as_ref()) == best {
                "  (best)"
            } else {
                ""
            }
        );
    }
}

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
    // Loaded before the registry because the recording settings live in the
    // persisted config: a `--output` on the command line is a dev override only.
    let config_path = config::default_path(config::APP_ID);
    let mut config = config::load(&config_path);
    // The daemon has no `--auth-token`: the token lives in `auth.json`, so no
    // command line and no startup entry carries a secret.
    let auth_token = auth::load();
    // The token's identity is cached in the config so the daemon does not have
    // to ask Twitch for it on every start; a file that already has it is left
    // alone, and a failure here only means the id is unknown.
    if config.account.is_none()
        && let Some(token) = auth_token.as_deref()
    {
        match auth::validate(&client, token).await {
            Ok(account) => {
                tracing::info!(
                    target: "auth",
                    "token belongs to {} ({})",
                    account.login,
                    account.user_id
                );
                config.account = Some(account);
                if let Err(error) = config::save(&config, &config_path).await {
                    error!(%error, "failed to persist the account");
                }
            }
            Err(error) => tracing::info!(target: "auth", "could not resolve the account: {error}"),
        }
    }
    let (jobs_tx, jobs_rx) = kanal::unbounded();
    let registry = Registry::new(
        Rc::clone(&client),
        arena,
        downloaders_tx.to_async(),
        Rc::clone(&downloader_metrics),
        RegistryOptions {
            client_id: cfg.client_id.clone(),
            auth_token: auth_token.clone(),
            low_latency: cfg.low_latency,
            recording: RegistryRecording {
                output: args
                    .output
                    .clone()
                    .or_else(|| config.record_dir.as_deref().map(std::path::PathBuf::from)),
                dual_format: config.record_dual_format,
            },
            notify: notifier,
            keep_runs: args.keep_runs,
        },
        Rc::clone(&bus),
        jobs_tx.clone(),
    );
    registry.sync_channels(&config.channels);

    if let Err(error) = pid::write(config::APP_ID) {
        error!(%error, "failed to write the daemon pid file");
    }

    let (accept_tx, accept_rx) = kanal::unbounded();
    let mut trust = None;
    let mut trust_shared = siphon_shared::riffle::quic::SharedTrust::default();
    if !args.no_admin {
        let endpoint = args
            .admin
            .clone()
            .unwrap_or_else(|| siphon_shared::riffle::admin_endpoint(config::APP_ID));
        if let Err(error) = riffle::spawn_listener(endpoint, accept_tx.clone()) {
            error!(%error, "rpc: listener failed");
        }
        // The pipe is the local transport and is always bound. QUIC exists for
        // another machine, so `network.listen` empty means no UDP listener at
        // all — that emptiness is the whole remote gate.
        if !config.network.listen.is_empty() {
            let dir = siphon_shared::riffle::state_dir(config::APP_ID);
            let daemon_trust = siphon_shared::riffle::quic::Trust::new(&dir);
            // A malformed allowlist is a startup error: the alternative is a
            // daemon that trusts nobody for a reason nobody can see. The same
            // read seeds the snapshot the verifier starts with; the loop's tick
            // keeps it current from here, so the verifier never reads a file.
            let snapshot = daemon_trust.snapshot()?;
            trust_shared = siphon_shared::riffle::quic::SharedTrust::new(snapshot);
            match siphon_shared::riffle::quic::Identity::load_or_create(&dir) {
                Ok(identity) => {
                    let identity = Rc::new(identity);
                    riffle::spawn_quic_listeners(
                        &identity,
                        &daemon_trust,
                        &trust_shared,
                        &config.network.listen,
                        &accept_tx,
                    );
                    trust = Some(daemon_trust);
                }
                Err(error) => {
                    error!(%error, "quic: no pinned identity, the remote listener is off");
                }
            }
        }
    }
    let app = App::new(AppInit {
        config,
        config_path,
        client: Rc::clone(&client),
        registry,
        bus,
        accept: accept_rx.to_async(),
        jobs: jobs_rx.to_async(),
        jobs_tx,
        client_id: cfg.client_id.clone(),
        trust,
        trust_shared,
    });
    compio::runtime::spawn(app.run()).detach();

    // Nothing else owns the process now: the app loop and the downloaders run
    // detached, so park here rather than returning and tearing them down.
    std::future::pending::<Result<(), Box<dyn std::error::Error>>>().await
}

/// Opens a pairing window and approves one client by the code both sides
/// derived from its handshake.
///
/// The window is a file, because the daemon that owns the QUIC listener is a
/// separate process: this writes `pairing.json` into the state directory, the
/// running daemon's verifier reads it to admit one unknown key, the daemon
/// writes the attempt's code back, and this prints it and writes the answer.
/// Nothing else is started — no registry, no HTTP client, no UI.
///
/// Deliberately synchronous: nothing else runs in this process, so the one
/// thread it owns can block on the console. It never touches the daemon's
/// reactor, which is in the other process.
fn pair(args: &Pair) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write as _;

    use siphon_shared::riffle::quic::Identity;
    use siphon_shared::riffle::quic::PairingWindow;
    use siphon_shared::riffle::quic::Trust;

    /// How often the window file is looked at. A person is typing at one end,
    /// so this is the cheap half of a two-process handshake, not a hot loop.
    const POLL: std::time::Duration = std::time::Duration::from_millis(200);

    let config = config::load(&config::default_path(config::APP_ID));
    if config.network.listen.is_empty() {
        return Err(
            "network.listen is empty in config.json: this daemon has no remote listener to pair with"
                .into(),
        );
    }
    let state = siphon_shared::riffle::state_dir(config::APP_ID);
    std::fs::create_dir_all(&state)?;
    let trust = Trust::new(&state);
    // The same key the daemon presents: the client pins it, and printing it
    // here is what lets an operator compare what the UI remembered.
    let identity = Identity::load_or_create(&state)?;
    // The same allowlist the daemon's verifier reads, and the same rule: a
    // malformed file is an error, not "everybody is revoked".
    let peers = siphon_shared::riffle::quic::Peers::load(trust.peers())?;

    println!("daemon key: {}", identity.pin());
    for addr in &config.network.listen {
        println!("address:    {addr}");
    }
    println!("paired:     {} client(s)", peers.entries().len());
    let window = std::time::Duration::from_secs(args.timeout.max(1));
    let opened = PairingWindow::open(trust.pairing(), window)?;
    println!(
        "window open for {} s; waiting for a client to connect...",
        window.as_secs()
    );

    let deadline = std::time::Instant::now() + window;
    loop {
        std::thread::sleep(POLL);
        let Some(current) = PairingWindow::read(trust.pairing()) else {
            return Err("the pairing window was removed".into());
        };
        // Another `pair` replaced the window: this one is no longer the window
        // the daemon is looking at.
        if current.expires_ms != opened.expires_ms {
            return Err("another pairing window replaced this one".into());
        }
        let Some(attempt) = current.attempt else {
            if std::time::Instant::now() >= deadline {
                let _ = PairingWindow::close(trust.pairing());
                println!("no client connected before the window closed");
                return Ok(());
            }
            continue;
        };
        if current.decision.is_some() {
            // Already answered (this process wrote it and is about to exit).
            return Ok(());
        }

        println!();
        println!("pairing code: {}", attempt.sas);
        println!("client key:   {}", attempt.spki);
        print!("approve this client? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        let approve = matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");
        PairingWindow::decide(trust.pairing(), &attempt.sas, approve)?;
        if approve {
            println!("approved: the client finishes the handshake on its side");
        } else {
            println!("refused; the client is dropped");
            let _ = PairingWindow::close(trust.pairing());
        }
        return Ok(());
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::variant::Variant;
    use crate::variant::choose_variant;
    use crate::variant::debug_variant;

    #[test]
    fn quality_defaults_to_the_first_variant() {
        let variants = vec![
            debug_variant("mock"),
            Variant {
                id: "audio_only".into(),
                ..debug_variant("mock")
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
    fn cli_parses_the_modes() {
        let daemon = Config::try_parse_from(["app", "daemon"]).expect("daemon");
        let Command::Daemon(args) = daemon.command else {
            panic!("expected daemon");
        };
        assert!(args.admin.is_none());
        assert!(!args.no_admin);

        // `-c` is not a daemon option.
        assert!(Config::try_parse_from(["app", "daemon", "-c", "sunnys"]).is_err());
        // Nor is the token: it reaches the daemon through `auth.json` only, so
        // no startup entry ever carries a secret.
        assert!(Config::try_parse_from(["app", "daemon", "--auth-token", "t"]).is_err());
        // Nor is a bind address: RTSP is gone, so there is nothing to bind but
        // the admin endpoint.
        assert!(Config::try_parse_from(["app", "daemon", "--bind", "0.0.0.0:8554"]).is_err());

        let record = Config::try_parse_from(["app", "record", "sunnys", "--auth-token", "t"])
            .expect("record token");
        let Command::Record(args) = record.command else {
            panic!("expected record");
        };
        assert_eq!(args.auth_token.as_deref(), Some("t"));

        let pair = Config::try_parse_from(["app", "pair"]).expect("pair");
        let Command::Pair(args) = pair.command else {
            panic!("expected pair");
        };
        assert_eq!(args.timeout, 120);
        let pair = Config::try_parse_from(["app", "pair", "--timeout", "30"]).expect("pair");
        let Command::Pair(args) = pair.command else {
            panic!("expected pair");
        };
        assert_eq!(args.timeout, 30);
    }
}
