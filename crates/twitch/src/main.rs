// Debug/stat accessors are exercised by the arena tests and future policies.
#[allow(dead_code, clippy::future_not_send)]
mod arena;
#[allow(clippy::future_not_send)] // one thread-per-core reactor, tasks are !Send
mod constants;
#[allow(clippy::future_not_send)] // one thread-per-core reactor, tasks are !Send
mod disk;
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
mod scheduler;
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

#[compio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cfg = Config::parse();

    let client = Rc::new(Client::new());
    let arena = Rc::new(ArenaSet::new());
    let wakes = WakeSet::new();

    let (variants, stream_id) = if cfg.debug {
        (
            vec![Variant {
                codec: None,
                frame_rate: None,
                kind: variant::VariantKind::Landscape,
                res: None,
                score: None,
                url: "http://127.0.0.2:8000/index.m3u8".into(),
                id: "1080p".into(),
            }],
            // Stable id so a debug restart resumes the same files.
            0,
        )
    } else {
        get_variants(
            &client,
            cfg.client_id.as_deref(),
            cfg.auth_token.as_deref(),
            &cfg.channel,
            cfg.low_latency,
        )
        .await
    };

    let (downloaders_tx, downloaders_rx) = kanal::bounded(32);
    let (outputs_tx, outputs_rx) = kanal::unbounded();

    downloader::spawn_downloaders::<{ constants::DOWNLOAD_TASKS }>(&client, &downloaders_rx);
    disk::spawn_disk_output(stream_id, &wakes, &outputs_tx);
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
        variants.first().unwrap().url.clone(),
        downloaders_tx.to_async(),
        outputs_rx,
        reader_wake,
    );

    scheduler.main().await;

    // Dropping the runtime cancels the remaining tasks.
    drop(outputs_tx);
    drop(downloaders_rx);
}
