#![allow(unused)]
use bytes::Buf;
use mimalloc::MiMalloc;

use clap::Parser;

use sonic_rs::{PointerTree, get, get_from_bytes, get_many, pointer};
use std::borrow::Cow;
use std::fmt::format;
use std::fs::File;
use std::io::{self, IoSlice, Seek, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tracing::subscriber::NoSubscriber;
use tracing::{Level, debug, debug_span, dispatcher, error, info, trace};
use tracing_subscriber::EnvFilter;
use ureq::ResponseExt;
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
use std::{array, thread};
mod downloader;
mod file;
mod helpers;
mod hls_parser;
mod scheduler;
mod stream;
mod stream_reader;
mod stream_writer;
use base64::engine::general_purpose::STANDARD;
use rand::{RngExt, distr::Alphanumeric};

use ureq::{Agent, http::header::AUTHORIZATION};

mod constants;
use crate::downloader::download_worker;
use crate::file::run_mpegts_writer;
use crate::helpers::{fetch_master_manifest, select_track};
use crate::scheduler::{Segment, SegmentMessage, StreamScheduler};

use crate::constants::{
    DEFAULT_CLIENT_ID, DEFAULT_PORT, DOWNLOAD_THREADS, PLAYER_VERSION, TWITCH_GQL_ENDPOINT,
    TWITCH_HLS_BASE, TWITCH_OAUTH_ENDPOINT,
};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("failed to get playback access token: {0}")]
    AccessTokenRequest(#[source] ureq::Error),

    #[error("failed to parse access token response: {0}")]
    AccessTokenParse(#[source] ureq::Error),

    #[error("token value is null or missing in response")]
    TokenMissing,

    #[error("signature is missing in response")]
    SignatureMissing,

    #[error("token value is not valid JSON: {0}")]
    TokenJsonInvalid(#[from] sonic_rs::Error),

    #[error("broski")]
    IDK,
    #[error("channel field is missing from token")]
    ChannelMissing,

    #[error("failed to fetch HLS manifest: {source}, {url}")]
    ManifestRequest {
        #[source]
        source: ureq::Error,
        url: String,
    },

    #[error("failed to read manifest body: {0}")]
    ManifestRead(#[source] ureq::Error),
}

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
}

// impl Default for Config {
//     fn default() -> Self {
//         Self {
//             auth_token: Option::None,
//             channel: String::default(),
//             client_id: None,
//             low_latency: true,
//             file_path: Option::None,
//         }
//     }
// }

fn choose_client_id<'a>(
    agent: &Agent,
    client_id: Option<&'a str>,
    auth_token: Option<&str>,
) -> Result<Cow<'a, str>, Box<dyn std::error::Error>> {
    match (client_id, auth_token) {
        (None, None) => Ok(DEFAULT_CLIENT_ID.into()),
        (Some(client_id), _) => Ok(client_id.into()),
        (None, Some(auth_token)) => {
            let response = agent
                .get(TWITCH_OAUTH_ENDPOINT)
                .header(AUTHORIZATION, format!("OAuth {auth_token}"))
                .call()?
                .into_body()
                .read_to_vec()?;
            Ok(sonic_rs::get(response.as_slice(), pointer!["client_id"])?
                .to_string()
                .into())
        }
    }
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

    let track_url = {
        let client_id =
            choose_client_id(&agent, cfg.client_id.as_deref(), cfg.auth_token.as_deref()).unwrap();

        let master_manifest = fetch_master_manifest(
            &agent,
            &cfg.channel,
            &client_id,
            cfg.auth_token.as_deref(),
            cfg.low_latency,
        )
        .unwrap();

        select_track(&master_manifest, cfg.quality.as_deref()).unwrap()
    };

    let (tx_downloaders, rx_downloaders) = kanal::unbounded();
    let (add_readers, rx_readers) = kanal::unbounded();

    let download_threads: [JoinHandle<()>; 8] = spawn_downloaders::<8>(&agent, rx_downloaders);
    let scheduler_thread: JoinHandle<()> =
        spawn_scheduler(agent.clone(), track_url, tx_downloaders, rx_readers);
    let file_thread: JoinHandle<()> = spawn_mpegts_writer(&add_readers);
    let tcp_listener_thread: JoinHandle<()> = spawn_tcp_listener(add_readers.clone(), start);

    let _ = scheduler_thread.join();
    let _ = tcp_listener_thread.join();
    let _ = file_thread.join();
    for thread in download_threads {
        let _ = thread.join();
    }
}

fn spawn_tcp_listener(
    add_readers: kanal::Sender<kanal::Sender<SegmentMessage>>,
    start: Instant,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name(format!("tcp listener"))
        .spawn(move || {
            let mut client_threads = Vec::new();
            const ADDRS: [std::net::SocketAddr; 4] = [
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), DEFAULT_PORT),
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PORT),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            ];
            let listener = TcpListener::bind(&ADDRS[..]).unwrap();

            println!("{}", listener.local_addr().unwrap().to_string());

            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let start = start.clone();
                        let (reader_rx, handle) = spawn_tcp_client_thread(stream, start);
                        _ = add_readers.send(reader_rx);
                        client_threads.push(handle);
                    }
                    Err(e) => eprintln!("Connection failed: {}", e),
                }
            }
        })
        .expect("failed to spawn listener thread")
}

fn spawn_tcp_client_thread(
    stream: std::net::TcpStream,
    start: Instant,
) -> (kanal::Sender<SegmentMessage>, JoinHandle<()>) {
    //TODO: probably make this bounded and drop slow connections
    let (reader_rx, reader_tx): (
        kanal::Sender<SegmentMessage>,
        kanal::Receiver<SegmentMessage>,
    ) = kanal::unbounded();
    let handle = thread::Builder::new()
        .name(format!(
            "tcp sender {0}",
            stream.peer_addr().unwrap().to_string()
        ))
        .spawn(move || {
            let mut stream = stream;
            stream.set_nodelay(true);

            let mut last_sequence_num = 0;
            for msg in reader_tx {
                match msg {
                    SegmentMessage::Init { mut reader } => {
                        while reader.wait_for_data() {
                            let buf = reader.chunk();

                            let written = match stream.write(buf) {
                                Ok(w) => w,
                                Err(e) => {
                                    eprintln!("Write error: {e}");
                                    return;
                                }
                            };

                            if written == 0 {
                                break;
                            }

                            reader.advance(written);
                        }
                    }
                    SegmentMessage::Segment(Segment {
                        mut reader,
                        sequence_num,
                        ..
                    })
                    | SegmentMessage::Prefetch {
                        mut reader,
                        expected_sequence: sequence_num,
                    } => {
                        if (last_sequence_num >= sequence_num) {
                            continue;
                        }
                        last_sequence_num = sequence_num;
                        while reader.wait_for_data() {
                            let buf = reader.chunk();

                            let written = match stream.write(buf) {
                                Ok(w) => w,
                                Err(e) => {
                                    eprintln!("Write error: {e}");
                                    return;
                                }
                            };

                            if written == 0 {
                                break;
                            }

                            reader.advance(written);
                        }
                    }
                    SegmentMessage::Discontinuity() => last_sequence_num = 0,
                }
            }
        })
        .expect("failed to spawn tcp client thread");
    // if this channel closes should lowkey just restart the whole process
    (reader_rx, handle)
}

#[derive(Debug, thiserror::Error)]
pub enum FileWriterError {
    #[error("failed to register MPEG-TS reader")]
    RegisterReader {
        #[source]
        source: kanal::SendError,
    },

    #[error("failed to spawn MPEG-TS writer thread")]
    SpawnThread {
        #[source]
        source: io::Error,
    },

    #[error("failed to create manifest directory")]
    CreateManifestDirectory {
        #[source]
        source: io::Error,
    },

    #[error("failed to create MPEG-TS file at {path}")]
    CreateTsFile {
        path: String,
        #[source]
        source: io::Error,
    },

    #[error("failed to create segment-boundary file at {path}")]
    CreateBoundariesFile {
        path: String,
        #[source]
        source: io::Error,
    },

    #[error("failed to write segment-boundary file header")]
    WriteBoundariesHeader {
        #[source]
        source: io::Error,
    },

    #[error("failed to get MPEG-TS file position")]
    GetTsFilePosition {
        #[source]
        source: io::Error,
    },

    #[error("failed to write MPEG-TS data")]
    WriteTsData {
        #[source]
        source: io::Error,
    },

    #[error("failed to flush MPEG-TS file")]
    FlushTsFile {
        #[source]
        source: io::Error,
    },

    #[error("failed to write segment boundary")]
    WriteBoundary {
        #[source]
        source: io::Error,
    },

    #[error("failed to flush segment-boundary file")]
    FlushBoundariesFile {
        #[source]
        source: io::Error,
    },
}

pub fn spawn_mpegts_writer(
    add_readers: &kanal::Sender<kanal::Sender<SegmentMessage>>,
) -> JoinHandle<()> {
    let (reader_rx, reader_tx): (
        kanal::Sender<SegmentMessage>,
        kanal::Receiver<SegmentMessage>,
    ) = kanal::unbounded();

    add_readers
        .send(reader_rx)
        .map_err(|source| FileWriterError::RegisterReader { source })
        .expect("fuuuuuuuuuu");

    let handle = thread::Builder::new()
        .name("file writer".to_owned())
        .spawn(move || {
            if let Err(error) = run_mpegts_writer(reader_tx) {
                error!(?error, "MPEG-TS writer stopped");
            }
        })
        .expect("failed to spawn file writer");

    handle
}

fn spawn_downloaders<const N: usize>(
    agent: &Agent,
    rx: kanal::Receiver<downloader::ToDownloader>,
) -> [JoinHandle<()>; N] {
    array::from_fn(|index| {
        let rx = rx.clone();
        let agent = agent.clone();
        std::thread::Builder::new()
            .name(format!("downloader {index}"))
            .spawn(move || download_worker(rx, agent))
            .expect("failed to spawn downloader thread")
    })
}

fn spawn_scheduler(
    agent: Agent,
    track_url: Box<str>,
    tx: kanal::Sender<downloader::ToDownloader>,
    srx: kanal::Receiver<kanal::Sender<SegmentMessage>>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name(format!("scheduler"))
        .spawn(move || StreamScheduler::new(agent, track_url, tx, srx).main())
        .expect("failed to spawn scheduler")
}
