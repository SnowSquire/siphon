#![allow(unused_variables)]
#![allow(dead_code)]
#![deny(unused_imports)]
mod constants;
mod disk;
mod downloader;
mod scheduler;
mod segment;
mod tcp;
mod variant;

use clap::Parser;
use mimalloc::MiMalloc;

use std::thread::JoinHandle;
use std::time::Instant;

use tracing_subscriber::EnvFilter;

use crate::disk::spawn_disk_writer;
use crate::downloader::spawn_downloaders;
use crate::scheduler::spawn_scheduler;
use crate::tcp::spawn_tcp_listener;
use crate::variant::{Variant, get_variants};

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

fn main() {
    let start = Instant::now();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_thread_names(true)
        .init();

    let agent = ureq::Agent::config_builder()
        .max_idle_connections_per_host(10)
        .build()
        .new_agent();

    let cfg = Config::parse();

    let (variants, stream_id) = if cfg.debug {
        (
            vec![Variant {
                codec: None,
                frame_rate: None,
                kind: variant::VariantKind::Landscape,
                res: None,
                score: None,
                url: "http://127.0.0.2:8000/index.m3u8".into(),
                variant_id: "1080p".into(),
            }],
            rand::random::<u64>(),
        )
    } else {
        get_variants(
            &agent,
            cfg.client_id.as_deref(),
            cfg.auth_token.as_deref(),
            &cfg.channel,
            cfg.low_latency,
        )
    };

    let (tx_downloaders, rx_downloaders) = kanal::unbounded();
    let (add_readers, rx_readers) = kanal::unbounded();

    let download_threads: [JoinHandle<()>; 8] = spawn_downloaders::<8>(&agent, rx_downloaders);
    let scheduler_thread: JoinHandle<()> = spawn_scheduler(
        agent.clone(),
        variants.first().unwrap().url.clone(),
        tx_downloaders,
        rx_readers,
    );

    let disk_thread: JoinHandle<()> = spawn_disk_writer(&add_readers, stream_id);
    let tcp_listener_thread: JoinHandle<()> = spawn_tcp_listener(add_readers.clone(), start);

    let _ = scheduler_thread.join();
    let _ = tcp_listener_thread.join();
    let _ = disk_thread.join();
    for thread in download_threads {
        let _ = thread.join();
    }
}
