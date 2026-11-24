//! The Siphon backend: configuration, GQL, Hermes, the stream registry and the
//! admin connections.
//!
//! Everything that owns state runs in this one loop: admin connections, the
//! registry and the snapshot projection. Spawned components (schedulers, disk,
//! downloaders) publish through the [`Bus`], which the loop drains into every
//! connection. A snapshot is therefore a plain `&self` function, not a request.

use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;
use std::time::Instant;

use futures_util::FutureExt as _;
use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use siphon_shared::riffle::ArenaSummary;
use siphon_shared::riffle::ChannelView;
use siphon_shared::riffle::Config;
use siphon_shared::riffle::ConnHealth;
use siphon_shared::riffle::ConnStats;
use siphon_shared::riffle::DownloaderSummary;
use siphon_shared::riffle::LogLevel;
use siphon_shared::riffle::MAX_AVATAR;
use siphon_shared::riffle::PipelineView;
use siphon_shared::riffle::PlaybackPosition;
use siphon_shared::riffle::Request;
use siphon_shared::riffle::Response;
use siphon_shared::riffle::RpcError;
use siphon_shared::riffle::ServerMessage;
use siphon_shared::riffle::Snapshot;
use siphon_shared::riffle::StreamEvent;
use siphon_shared::riffle::StreamEventKind;
use siphon_shared::riffle::StreamKey;
use siphon_shared::riffle::StreamPlayback;
use siphon_shared::riffle::StreamStarted;
use siphon_shared::riffle::Update;
use siphon_shared::riffle::UpdateView;
use siphon_shared::riffle::quic::PairingWindow;
use siphon_shared::riffle::quic::Peers;
use siphon_shared::riffle::quic::SharedTrust;
use siphon_shared::riffle::quic::Trust;
use tracing::info;
use tracing::warn;

use crate::bus::Bus;
use crate::config;
use crate::config::ConfigError;
use crate::gql;
use crate::gql::ChannelDetail;
use crate::gql::GqlError;
use crate::hermes::ConnectOutcome;
use crate::hermes::SubEntry;
use crate::hermes::SubState;
use crate::hermes::Topic;
use crate::http::Client;
use crate::http::ws::WsStream;
use crate::lane::Lanes;
use crate::lane::Start;
use crate::registry::Job;
use crate::registry::LanePlace;
use crate::registry::Registry;
use crate::riffle;
use crate::time::now_ms;
use crate::update;
use crate::utils::CheapRng;
use crate::utils::NanoId;
/// One thing the daemon loop wakes for.
enum Event {
    /// One admin connection produced input or finished a write.
    Conn(riffle::ConnEvent),
    /// Hermes produced a text frame, closed cleanly, or failed the read.
    Socket(Result<Option<String>, crate::http::Error>),
    /// A background job finished.
    Job(Option<Box<JobDone>>),
    /// Something published on the bus.
    Bus,
    /// A pipe connection's media sender queued a frame: there is something to
    /// write.
    Media,
    /// The nearest deadline came due.
    Deadline,
}

/// A loop operation: connection I/O, jobs and the bus wake live in one set.
type Op = futures_util::future::LocalBoxFuture<'static, Event>;

fn accept_op(queue: &kanal::AsyncReceiver<riffle::AdminStream>) -> Op {
    let queue = queue.clone();
    Box::pin(async move { Event::Conn(riffle::accept(&queue).await) })
}

fn read_op(conn: usize, reader: riffle::AdminReader) -> Op {
    Box::pin(async move { Event::Conn(riffle::read(conn, reader).await) })
}

fn write_op(conn: usize, writer: riffle::AdminWriter, buf: Vec<u8>) -> Op {
    Box::pin(async move { Event::Conn(riffle::write(conn, writer, buf).await) })
}

fn bus_op(bus: Rc<Bus>) -> Op {
    Box::pin(async move {
        bus.wait().await;
        Event::Bus
    })
}

/// The smallest free slot, so a closed connection's index is reused.
fn insert_conn(conns: &mut Vec<Option<riffle::Conn>>, conn: riffle::Conn) -> usize {
    if let Some(index) = conns.iter().position(Option::is_none) {
        conns[index] = Some(conn);
        index
    } else {
        conns.push(Some(conn));
        conns.len() - 1
    }
}

/// Everything the app needs at startup.
pub struct AppInit {
    pub config: Config,
    pub config_path: PathBuf,
    pub client: Rc<Client>,
    pub registry: Registry,
    pub bus: Rc<Bus>,
    /// Accepted admin connections, from the listener task.
    pub accept: kanal::AsyncReceiver<riffle::AdminStream>,
    /// Finished background jobs.
    pub jobs: kanal::AsyncReceiver<Box<JobDone>>,
    pub jobs_tx: kanal::Sender<Box<JobDone>>,
    pub client_id: Option<String>,
    /// Where a pairing window and the client allowlist live. `None` when the
    /// daemon has no QUIC listener, so no pairing can be in progress.
    pub trust: Option<Trust>,
    /// The snapshot the QUIC verifier consults. The tick re-reads the files
    /// above into it; without a QUIC listener it is an empty snapshot nobody
    /// reads.
    pub trust_shared: SharedTrust,
}

/// One message for the loop's queue.
pub enum JobDone {
    AddResolved(String, Box<Result<Option<ChannelDetail>, GqlError>>),
    LiveRefreshed(u64, Result<Vec<ChannelDetail>, GqlError>),
    SaveFinished(Result<(), ConfigError>),
    UpdateCheckFinished(Result<update::Release, update::UpdateError>),
    UpdateDownloadFinished(Result<PathBuf, update::UpdateError>),
    SubscribeCheckDue(Topic, NanoId, u64),
    ConnectFinished(Box<ConnectOutcome>),
    /// A picture finished caching: the channel id and the avatar's content id,
    /// or `None` when the download failed.
    AvatarCached(u64, Option<String>),
    /// Work the registry queued for the loop: it owns no reactor handle, so an
    /// activation or a recorder that ended comes back here to be performed.
    Registry(Job),
}

/// One in-flight job. `!Send` is fine: everything stays on the reactor thread.
#[allow(clippy::struct_excessive_bools)]
pub struct App {
    pub(crate) config: Config,
    pub(crate) config_path: PathBuf,
    pub(crate) client: Rc<Client>,
    pub(crate) registry: Registry,
    pub(crate) bus: Rc<Bus>,
    pub(crate) accept: kanal::AsyncReceiver<riffle::AdminStream>,
    pub(crate) jobs: kanal::AsyncReceiver<Box<JobDone>>,
    pub(crate) jobs_tx: kanal::Sender<Box<JobDone>>,
    /// One encoded frame reused for every connection.
    frame: Vec<u8>,
    /// When the next counter flush is due, while a UI is connected.
    telemetry_at: Instant,
    pub(crate) client_id: Option<String>,

    pub(crate) channels: HashMap<u64, ChannelDetail>,
    /// Avatar downloads in flight, so a refresh does not start a second one.
    pub(crate) avatars_pending: HashSet<u64>,
    pub(crate) pending: Vec<String>,
    pub(crate) conn: ConnHealth,
    /// Whether the Hermes socket should connect (off in debug unless a stand-in
    /// endpoint is configured).
    pub(crate) hermes_enabled: bool,
    pub(crate) error: Option<String>,

    pub(crate) socket: Option<WsStream>,
    pub(crate) welcomed: bool,
    pub(crate) connecting: bool,
    pub(crate) keepalive_secs: u64,
    pub(crate) last_message: Instant,
    /// When the current socket finished its handshake. The reconnect backoff
    /// resets on how long a connection *stayed up*, never on `welcome`: a
    /// socket that dies just after the handshake is a flapping one, and
    /// resetting on welcome turned it into a 500 ms reconnect loop.
    pub(crate) connected_at: Instant,
    /// The `type` of the last frame seen, for disconnect diagnosis.
    pub(crate) last_frame_kind: &'static str,
    /// Frames decoded on the current socket.
    pub(crate) frames_received: u32,
    pub(crate) reconnect_at: Instant,
    pub(crate) reconnect_attempt: u32,
    pub(crate) sub_ids: HashMap<Topic, SubEntry>,
    pub(crate) sub_gen: u64,
    pub(crate) rng: CheapRng,

    pub(crate) update: Option<update::Release>,
    pub(crate) update_busy: bool,
    pub(crate) update_check_at: Instant,

    last_arena: ArenaSummary,
    last_downloader: DownloaderSummary,
    last_pipelines: HashMap<StreamKey, PipelineView>,

    pub(crate) pending_unsubscribes: Vec<(NanoId, Topic)>,

    save_dirty: bool,
    save_running: bool,
    /// The allowlist and pairing window a `PairConfirm` is checked against.
    trust: Option<Trust>,
    /// The snapshot the QUIC verifier consults, refreshed from `trust`.
    trust_shared: SharedTrust,
    /// When the trust snapshot is next due to be re-read.
    trust_at: Instant,
    /// Set when a pipe connection's media sender has queued frames. The loop is
    /// the only writer of a connection's out buffer, so this is what arms the
    /// next write instead of leaving a picture to arrive at the next tick.
    media_wake: Rc<crate::media::lane::PipeSignal>,
}

impl App {
    #[must_use]
    pub fn new(init: AppInit) -> Self {
        let AppInit {
            config,
            config_path,
            client,
            registry,
            bus,
            accept,
            jobs,
            jobs_tx,
            client_id,
            trust,
            trust_shared,
        } = init;
        // Hermes is wanted whenever there is a channel to watch; the endpoint is
        // whatever the edge is (Twitch, or the mock under `--debug`).
        let hermes_enabled = !config.channels.is_empty();
        let conn = if config.channels.is_empty() {
            ConnHealth::NoChannels
        } else {
            ConnHealth::Connecting
        };
        let channels: HashMap<u64, ChannelDetail> = HashMap::new();
        Self {
            config,
            config_path,
            client,
            registry,
            bus,
            accept,
            jobs,
            jobs_tx,
            frame: Vec::new(),
            telemetry_at: Instant::now(),
            client_id,
            channels,
            avatars_pending: HashSet::new(),
            pending: Vec::new(),
            conn,
            hermes_enabled,
            error: None,
            socket: None,
            welcomed: false,
            connecting: false,
            keepalive_secs: 15,
            last_message: Instant::now(),
            connected_at: Instant::now(),
            last_frame_kind: "none",
            frames_received: 0,
            reconnect_at: Instant::now(),
            reconnect_attempt: 0,
            sub_ids: HashMap::new(),
            sub_gen: 0,
            rng: CheapRng::new(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0x853c_49e6_748f_ea9b, |elapsed| {
                        u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
                    }),
            ),
            update: None,
            update_busy: false,
            update_check_at: Instant::now(),
            last_arena: ArenaSummary::default(),
            last_downloader: DownloaderSummary::default(),
            last_pipelines: HashMap::new(),
            pending_unsubscribes: Vec::new(),
            save_dirty: false,
            save_running: false,
            trust,
            trust_shared,
            trust_at: Instant::now(),
            media_wake: Rc::new(crate::media::lane::PipeSignal::default()),
        }
    }

    pub async fn run(mut self) {
        let mut conns: Vec<Option<riffle::Conn>> = Vec::new();
        let mut ops: FuturesUnordered<Op> = FuturesUnordered::new();
        ops.push(accept_op(&self.accept));
        ops.push(bus_op(Rc::clone(&self.bus)));

        loop {
            self.maybe_reconnect();
            if Instant::now() >= self.update_check_at {
                self.push_update_check();
            }

            let deadline = self.next_deadline(&conns);
            // An owned clone, so the select does not borrow `self` twice.
            let media_wake = Rc::clone(&self.media_wake);
            let event = {
                // Biased, socket first: a completed read is never dropped for a
                // connection event. The socket cannot move into the op set,
                // because one `WsStream` serves both directions and a send must
                // not queue behind a read.
                let socket_fut = async {
                    match self.socket.as_mut() {
                        Some(socket) => socket.next_text().await,
                        None => std::future::pending().await,
                    }
                }
                .fuse();
                let ops_fut = ops.next().fuse();
                let jobs = self.jobs.clone();
                let job_fut = jobs.recv().fuse();
                let media_fut = media_wake.wait().fuse();
                let timer = compio::time::sleep(deadline).fuse();

                futures_util::pin_mut!(socket_fut, ops_fut, job_fut, media_fut, timer);
                futures_util::select_biased! {
                    message = socket_fut => Event::Socket(message),
                    some = ops_fut => match some {
                        Some(event) => event,
                        None => break,
                    },
                    job = job_fut => Event::Job(job.ok()),
                    () = media_fut => Event::Media,
                    () = timer => Event::Deadline,
                }
            };

            match event {
                Event::Conn(event) => self.handle_conn(event, &mut conns, &ops).await,
                Event::Socket(Ok(Some(text))) => self.handle_message(&text).await,
                Event::Socket(Ok(None)) => {
                    tracing::info!(target: "hermes", "connection closed by peer");
                    self.on_disconnect("peer-closed").await;
                }
                Event::Socket(Err(error)) => {
                    tracing::info!(target: "hermes", "socket read failed: {error}");
                    self.on_disconnect("read-error").await;
                }
                Event::Job(None) => break,
                Event::Job(Some(done)) => self.handle_job(*done).await,
                Event::Bus => ops.push(bus_op(Rc::clone(&self.bus))),
                // The frames are already queued: `flush_bus` below arms the
                // write that carries them.
                Event::Media => {}
                Event::Deadline => {
                    self.on_tick().await;
                    // A provisional connection that has not confirmed by the
                    // window's end is dropped; the tick is what wakes the loop
                    // for it.
                    self.expire_provisional(&mut conns);
                }
            }

            self.flush_bus(&mut conns, &ops);
            self.flush_unsubscribes().await;
        }
    }

    /// Starts a Hermes connection if one is due. Called once per loop pass; it
    /// is cheap and makes the reconnect deadline a plain deadline.
    fn maybe_reconnect(&mut self) {
        if self.hermes_enabled
            && self.socket.is_none()
            && !self.connecting
            && !self.config.channels.is_empty()
            && Instant::now() >= self.reconnect_at
        {
            self.start_connect();
        }
    }

    fn next_deadline(&self, conns: &[Option<riffle::Conn>]) -> Duration {
        let now = Instant::now();
        let mut at = now + Duration::from_mins(1);
        if self.hermes_enabled
            && self.socket.is_none()
            && !self.connecting
            && !self.config.channels.is_empty()
        {
            at = at.min(self.reconnect_at);
        }
        at = at.min(self.update_check_at);
        if self.socket.is_some() {
            let alive = if self.welcomed {
                Duration::from_secs(self.keepalive_secs * crate::constants::HERMES_KEEPALIVE_MISSES)
            } else {
                crate::constants::HERMES_WELCOME_TIMEOUT
            };
            at = at.min(self.last_message + alive);
        }
        if conns.iter().flatten().any(|conn| conn.ready) {
            at = at.min(self.telemetry_at);
        }
        // The verifier reads a snapshot, so somebody has to keep it in step
        // with the files: once a second while this daemon has a QUIC listener.
        if self.trust.is_some() {
            at = at.min(self.trust_at);
        }
        // A pairing attempt has a real deadline of its own, shorter than the
        // minute the loop would otherwise sleep for.
        for provisional in conns
            .iter()
            .flatten()
            .filter_map(|conn| conn.provisional.as_ref())
        {
            at = at.min(provisional.expires + Duration::from_millis(1));
        }
        at.saturating_duration_since(now)
            .max(Duration::from_millis(10))
    }

    async fn handle_conn(
        &mut self,
        event: riffle::ConnEvent,
        conns: &mut Vec<Option<riffle::Conn>>,
        ops: &FuturesUnordered<Op>,
    ) {
        match event {
            riffle::ConnEvent::Accepted(Ok(stream)) => {
                let id = insert_conn(conns, riffle::Conn::new(stream));
                if let Some(reader) = conns[id].as_mut().and_then(riffle::Conn::take_reader) {
                    ops.push(read_op(id, reader));
                }
                ops.push(accept_op(&self.accept));
            }
            // The listener is gone; do not re-arm into a spin.
            riffle::ConnEvent::Accepted(Err(_)) => {}
            riffle::ConnEvent::Read {
                conn: id,
                result,
                buf,
                reader,
            } => {
                match result {
                    Ok(0) | Err(_) => {
                        self.close_conn(conns, id);
                        return;
                    }
                    Ok(read) => {
                        let Some(conn) = conns[id].as_mut() else {
                            return;
                        };
                        // The read half is back for the next read.
                        conn.reader = Some(reader);
                        conn.decoder.push(&buf[..read]);
                    }
                }
                self.handle_frames(id, conns).await;
                if let Some(reader) = conns[id].as_mut().and_then(riffle::Conn::take_reader) {
                    ops.push(read_op(id, reader));
                }
            }
            riffle::ConnEvent::Write {
                conn: id,
                result,
                mut buf,
                writer,
            } => {
                let Some(conn) = conns[id].as_mut() else {
                    return;
                };
                conn.writer = Some(writer);
                conn.writing = false;
                if result.is_err() {
                    self.close_conn(conns, id);
                    return;
                }
                // `write_all` hands the buffer back untouched. It is the
                // *spare*, never `out`: bytes queued while the write was in
                // flight are still there and must not be replaced.
                buf.clear();
                conn.spare = buf;
            }
        }
    }

    /// Drops one connection and releases every pipeline its lanes claimed.
    ///
    /// Lanes are per connection, so a UI that goes away must not leave a
    /// watch-only runtime running: the claim is the only thing keeping it up.
    fn close_conn(&self, conns: &mut [Option<riffle::Conn>], id: usize) {
        let Some(mut conn) = conns[id].take() else {
            return;
        };
        // Collect first: stopping a producer mutates the connection while the
        // lane set is borrowed.
        let keys: Vec<StreamKey> = conn.lanes.iter().cloned().collect();
        for key in &keys {
            conn.stop_lane(key);
            self.registry.detach(key);
        }
    }

    /// Decodes and answers every complete frame already buffered.
    async fn handle_frames(&mut self, id: usize, conns: &mut [Option<riffle::Conn>]) {
        let mut close = false;
        loop {
            let Some(conn) = conns[id].as_mut() else {
                return;
            };
            let frame = match conn.decoder.next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(_) => {
                    close = true;
                    break;
                }
            };
            // Media is the daemon's direction. A client that sends a class-1
            // frame is not speaking this protocol, and the frame is not a
            // message to guess at.
            if frame.class() != siphon_shared::riffle::FrameClass::Control {
                warn!(target: "rpc", class = %frame.class(), "rpc: a client sent a media frame");
                close = true;
                break;
            }
            match siphon_shared::riffle::codec::decode::<siphon_shared::riffle::ClientMessage>(
                frame.payload(),
            ) {
                // Sent instead of `Hello`, and only on a provisional connection.
                Ok(siphon_shared::riffle::ClientMessage::PairConfirm { sas, name }) => {
                    if !self.confirm_pairing(conn, &sas, &name).await {
                        close = true;
                        break;
                    }
                }
                // A provisional connection may send that one message and
                // nothing else; the daemon serves it no state until the code
                // matches.
                Ok(_) if conn.provisional.is_some() => {
                    warn!(target: "rpc", "quic: a provisional connection sent something else");
                    close = true;
                    break;
                }
                Ok(siphon_shared::riffle::ClientMessage::Hello {
                    magic,
                    protocol,
                    app_version,
                }) => {
                    if magic != siphon_shared::riffle::MAGIC
                        || protocol != siphon_shared::riffle::PROTOCOL_VERSION
                    {
                        let reason = format!(
                            "protocol {protocol} (backend speaks {})",
                            siphon_shared::riffle::PROTOCOL_VERSION
                        );
                        close = !conn.push(&ServerMessage::Rejected(reason));
                        break;
                    }
                    info!(%app_version, "rpc: client connected");
                    // A fresh connection starts with no lanes; this only ever
                    // carries the snapshot's own (empty) set.
                    let snapshot = self.snapshot(&conn.lanes, conn.stats(Instant::now()));
                    let queued = conn.push(&ServerMessage::Welcome {
                        protocol: siphon_shared::riffle::PROTOCOL_VERSION,
                        server_version: env!("CARGO_PKG_VERSION").to_owned(),
                    }) && conn.push(&ServerMessage::Snapshot(Box::new(snapshot)));
                    if !queued {
                        close = true;
                        break;
                    }
                    conn.ready = true;
                }
                Ok(siphon_shared::riffle::ClientMessage::Request {
                    id: request_id,
                    request,
                }) => {
                    // The request handler owns the connection it arrived on:
                    // lanes are per connection, and a lane update must be
                    // written to that connection's queue rather than broadcast.
                    let result = self.handle_request(request, conn).await;
                    if !conn.push(&ServerMessage::Response {
                        id: request_id,
                        result,
                    }) {
                        close = true;
                        break;
                    }
                }
                Err(_) => {
                    close = true;
                    break;
                }
            }
        }
        if close {
            self.close_conn(conns, id);
        }
    }

    /// Answers one `PairConfirm`. `false` closes the connection.
    ///
    /// Three things must hold: the connection is provisional, the code repeats
    /// the one the daemon derived for this handshake, and the operator approved
    /// that code with `siphon-daemon pair`. The code is what rules out a man in
    /// the middle; the approval is what rules out a client the operator never
    /// looked at. Only then is the key written to `peers.json`, and the
    /// connection becomes an ordinary one.
    ///
    /// Both files are touched with `compio::fs`, because this runs on the
    /// reactor. Pairing happens once, on a human's keystroke, but a blocking
    /// read there would stall every other task all the same.
    async fn confirm_pairing(&self, conn: &mut riffle::Conn, sas: &str, name: &str) -> bool {
        let Some(provisional) = conn.provisional.as_ref() else {
            warn!(target: "rpc", "rpc: PairConfirm on a connection that was not pairing");
            return false;
        };
        if sas != provisional.sas {
            warn!(target: "rpc", "quic: the pairing code did not match");
            return false;
        }
        let Some(trust) = self.trust.as_ref() else {
            warn!(target: "rpc", "quic: pairing is not configured on this daemon");
            return false;
        };
        let approved = PairingWindow::read_async(trust.pairing())
            .await
            .is_some_and(|pairing| pairing.approved(&provisional.sas));
        if !approved {
            warn!(target: "rpc", "quic: the pairing code was not approved by an operator");
            return false;
        }
        let spki = provisional.spki.clone();
        let mut peers = match Peers::load_async(trust.peers()).await {
            Ok(peers) => peers,
            Err(error) => {
                warn!(target: "rpc", %error, "quic: could not read peers.json to pair a client");
                return false;
            }
        };
        if let Err(error) = peers.add_async(name, &spki).await {
            warn!(target: "rpc", %error, "quic: could not write peers.json");
            return false;
        }
        info!(target: "rpc", %name, %spki, "quic: paired a new client");

        conn.provisional = None;
        let snapshot = self.snapshot(&conn.lanes, conn.stats(Instant::now()));
        let queued = conn.push(&ServerMessage::Welcome {
            protocol: siphon_shared::riffle::PROTOCOL_VERSION,
            server_version: env!("CARGO_PKG_VERSION").to_owned(),
        }) && conn.push(&ServerMessage::Snapshot(Box::new(snapshot)));
        if queued {
            conn.ready = true;
            // The window has done its job: nothing else may be admitted with an
            // unknown key.
            if let Err(error) = PairingWindow::close_async(trust.pairing()).await {
                warn!(target: "rpc", %error, "quic: could not close the pairing window");
            }
            // Publish both changes now rather than at the next tick: the new
            // key is allowed from here on, and the closed window must stop
            // admitting unknown ones immediately.
            if let Err(error) = trust.refresh(&self.trust_shared).await {
                warn!(target: "rpc", %error, "quic: could not refresh the trust snapshot");
            }
        }
        queued
    }

    /// Drops provisional connections whose window has ended.
    fn expire_provisional(&self, conns: &mut [Option<riffle::Conn>]) {
        let now = Instant::now();
        let expired: Vec<usize> = conns
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                slot.as_ref()
                    .and_then(|conn| conn.provisional.as_ref())
                    .is_some_and(|provisional| provisional.expires <= now)
            })
            .map(|(id, _)| id)
            .collect();
        for id in expired {
            info!(target: "rpc", "quic: a pairing attempt timed out");
            self.close_conn(conns, id);
        }
    }

    /// Encodes each queued update once and hands the bytes to every connection.
    fn flush_bus(&mut self, conns: &mut [Option<riffle::Conn>], ops: &FuturesUnordered<Op>) {
        let mut overflowed: Vec<usize> = Vec::new();
        for broadcast in self.bus.take_pending() {
            self.frame.clear();
            if siphon_shared::riffle::codec::encode(
                &ServerMessage::Update(broadcast.update),
                &mut self.frame,
            )
            .is_err()
            {
                continue;
            }
            for conn in conns.iter_mut().flatten() {
                if conn.ready {
                    conn.out.borrow_mut().extend_from_slice(&self.frame);
                }
            }
        }
        for (id, slot) in conns.iter_mut().enumerate() {
            let Some(conn) = slot.as_mut() else {
                continue;
            };
            let out_len = conn.out.borrow().len();
            if out_len > riffle::MAX_OUT {
                // Closed after this loop: `close_conn` needs all of `conns`, and
                // this loop holds one connection at a time.
                overflowed.push(id);
                continue;
            }
            if conn.ready
                && !conn.writing
                && out_len > 0
                && let Some(writer) = conn.take_writer()
            {
                conn.writing = true;
                std::mem::swap(&mut *conn.out.borrow_mut(), &mut conn.spare);
                let buf = std::mem::take(&mut conn.spare);
                // The out buffer is empty again, so a pipe lane that stopped for
                // room may fill it. The write below carries what was there.
                conn.resume_media();
                ops.push(write_op(id, writer, buf));
            }
        }
        for id in overflowed {
            self.close_conn(conns, id);
        }
    }

    /// Runs one background future and delivers its result through the app
    /// queue, so no select ever polls a job future and drops a completed one.
    pub fn spawn_job<F: Future<Output = JobDone> + 'static>(&self, future: F) {
        let tx = self.jobs_tx.clone();
        compio::runtime::spawn(async move {
            let done = future.await;
            let _ = tx.send(Box::new(done));
        })
        .detach();
    }

    async fn handle_job(&mut self, done: JobDone) {
        match done {
            JobDone::AddResolved(login, result) => {
                self.complete_add_login(login, *result).await;
            }
            JobDone::LiveRefreshed(id, result) => {
                self.complete_live_refresh(id, result);
            }
            JobDone::SaveFinished(result) => {
                self.save_running = false;
                if let Err(error) = result {
                    self.set_error(format!("config save failed: {error}"));
                }
                if self.save_dirty {
                    self.spawn_save();
                }
            }
            JobDone::UpdateCheckFinished(result) => {
                self.update_busy = false;
                match result {
                    Ok(release) => {
                        if update::newer_than_current(&release) {
                            tracing::info!(target: "update", "new release: {}", release.version);
                            self.update = Some(release.clone());
                            self.bus
                                .publish(Update::Update(Some(update_view(&release))));
                        } else if self.update.take().is_some() {
                            self.bus.publish(Update::Update(None));
                        }
                    }
                    Err(error) => {
                        tracing::info!(target: "update", "check failed: {error}");
                    }
                }
            }
            JobDone::UpdateDownloadFinished(result) => self.finish_update_download(result),
            JobDone::SubscribeCheckDue(topic, sub_id, attempt) => {
                let unconfirmed = matches!(
                    self.sub_ids.get(&topic),
                    Some(entry)
                        if entry.sub_id == sub_id
                            && entry.attempt == attempt
                            && entry.state == SubState::Pending
                );
                if unconfirmed {
                    // A live socket with a dead topic never heals on its own:
                    // run it through the socket reconnect so backoff and the
                    // welcome replay retry the topic.
                    tracing::info!(
                        target: "hermes",
                        "subscription to {topic} not confirmed, reconnecting"
                    );
                    if self.socket.is_some() {
                        self.on_disconnect("subscription-unconfirmed").await;
                    }
                }
            }
            JobDone::ConnectFinished(outcome) => {
                self.complete_connect(*outcome).await;
            }
            JobDone::AvatarCached(id, avatar) => {
                self.avatars_pending.remove(&id);
                let Some(avatar) = avatar else {
                    return;
                };
                let flags = self
                    .config
                    .channels
                    .iter()
                    .find(|channel| channel.id == id)
                    .cloned();
                if let Some(user) = self.channels.get_mut(&id) {
                    user.avatar = Some(avatar);
                    let view = user.view(flags.as_ref());
                    self.bus.publish(Update::ChannelUpsert(Box::new(view)));
                }
            }
            JobDone::Registry(job) => self.registry.apply(job).await,
        }
    }

    async fn handle_request(
        &mut self,
        request: Request,
        conn: &mut riffle::Conn,
    ) -> Result<Response, RpcError> {
        match request {
            Request::AddLogin { login } => {
                self.request_add_login(&login);
                Ok(Response::Ack)
            }
            Request::RemoveChannel(channel) => self.apply_remove_channel(channel),
            Request::SetRecordDir(dir) => {
                // An empty path is how a settings field clears itself.
                let dir = dir.filter(|dir| !dir.trim().is_empty());
                self.config.record_dir.clone_from(&dir);
                self.registry.set_output(dir.map(PathBuf::from));
                self.publish_config();
                self.queue_save();
                Ok(Response::Ack)
            }
            Request::SetRecordDualFormat(value) => {
                self.config.record_dual_format = value;
                self.registry.set_dual_format(value);
                self.publish_config();
                self.queue_save();
                Ok(Response::Ack)
            }
            Request::SetRecord { channel, record } => self.apply_set_record(channel, record),
            Request::ClearError => {
                self.clear_error();
                Ok(Response::Ack)
            }
            Request::ApplyUpdate => {
                self.handle_apply_update();
                Ok(Response::Ack)
            }
            Request::PipelineStop { key, force } => {
                self.registry.stop_pipeline(&key, force);
                Ok(Response::Ack)
            }
            Request::PipelineRestart(key) => {
                self.registry.restart_pipeline(&key);
                Ok(Response::Ack)
            }
            Request::DisconnectViewers(key) => {
                self.registry.disconnect_viewers(&key);
                Ok(Response::Ack)
            }
            Request::RecordOnce { login, quality } => self.apply_record_once(&login, quality),
            Request::StreamOpen { key, playback } => self.stream_open(conn, key, playback).await,
            Request::StreamUpdate { key, playback } => self.stream_update(conn, &key, playback),
            Request::StreamClose { key } => self.stream_close(conn, key),
            Request::StreamSeek { key, target } => self.stream_seek(conn, &key, target).await,
            Request::GetSnapshot => Ok(Response::Snapshot(Box::new(
                self.snapshot(&conn.lanes, conn.stats(Instant::now())),
            ))),
            Request::GetStreamDetail(key) => self
                .registry
                .detail(&key, 256)
                .map_or(Err(RpcError::NotFound), |detail| {
                    Ok(Response::StreamDetail(Box::new(detail)))
                }),
            Request::ListSegments { key, limit } => self
                .registry
                .segments(&key, usize::try_from(limit).unwrap_or(usize::MAX))
                .map_or(Err(RpcError::NotFound), |segments| {
                    Ok(Response::Segments(segments))
                }),
            Request::GetArena => Ok(Response::Arena(self.registry.arena_summary())),
            Request::GetDownloader => Ok(Response::Downloader(self.registry.downloader_summary())),
            Request::GetDisk(key) => self
                .registry
                .disk_view(&key)
                .map_or(Err(RpcError::NotFound), |view| Ok(Response::Disk(view))),
            Request::GetAvatar { id } => get_avatar(&id).await,
        }
    }

    /// Attaches a lane, or updates one already attached on this connection,
    /// and starts the task that feeds it.
    ///
    /// Re-opening is idempotent: the lane set is checked before `attach`,
    /// because the registry counts claims on a pipeline and knows nothing about
    /// lanes, so a second `attach` would be a second claim that no close would
    /// ever release.
    ///
    /// `PlaybackPosition::Live` starts at the next published segment: there is
    /// nothing to align, because that segment has not arrived yet. An absolute
    /// position is a seek with no reply, so it is GOP-aligned exactly like one —
    /// the lane starts at the keyframe at or before the target, in the segment
    /// that holds it, and its producer is (re)started there.
    ///
    /// A connection whose carrier cannot send media at all is a typed refusal
    /// *before* the pipeline is attached: a lane that attached and then fed
    /// nothing would look like a live stream that produces no picture. Only a
    /// QUIC peer without DATAGRAM is that; the pipe carries class-1 frames on
    /// the connection the control plane is already using.
    async fn stream_open(
        &self,
        conn: &mut riffle::Conn,
        key: StreamKey,
        playback: StreamPlayback,
    ) -> Result<Response, RpcError> {
        let Some(media) = conn.media(Rc::clone(&self.media_wake)) else {
            return Err(quic_without_datagrams());
        };
        let now = Instant::now();
        let opened = !conn.lanes.contains(&key);
        let (place, runtime) = if opened {
            let runtime = self.registry.attach(&key).await?;
            (Some(runtime.place()), Some(runtime))
        } else {
            (self.registry.place(&key), None)
        };
        let target_ms = position_of(playback.position, place);
        let point = match playback.position {
            PlaybackPosition::Live => None,
            PlaybackPosition::Absolute { .. } => self.registry.seek_point(&key, target_ms).await,
        };
        let live_edge_ms = place.map_or(0, |place| place.position_ms);
        // The estimate starts where the daemon will actually serve, so a starved
        // check and the push rule agree with the first access unit.
        let position_ms = point.map_or(target_ms, |point| point.pts_ms);
        conn.lanes.open(
            key.clone(),
            playback,
            position_ms,
            live_edge_ms,
            now,
            wall_ms(),
        );
        if let Some(point) = point {
            conn.lanes.begin_at(
                &key,
                Start {
                    sequence: point.sequence,
                    pts_ms: point.pts_ms,
                },
                live_edge_ms,
                now,
            );
        }
        // A fresh lane needs a producer; so does one re-opened at an absolute
        // position, because where it starts moved.
        if opened || point.is_some() {
            let Some(runtime) = runtime.or_else(|| self.registry.stream(&key)) else {
                conn.lanes.remove(&key);
                self.registry.detach(&key);
                return Err(RpcError::Failed(
                    "the pipeline stopped before the lane could start".to_owned(),
                ));
            };
            let Some(handle) = conn.lanes.handle(&key) else {
                return Err(RpcError::NotFound);
            };
            let Some(consumer) = runtime.lane_consumer() else {
                conn.lanes.remove(&key);
                self.registry.detach(&key);
                return Err(RpcError::Failed(
                    "the pipeline has no scheduler left".to_owned(),
                ));
            };
            if !conn.start_lane(key.clone(), handle, consumer, Rc::new(media)) {
                conn.lanes.remove(&key);
                self.registry.detach(&key);
                return Err(RpcError::Failed(
                    "the pipeline stopped before the lane could register".to_owned(),
                ));
            }
        }
        if !Self::push_lane(conn, &key, now, place) {
            return Err(RpcError::Disconnected);
        }
        Ok(Response::Ack)
    }

    /// The periodic correction. A key this connection never opened is
    /// `NotFound`: an update states where a lane is, it does not attach one.
    fn stream_update(
        &self,
        conn: &mut riffle::Conn,
        key: &StreamKey,
        playback: StreamPlayback,
    ) -> Result<Response, RpcError> {
        if !conn.lanes.contains(key) {
            return Err(RpcError::NotFound);
        }
        let now = Instant::now();
        let place = self.registry.place(key);
        let position_ms = position_of(playback.position, place);
        let live_edge_ms = place.map_or(0, |place| place.position_ms);
        conn.lanes.open(
            key.clone(),
            playback,
            position_ms,
            live_edge_ms,
            now,
            wall_ms(),
        );
        if !Self::push_lane(conn, key, now, place) {
            return Err(RpcError::Disconnected);
        }
        Ok(Response::Ack)
    }

    /// Closes a lane, stops its producer and releases its claim. Closing twice
    /// is an ack, not an error: the lane is simply not there any more.
    fn stream_close(&self, conn: &mut riffle::Conn, key: StreamKey) -> Result<Response, RpcError> {
        if conn.lanes.remove(&key) {
            conn.stop_lane(&key);
            self.registry.detach(&key);
            if !conn.push(&ServerMessage::Update(Update::LaneRemove(key))) {
                return Err(RpcError::Disconnected);
            }
        }
        Ok(Response::Ack)
    }

    /// Answers where the daemon will start serving, and moves the lane there.
    ///
    /// The answer is a keyframe, not the target: the last video keyframe at or
    /// before the target in the retained window, plus the playlist sequence of
    /// the segment that holds it. The lane's estimate moves to that keyframe and
    /// its producer is restarted from that segment, so the first access unit the
    /// client receives is the one the answer named.
    ///
    /// A target older than the window is clamped forward to the window's oldest
    /// segment and reported as `trimmed`, because the daemon cannot serve what it
    /// no longer retains. A gap between the served start and the target is the
    /// client's to discard, exactly as on a forward seek.
    ///
    /// [`LaneState::Starved`]: siphon_shared::riffle::LaneState::Starved
    async fn stream_seek(
        &self,
        conn: &mut riffle::Conn,
        key: &StreamKey,
        target: PlaybackPosition,
    ) -> Result<Response, RpcError> {
        if !conn.lanes.contains(key) {
            return Err(RpcError::NotFound);
        }
        let Some(media) = conn.media(Rc::clone(&self.media_wake)) else {
            return Err(quic_without_datagrams());
        };
        let now = Instant::now();
        let place = self.registry.place(key);
        let target_ms = position_of(target, place);
        // A pipeline that retains nothing yet has no keyframe to name. The
        // answer is where it will start, and it is a clamp: the target was not
        // honored as asked.
        let Some(point) = self.registry.seek_point(key, target_ms).await else {
            return Ok(Response::StreamStarted(StreamStarted {
                start_pts_ms: target_ms,
                start_sequence: place.map_or(0, |place| place.sequence),
                trimmed: true,
            }));
        };
        let start = Start {
            sequence: point.sequence,
            pts_ms: point.pts_ms,
        };
        let live_edge_ms = place.map_or(0, |place| place.position_ms);
        if !conn.lanes.begin_at(key, start, live_edge_ms, now) {
            return Err(RpcError::NotFound);
        }
        let Some(runtime) = self.registry.stream(key) else {
            return Err(RpcError::Failed(
                "the pipeline stopped before the seek could be served".to_owned(),
            ));
        };
        let Some(handle) = conn.lanes.handle(key) else {
            return Err(RpcError::NotFound);
        };
        let Some(consumer) = runtime.lane_consumer() else {
            return Err(RpcError::Failed(
                "the pipeline has no scheduler left".to_owned(),
            ));
        };
        if !conn.start_lane(key.clone(), handle, consumer, Rc::new(media)) {
            return Err(RpcError::Failed(
                "the pipeline stopped before the lane could register".to_owned(),
            ));
        }
        if !Self::push_lane(conn, key, now, place) {
            return Err(RpcError::Disconnected);
        }
        Ok(Response::StreamStarted(StreamStarted {
            start_pts_ms: point.pts_ms,
            start_sequence: point.sequence,
            trimmed: point.trimmed,
        }))
    }

    /// Writes one lane row straight to the owning connection's queue.
    ///
    /// The update bus broadcasts the same encoded bytes to every client, so a
    /// lane update must never be published there: it would leak this
    /// connection's lanes into every other UI. Everything the bus carries is
    /// daemon-wide and identical for every connection; a lane is neither.
    fn push_lane(
        conn: &riffle::Conn,
        key: &StreamKey,
        now: Instant,
        place: Option<LanePlace>,
    ) -> bool {
        let window_start_ms = place.map_or(0, |place| place.window_start_ms);
        let Some(view) = conn.lanes.view(key, now, window_start_ms) else {
            return true;
        };
        conn.push(&ServerMessage::Update(Update::Lane(view)))
    }

    /// Snapshot built on demand from app state, the registry and metrics.
    ///
    /// `lanes` is the asking connection's own set: lanes are per connection, so
    /// there is no global list to read here.
    #[must_use]
    pub fn snapshot(&self, lanes: &Lanes, stats: ConnStats) -> Snapshot {
        let mut channels = Vec::with_capacity(self.config.channels.len());
        for channel in &self.config.channels {
            let view = self.channels.get(&channel.id).map_or_else(
                || ChannelView {
                    id: channel.id,
                    login: channel.login.clone(),
                    display_name: channel
                        .display_name
                        .clone()
                        .unwrap_or_else(|| channel.login.clone()),
                    chat_color: gql::fallback_color(channel.id),
                    avatar: None,
                    live: false,
                    title: None,
                    game: None,
                    game_id: None,
                    viewers: None,
                    collaboration_viewers: None,
                    stream_start_ms: None,
                    last_live_ms: None,
                    stream_id: None,
                    error: None,
                    record: channel.record,
                },
                |detail| detail.view(Some(channel)),
            );
            channels.push(view);
        }
        Snapshot {
            config: self.config.clone(),
            conn: self.conn,
            conn_stats: stats,
            channels,
            pending: self.pending.clone(),
            pipelines: self.registry.views(),
            lanes: lanes.views(Instant::now(), |key| self.registry.window_start_ms(key)),
            arena: self.registry.arena_summary(),
            downloader: self.registry.downloader_summary(),
            logs: self.bus.logs(),
            error: self.error.clone(),
            update: self.update.as_ref().map(update_view),
        }
    }

    pub fn publish_config(&self) {
        self.bus.publish(Update::Config(self.config.clone()));
    }

    pub fn publish_pending(&self) {
        self.bus.publish(Update::Pending(self.pending.clone()));
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        let message = message.into();
        tracing::info!(target: "app", "{message}");
        self.bus.log(LogLevel::Error, "app", message.clone());
        self.error = Some(message.clone());
        self.bus.publish(Update::Error(Some(message)));
    }

    pub fn clear_error(&mut self) {
        if self.error.take().is_some() {
            self.bus.publish(Update::Error(None));
        }
    }

    /// Re-reads `peers.json` and `pairing.json` into the verifier's snapshot.
    ///
    /// The verifier cannot read a file — rustls calls it synchronously on this
    /// thread — so this tick is what makes the files live: an operator's edit to
    /// `peers.json` takes effect within a second, and revoking a device no
    /// longer needs a restart. `compio::fs`, never `std::fs`: this is the
    /// reactor.
    ///
    /// A file that does not parse keeps the previous snapshot. An edit in
    /// progress must not revoke every client, and a malformed allowlist is
    /// already an error at startup, where the operator is looking.
    pub(crate) async fn refresh_trust(&mut self) {
        // The tick can be much faster than a second (a live connection's
        // keepalive and telemetry deadlines are shorter), so the snapshot has
        // its own due time rather than being re-read on every deadline.
        if self.trust.is_none() || Instant::now() < self.trust_at {
            return;
        }
        self.trust_at = Instant::now() + crate::constants::TRUST_REFRESH;
        let Some(trust) = self.trust.as_ref() else {
            return;
        };
        match trust.refresh(&self.trust_shared).await {
            Ok(true) => info!(target: "rpc", "quic: the trust snapshot changed"),
            Ok(false) => {}
            Err(error) => warn!(
                target: "rpc",
                %error,
                "quic: keeping the last trust snapshot, the files did not parse"
            ),
        }
    }

    /// Publishes telemetry, so a UI showing live counters updates without polling.
    pub(crate) fn publish_telemetry(&mut self) {
        let arena = self.registry.arena_summary();
        if arena != self.last_arena {
            self.last_arena = arena;
            self.bus.publish(Update::Arena(arena));
        }
        let downloader = self.registry.downloader_summary();
        if downloader != self.last_downloader {
            self.last_downloader = downloader;
            self.bus.publish(Update::Downloader(downloader));
        }
        let views = self.registry.views();
        let mut seen = Vec::with_capacity(views.len());
        for view in views {
            let changed = self
                .last_pipelines
                .get(&view.key)
                .is_none_or(|old| old != &view);
            if changed {
                self.bus.publish(Update::PipelineUpsert(view.clone()));
            }
            seen.push(view.key.clone());
            self.last_pipelines.insert(view.key.clone(), view);
        }
        let removed: Vec<StreamKey> = self
            .last_pipelines
            .keys()
            .filter(|key| !seen.contains(key))
            .cloned()
            .collect();
        for key in removed {
            self.last_pipelines.remove(&key);
            self.bus.publish(Update::PipelineRemove(key));
        }
    }

    pub fn publish_conn_health(&mut self) {
        let conn = if self.socket.is_some() && self.welcomed {
            ConnHealth::Connected
        } else {
            ConnHealth::Connecting
        };
        self.conn = conn;
        self.bus.publish(Update::Conn(conn));
    }

    pub fn publish_no_channels(&mut self) {
        self.conn = ConnHealth::NoChannels;
        self.bus.publish(Update::Conn(ConnHealth::NoChannels));
    }

    fn queue_save(&mut self) {
        self.save_dirty = true;
        if !self.save_running {
            self.spawn_save();
        }
    }

    fn spawn_save(&mut self) {
        self.save_running = true;
        self.save_dirty = false;
        let config = self.config.clone();
        let path = self.config_path.clone();
        self.spawn_job(async move { JobDone::SaveFinished(config::save(&config, &path).await) });
    }

    /// Publishes one transition, with the row as it stands.
    ///
    /// A notification is an edge, and `ChannelUpsert` is coalesced per channel,
    /// so the event carries the whole view rather than relying on the update
    /// that accompanies it. The client applies its own switches and word filter.
    pub(crate) fn publish_event(
        &self,
        kind: StreamEventKind,
        previous: Option<String>,
        view: ChannelView,
    ) {
        self.bus.publish(Update::StreamEvent(Box::new(StreamEvent {
            kind,
            at_ms: now_ms(),
            previous,
            view: Box::new(view),
        })));
    }

    /// Validates an add request and queues the single resolve. Duplicates,
    /// in-flight resolves and empty input are ignored without touching disk.
    pub fn request_add_login(&mut self, login: &str) -> Option<String> {
        let login = login.trim().to_lowercase();
        if login.chars().count() > 25 {
            self.set_error("a Twitch login is at most 25 characters");
            return None;
        }
        if login.is_empty()
            || self
                .config
                .channels
                .iter()
                .any(|channel| channel.login.eq_ignore_ascii_case(&login))
            || self
                .pending
                .iter()
                .any(|pending| pending.eq_ignore_ascii_case(&login))
        {
            return None;
        }
        self.pending.push(login.clone());
        self.clear_error();
        self.publish_pending();
        let client = Rc::clone(&self.client);
        let client_id = self.client_id.clone();
        let job_login = login.clone();
        self.spawn_job(async move {
            let result = gql::fetch_channel(&client, client_id.as_deref(), &job_login).await;
            JobDone::AddResolved(job_login, Box::new(result))
        });
        Some(login)
    }

    pub(crate) async fn complete_add_login(
        &mut self,
        login: String,
        result: Result<Option<ChannelDetail>, GqlError>,
    ) {
        if !self
            .pending
            .iter()
            .any(|pending| pending.eq_ignore_ascii_case(&login))
        {
            return;
        }
        self.pending
            .retain(|pending| !pending.eq_ignore_ascii_case(&login));
        self.publish_pending();
        match result {
            Ok(Some(user)) => {
                tracing::info!(
                    target: "config",
                    "resolved {} to id {} ({})",
                    login,
                    user.id,
                    user.display_name
                );
                if self
                    .config
                    .channels
                    .iter()
                    .any(|existing| existing.id == user.id)
                {
                    return;
                }
                let id = user.id;
                self.bus.log(
                    LogLevel::Info,
                    "channel",
                    format!("added {} ({})", user.login, id),
                );
                self.config.channels.push(siphon_shared::riffle::Channel {
                    login: user.login.clone(),
                    id,
                    display_name: Some(user.display_name.clone()),
                    record: false,
                });
                self.publish_config();
                self.track_channel(user);
                self.queue_save();
                self.ensure_subs(id);
                self.sync_channels();
                if self.welcomed {
                    self.subscribe_channel(id).await;
                }
            }
            Ok(None) => {
                tracing::info!(
                    target: "config",
                    "gql returned no channel named {login}, is it a typo?"
                );
                self.set_error(format!("channel {login} not found"));
            }
            Err(error) => {
                tracing::info!(target: "config", "channel resolution failed: {error}");
                self.set_error(format!("failed to resolve {login}: {error}"));
            }
        }
    }

    fn apply_remove_channel(&mut self, channel: u64) -> Result<Response, RpcError> {
        let Some(index) = self
            .config
            .channels
            .iter()
            .position(|candidate| candidate.id == channel)
        else {
            return Err(RpcError::NotFound);
        };
        let removed = self.config.channels.remove(index);
        self.registry.set_record(&removed.login, false);
        self.sync_channels();
        self.bus.log(
            LogLevel::Info,
            "channel",
            format!("removed {} ({})", removed.login, removed.id),
        );
        let removed = self.drop_channel(channel);
        self.pending_unsubscribes.extend(removed);
        self.publish_config();
        self.bus.publish(Update::ChannelRemove(channel));
        self.queue_save();
        Ok(Response::Ack)
    }

    /// Drops ids that no longer resolve (deleted/renamed): they disappear from
    /// the persisted list with an error instead of lingering.
    pub fn apply_prune_ids(&mut self, ids: &[u64]) {
        let pruned: Vec<String> = self
            .config
            .channels
            .iter()
            .filter(|channel| ids.contains(&channel.id))
            .map(|channel| {
                channel
                    .display_name
                    .clone()
                    .unwrap_or_else(|| channel.login.clone())
            })
            .collect();
        if pruned.is_empty() {
            return;
        }
        for id in ids {
            self.drop_channel(*id);
        }
        self.config
            .channels
            .retain(|channel| !ids.contains(&channel.id));
        self.publish_config();
        self.queue_save();
        let names = pruned.join(", ");
        if pruned.len() == 1 {
            self.set_error(format!("{names} no longer resolves and was removed"));
        } else {
            self.set_error(format!("{names} no longer resolve and were removed"));
        }
    }

    fn apply_record_once(
        &self,
        login: &str,
        quality: Option<String>,
    ) -> Result<Response, RpcError> {
        if !self.registry.can_record() {
            return Err(RpcError::Failed(
                "recording needs an output directory".to_owned(),
            ));
        }
        let login = login.trim().to_lowercase();
        if login.is_empty() {
            return Err(RpcError::BadRequest("empty login".to_owned()));
        }
        let Some(channel) = self.config.channels.iter().find(|c| c.login == login) else {
            return Err(RpcError::NotFound);
        };
        self.registry
            .record_once(StreamKey::new(channel.id, quality));
        Ok(Response::Ack)
    }

    /// Mirrors the persisted follow list onto the registry's channel table.
    fn sync_channels(&self) {
        self.registry.sync_channels(&self.config.channels);
    }

    fn apply_set_record(&mut self, channel: u64, record: bool) -> Result<Response, RpcError> {
        let Some(entry) = self
            .config
            .channels
            .iter_mut()
            .find(|candidate| candidate.id == channel)
        else {
            return Err(RpcError::NotFound);
        };
        entry.record = record;
        let flags = entry.clone();
        let login = flags.login.clone();
        self.publish_config();
        self.publish_channel_flags(&flags);
        self.queue_save();
        self.bus.log(
            LogLevel::Info,
            "channel",
            format!(
                "recording {} for {login}",
                if record { "enabled" } else { "disabled" }
            ),
        );
        if record {
            if self.registry.can_record() {
                self.registry.set_record(&login, true);
            } else {
                self.set_error("recording needs an output directory");
            }
        } else {
            self.registry.set_record(&login, false);
        }
        Ok(Response::Ack)
    }

    /// Re-sends one channel row after a switch moved, so a controlled toggle
    /// in the UI follows the backend rather than snapping back.
    fn publish_channel_flags(&self, flags: &siphon_shared::riffle::Channel) {
        if let Some(detail) = self.channels.get(&flags.id) {
            self.bus
                .publish(Update::ChannelUpsert(Box::new(detail.view(Some(flags)))));
        }
    }

    fn handle_apply_update(&mut self) {
        let Some(offer) = self.update.clone() else {
            return;
        };
        if self.update_busy {
            return;
        }
        let installer = matches!(update::install_mode(), update::InstallMode::Installed)
            .then(|| offer.installer_url.clone())
            .flatten();
        if let Some(url) = installer {
            self.update_busy = true;
            let client = Rc::clone(&self.client);
            let version = offer.version;
            self.spawn_job(async move {
                JobDone::UpdateDownloadFinished(
                    update::download_installer(&client, &url, &version).await,
                )
            });
        } else {
            update::open_page(&offer.page_url);
        }
    }

    fn finish_update_download(&mut self, result: Result<PathBuf, update::UpdateError>) {
        match result {
            Ok(path) => match update::install_and_relaunch(&path) {
                Ok(()) => {
                    tracing::info!(target: "update", "installer launched, stopping for upgrade");
                    // The RTSP listener has no shutdown path; exiting releases
                    // the binaries the installer replaces.
                    std::process::exit(0);
                }
                Err(error) => {
                    self.update_busy = false;
                    self.set_error(format!("couldn't launch installer: {error}"));
                }
            },
            Err(error) => {
                self.update_busy = false;
                self.set_error(format!("update download failed: {error}"));
            }
        }
    }

    pub fn push_update_check(&mut self) {
        self.update_check_at = Instant::now() + update::CHECK_INTERVAL;
        if self.update_busy {
            return;
        }
        self.update_busy = true;
        let client = Rc::clone(&self.client);
        self.spawn_job(
            async move { JobDone::UpdateCheckFinished(update::fetch_latest(&client).await) },
        );
    }
}

fn update_view(release: &update::Release) -> UpdateView {
    UpdateView {
        version: release.version.to_string(),
        page_url: release.page_url.clone(),
        installer: release.installer_url.is_some(),
    }
}

/// The position a lane is at from what it reported.
///
/// `Live` is the pipeline's edge at that moment: the client cannot name a
/// number it does not have, so the daemon resolves it.
fn position_of(position: PlaybackPosition, place: Option<LanePlace>) -> u64 {
    match position {
        PlaybackPosition::Live => place.map_or(0, |place| place.position_ms),
        PlaybackPosition::Absolute { pts_ms } => pts_ms,
    }
}

/// Wall-clock milliseconds for [`siphon_shared::riffle::LaneView::started_ms`],
/// which is unsigned while [`now_ms`] is signed for the channel rows.
fn wall_ms() -> u64 {
    u64::try_from(now_ms()).unwrap_or_default()
}

/// The refusal a QUIC connection that never negotiated DATAGRAM gets.
fn quic_without_datagrams() -> RpcError {
    RpcError::NoMediaCarrier("this connection did not negotiate QUIC DATAGRAM".to_owned())
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::arena::ArenaSet;
    use crate::metrics::DownloaderMetrics;
    use crate::registry::Options as RegistryOptions;
    use crate::registry::Recording;

    fn app() -> App {
        let (downloaders, _downloaders_rx) = kanal::bounded(1);
        let (_accept, accept_rx) = kanal::unbounded();
        let (jobs_tx, jobs_rx) = kanal::unbounded();
        App::new(AppInit {
            config: Config::default(),
            config_path: PathBuf::from("config.json"),
            client: Rc::new(Client::new()),
            registry: Registry::new(
                Rc::new(Client::new()),
                Rc::new(ArenaSet::new()),
                downloaders.to_async(),
                Rc::new(DownloaderMetrics::new(1)),
                RegistryOptions {
                    client_id: None,
                    auth_token: None,
                    low_latency: true,
                    recording: Recording {
                        output: None,
                        dual_format: false,
                    },
                    notify: None,
                    keep_runs: false,
                },
                Bus::new(),
                jobs_tx.clone(),
            ),
            bus: Bus::new(),
            accept: accept_rx.to_async(),
            jobs: jobs_rx.to_async(),
            jobs_tx,
            client_id: None,
            trust: None,
            trust_shared: SharedTrust::default(),
        })
    }

    const fn playback() -> StreamPlayback {
        StreamPlayback {
            position: PlaybackPosition::Absolute { pts_ms: 1_000 },
            rate: 1.0,
            buffer_ms: 30_000,
        }
    }

    /// Lanes are per connection, so the snapshot is built from the asking
    /// connection's set and never from a daemon-wide one.
    #[test]
    fn a_snapshot_carries_only_the_asking_connections_lanes() {
        let app = app();
        let mut mine = Lanes::default();
        mine.open(
            StreamKey::new(1, None),
            playback(),
            1_000,
            0,
            Instant::now(),
            0,
        );
        let mut theirs = Lanes::default();
        theirs.open(
            StreamKey::new(2, None),
            playback(),
            1_000,
            0,
            Instant::now(),
            0,
        );

        let mine_view = app.snapshot(&mine, ConnStats::default()).lanes;
        assert_eq!(mine_view.len(), 1);
        assert_eq!(mine_view[0].key.channel, 1);

        let theirs_view = app.snapshot(&theirs, ConnStats::default()).lanes;
        assert_eq!(theirs_view.len(), 1);
        assert_eq!(theirs_view[0].key.channel, 2);

        assert_eq!(
            app.snapshot(&Lanes::default(), ConnStats::default())
                .lanes
                .len(),
            0
        );
    }
}

/// Reads one cached avatar for a client.
///
/// The read is asynchronous, like every other file the daemon touches: the app
/// loop is the reactor's own task, and a picture is a disk read rather than a
/// `memcpy`. The id is checked against [`crate::avatar::path_for`] first, because
/// it arrives from the client and becomes a path.
///
/// Nothing is re-encoded or resized: the daemon has no image encoder, so the
/// bytes are the ones it downloaded and the client scales them where it draws.
async fn get_avatar(id: &str) -> Result<Response, RpcError> {
    let path = crate::avatar::path_for(id).ok_or(RpcError::NotFound)?;
    let metadata = compio::fs::metadata(&path)
        .await
        .map_err(|_| RpcError::NotFound)?;
    let cap = u64::try_from(MAX_AVATAR).unwrap_or(u64::MAX);
    if metadata.len() > cap {
        return Err(RpcError::Failed(format!(
            "avatar is {} bytes, over the {MAX_AVATAR} byte cap",
            metadata.len()
        )));
    }
    compio::fs::read(&path)
        .await
        .map(Response::Avatar)
        .map_err(|error| RpcError::Failed(format!("avatar read failed: {error}")))
}
