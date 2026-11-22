//! Admin connections as plain I/O, driven by the daemon's loop.
//!
//! There is no per-connection task and no per-connection queue: the loop owns
//! every connection's decoder and outbound buffer, arms one read and one write
//! future at a time, and encodes each message once per connection. That is what
//! makes a snapshot a synchronous function call rather than a round trip.
//!
//! A listener task does one thing — accept — and hands the raw stream to the
//! loop through a kanal queue.

use std::io;
use std::rc::Rc;

use compio::io::AsyncRead;
use compio::io::AsyncWriteExt as _;
use siphon_shared::rpc::FrameDecoder;
use siphon_shared::rpc::ServerMessage;
use siphon_shared::rpc::codec;
use tracing::error;
use tracing::info;
use tracing::warn;

/// The admin transport: a socket on Unix, a named pipe on Windows.
#[cfg(unix)]
pub type AdminStream = compio::net::UnixStream;
#[cfg(windows)]
pub type AdminStream = compio::fs::named_pipe::NamedPipeServer;

/// Bytes one read asks for. Control traffic, not a hot loop.
const READ_CHUNK: usize = 16 * 1024;

/// A connection whose outbound buffer exceeds this is dropped; the UI reconnects
/// and resyncs from a snapshot.
pub const MAX_OUT: usize = 1 << 20;

/// One admin connection's protocol state. Owned by the daemon loop.
pub struct Conn {
    pub stream: Rc<AdminStream>,
    pub decoder: FrameDecoder,
    /// Encoded frames waiting to be written.
    pub out: Vec<u8>,
    /// The buffer a completed write hands back, emptied for reuse.
    pub spare: Vec<u8>,
    /// The handshake completed and the snapshot was queued.
    pub ready: bool,
    /// A write future owns the buffer right now.
    pub writing: bool,
}

impl Conn {
    #[must_use]
    pub fn new(stream: AdminStream) -> Self {
        Self {
            stream: Rc::new(stream),
            decoder: FrameDecoder::new(),
            out: Vec::new(),
            spare: Vec::new(),
            ready: false,
            writing: false,
        }
    }

    /// Queues one frame. `false` means the connection can no longer be used.
    pub fn push(&mut self, message: &ServerMessage) -> bool {
        if codec::encode(message, &mut self.out).is_err() {
            return false;
        }
        self.out.len() <= MAX_OUT
    }
}

/// What one arming of a connection future produces.
pub enum ConnEvent {
    Accepted(io::Result<AdminStream>),
    Read {
        conn: usize,
        result: io::Result<usize>,
        buf: Vec<u8>,
    },
    Write {
        conn: usize,
        result: io::Result<()>,
        buf: Vec<u8>,
    },
}

/// Resolves with the next accepted connection.
pub async fn accept(queue: &kanal::AsyncReceiver<AdminStream>) -> ConnEvent {
    let queue = queue.clone();
    ConnEvent::Accepted(
        queue
            .recv()
            .await
            .map_err(|error| io::Error::other(error.to_string())),
    )
}

/// Reads up to [`READ_CHUNK`] bytes. The buffer comes back for reuse.
pub async fn read(conn: usize, stream: Rc<AdminStream>) -> ConnEvent {
    let buf = Vec::with_capacity(READ_CHUNK);
    let mut source: &AdminStream = &stream;
    let read = source.read(buf).await;
    ConnEvent::Read {
        conn,
        result: read.0,
        buf: read.1,
    }
}

/// Writes the whole buffer, which comes back for reuse.
pub async fn write(conn: usize, stream: Rc<AdminStream>, buf: Vec<u8>) -> ConnEvent {
    let mut sink: &AdminStream = &stream;
    let written = sink.write_all(buf).await;
    ConnEvent::Write {
        conn,
        result: written.0,
        buf: written.1,
    }
}

/// Serves the admin endpoint forever, handing each accepted stream to the loop.
///
/// The bind happens inside the spawned task, so the caller never blocks on it;
/// a failure is logged and leaves the daemon without an admin endpoint.
#[cfg(unix)]
pub fn spawn_listener(endpoint: String, queue: kanal::Sender<AdminStream>) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    use compio::net::UnixListener;

    if siphon_shared::rpc::probe(&endpoint) {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "admin socket is already in use",
        ));
    }
    let _ = std::fs::remove_file(&endpoint);
    compio::runtime::spawn(async move {
        let listener = match UnixListener::bind(&endpoint).await {
            Ok(listener) => listener,
            Err(error) => {
                error!(%error, "rpc: bind failed");
                return;
            }
        };
        if let Err(error) =
            std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600))
        {
            warn!(%error, "rpc: could not restrict the admin socket");
        }
        info!(%endpoint, "rpc: listening");
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    if queue.try_send(stream).is_err() {
                        warn!("rpc: accepting faster than the loop drains");
                    }
                }
                Err(error) => warn!(%error, "rpc: accept failed"),
            }
        }
    })
    .detach();
    Ok(())
}

/// Serves the admin endpoint forever, handing each accepted pipe to the loop.
#[cfg(windows)]
pub fn spawn_listener(endpoint: String, queue: kanal::Sender<AdminStream>) -> io::Result<()> {
    use compio::fs::named_pipe::ServerOptions;

    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&endpoint)?;
    compio::runtime::spawn(async move {
        info!(%endpoint, "rpc: listening");
        loop {
            if let Err(error) = server.connect().await {
                warn!(%error, "rpc: pipe connect failed");
            }
            // Create the next instance before handing this one over, so a
            // client never finds the pipe briefly absent.
            let next = match ServerOptions::new().create(&endpoint) {
                Ok(next) => next,
                Err(error) => {
                    error!(%error, "rpc: could not create the next pipe instance");
                    return;
                }
            };
            if queue.try_send(server).is_err() {
                warn!("rpc: accepting faster than the loop drains");
            }
            server = next;
        }
    })
    .detach();
    Ok(())
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use siphon_shared::rpc::Channel;
    use siphon_shared::rpc::Client;
    use siphon_shared::rpc::ClientEvent;
    use siphon_shared::rpc::Config;
    use siphon_shared::rpc::Request;
    use siphon_shared::rpc::Response;
    use siphon_shared::rpc::Update;

    use super::*;
    use crate::app::App;
    use crate::app::AppInit;
    use crate::arena::ArenaSet;
    use crate::bus::Bus;
    use crate::http::Client as HttpClient;
    use crate::metrics::DownloaderMetrics;
    use crate::registry::Options as RegistryOptions;
    use crate::registry::Registry;
    use crate::variant::debug_portrait_variant;
    use crate::variant::debug_variant;

    fn scratch_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("siphon-rpc-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn next_config(events: &kanal::AsyncReceiver<ClientEvent>) {
        for _ in 0..50 {
            let event = compio::time::timeout(Duration::from_millis(200), events.recv())
                .await
                .ok()
                .and_then(Result::ok);
            if let Some(ClientEvent::Update(update)) = event
                && matches!(*update, Update::Config(_))
            {
                return;
            }
        }
        panic!("config update did not arrive");
    }

    async fn next_pipeline(
        events: &kanal::AsyncReceiver<ClientEvent>,
        channel: u64,
        removed: bool,
    ) {
        for _ in 0..50 {
            let event = compio::time::timeout(Duration::from_millis(200), events.recv())
                .await
                .ok()
                .and_then(Result::ok);
            let Some(event) = event else {
                continue;
            };
            let ClientEvent::Update(update) = event else {
                continue;
            };
            match *update {
                Update::PipelineUpsert(view) if !removed && view.key.channel == channel => return,
                Update::PipelineRemove(key) if removed && key.channel == channel => return,
                _ => {}
            }
        }
        panic!("pipeline update for channel {channel} did not arrive");
    }

    #[test]
    fn record_and_stop_round_trip_over_the_admin_socket() {
        let dir = scratch_dir();
        let runtime = compio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let http = Rc::new(HttpClient::new());
            let (downloaders_tx, downloaders_rx) = kanal::bounded(32);
            let downloader_metrics = Rc::new(DownloaderMetrics::new(32));
            crate::downloader::spawn_downloaders::<1>(&http, &downloaders_rx, &downloader_metrics);

            let bus = Bus::new();
            let registry = Rc::new(Registry::new(
                Rc::clone(&http),
                Rc::new(ArenaSet::new()),
                downloaders_tx.to_async(),
                downloader_metrics,
                RegistryOptions {
                    client_id: None,
                    auth_token: None,
                    low_latency: true,
                    recording: crate::registry::Recording {
                        output: Some(dir.clone()),
                        dual_format: false,
                    },
                    notify: None,
                    keep_runs: false,
                },
                Rc::clone(&bus),
            ));
            registry.seed_channel(
                "sunnys",
                7,
                false,
                vec![debug_variant("mock"), debug_portrait_variant("mock")],
                42,
            );
            registry.watch_finished();

            let (accept_tx, accept_rx) = kanal::unbounded();
            let endpoint = siphon_shared::rpc::admin_endpoint(&format!(
                "test-{}-{}",
                std::process::id(),
                dir.file_name().unwrap().to_string_lossy()
            ));
            spawn_listener(endpoint.clone(), accept_tx).unwrap();

            let (jobs_tx, jobs_rx) = kanal::unbounded();
            let daemon = App::new(AppInit {
                config: Config {
                    channels: vec![Channel {
                        login: "sunnys".to_owned(),
                        id: 7,
                        display_name: Some("Sunnys".to_owned()),
                        record: false,
                        ..Channel::default()
                    }],
                    ..Config::default()
                },
                config_path: dir.join("config.json"),
                client: Rc::clone(&http),
                registry,
                bus,
                accept: accept_rx.to_async(),
                jobs: jobs_rx.to_async(),
                jobs_tx,
                client_id: None,
            });
            compio::runtime::spawn(daemon.run()).detach();

            for _ in 0..50 {
                if siphon_shared::rpc::probe(&endpoint) {
                    break;
                }
                compio::time::sleep(Duration::from_millis(20)).await;
            }

            let (client, snapshot) = Client::connect(&endpoint, "test").await.unwrap();
            assert_eq!(snapshot.channels.len(), 1);
            let events = client.events();
            // A second UI must receive the same entity updates.
            let (second, second_snapshot) = Client::connect(&endpoint, "test-2").await.unwrap();
            assert_eq!(second_snapshot.channels.len(), 1);
            let second_events = second.events();

            let response = client
                .request(Request::SetRecord {
                    channel: 7,
                    record: true,
                })
                .await
                .unwrap();
            assert!(matches!(response, Response::Ack));
            next_pipeline(events, 7, false).await;
            next_config(second_events).await;

            let key = siphon_shared::rpc::StreamKey::new(7, None);
            let response = client.request(Request::GetDisk(key.clone())).await.unwrap();
            assert!(matches!(response, Response::Disk(_)));

            let response = client
                .request(Request::PipelineStop {
                    key: key.clone(),
                    force: true,
                })
                .await
                .unwrap();
            assert!(matches!(response, Response::Ack));
            next_pipeline(events, 7, true).await;

            // A one-shot recording is a pipeline, not a stored preference.
            let response = client
                .request(Request::RecordOnce {
                    login: "sunnys".to_owned(),
                    quality: None,
                })
                .await
                .unwrap();
            assert!(matches!(response, Response::Ack));
            next_pipeline(events, 7, false).await;

            let response = client
                .request(Request::DisconnectViewers(key.clone()))
                .await
                .unwrap();
            assert!(matches!(response, Response::Ack));

            let response = client
                .request(Request::PipelineStop { key, force: false })
                .await
                .unwrap();
            assert!(matches!(response, Response::Ack));
            next_pipeline(events, 7, true).await;
        });
        let _ = std::fs::remove_dir_all(dir);
    }
}
