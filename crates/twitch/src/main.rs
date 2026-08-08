#![allow(unused)]
use bytes::Buf;
use mimalloc::MiMalloc;

use clap::Parser;
use serde::Deserialize;
use sonic_rs::{PointerTree, get, get_from_bytes, get_many, pointer};
use std::borrow::Cow;
use std::io::{IoSlice, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::thread::JoinHandle;
use tracing::subscriber::NoSubscriber;
use tracing::{Level, debug, dispatcher, error, info, trace};
use tracing_subscriber::EnvFilter;
use ureq::ResponseExt;
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
use std::{array, thread};

mod downloader;
mod scheduler;
mod stream;
mod stream_reader;
mod stream_writer;

use base64::engine::general_purpose::STANDARD;
use rand::{RngExt, distr::Alphanumeric};
use serde_json::Value;

use ureq::{Agent, http::header::AUTHORIZATION};

use std::ops::Range;
mod constants;
use crate::downloader::download_worker;
use crate::scheduler::{SegmentMessage, StreamScheduler};

use crate::constants::{
    DEFAULT_CLIENT_ID, DOWNLOAD_THREADS, PLAYER_VERSION, TWITCH_GQL_ENDPOINT, TWITCH_HLS_BASE,
    TWITCH_OAUTH_ENDPOINT,
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
    TokenJsonInvalid(#[from] serde_json::Error),

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

fn fetch_master_manifest(
    agent: &Agent,
    channel: &str,
    client_id: &str,
    auth_token: Option<&str>,
    low_latency: bool,
) -> Result<String, ManifestError> {
    let req = agent
        .post(TWITCH_GQL_ENDPOINT)
        .content_type("text/plain;charset=UTF-8")
        .header("Device-ID", random_id())
        .header("Client-ID", client_id);

    let req = if let Some(auth_token) = auth_token {
        req.header("Authorization", format!("OAuth {auth_token}"))
    } else {
        req
    };

    let res = req
        .send_json(sonic_rs::json!({
            "extensions": {
                "persistedQuery": {
                    "sha256Hash": "ed230aa1e33e07eebb8928504583da78a5173989fadfb1ac94be06a04f3cdbe9",
                    "version": 1
                }
            },
            "operationName": "PlaybackAccessToken",
            "variables": {
                "isLive": true,
                "isVod": false,
                "login": channel,
                "playerType": "site",
                "platform": "site",
                "vodID": ""
            }
        }))
        .map_err(ManifestError::AccessTokenRequest)?;

    let res = res
        .into_body()
        .read_json::<serde_json::Value>()
        .map_err(ManifestError::AccessTokenParse)?;

    let token_value_str = res["data"]["streamPlaybackAccessToken"]["value"]
        .as_str()
        .ok_or(ManifestError::TokenMissing)?;

    let signature = res["data"]["streamPlaybackAccessToken"]["signature"]
        .as_str()
        .ok_or(ManifestError::SignatureMissing)?;

    let access_token_json = serde_json::from_str::<serde_json::Value>(token_value_str)?;

    let token_channel = access_token_json["channel"]
        .as_str()
        .ok_or(ManifestError::ChannelMissing)?;

    let acmb = sonic_rs::json!({
        "AppVersion": "785b2bb2-310f-427c-b4e1-52f2735b013f",
        "ClientApp": "twilight",
        "URL": format!("https://www.twitch.tv/{channel}"),
    });

    let acmb_str = sonic_rs::to_string(&acmb).map_err(|_| ManifestError::ChannelMissing)?;
    let acmb_b64 = base64::Engine::encode(&STANDARD, acmb_str.as_bytes());

    let p_value = rand::random_range(0..9_999_999);
    // info!(%acmb_str, %token_value_str, %signature);
    let manifest = agent
        .get(format!("{TWITCH_HLS_BASE}/{channel}.m3u8"))
        .query_pairs([
            ("acmb", acmb_b64.as_str()),
            ("allow_source", "true"),
            // ("allow_audio_only", "true"),
            ("browser_family", "chrome"),
            ("browser_version", "151.0"),
            ("cdm", "wv"),
            ("enable_score", "true"),
            (
                "fast_bread",
                low_latency.then_some("true").unwrap_or("false"),
            ),
            ("include_unavailable", "true"),
            ("lang", "en"),
            ("os_name", "Windows"),
            ("os_version", "NT 10.0"),
            ("p", &p_value.to_string()),
            ("platform", "web"),
            ("play_session_id", &random_id()),
            ("player_backend", "mediaplayer"),
            ("player_version", PLAYER_VERSION),
            ("playlist_include_framerate", "true"),
            ("reassignments_supported", "true"),
            ("sig", signature),
            ("supported_codecs", "av1,h265,h264"),
            ("token", token_value_str),
            ("transcode_mode", "cbr_v1"),
            // ("multigroup_video", "false"),
            // ("warp", low_latency.then_some("true").unwrap_or("false")),
        ]);

    // let uri = manifest
    //     .uri_ref()
    //     .unwrap()
    //     .path_and_query()
    //     .unwrap()
    //     .to_string();

    let manifest = manifest
        .call()
        .map_err(|e| ManifestError::ManifestRequest {
            source: e,
            url: "".into(),
        })?;
    info!("{}", manifest.get_uri().to_string());

    manifest
        .into_body()
        .read_to_string()
        .map_err(ManifestError::ManifestRead)
}
#[derive(Parser, Debug)]
#[command(author, version, about = "Twitch Manifest Fetcher", long_about = None)]
struct Config {
    /// Twitch channel name to fetch
    #[arg(short, long)]
    channel: String,

    /// Optional Twitch Client-ID (uses default web client ID if missing)
    #[arg(long)]
    client_id: Option<String>,

    /// Optional OAuth authorization token
    #[arg(short, long)]
    auth_token: Option<String>,

    /// Enable low latency mode flags
    #[arg(short, long)]
    low_latency: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auth_token: Option::default(),
            channel: String::default(),
            client_id: None,
            low_latency: true,
        }
    }
}

fn choose_client_id<'a>(
    agent: &Agent,
    client_id: Option<&'a str>,
    auth_token: Option<&str>,
) -> Result<Cow<'a, str>, Box<dyn std::error::Error>> {
    if let Some(client_id) = client_id {
        Ok(client_id.into())
    } else if let Some(auth_token) = auth_token {
        let response = agent
            .get(TWITCH_OAUTH_ENDPOINT)
            .header(AUTHORIZATION, format!("OAuth {auth_token}"))
            .call()?
            .into_body()
            .read_json::<Value>()?;

        Ok(response["client_id"].to_string().into())
    } else {
        Ok(DEFAULT_CLIENT_ID.into())
    }
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

    info!(ll = %cfg.low_latency);

    // possibly broken
    let track_url = {
        let client_id =
            choose_client_id(&agent, cfg.client_id.as_deref(), cfg.auth_token.as_deref())
                .unwrap_or(DEFAULT_CLIENT_ID.into());

        info!(%client_id, "got client id");
        let master_manifest =
            fetch_master_manifest(&agent, &cfg.channel, &client_id, None, cfg.low_latency).unwrap();

        select_track(master_manifest, None)
    }
    .unwrap();

    let (tx, rx) = kanal::unbounded();
    let (add_readers, srx) = kanal::unbounded();
    let download_threads: [JoinHandle<()>; DOWNLOAD_THREADS] = array::from_fn(|index| {
        let rx = rx.clone();
        let agent = agent.clone();
        thread::Builder::new()
            .name(format!("downloader {index}"))
            .spawn(move || download_worker(rx, agent))
            .expect("failed to spawn downloader thread")
    });

    let scheduler_thread = {
        let agent = agent.clone();
        thread::Builder::new()
            .name(format!("scheduler"))
            .spawn(move || StreamScheduler::new(agent, track_url, tx, srx).main())
    }
    .expect("failed to spawn scheduler");

    let tcp_listener_thread = {
        let add_readers_tx = add_readers.clone();
        thread::Builder::new()
            .name(format!("tcp listener"))
            .spawn(move || {
                let mut client_threads = Vec::new();
                let addrs = [
                    SocketAddr::from((Ipv6Addr::LOCALHOST, 0)),
                    SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                ];
                let listener = TcpListener::bind(&addrs[..]).unwrap();

                println!("{}", listener.local_addr().unwrap().to_string());

                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => {
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
                                    for msg in reader_tx {
                                        match msg {
                                            SegmentMessage::Init { mut reader }
                                            | SegmentMessage::Segment { mut reader, .. } => {
                                                while reader.has_remaining() {
                                                    let mut slices = [{ IoSlice::new(&[]) }; 16];
                                                    let count = reader.chunks_vectored(&mut slices);
                                                    if count == 0 {
                                                        break;
                                                    }
                                                    let written = match stream
                                                        .write_vectored(&slices[..count])
                                                    {
                                                        Ok(w) => w,
                                                        Err(e) => {
                                                            eprintln!("Write error: {e}");
                                                            return;
                                                        }
                                                    };
                                                    if written == 0 {
                                                        break;
                                                    }
                                                    // slices goes out of scope here (it's declared inside the while loop)
                                                    reader.advance(written);
                                                    reader.wait_for_data();
                                                }
                                            }
                                        }
                                    }
                                });
                            // if this channel closes should lowkey just restart the whole process
                            _ = add_readers_tx.send(reader_rx);
                            client_threads.push(handle);
                        }
                        Err(e) => eprintln!("Connection failed: {}", e),
                    }
                }
            })
            .expect("failed to spawn listener thread")
    };
    let _ = scheduler_thread.join();
    let _ = tcp_listener_thread.join();
    for thread in download_threads {
        let _ = thread.join();
    }

    println!("Hello, world!");
}
// track not the best naming but "stream" is already used in this project to mean a stream of bytes (from an http source), and this combo has the best overall fit i can thing of now
pub fn select_track(buffer: String, preferred_prefix: Option<&str>) -> Option<Box<str>> {
    let mut media: Option<&str> = None;
    let mut stream_inf: Option<&str> = None;

    let mut best_url_range: Option<Range<usize>> = None;
    let mut best_area: u32 = 0;

    // Helper closure to dynamically find the index range of a slice within `buffer`
    let get_range = |sub: &str| -> Range<usize> {
        let start = sub.as_ptr() as usize - buffer.as_ptr() as usize;
        start..(start + sub.len())
    };

    for line in buffer.lines() {
        let line = line.trim();
        if line.starts_with("#EXT-X-MEDIA") {
            media = Some(line);
        } else if line.starts_with("#EXT-X-STREAM-INF") {
            stream_inf = Some(line);
        } else if line.starts_with("http") && stream_inf.is_some() {
            if let Some(inf) = stream_inf {
                if let Some(name) = parse_name(media, inf) {
                    let resolution = parse_resolution(inf);

                    // If preferred prefix matches, allocate and return immediately
                    if let Some(prefix) = preferred_prefix {
                        if name.starts_with(prefix) {
                            return Some(buffer[get_range(line)].into());
                        }
                    }

                    // Calculate area to find the highest resolution
                    let area = resolution.map(|(w, h)| w as u32 * h as u32).unwrap_or(0);

                    if best_url_range.is_none() || area > best_area {
                        best_url_range = Some(get_range(line));
                        best_area = area;
                    }
                }
            }
        }
    }

    // Allocate and return only the final selected URL
    best_url_range.map(|range| buffer[range].into())
}

fn parse_name<'a>(media: Option<&'a str>, stream_inf: &'a str) -> Option<&'a str> {
    media
        .map_or_else(
            || stream_inf.split_once("IVS-NAME=\""),
            |media| media.split_once("NAME=\""),
        )
        .map(|s| s.1.split('"'))
        .and_then(|mut s| s.next())
        .map(|s| s.strip_suffix(" (source)").unwrap_or(s))
}

fn parse_resolution(stream_inf: &str) -> Option<(u16, u16)> {
    stream_inf
        .split_once("RESOLUTION=")
        .and_then(|(_, tail)| tail.split_once(','))
        .and_then(|(head, _)| head.split_once('x'))
        .and_then(|(width, height)| {
            if let (Ok(width), Ok(height)) = (width.parse(), height.parse()) {
                Some((width, height))
            } else {
                None
            }
        })
}
fn random_id() -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}
