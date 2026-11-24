//! Admin connections as plain I/O, driven by the daemon's loop.
//!
//! There is no per-connection task and no per-connection queue: the loop owns
//! every connection's decoder and outbound buffer, arms one read and one write
//! future at a time, and encodes each message once per connection. That is what
//! makes a snapshot a synchronous function call rather than a round trip.
//!
//! A listener task does one thing — accept — and hands the raw stream to the
//! loop through a kanal queue.

use std::collections::HashMap;
use std::io;
use std::rc::Rc;
use std::time::Instant;

use compio::buf::BufResult;
use compio::buf::IoBuf;
use compio::buf::IoBufMut;
use compio::io::AsyncRead;
use compio::io::AsyncWrite;
use compio::io::AsyncWriteExt as _;
use siphon_shared::riffle::ConnStats;
use siphon_shared::riffle::FrameDecoder;
use siphon_shared::riffle::ServerMessage;
use siphon_shared::riffle::StreamKey;
use siphon_shared::riffle::codec;
use siphon_shared::riffle::quic;
use siphon_shared::riffle::quic::PairingWindow;
use siphon_shared::riffle::quic::Peers;
use siphon_shared::riffle::quic::SharedTrust;
use siphon_shared::riffle::quic::Trust;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::lane::Lanes;
use crate::media::lane::LaneProducer;
use crate::media::lane::MediaSender;
use crate::media::lane::ProducerHandle;
use crate::media::lane::Sink;

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
    /// The connection's datagram channel, when it is QUIC. The pipe is a byte
    /// stream and has none, which is what makes a lane on it a typed refusal
    /// rather than a silent no-op.
    pub datagrams: Option<quic::Datagrams>,
    /// Set when the client's key is not in `peers.json`: the connection may
    /// send one `PairConfirm` and nothing else, and is dropped when the
    /// window ends.
    pub provisional: Option<Provisional>,
}

/// One pairing attempt, as the listener saw it.
#[derive(Debug, Clone)]
pub struct Provisional {
    /// The code the daemon derived; a `PairConfirm` must repeat it exactly.
    pub sas: String,
    /// The client's key, to add on a match.
    pub spki: String,
    /// The moment the pairing window closes and this connection is dropped.
    pub expires: Instant,
}

impl AdminStream {
    /// The local pipe/socket. Both halves are the same handle.
    fn pipe(stream: PlatformStream) -> Self {
        let stream = Rc::new(stream);
        Self {
            reader: AdminReader::Pipe(Rc::clone(&stream)),
            writer: AdminWriter::Pipe(stream),
            datagrams: None,
            provisional: None,
        }
    }

    /// One bidirectional QUIC stream, plus the datagram channel of the
    /// connection it rides on.
    fn quic(session: quic::Session, provisional: Option<Provisional>) -> Self {
        let datagrams = session.datagrams();
        let (reader, writer) = session.into_stream().split();
        Self {
            reader: AdminReader::Quic(reader),
            writer: AdminWriter::Quic(writer),
            datagrams: Some(datagrams),
            provisional,
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
    /// The datagram channel of a QUIC connection. `None` on the pipe, so a lane
    /// here cannot be fed and `StreamOpen` fails with a typed error.
    datagrams: Option<quic::Datagrams>,
    /// The one task that sends this connection's media, created with the first
    /// lane. Dropping it (with the connection) ends the sender.
    media: Option<MediaSender>,
    /// One producer per attached lane. Dropping a handle stops its producer.
    producers: HashMap<StreamKey, ProducerHandle>,
    /// How many lanes this connection has opened, for the disjoint object-id
    /// range each one gets.
    lanes_opened: u64,
    /// Set while the connection is a pairing attempt: it may send exactly one
    /// `PairConfirm` and nothing else.
    pub provisional: Option<Provisional>,
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
            datagrams: stream.datagrams,
            media: None,
            producers: HashMap::new(),
            lanes_opened: 0,
            provisional: stream.provisional,
        }
    }

    /// The media sender of this connection, created on first use.
    ///
    /// `None` means the carrier cannot send media at all: the local pipe is
    /// bytes only, and a QUIC peer that never advertised DATAGRAM cannot be
    /// sent a datagram either. Both are the caller's typed refusal, not a lane
    /// that silently produces nothing.
    pub fn media(&mut self) -> Option<MediaSender> {
        if self.media.is_none() {
            let datagrams = self.datagrams.clone()?;
            // A peer without DATAGRAM would fail every send; refuse the lane
            // instead.
            datagrams.max_size()?;
            self.media = Some(MediaSender::spawn(datagrams));
        }
        self.media.clone()
    }

    /// Registers one lane's producer, stopping any producer already there.
    pub fn start_lane<S: Sink + 'static>(
        &mut self,
        key: StreamKey,
        handle: crate::lane::LaneHandle,
        consumer: crate::media::lane::LaneConsumer,
        sink: Rc<S>,
    ) -> bool {
        let lane = self.lanes_opened;
        let Some(producer) = LaneProducer::spawn(key.clone(), handle, consumer, sink, lane) else {
            return false;
        };
        self.lanes_opened = self.lanes_opened.wrapping_add(1);
        self.producers.insert(key, producer);
        true
    }

    /// Stops one lane's producer. Idempotent.
    pub fn stop_lane(&mut self, key: &StreamKey) {
        self.producers.remove(key);
    }

    /// Whether the carrier is QUIC, i.e. whether this connection could carry a
    /// media lane at all.
    #[must_use]
    pub const fn is_quic(&self) -> bool {
        self.datagrams.is_some()
    }

    /// This connection's counters: the lanes' drops plus the carrier's refusals.
    #[must_use]
    pub fn stats(&self, now: Instant) -> ConnStats {
        let mut stats = self.lanes.stats(now);
        if let Some(media) = &self.media {
            stats.lost = media.stats().failed();
        }
        stats
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
    trust: &Trust,
    shared: &SharedTrust,
    addrs: &[String],
    queue: &kanal::Sender<AdminStream>,
) {
    for addr in addrs {
        spawn_quic_listener(
            Rc::clone(identity),
            trust.clone(),
            shared.clone(),
            addr.clone(),
            queue.clone(),
        );
    }
}

fn spawn_quic_listener(
    identity: Rc<quic::Identity>,
    trust: Trust,
    shared: SharedTrust,
    addr: String,
    queue: kanal::Sender<AdminStream>,
) {
    compio::runtime::spawn(async move {
        let listener = match quic::Listener::bind(addr.as_str(), &identity, shared).await {
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
            let trust = trust.clone();
            let identity = Rc::clone(&identity);
            compio::runtime::spawn(async move {
                let handshake = match quic::accept(incoming, &identity).await {
                    Ok(handshake) => handshake,
                    Err(error) => {
                        warn!(%error, "quic: handshake failed");
                        return;
                    }
                };
                let Some(admission) = admit(&handshake, &trust).await else {
                    return;
                };
                let (session, provisional) = match admission {
                    Admission::Trusted => match handshake.stream().await {
                        Ok(session) => (session, None),
                        Err(error) => {
                            warn!(%error, "quic: the client opened no control stream");
                            return;
                        }
                    },
                    Admission::Provisional(provisional) => {
                        // The code has been published; the client sends its
                        // `PairConfirm` once a person has compared the two, so
                        // this waits for that frame and only until the window
                        // closes.
                        let wait = provisional
                            .expires
                            .saturating_duration_since(Instant::now());
                        match compio::time::timeout(wait, handshake.stream()).await {
                            Ok(Ok(session)) => (session, Some(provisional)),
                            Ok(Err(error)) => {
                                warn!(%error, "quic: the pairing client opened no stream");
                                return;
                            }
                            Err(_) => {
                                info!(target: "rpc", "quic: a pairing attempt timed out");
                                return;
                            }
                        }
                    }
                };
                if queue
                    .try_send(AdminStream::quic(session, provisional))
                    .is_err()
                {
                    warn!("quic: accepting faster than the loop drains");
                }
            })
            .detach();
        }
    })
    .detach();
}

/// What the allowlist says about one completed handshake.
enum Admission {
    /// A key in `peers.json`: an ordinary connection.
    Trusted,
    /// An unknown key, admitted by an open pairing window: one `PairConfirm`
    /// and nothing else.
    Provisional(Provisional),
}

/// Decides what one handshake is allowed to be.
///
/// A key in the allowlist is an ordinary connection. Anything else — the
/// verifier only let it this far because a pairing window is open — is a
/// provisional one, and its code is published in `pairing.json` for
/// `siphon-daemon pair` to print. `None` drops it: an unknown key with no open
/// window, or a window that cannot be read, is not a connection.
///
/// Both files are read and written with `compio::fs`: this runs on the reactor,
/// where the blocking calls in [`Peers::load`] would stall every task. The
/// verifier has already admitted the key from its snapshot; the files are read
/// again here because this is where the attempt is recorded and the operator's
/// answer is the authority.
async fn admit(handshake: &quic::Handshake, trust: &Trust) -> Option<Admission> {
    let pin = handshake.pin().to_owned();
    let sas = handshake.sas().to_owned();
    let mut peers = match Peers::load_async(trust.peers()).await {
        Ok(peers) => peers,
        Err(error) => {
            error!(%error, "quic: refusing every client, peers.json is unreadable");
            return None;
        }
    };
    if peers.allows(&pin) {
        if let Err(error) = peers.touch_async(&pin).await {
            warn!(%error, "quic: could not record when a peer last connected");
        }
        info!(%pin, "quic: client connected");
        return Some(Admission::Trusted);
    }

    let pairing = PairingWindow::read_async(trust.pairing())
        .await
        .filter(|window| window.is_open() && window.decision.is_none())?;
    match PairingWindow::record_attempt_async(trust.pairing(), &sas, &pin).await {
        Ok(_) => info!(
            target: "rpc",
            %sas,
            %pin,
            "quic: pairing attempt, compare the code with the client"
        ),
        Err(error) => {
            warn!(%error, "quic: could not record the pairing attempt");
            return None;
        }
    }
    Some(Admission::Provisional(Provisional {
        sas,
        spki: pin,
        expires: Instant::now() + pairing.remaining(),
    }))
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use siphon_shared::riffle::Channel;
    use siphon_shared::riffle::Client;
    use siphon_shared::riffle::ClientEvent;
    use siphon_shared::riffle::Config;
    use siphon_shared::riffle::PlaybackPosition;
    use siphon_shared::riffle::Request;
    use siphon_shared::riffle::Response;
    use siphon_shared::riffle::RpcError;
    use siphon_shared::riffle::StreamPlayback;
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
            let (jobs_tx, jobs_rx) = kanal::unbounded();
            let registry = Registry::new(
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
                jobs_tx.clone(),
            );
            registry.seed_channel(
                "sunnys",
                7,
                false,
                vec![debug_variant("mock"), debug_portrait_variant("mock")],
                42,
            );

            let (accept_tx, accept_rx) = kanal::unbounded();
            let endpoint = siphon_shared::riffle::admin_endpoint(&format!(
                "test-{}-{}",
                std::process::id(),
                dir.file_name().unwrap().to_string_lossy()
            ));
            spawn_listener(endpoint.clone(), accept_tx).unwrap();

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
                trust: None,
                trust_shared: SharedTrust::default(),
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

            let key = StreamKey::new(7, None);
            let before = client.request(Request::GetSnapshot).await.unwrap();
            let Response::Snapshot(before) = before else {
                panic!("a snapshot");
            };
            // The pipe carries control only. A media lane over it is a typed
            // refusal, and it must attach nothing: a lane that attached and
            // then pushed nothing would look like a live stream with no picture.
            let refused = client
                .request(Request::StreamOpen {
                    key: key.clone(),
                    playback: StreamPlayback {
                        position: PlaybackPosition::Live,
                        rate: 1.0,
                        buffer_ms: 30_000,
                    },
                })
                .await;
            assert!(
                matches!(refused, Err(RpcError::NoMediaCarrier(_))),
                "the pipe has no datagram channel: {refused:?}"
            );
            let response = client.request(Request::GetSnapshot).await.unwrap();
            let Response::Snapshot(snapshot) = response else {
                panic!("a snapshot");
            };
            assert_eq!(snapshot.lanes.len(), 0);
            assert_eq!(
                snapshot.pipelines.len(),
                before.pipelines.len(),
                "a refused lane claims no pipeline"
            );

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

    /// A daemon with the admin pipe left silent and one QUIC listener, plus the
    /// two keys and the trust state the tests need.
    struct QuicDaemon {
        endpoint: String,
        pin: String,
        identity: quic::Identity,
        trust: Trust,
        /// The snapshot the listener's verifier reads; the app loop refreshes it
        /// every second, and tests call [`Self::refresh`] to skip the wait.
        shared: SharedTrust,
        /// Fixture segments for the first pipeline this daemon spawns, and how
        /// many consumers it is serving. The registry is inside the app, so the
        /// source is created here and handed over.
        fixture: kanal::Sender<crate::scheduler::SegmentMessage>,
        fixture_consumers: Rc<std::cell::Cell<usize>>,
    }

    impl QuicDaemon {
        /// Publishes the files into the snapshot, as the daemon's tick does.
        async fn refresh(&self) {
            self.trust.refresh(&self.shared).await.expect("refresh");
        }
    }

    /// Builds the daemon: a real [`App`], a registry with one seeded channel,
    /// and the listener task wired exactly as `spawn_quic_listener` wires it.
    async fn quic_daemon(dir: &std::path::Path) -> QuicDaemon {
        let http = Rc::new(HttpClient::new());
        let (downloaders_tx, downloaders_rx) = kanal::bounded(32);
        let downloader_metrics = Rc::new(DownloaderMetrics::new(32));
        crate::downloader::spawn_downloaders::<1>(&http, &downloaders_rx, &downloader_metrics);

        let bus = Bus::new();
        let (jobs_tx, jobs_rx) = kanal::unbounded();
        let registry = Registry::new(
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
            jobs_tx.clone(),
        );
        registry.seed_channel(
            "sunnys",
            7,
            false,
            vec![debug_variant("mock"), debug_portrait_variant("mock")],
            42,
        );
        // The pipeline this daemon spawns is fed from the test rather than the
        // playlist, which is what a `--debug` run does with the mock edge.
        let (fixture, fixture_consumers) = registry.fixture_source();

        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let trust = Trust::new(&state);
        let shared = SharedTrust::default();
        let (accept_tx, accept_rx) = kanal::unbounded();
        let identity = Rc::new(quic::Identity::load_or_create(&state).expect("identity"));
        let client_identity =
            quic::Identity::load_or_create_client(&dir.join("ui")).expect("client identity");
        let listener = quic::Listener::bind("127.0.0.1:0", &identity, shared.clone())
            .await
            .expect("bind");
        let endpoint = listener.local_addr().expect("local addr").to_string();
        let queue = accept_tx.clone();
        let listener_identity = Rc::clone(&identity);
        let listener_trust = trust.clone();
        compio::runtime::spawn(async move {
            while let Some(incoming) = listener.incoming().await {
                let queue = queue.clone();
                let identity = Rc::clone(&listener_identity);
                let trust = listener_trust.clone();
                compio::runtime::spawn(async move {
                    let Ok(handshake) = quic::accept(incoming, &identity).await else {
                        return;
                    };
                    let Some(admission) = admit(&handshake, &trust).await else {
                        return;
                    };
                    let (session, provisional) = match admission {
                        Admission::Trusted => {
                            let Ok(session) = handshake.stream().await else {
                                return;
                            };
                            (session, None)
                        }
                        Admission::Provisional(provisional) => {
                            let wait = provisional
                                .expires
                                .saturating_duration_since(Instant::now());
                            match compio::time::timeout(wait, handshake.stream()).await {
                                Ok(Ok(session)) => (session, Some(provisional)),
                                _ => return,
                            }
                        }
                    };
                    let _ = queue.try_send(AdminStream::quic(session, provisional));
                })
                .detach();
            }
        })
        .detach();

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
            trust: Some(trust.clone()),
            trust_shared: shared.clone(),
        });
        compio::runtime::spawn(daemon.run()).detach();

        QuicDaemon {
            endpoint,
            pin: identity.pin().to_owned(),
            identity: client_identity,
            trust,
            shared,
            fixture,
            fixture_consumers,
        }
    }

    /// Waits for the daemon to publish a pairing attempt, as `pair` does.
    async fn published_attempt(trust: &Trust) -> quic::PairingAttempt {
        for _ in 0..100 {
            if let Some(attempt) = PairingWindow::read(trust.pairing()).and_then(|w| w.attempt) {
                return attempt;
            }
            compio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the daemon never published a pairing attempt");
    }

    /// The client half of one pairing attempt, as the UI's screen would make it.
    async fn begin_pairing(daemon: &QuicDaemon) -> siphon_shared::riffle::Pairing {
        siphon_shared::riffle::Pairing::begin(&daemon.endpoint, &daemon.pin, &daemon.identity)
            .await
            .expect("provisional handshake")
    }

    /// The whole control plane over QUIC: a real [`App`], the existing
    /// handshake, the snapshot and one query. Only the carrier differs from
    /// the pipe test above.
    #[test]
    fn a_control_plane_round_trip_over_quic() {
        let dir = scratch_dir("quic");
        let runtime = compio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let daemon = quic_daemon(&dir).await;
            // The daemon's allowlist is the only thing that admits a client, and
            // the verifier sees it through the snapshot the tick publishes.
            Peers::load(daemon.trust.peers())
                .unwrap()
                .add("test", daemon.identity.pin())
                .unwrap();
            daemon.refresh().await;

            let (client, snapshot) =
                Client::connect_quic(&daemon.endpoint, &daemon.pin, &daemon.identity, "test")
                    .await
                    .expect("quic connect");
            assert_eq!(snapshot.channels.len(), 1);
            // The connect path keeps the connection, so the media lane has a
            // datagram channel on the same connection as the control stream.
            assert!(
                client
                    .datagrams()
                    .is_some_and(|channel| channel.max_size().is_some()),
                "a QUIC client keeps its datagram channel"
            );

            let response = client.request(Request::GetSnapshot).await.unwrap();
            assert!(matches!(response, Response::Snapshot(_)));
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A real QUIC connection opens a lane, and media flows: the fixture
    /// segment is demuxed, packed and pushed as datagrams on the same
    /// connection as the control plane, where the client's `recv` sees valid
    /// object headers with monotonic ids and the keyframe bit on the segment's
    /// opening access unit.
    #[test]
    fn a_lane_pushes_media_datagrams_over_quic() {
        let dir = scratch_dir("quic-media");
        let runtime = compio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let Some(segment) = std::fs::read(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../captures/servermock/live/000000.ts"),
            )
            .ok() else {
                println!("captures/ is absent; skipping the media lane test");
                return;
            };
            let daemon = quic_daemon(&dir).await;
            Peers::load(daemon.trust.peers())
                .unwrap()
                .add("test", daemon.identity.pin())
                .unwrap();
            daemon.refresh().await;

            let (client, _) =
                Client::connect_quic(&daemon.endpoint, &daemon.pin, &daemon.identity, "test")
                    .await
                    .expect("quic connect");
            let key = StreamKey::new(7, None);
            let response = client
                .request(Request::StreamOpen {
                    key: key.clone(),
                    playback: StreamPlayback {
                        position: PlaybackPosition::Live,
                        rate: 1.0,
                        buffer_ms: 30_000,
                    },
                })
                .await
                .expect("stream open");
            assert!(matches!(response, Response::Ack));

            // One HLS segment, as the scheduler would have spooled it.
            let arena = Rc::new(ArenaSet::new());
            daemon
                .fixture
                .send(crate::scheduler::SegmentMessage::Segment(spooled(
                    &arena, &segment, 0,
                )))
                .expect("fixture segment");
            let send = |arena: &Rc<ArenaSet>, timestamp_ms: u64| {
                daemon
                    .fixture
                    .send(crate::scheduler::SegmentMessage::Segment(spooled(
                        arena,
                        &segment,
                        timestamp_ms,
                    )))
                    .expect("fixture segment");
            };

            // Collect until the lane goes quiet; the whole segment is inside
            // the client's buffer, so the producer sends all of it.
            let datagrams = client.datagrams().expect("a datagram channel");
            let budget = datagrams.max_size().expect("a negotiated budget");
            let mut received = Vec::new();
            while let Ok(Some(datagram)) =
                compio::time::timeout(Duration::from_millis(300), datagrams.recv()).await
            {
                received.push(datagram);
            }
            assert!(
                received.len() > 1,
                "a 2 s segment is many datagrams, got {}",
                received.len()
            );

            // Every datagram is one object, within the path's budget.
            let mut objects = Vec::new();
            for datagram in &received {
                assert!(
                    datagram.len() <= budget,
                    "a datagram is {} bytes, past the {budget} byte budget",
                    datagram.len()
                );
                objects.push(
                    siphon_shared::riffle::Object::decode_datagram(datagram)
                        .expect("a valid object header"),
                );
            }
            // Audio is sent twice, so the same id repeats; every id is distinct
            // otherwise and arrives in order.
            let mut ids: Vec<u64> = objects.iter().map(|object| object.obj_id).collect();
            ids.dedup();
            assert!(
                ids.windows(2).all(|pair| pair[0] < pair[1]),
                "obj_id increases across the segment"
            );
            let first_video = objects
                .iter()
                .enumerate()
                .find(|(_, object)| object.class == siphon_shared::riffle::Class::Video)
                .map(|(index, _)| index)
                .expect("the fixture has video");
            assert!(
                objects[first_video]
                    .flags
                    .contains(siphon_shared::riffle::Flags::KEYFRAME),
                "the segment's first access unit is an IDR"
            );
            assert!(
                objects
                    .iter()
                    .any(|object| object.class == siphon_shared::riffle::Class::Audio),
                "the fixture has audio"
            );
            assert_eq!(
                daemon.fixture_consumers.get(),
                1,
                "the lane is one consumer of the pipeline"
            );

            // The counters the UI reads come from the same path.
            let response = client.request(Request::GetSnapshot).await.unwrap();
            let Response::Snapshot(snapshot) = response else {
                panic!("a snapshot");
            };
            assert_eq!(snapshot.lanes.len(), 1, "this connection's lane");
            assert!(
                snapshot.lanes[0].buffered_ahead_ms > 0,
                "what was pushed past the playhead is reported"
            );
            assert_eq!(snapshot.lanes[0].dropped_video, 0);
            assert_eq!(snapshot.conn_stats.dropped_video, 0);
            assert_eq!(
                snapshot.conn_stats.lost, 0,
                "every datagram was queued for the carrier"
            );
            assert!(!snapshot.conn_stats.recovering());

            // Closing the lane releases the pipeline and unregisters the
            // consumer; a later segment reaches nobody.
            let response = client.request(Request::StreamClose { key }).await.unwrap();
            assert!(matches!(response, Response::Ack));
            let response = client.request(Request::GetSnapshot).await.unwrap();
            let Response::Snapshot(snapshot) = response else {
                panic!("a snapshot");
            };
            assert_eq!(snapshot.lanes.len(), 0);
            assert_eq!(snapshot.pipelines.len(), 0, "the claim was released");

            // A closed lane's consumer is gone, so a later segment is
            // dispatched to nobody: the producer ended, it was not left parked.
            send(&arena, 2_000);
            for _ in 0..50 {
                if daemon.fixture_consumers.get() == 0 {
                    break;
                }
                compio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(
                daemon.fixture_consumers.get(),
                0,
                "the lane unregistered when it closed"
            );
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Writes one segment into a spool, as a completed fetch leaves it.
    fn spooled(arena: &Rc<ArenaSet>, bytes: &[u8], timestamp_ms: u64) -> crate::scheduler::Segment {
        let wakes = crate::spool::WakeSet::new();
        let (mut writer, reader) = crate::spool::Spool::open(arena, &wakes).expect("spool");
        let mut offset = 0;
        while offset < bytes.len() {
            let tail = writer.tail().expect("spool capacity");
            let len = tail.len().min(bytes.len() - offset);
            tail[..len].copy_from_slice(&bytes[offset..offset + len]);
            writer.commit(len);
            offset += len;
        }
        drop(writer);
        crate::scheduler::Segment {
            timestamp_ms,
            pdt_ms: 0,
            duration_ms: 2_000,
            sequence_num: 1,
            reader,
        }
    }

    /// No allowlist entry and no window: the handshake is refused, so the
    /// daemon never serves the client anything.
    #[test]
    fn an_unlisted_client_is_refused_by_the_handshake() {
        let dir = scratch_dir("quic-refused");
        let runtime = compio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let daemon = quic_daemon(&dir).await;
            let result =
                Client::connect_quic(&daemon.endpoint, &daemon.pin, &daemon.identity, "test").await;
            assert!(result.is_err(), "an unlisted key must not connect");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The whole pairing flow: the daemon publishes the code, the operator
    /// approves it, the client sends it back, and the key lands in
    /// `peers.json`.
    #[test]
    fn a_matching_pairing_code_adds_the_client() {
        let dir = scratch_dir("quic-pair");
        let runtime = compio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let daemon = quic_daemon(&dir).await;
            PairingWindow::open(daemon.trust.pairing(), Duration::from_secs(120)).unwrap();
            // The verifier reads the snapshot, so opening the window on disk is
            // only half of it: this is what the daemon's tick would do.
            daemon.refresh().await;

            let pairing = begin_pairing(&daemon).await;
            let attempt = published_attempt(&daemon.trust).await;
            assert_eq!(attempt.sas, pairing.sas(), "both sides derive one code");
            PairingWindow::decide(daemon.trust.pairing(), &attempt.sas, true).unwrap();

            let (client, snapshot) = pairing.confirm("test-ui").await.expect("paired");
            assert_eq!(snapshot.channels.len(), 1);
            // The connection is ordinary from here: a request round-trips.
            let response = client.request(Request::GetSnapshot).await.unwrap();
            assert!(matches!(response, Response::Snapshot(_)));

            let peers = Peers::load(daemon.trust.peers()).unwrap();
            assert_eq!(peers.entries().len(), 1, "the key was written");
            assert_eq!(peers.entries()[0].spki, daemon.identity.pin());
            assert_eq!(peers.entries()[0].name, "test-ui");
            assert!(peers.entries()[0].added_ms > 0);
            assert!(
                !PairingWindow::read(daemon.trust.pairing())
                    .unwrap()
                    .is_open(),
                "a successful pairing closes the window"
            );
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A code that is not the daemon's closes the connection and writes
    /// nothing.
    #[test]
    fn a_mismatched_pairing_code_adds_nothing() {
        let dir = scratch_dir("quic-pair-wrong");
        let runtime = compio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            use siphon_shared::riffle::ClientMessage;
            use siphon_shared::riffle::codec;

            let daemon = quic_daemon(&dir).await;
            PairingWindow::open(daemon.trust.pairing(), Duration::from_secs(120)).unwrap();
            daemon.refresh().await;

            let session = quic::connect(&daemon.endpoint, &daemon.pin, &daemon.identity)
                .await
                .expect("provisional handshake");
            published_attempt(&daemon.trust).await;
            let mut stream = session.into_stream();
            codec::write(
                &mut stream,
                &ClientMessage::PairConfirm {
                    sas: "000-000".to_owned(),
                    name: "impostor".to_owned(),
                },
            )
            .await
            .expect("write the wrong code");
            // The daemon drops the connection rather than answering.
            let mut reader = siphon_shared::riffle::FrameReader::new();
            let reply: Result<ServerMessage, _> = reader.read(&mut stream).await;
            assert!(reply.is_err(), "a wrong code must not be answered");
            assert!(
                PairingWindow::read(daemon.trust.pairing())
                    .unwrap()
                    .decision
                    .is_none(),
                "nothing approved this attempt"
            );
            let peers = Peers::load(daemon.trust.peers()).unwrap();
            assert!(peers.entries().is_empty(), "no key was written");
        });
        let _ = std::fs::remove_dir_all(dir);
    }
}
