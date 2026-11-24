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

use compio::buf::BufResult;
use compio::buf::IoBuf;
use compio::buf::IoBufMut;
use compio::io::AsyncRead;
use compio::io::AsyncWrite;
use compio::io::AsyncWriteExt as _;
use siphon_shared::riffle::FrameDecoder;
use siphon_shared::riffle::ServerMessage;
use siphon_shared::riffle::codec;
use siphon_shared::riffle::quic;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::lane::Lanes;

/// The local admin transport: a socket on Unix, a named pipe on Windows.
#[cfg(unix)]
type PlatformStream = compio::net::UnixStream;
#[cfg(windows)]
type PlatformStream = compio::fs::named_pipe::NamedPipeServer;

/// The read half of an admin connection.
///
/// The halves are owned and shuttled through the loop's events rather than
/// shared behind `Rc`: a QUIC connection is two separate stream handles, so a
/// read and a write can be armed at once without an interior-mutable wrapper.
pub enum AdminReader {
    /// A pipe or domain socket. The two halves share one `Rc`.
    Pipe(Rc<PlatformStream>),
    /// One direction of a QUIC bidirectional stream.
    Quic(quic::Reader),
}

impl AsyncRead for AdminReader {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        match self {
            Self::Pipe(stream) => {
                let mut stream: &PlatformStream = stream;
                stream.read(buf).await
            }
            Self::Quic(stream) => stream.read(buf).await,
        }
    }
}

/// The write half of an admin connection.
pub enum AdminWriter {
    Pipe(Rc<PlatformStream>),
    Quic(quic::Writer),
}

impl AsyncWrite for AdminWriter {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        match self {
            Self::Pipe(stream) => {
                let mut stream: &PlatformStream = stream;
                stream.write(buf).await
            }
            Self::Quic(stream) => stream.write(buf).await,
        }
    }

    async fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Pipe(stream) => {
                let mut stream: &PlatformStream = stream;
                stream.flush().await
            }
            Self::Quic(stream) => stream.flush().await,
        }
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        match self {
            Self::Pipe(stream) => {
                let mut stream: &PlatformStream = stream;
                stream.shutdown().await
            }
            Self::Quic(stream) => stream.shutdown().await,
        }
    }
}

/// One accepted admin connection, whichever carrier it arrived on.
pub struct AdminStream {
    pub reader: AdminReader,
    pub writer: AdminWriter,
}

impl AdminStream {
    /// The local pipe/socket. Both halves are the same handle.
    fn pipe(stream: PlatformStream) -> Self {
        let stream = Rc::new(stream);
        Self {
            reader: AdminReader::Pipe(Rc::clone(&stream)),
            writer: AdminWriter::Pipe(stream),
        }
    }

    /// One bidirectional QUIC stream.
    fn quic(stream: quic::Stream) -> Self {
        let (reader, writer) = stream.split();
        Self {
            reader: AdminReader::Quic(reader),
            writer: AdminWriter::Quic(writer),
        }
    }
}

/// Bytes one read asks for. Control traffic, not a hot loop.
const READ_CHUNK: usize = 16 * 1024;

/// A connection whose outbound buffer exceeds this is dropped; the UI reconnects
/// and resyncs from a snapshot.
pub const MAX_OUT: usize = 1 << 20;

/// One admin connection's protocol state. Owned by the daemon loop.
pub struct Conn {
    /// The read half, absent only while a read future owns it.
    pub reader: Option<AdminReader>,
    /// The write half, absent only while a write future owns it.
    pub writer: Option<AdminWriter>,
    pub decoder: FrameDecoder,
    /// Encoded frames waiting to be written.
    pub out: Vec<u8>,
    /// The buffer a completed write hands back, emptied for reuse.
    pub spare: Vec<u8>,
    /// The handshake completed and the snapshot was queued.
    pub ready: bool,
    /// A write future owns the buffer right now.
    pub writing: bool,
    /// The lanes this connection opened. Per connection, because a lane is a
    /// reader position on one UI: it is not daemon state, and one UI cannot see
    /// or close another's.
    pub lanes: Lanes,
}

impl Conn {
    #[must_use]
    pub fn new(stream: AdminStream) -> Self {
        Self {
            reader: Some(stream.reader),
            writer: Some(stream.writer),
            decoder: FrameDecoder::new(),
            out: Vec::new(),
            spare: Vec::new(),
            ready: false,
            writing: false,
            lanes: Lanes::default(),
        }
    }

    /// Takes the read half to arm the next read. A connection has at most one
    /// read in flight, so `None` only while a future owns it.
    pub const fn take_reader(&mut self) -> Option<AdminReader> {
        self.reader.take()
    }

    /// Takes the write half to arm the next write.
    pub const fn take_writer(&mut self) -> Option<AdminWriter> {
        self.writer.take()
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
        reader: AdminReader,
    },
    Write {
        conn: usize,
        result: io::Result<()>,
        buf: Vec<u8>,
        writer: AdminWriter,
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

/// Reads up to [`READ_CHUNK`] bytes. The buffer and the read half come back
/// for reuse.
pub async fn read(conn: usize, mut reader: AdminReader) -> ConnEvent {
    let buf = Vec::with_capacity(READ_CHUNK);
    let read = reader.read(buf).await;
    ConnEvent::Read {
        conn,
        result: read.0,
        buf: read.1,
        reader,
    }
}

/// Writes the whole buffer. The buffer and the write half come back for reuse.
pub async fn write(conn: usize, mut writer: AdminWriter, buf: Vec<u8>) -> ConnEvent {
    let written = writer.write_all(buf).await;
    ConnEvent::Write {
        conn,
        result: written.0,
        buf: written.1,
        writer,
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

    if siphon_shared::riffle::probe(&endpoint) {
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
                    if queue.try_send(AdminStream::pipe(stream)).is_err() {
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
            if queue.try_send(AdminStream::pipe(server)).is_err() {
                warn!("rpc: accepting faster than the loop drains");
            }
            server = next;
        }
    })
    .detach();
    Ok(())
}

/// Serves every `network.listen` address over QUIC, handing each accepted
/// control stream to the loop through the same queue as the pipe.
///
/// One task per address, and one per connection: a stalled handshake or a peer
/// that connects but never opens its control stream must not stop anything
/// else from accepting.
pub fn spawn_quic_listeners(
    identity: &Rc<quic::Identity>,
    addrs: &[String],
    queue: &kanal::Sender<AdminStream>,
) {
    for addr in addrs {
        spawn_quic_listener(Rc::clone(identity), addr.clone(), queue.clone());
    }
}

fn spawn_quic_listener(
    identity: Rc<quic::Identity>,
    addr: String,
    queue: kanal::Sender<AdminStream>,
) {
    compio::runtime::spawn(async move {
        let listener = match quic::Listener::bind(addr.as_str(), &identity).await {
            Ok(listener) => listener,
            Err(error) => {
                error!(%error, %addr, "quic: bind failed");
                return;
            }
        };
        match listener.local_addr() {
            Ok(local) => info!(%local, pin = identity.pin(), "quic: listening"),
            Err(error) => warn!(%error, %addr, "quic: bound but the local address is unknown"),
        }
        loop {
            let Some(incoming) = listener.incoming().await else {
                break;
            };
            let queue = queue.clone();
            compio::runtime::spawn(async move {
                match quic::establish(incoming).await {
                    Ok(stream) => {
                        if queue.try_send(AdminStream::quic(stream)).is_err() {
                            warn!("quic: accepting faster than the loop drains");
                        }
                    }
                    Err(error) => warn!(%error, "quic: connection failed"),
                }
            })
            .detach();
        }
    })
    .detach();
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use siphon_shared::riffle::Channel;
    use siphon_shared::riffle::Client;
    use siphon_shared::riffle::ClientEvent;
    use siphon_shared::riffle::Config;
    use siphon_shared::riffle::Request;
    use siphon_shared::riffle::Response;
    use siphon_shared::riffle::Update;
    use siphon_shared::riffle::quic;

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

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("siphon-rpc-test-{}-{name}", std::process::id()));
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
        let dir = scratch_dir("pipe");
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
            let endpoint = siphon_shared::riffle::admin_endpoint(&format!(
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
                if siphon_shared::riffle::probe(&endpoint) {
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

            let key = siphon_shared::riffle::StreamKey::new(7, None);
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

    /// The whole control plane over QUIC: a real [`App`], the existing
    /// handshake, the snapshot and one query. Only the carrier differs from
    /// the pipe test above.
    #[test]
    fn a_control_plane_round_trip_over_quic() {
        let dir = scratch_dir("quic");
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
                        output: None,
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
            let identity =
                Rc::new(quic::Identity::load_or_create(&dir.join("net")).expect("identity"));
            let pin = identity.pin().to_owned();
            let listener = quic::Listener::bind("127.0.0.1:0", &identity)
                .await
                .expect("bind");
            let endpoint = listener.local_addr().expect("local addr").to_string();
            let queue = accept_tx.clone();
            compio::runtime::spawn(async move {
                while let Some(incoming) = listener.incoming().await {
                    let queue = queue.clone();
                    compio::runtime::spawn(async move {
                        if let Ok(stream) = quic::establish(incoming).await {
                            let _ = queue.try_send(AdminStream::quic(stream));
                        }
                    })
                    .detach();
                }
            })
            .detach();

            let (jobs_tx, jobs_rx) = kanal::unbounded();
            let daemon = App::new(AppInit {
                config: Config {
                    channels: vec![Channel {
                        login: "sunnys".to_owned(),
                        id: 7,
                        display_name: Some("Sunnys".to_owned()),
                        record: false,
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

            let (client, snapshot) = Client::connect_quic(&endpoint, &pin, "test")
                .await
                .expect("quic connect");
            assert_eq!(snapshot.channels.len(), 1);

            let response = client.request(Request::GetSnapshot).await.unwrap();
            assert!(matches!(response, Response::Snapshot(_)));
        });
        let _ = std::fs::remove_dir_all(dir);
    }
}
