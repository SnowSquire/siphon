mod constants;
// compio tasks are deliberately !Send (one thread-per-core reactor).
#[allow(dead_code, clippy::future_not_send)] // TODO(compio): drop dead_code once used
mod http;
mod disk;
mod downloader;
mod playlist;
mod scheduler;
mod segment;
mod tcp;
mod utils;
mod variant;

use clap::Parser;
#[cfg(not(miri))]
use mimalloc::MiMalloc;
use tracing_subscriber::EnvFilter;

use crate::disk::spawn_disk_output;
use crate::downloader::spawn_downloaders;
use crate::scheduler::spawn_scheduler;
use crate::tcp::spawn_tcp_listener;
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

    /// Throwaway: fetch a URL through the new client and print the response.
    #[arg(long)]
    probe: Option<String>,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_thread_names(true)
        .init();

    let agent = ureq::Agent::config_builder()
        .max_idle_connections_per_host(10)
        .build()
        .new_agent();

    let cfg = Config::parse();

    if let Some(url) = &cfg.probe {
        run_probe(url);
        return;
    }

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
    let (add_output, rx_outputs) = kanal::unbounded();

    let download_threads =
        spawn_downloaders::<{ constants::DOWNLOAD_THREADS }>(&agent, &rx_downloaders);
    let scheduler_thread = spawn_scheduler(
        agent,
        variants.first().unwrap().url.clone(),
        tx_downloaders,
        rx_outputs,
    );

    let disk_thread = spawn_disk_output(&add_output, stream_id);
    let tcp_listener_thread = spawn_tcp_listener(add_output);

    let _ = scheduler_thread.join();
    let _ = tcp_listener_thread.join();
    let _ = disk_thread.join();
    for thread in download_threads {
        let _ = thread.join();
    }
}

fn run_probe(url: &str) {
    let client = http::Client::new();
    let runtime = compio::runtime::Runtime::new().expect("compio runtime");
    runtime.block_on(async {
        let mut response = match client.get(url).await {
            Ok(response) => response,
            Err(error) => {
                eprintln!("request failed: {error}");
                return;
            }
        };
        println!("{}", response.status);
        for (name, value) in &response.headers {
            println!("{name}: {}", value.to_str().unwrap_or("<binary>"));
        }
        match response.body.read_to_end(128 * 1024 * 1024).await {
            Ok(body) => {
                println!("--- {} bytes", body.len());
                let _ = std::io::Write::write_all(&mut std::io::stdout(), &body);
            }
            Err(error) => eprintln!("body failed: {error}"),
        }
    });
}
