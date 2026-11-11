// Debug/stat accessors are exercised by the arena tests and future policies.
#[allow(dead_code, clippy::future_not_send)]
mod arena;
#[allow(clippy::future_not_send)] // one thread-per-core reactor, tasks are !Send
mod constants;
// Disk output is disabled for now: RTSP/RTP is the only sink. The module is kept
// (and mirrors index.hexpat) for when the DVR output comes back.
// #[allow(clippy::future_not_send)]
// mod disk;
#[allow(clippy::future_not_send)] // one thread-per-core reactor, tasks are !Send
mod downloader;
#[allow(clippy::future_not_send)] // one thread-per-core reactor, tasks are !Send
mod spool;
// compio tasks are deliberately !Send (one thread-per-core reactor).
// `dead_code`: the client exposes a slightly wider surface than this binary uses.
#[allow(dead_code, clippy::future_not_send)]
mod http;
#[allow(clippy::future_not_send)] // one thread-per-core reactor, tasks are !Send
mod playlist;
#[allow(clippy::future_not_send)] // one thread-per-core reactor, tasks are !Send
mod rtsp;
#[allow(clippy::future_not_send)] // one thread-per-core reactor, tasks are !Send
mod scheduler;
#[allow(clippy::future_not_send)] // one thread-per-core reactor, tasks are !Send
mod sha1;
#[allow(clippy::future_not_send)] // one thread-per-core reactor, tasks are !Send
mod tcp;
#[allow(clippy::future_not_send)] // one thread-per-core reactor, tasks are !Send
mod utils;
#[allow(clippy::future_not_send)] // one thread-per-core reactor, tasks are !Send
mod variant;

use std::rc::Rc;

use clap::Parser;
#[cfg(not(miri))]
use mimalloc::MiMalloc;
use tracing::error;
use tracing_subscriber::EnvFilter;

use crate::arena::ArenaSet;
use crate::http::Client;
use crate::spool::ConsumerWake;
use crate::spool::WakeSet;
use crate::variant::Variant;
use crate::variant::get_variants;

// Miri cannot execute a foreign allocator, so it runs on the system allocator.
#[cfg(not(miri))]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Parser, Debug)]
#[command(author, version, about = "Twitch Manifest Fetcher")]
struct Config {
    /// Twitch channel name to fetch
    #[arg(short, long)]
    channel: String,

    #[arg(long)]
    client_id: Option<String>,

    /// Optional OAuth authorization token
    #[arg(long)]
    auth_token: Option<String>,

    /// Enable low latency mode flags
    #[arg(short, long, default_value_t = true)]
    low_latency: bool,

    #[arg(short, long)]
    file_path: Option<String>,

    #[arg(short, long)]
    quality: Option<String>,

    #[arg(short, long)]
    debug: bool,
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
async fn main() -> Result<(), variant::VariantPlaylistError> {
    abort_on_panic();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cfg = Config::parse();

    // Debug mode replays the mock's captured segments, which are from sunnys.
    let title = if cfg.debug {
        "sunnys".to_owned()
    } else {
        cfg.channel.clone()
    };

    let client = Rc::new(Client::new());
    let arena = Rc::new(ArenaSet::new());
    let wakes = WakeSet::new();

    // Debug mode replays the mock's DVR playlist. Otherwise discovery (GQL plus
    // domain probing) and the periodic playlist refresh run in the background,
    // concurrently with the live variant and manifest fetches.
    let dvr = rtsp::vod::Dvr::new(
        cfg.debug
            .then(|| "http://127.0.0.2:8000/index-dvr.m3u8".to_owned()),
    );
    rtsp::vod::spawn(
        Rc::clone(&client),
        (!cfg.debug).then(|| cfg.channel.clone()),
        Rc::clone(&dvr),
    );

    let variants = if cfg.debug {
        vec![Variant {
            codec: None,
            frame_rate: None,
            kind: variant::VariantKind::Landscape,
            res: None,
            score: None,
            url: "http://127.0.0.2:8000/index.m3u8".into(),
            id: "1080p".into(),
        }]
    } else {
        // A manifest failure is reported and turned into a non-zero exit; it is
        // not a reason to kill the process from deep inside the request path.
        match get_variants(
            &client,
            cfg.client_id.as_deref(),
            cfg.auth_token.as_deref(),
            &cfg.channel,
            cfg.low_latency,
        )
        .await
        {
            Ok((variants, _stream_id)) => variants,
            Err(error) => {
                error!(%error, channel = %cfg.channel, "failed to resolve variants");
                return Err(error);
            }
        }
    };

    let (downloaders_tx, downloaders_rx) = kanal::bounded(32);
    let (outputs_tx, outputs_rx) = kanal::unbounded();
    // The public archive's master is fetched in parallel; picking the media
    // playlist with the same stable-variant-id as the live stream is deferred
    // until now.
    if let Some(variant) = variants.first() {
        dvr.set_live_variant(&variant.id);
    }
    let stream_url = variants.first().unwrap().url.clone();

    downloader::spawn_downloaders::<{ constants::DOWNLOAD_TASKS }>(&client, &downloaders_rx);
    // Disk output is disabled for now; RTSP is the primary sink.
    // disk::spawn_disk_output(stream_id, &wakes, &outputs_tx);

    let hub = Rc::new(rtsp::Hub::new(title));
    rtsp::spawn_server(Rc::clone(&hub));
    // The scheduler publishes its current live playlist window here. When VOD
    // playback reaches the end of the DVR timeline, the RTSP engine replays the
    // live segments in this window up to the live edge, bridging the DVR lag.
    let live_window = Rc::new(std::cell::RefCell::new(Vec::new()));
    rtsp::spawn_media(
        Rc::clone(&hub),
        Rc::clone(&client),
        Rc::clone(&dvr),
        &wakes,
        &outputs_tx,
        Rc::clone(&live_window),
    );

    let reader_wake = ConsumerWake::new();
    tcp::spawn_tcp_listener(
        Rc::clone(&wakes),
        outputs_tx.clone(),
        Rc::clone(&reader_wake),
    );

    let scheduler = scheduler::StreamScheduler::new(
        client,
        arena,
        wakes,
        stream_url,
        downloaders_tx.to_async(),
        outputs_rx,
        reader_wake,
    )
    .with_live_window(live_window);

    scheduler.main().await;

    // Dropping the runtime cancels the remaining tasks.
    drop(outputs_tx);
    drop(downloaders_rx);
    Ok(())
}
