//! The UI-side RPC worker: connect, forward snapshots and updates, send intents.
//!
//! The worker runs on its own compio thread. The UI thread only renders the
//! latest snapshot and sends [`Request`]s; it never blocks on the socket.

#![allow(clippy::future_not_send)]

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;

use siphon_shared::riffle::ArenaSummary;
use siphon_shared::riffle::ChannelView;
use siphon_shared::riffle::Client;
use siphon_shared::riffle::ClientEvent;
use siphon_shared::riffle::ConnHealth;
use siphon_shared::riffle::DownloaderSummary;
use siphon_shared::riffle::LaneView;
use siphon_shared::riffle::LogEntry;
use siphon_shared::riffle::Pairing;
use siphon_shared::riffle::PipelineView;
pub use siphon_shared::riffle::Request;
use siphon_shared::riffle::Response;
use siphon_shared::riffle::Snapshot;
use siphon_shared::riffle::StreamEvent;
use siphon_shared::riffle::StreamEventKind;
use siphon_shared::riffle::StreamKey;
use siphon_shared::riffle::StreamStarted;
use siphon_shared::riffle::Update;
use siphon_shared::riffle::UpdateView;
use siphon_shared::settings::ChannelPrefs;
use siphon_shared::settings::ClientSettings;
use siphon_shared::settings::Connection;
use zgui::reactive::LocalStorage;
use zgui::reactive::RwSignal;
use zgui::reactive::prelude::*;

use crate::media::MediaView;

/// One snapshot, entity update, or fetched picture for the UI thread.
#[derive(Debug, Clone)]
pub enum UiEvent {
    Snapshot(Box<Snapshot>),
    Update(Box<Update>),
    /// One avatar's bytes landed in this client's cache: the content id and the
    /// local file the image element can read.
    Avatar {
        id: String,
        path: Option<String>,
    },
    /// The daemon's answer to a `StreamSeek`: where it will actually start,
    /// which is not the target the client asked for.
    Started {
        key: StreamKey,
        started: StreamStarted,
    },
    /// A provisional pairing connection is up: this is the code both sides
    /// derived, which the person compares with the daemon's terminal before
    /// anything is sent over it.
    PairCode {
        sas: String,
    },
    /// A pairing finished. The worker connects to that daemon from now on; the
    /// window is told so it can write the connection down.
    Paired {
        endpoint: String,
        fingerprint: String,
        label: String,
    },
    /// A pairing step failed, or the typed code was not the one the handshake
    /// derived. The provisional connection is already gone.
    PairFailed(String),
    /// The receive side's counters and the last access unit it reassembled.
    /// Client-side numbers: the daemon's `ConnStats` is the send side.
    Media(MediaView),
}

/// A single-waiter wake, shared between the UI thread (intent sender) and the
/// RPC thread (worker). `Waker` is thread-safe, so the slot is a mutex.
pub struct Wake {
    waker: Mutex<Option<Waker>>,
}

impl Wake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            waker: Mutex::new(None),
        })
    }

    fn park(&self, context: &std::task::Context<'_>) {
        *self
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(context.waker().clone());
    }

    fn wake(&self) {
        let waker = self
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// One thing the window asks the RPC worker to do.
///
/// An RPC intent travels to the daemon; the pairing steps are the worker's own
/// business, because the provisional connection and this client's identity live
/// there and nothing about them is on the wire until the code matches.
#[derive(Debug, Clone)]
pub enum Intent {
    Rpc(Request),
    /// Completes a provisional handshake with a daemon's pairing window.
    PairBegin {
        endpoint: String,
        pin: String,
    },
    /// Sends the code back, if it is the one this handshake derived.
    PairConfirm {
        code: String,
    },
}

/// The UI's sending end for intents.
#[derive(Clone)]
pub struct IntentSender {
    tx: kanal::Sender<Intent>,
    wake: Arc<Wake>,
}

impl IntentSender {
    /// Queues an intent and wakes the worker. The request stays in the channel
    /// until the worker drains it, so a wake that races an update cannot drop a
    /// user action.
    ///
    /// Returns `false` when the queue is full or the worker is gone; the caller
    /// is free to ignore that, the intent will simply not be applied.
    pub fn send(&self, request: Request) -> bool {
        self.queue(Intent::Rpc(request))
    }

    fn queue(&self, intent: Intent) -> bool {
        let queued = matches!(self.tx.try_send(intent), Ok(true));
        self.wake.wake();
        queued
    }

    /// Opens a provisional connection to a daemon whose pairing window is open.
    ///
    /// The code is not known yet: it exists once the handshake has completed, so
    /// the worker answers it as [`UiEvent::PairCode`] and waits for
    /// [`Self::pair_confirm`].
    pub fn pair_begin(&self, endpoint: String, pin: String) {
        self.queue(Intent::PairBegin { endpoint, pin });
    }

    /// Sends the code a person read off the daemon's terminal.
    ///
    /// The worker compares it with the one it derived before spending the one
    /// attempt the pairing window allows.
    pub fn pair_confirm(&self, code: String) {
        self.queue(Intent::PairConfirm { code });
    }

    /// Asks the worker for one avatar's bytes.
    ///
    /// A query rather than an intent, but it travels the same queue: the worker
    /// fetches it in the background and reports the local path as
    /// [`UiEvent::Avatar`], so the frame loop never waits on a picture.
    pub fn fetch_avatar(&self, id: &str) {
        let _ = self.send(Request::GetAvatar { id: id.to_owned() });
    }
}

/// Creates the intent channel, its sender and the shared wake.
#[must_use]
pub fn intent_channel() -> (IntentSender, kanal::Receiver<Intent>, Arc<Wake>) {
    let (tx, rx) = kanal::bounded(INTENT_CAP);
    let wake = Wake::new();
    (
        IntentSender {
            tx,
            wake: Arc::clone(&wake),
        },
        rx,
        wake,
    )
}

/// How many notifications and log lines the window keeps.
const NOTIFICATION_CAP: usize = 64;
const LOG_CAP: usize = 512;

/// A fresh snapshot for the first frame.
#[must_use]
pub fn empty_snapshot() -> Snapshot {
    Snapshot::default()
}

/// How many events may sit between the RPC worker and the UI thread.
///
/// Past this the UI is further behind than a replay can fix, so further updates
/// are dropped and a snapshot is requested once there is room again.
pub const EVENT_QUEUE_CAP: usize = 512;
/// How many decoded events the worker buffers before dropping the oldest.
const IN_FLIGHT_CAP: usize = 1024;
/// How many user intents may queue. A person cannot click fast enough to fill
/// this unless something is already wrong.
const INTENT_CAP: usize = 256;

/// Why one connection ended.
enum End {
    /// The UI dropped its receiver; the thread is shutting down.
    UiGone,
    /// The backend went away; reconnect to the same connection.
    Lost(String),
    /// A pairing succeeded: that daemon is this client's connection now.
    Paired(Target),
}

/// Where the worker connects.
///
/// Resolved once from the client document — the settings file is not polled:
/// the pairing event is the one thing that changes it while the window runs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    /// This install's pipe, or its socket on Unix.
    Local(String),
    /// A daemon this client paired with: where it is, and the key it must
    /// present before a byte is trusted.
    Remote { endpoint: String, pin: String },
}

impl Target {
    /// The connection a client document names as active.
    fn of(connection: &Connection, app_id: &str) -> Self {
        match connection {
            Connection::Local { .. } => Self::Local(siphon_shared::riffle::admin_endpoint(app_id)),
            Connection::Remote {
                endpoint,
                fingerprint,
                ..
            } => Self::Remote {
                endpoint: endpoint.clone(),
                pin: fingerprint.clone(),
            },
        }
    }

    /// What to log: the pipe's name, or the daemon's address.
    fn describe(&self) -> &str {
        match self {
            Self::Local(endpoint) | Self::Remote { endpoint, .. } => endpoint,
        }
    }
}

/// How long the daemon may take to answer the confirmation.
///
/// The operator's approval happens in the daemon's terminal before the click,
/// so this covers one frame exchange on a connection that already exists, not a
/// handshake or a person at a keyboard.
const PAIR_CONFIRM_TIMEOUT: Duration = Duration::from_secs(20);

/// The name this client is enrolled under in the daemon's `peers.json`.
const PAIRING_NAME: &str = "Siphon UI";

/// Runs forever, reconnecting with backoff. One connection at a time.
pub async fn run(
    intents: kanal::Receiver<Intent>,
    wake: Arc<Wake>,
    events: kanal::Sender<UiEvent>,
) {
    let settings = siphon_shared::settings::load(crate::service::APP_ID);
    let mut target = Target::of(settings.active(), crate::service::APP_ID);
    let mut backoff = Duration::from_secs(1);
    loop {
        tracing::info!(target: "riffle", endpoint = target.describe(), "backend");
        let failure = match connect(&target).await {
            Ok((client, snapshot)) => {
                backoff = Duration::from_secs(1);
                let _ = events.try_send(UiEvent::Snapshot(Box::new(snapshot)));
                match serve_connection(client, &intents, &wake, &events).await {
                    End::UiGone => return,
                    // The window has already been told, and the file it writes
                    // is what a later start reads; this connection is finished.
                    End::Paired(next) => {
                        target = next;
                        backoff = Duration::from_secs(1);
                        continue;
                    }
                    End::Lost(message) => Some(message),
                }
            }
            Err(error) => Some(format!("backend: {error}")),
        };
        let _ = events.try_send(UiEvent::Update(Box::new(Update::Conn(
            ConnHealth::Connecting,
        ))));
        if let Some(message) = failure {
            let _ = events.try_send(UiEvent::Update(Box::new(Update::Error(Some(message)))));
        }
        compio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(2));
    }
}

/// Opens one connection: the pipe, or a pinned QUIC session with this client's
/// key.
async fn connect(target: &Target) -> Result<(Client, Snapshot), String> {
    match target {
        Target::Local(endpoint) => Client::connect(endpoint, env!("CARGO_PKG_VERSION"))
            .await
            .map_err(|error| error.to_string()),
        Target::Remote { endpoint, pin } => {
            // Only a remote connection needs this client's key, so a window on
            // the pipe never generates a certificate nobody asked for.
            let identity = crate::service::identity().map_err(|error| error.to_string())?;
            Client::connect_quic(endpoint, pin, &identity, env!("CARGO_PKG_VERSION"))
                .await
                .map_err(|error| error.to_string())
        }
    }
}

/// A provisional connection between the handshake and the typed code.
struct Provisional {
    pairing: Pairing,
    endpoint: String,
    /// The daemon's key as the handshake presented it: what `ui.json` keeps,
    /// rather than the text a person typed.
    pin: String,
}

/// Completes a provisional handshake with a daemon whose window is open.
///
/// Nothing is sent yet, so the daemon is holding a connection that has said
/// nothing until the code comes back.
async fn begin_pairing(endpoint: String, pin: String) -> Result<Provisional, String> {
    let identity = crate::service::identity().map_err(|error| error.to_string())?;
    let pairing = Pairing::begin(&endpoint, &pin, &identity)
        .await
        .map_err(|error| error.to_string())?;
    let pin = pairing.pin().to_owned();
    Ok(Provisional {
        pairing,
        endpoint,
        pin,
    })
}

/// Whether a typed code is the one the handshake derived.
///
/// Case, the hyphen and stray spaces are the person's business: the two sides
/// derive the same six characters, and a terminal is not where one learns to
/// type them exactly as a parser wants.
fn codes_match(typed: &str, derived: &str) -> bool {
    let normalise = |code: &str| -> String {
        code.chars()
            .filter(|character| !character.is_whitespace() && *character != '-')
            .flat_map(char::to_uppercase)
            .collect()
    };
    normalise(typed) == normalise(derived)
}

/// Serves one connection until the backend or the UI goes away.
async fn serve_connection(
    client: Client,
    intents: &kanal::Receiver<Intent>,
    wake: &Arc<Wake>,
    events: &kanal::Sender<UiEvent>,
) -> End {
    let client = Rc::new(client);
    // Media rides the same connection as the control plane on both carriers: a
    // QUIC datagram channel, or class-1 frames on the pipe. The receiver is its
    // own task, so a media burst never holds up the control-plane events, and it
    // ends when the carrier closes.
    let source = client.datagrams().cloned().map_or_else(
        || client.frames().cloned().map(crate::media::Source::Frames),
        |datagrams| Some(crate::media::Source::Datagrams(datagrams)),
    );
    if let Some(source) = source {
        let events = events.clone();
        compio::runtime::spawn(crate::media::run(source, events)).detach();
    }
    let stream = client.events().clone();
    let queue: Rc<RefCell<VecDeque<ClientEvent>>> = Rc::new(RefCell::new(VecDeque::new()));
    let closed = Rc::new(Cell::new(false));
    // Set whenever an event had to be dropped. The next event that does fit
    // then asks for a snapshot, which supersedes everything that was lost.
    let lost = Rc::new(Cell::new(false));

    // The forwarder owns the async receive; the worker drains its queue. A
    // select between the two receivers would drop a completed receive.
    let feed_queue = Rc::clone(&queue);
    let feed_closed = Rc::clone(&closed);
    let feed_lost = Rc::clone(&lost);
    let feed_wake = Arc::clone(wake);
    compio::runtime::spawn(async move {
        while let Ok(event) = stream.recv().await {
            if feed_queue.borrow().len() >= IN_FLIGHT_CAP {
                // Bound the buffer even while the window is in the tray: the
                // oldest events go, and the snapshot below repairs the state.
                feed_queue.borrow_mut().pop_front();
                feed_lost.set(true);
            }
            feed_queue.borrow_mut().push_back(event);
            feed_wake.wake();
        }
        feed_closed.set(true);
        feed_wake.wake();
    })
    .detach();

    // The provisional connection, if the window is between the two steps of a
    // pairing. It lives across intents because the handshake has to finish
    // before the code exists, and the code comes back in a later one.
    let provisional: Rc<RefCell<Option<Provisional>>> = Rc::new(RefCell::new(None));

    loop {
        wait(wake, || {
            closed.get() || !queue.borrow().is_empty() || !intents.is_empty()
        })
        .await;

        let disconnected = closed.get() && queue.borrow().is_empty();

        while let Ok(Some(intent)) = intents.try_recv() {
            // A picture is the one request whose answer is the point, so it is
            // fetched on its own task: the update stream keeps flowing while the
            // bytes arrive.
            match intent {
                Intent::Rpc(Request::GetAvatar { id }) => {
                    let client = Rc::clone(&client);
                    let events = events.clone();
                    compio::runtime::spawn(async move {
                        let path = crate::avatar::fetch(&client, &id).await;
                        let _ = events.try_send(UiEvent::Avatar { id, path });
                    })
                    .detach();
                }
                // `StreamStarted` is a query's answer rather than an entity
                // update, so it waits on its own task too: the lane rows keep
                // flowing while the daemon says where it will start.
                Intent::Rpc(Request::StreamSeek { key, target }) => {
                    let client = Rc::clone(&client);
                    let events = events.clone();
                    compio::runtime::spawn(async move {
                        let request = Request::StreamSeek {
                            key: key.clone(),
                            target,
                        };
                        match client.request(request).await {
                            Ok(Response::StreamStarted(started)) => {
                                let _ = events.try_send(UiEvent::Started { key, started });
                            }
                            Ok(_) => {}
                            Err(error) => {
                                let _ = events.try_send(UiEvent::Update(Box::new(Update::Error(
                                    Some(error.to_string()),
                                ))));
                            }
                        }
                    })
                    .detach();
                }
                // Fire and forget: a slow handler must not stall the update
                // stream, and the ack would be discarded anyway. A rejected
                // intent arrives as an `Update`.
                Intent::Rpc(request) => {
                    if let Err(error) = client.send(request) {
                        let _ = events.try_send(UiEvent::Update(Box::new(Update::Error(Some(
                            error.to_string(),
                        )))));
                    }
                }
                Intent::PairBegin { endpoint, pin } => {
                    // A second attempt replaces the first: the daemon has room
                    // for one provisional connection at a time, and dropping it
                    // here is what tears the old one down.
                    let previous = provisional.borrow_mut().take();
                    drop(previous);
                    let slot = Rc::clone(&provisional);
                    let events = events.clone();
                    compio::runtime::spawn(async move {
                        match begin_pairing(endpoint.clone(), pin).await {
                            Ok(hold) => {
                                let sas = hold.pairing.sas().to_owned();
                                // Stored before the event: the confirm that
                                // follows the code has to find it.
                                *slot.borrow_mut() = Some(hold);
                                let _ = events.try_send(UiEvent::PairCode { sas });
                            }
                            Err(error) => {
                                let _ = events
                                    .try_send(UiEvent::PairFailed(format!("{endpoint}: {error}")));
                            }
                        }
                    })
                    .detach();
                }
                Intent::PairConfirm { code } => {
                    let taken = provisional.borrow_mut().take();
                    let Some(Provisional {
                        pairing,
                        endpoint,
                        pin,
                    }) = taken
                    else {
                        let _ = events
                            .try_send(UiEvent::PairFailed("no pairing is in progress".to_owned()));
                        continue;
                    };
                    // Nothing is sent on a code that does not match: the
                    // provisional connection is dropped instead, and the daemon
                    // never hears a confirmation for this attempt.
                    if !codes_match(&code, pairing.sas()) {
                        let _ = events.try_send(UiEvent::PairFailed(
                            "that is not the code this handshake derived".to_owned(),
                        ));
                        continue;
                    }
                    let confirmed =
                        compio::time::timeout(PAIR_CONFIRM_TIMEOUT, pairing.confirm(PAIRING_NAME))
                            .await;
                    // Awaited here rather than spawned on its own task: the
                    // answer is this connection's replacement, which is a return
                    // value and not something an event could carry. The timeout
                    // is what keeps a daemon that never answers from parking the
                    // worker for good.
                    match confirmed {
                        Ok(Ok(_)) => {
                            let _ = events.try_send(UiEvent::Paired {
                                label: endpoint.clone(),
                                endpoint: endpoint.clone(),
                                fingerprint: pin.clone(),
                            });
                            return End::Paired(Target::Remote { endpoint, pin });
                        }
                        Ok(Err(error)) => {
                            let _ = events.try_send(UiEvent::PairFailed(error.to_string()));
                        }
                        Err(_) => {
                            let _ = events.try_send(UiEvent::PairFailed(
                                "the daemon did not answer the confirmation".to_owned(),
                            ));
                        }
                    }
                }
            }
        }

        loop {
            let event = queue.borrow_mut().pop_front();
            let Some(event) = event else { break };
            let forwarded = match event {
                ClientEvent::Snapshot(snapshot) => events.try_send(UiEvent::Snapshot(snapshot)),
                ClientEvent::Update(update) => events.try_send(UiEvent::Update(update)),
            };
            match forwarded {
                // Room again: replace whatever was dropped with one snapshot.
                Ok(true) => {
                    if lost.replace(false) {
                        let _ = client.send(Request::GetSnapshot);
                    }
                }
                Ok(false) => lost.set(true),
                // The UI is gone; the loop below reports it.
                Err(_) => {}
            }
        }

        if disconnected {
            return End::Lost("backend disconnected".to_owned());
        }
        if (intents.is_closed() && intents.is_empty()) || events.is_closed() {
            return End::UiGone;
        }
    }
}

async fn wait(wake: &Wake, ready: impl Fn() -> bool) {
    poll_fn(|context| {
        if ready() {
            return Poll::Ready(());
        }
        wake.park(context);
        if ready() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

/// The window's state, one signal per thing the interface draws.
///
/// The protocol is already incremental — an `Update` names one entity and carries
/// it whole — so the signals *are* the state: an update is applied to the signal
/// it names and nothing else is touched. A snapshot is the only whole-state
/// message, and it exists for the first connect and for a reconnect, where every
/// field may have moved.
#[derive(Clone)]
pub struct UiState {
    pub conn: RwSignal<ConnHealth, LocalStorage>,
    pub error: RwSignal<Option<String>, LocalStorage>,
    pub update: RwSignal<Option<UpdateView>, LocalStorage>,
    pub channels: RwSignal<Rc<Vec<RwSignal<ChannelView, LocalStorage>>>, LocalStorage>,
    /// The client's own document: display, notification policy, per-channel
    /// switches, pins and connections. One handle, so one writer.
    pub settings: RwSignal<ClientSettings, LocalStorage>,
    /// Channel ids this client keeps at the top of the list, mirrored from
    /// `settings` for the ordering comparators.
    pub pins: RwSignal<Rc<Vec<u64>>, LocalStorage>,
    pub pending: RwSignal<Rc<Vec<String>>, LocalStorage>,
    pub pipelines: RwSignal<Rc<Vec<RwSignal<PipelineView, LocalStorage>>>, LocalStorage>,
    /// The lanes this connection has attached, one row each. The daemon writes
    /// them straight to this connection, so the list is per window.
    pub lanes: RwSignal<Rc<Vec<RwSignal<LaneView, LocalStorage>>>, LocalStorage>,
    /// The last `StreamStarted` the daemon answered a seek with. A lane's real
    /// start is the daemon's to decide, not the target the client asked for.
    pub started: RwSignal<Option<(StreamKey, StreamStarted)>, LocalStorage>,
    /// What the receive half has assembled so far: its counters and the last
    /// unit's lane and `pts`. Client-side numbers, not the daemon's `ConnStats`.
    pub media: RwSignal<Option<MediaView>, LocalStorage>,
    pub arena: RwSignal<ArenaSummary, LocalStorage>,
    pub downloader: RwSignal<DownloaderSummary, LocalStorage>,
    /// The notification centre. Client state: it does not survive a window
    /// restart, and nothing in the daemon knows it exists.
    pub notices: RwSignal<Rc<Vec<Notice>>, LocalStorage>,
    pub logs: RwSignal<Rc<Vec<LogEntry>>, LocalStorage>,
    /// Where each avatar's bytes live in this client's cache, keyed by the
    /// content id a row carries. A row draws its initials until its id is here.
    pub avatars: RwSignal<Rc<HashMap<String, String>>, LocalStorage>,
    /// The pairing panel's state: what its button does and what its status line
    /// says. Driven by the worker's pairing events.
    pub pair: RwSignal<PairPhase, LocalStorage>,
    /// The app id whose `ui.json` a successful pairing rewrites. `None` on a
    /// state a test builds: the assertion is the signal, and no test may
    /// overwrite the person's document.
    app_id: Option<&'static str>,
    /// Avatar ids this session has already asked for, so a viewer-count tick
    /// does not ask again. A snapshot clears it: requests a dropped connection
    /// never answered have to be made again.
    avatar_requests: Rc<RefCell<HashSet<String>>>,
    /// The queue to the RPC worker, so a row whose picture is not cached can ask
    /// for it without the frame loop waiting.
    intents: IntentSender,
    /// When each channel was last seen going offline, for the title/game mute.
    muted: Rc<RefCell<HashMap<u64, i64>>>,
    /// The next notification-centre id.
    notice_seq: Rc<Cell<u64>>,
}

/// One entry of the notification centre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub id: u64,
    /// Milliseconds since the unix epoch.
    pub at_ms: i64,
    pub summary: String,
    pub body: String,
}

/// Where the pairing panel is, and so what its one button does.
///
/// The two steps are the protocol's, not the interface's: the code does not
/// exist until the handshake has completed, and the handshake is what the first
/// step starts. So the first press connects and the second sends the code a
/// person read off the daemon's terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairPhase {
    /// Nothing in progress.
    Idle,
    /// The provisional handshake is running.
    Connecting,
    /// A provisional connection is up and this is the code both sides derived.
    /// The daemon's terminal shows the same one; nothing has been sent yet.
    Code(String),
    /// The confirmation is with the daemon.
    Confirming,
    /// The window is on that daemon now.
    Paired(String),
    /// A step failed, and this is why.
    Failed(String),
}

impl PairPhase {
    /// Whether the worker is doing something, so the button waits.
    #[must_use]
    pub const fn busy(&self) -> bool {
        matches!(self, Self::Connecting | Self::Confirming)
    }
}

/// A list of rows, each behind its own signal.
///
/// The keyed list watches the *key list* and nothing else, so an update to one
/// row's data has to travel through that row's signal: rewriting the vector
/// would rebuild every row, and rebuilding only the changed one is not
/// something a keyed list does for you. Mutating a row signal touches no list.
type Rows<T> = RwSignal<Rc<Vec<RwSignal<T, LocalStorage>>>, LocalStorage>;

/// Replaces one row's data, or appends a row for a key not seen before.
fn upsert_row<T, K>(list: Rows<T>, item: T, key: impl Fn(&T) -> K)
where
    T: PartialEq + 'static,
    K: PartialEq,
{
    let wanted = key(&item);
    let rows = list.get_untracked();
    if let Some(row) = rows
        .iter()
        .find(|row| row.with_untracked(|current| key(current) == wanted))
    {
        if !row.with_untracked(|current| *current == item) {
            row.set(item);
        }
        return;
    }
    let mut next = (*rows).clone();
    next.push(RwSignal::new_local(item));
    list.set(Rc::new(next));
}

/// Replaces a whole list, reusing the signal of every row that still exists.
///
/// Reusing matters more than the values: a snapshot with fresh signals behind
/// the same keys would leave every row drawing its predecessor's data.
fn sync_rows<T, K>(list: Rows<T>, incoming: Vec<T>, key: impl Fn(&T) -> K)
where
    T: PartialEq + 'static,
    K: PartialEq,
{
    let rows = list.get_untracked();
    let mut next = Vec::with_capacity(incoming.len());
    for item in incoming {
        let wanted = key(&item);
        match rows
            .iter()
            .find(|row| row.with_untracked(|current| key(current) == wanted))
        {
            Some(row) => {
                if !row.with_untracked(|current| *current == item) {
                    row.set(item);
                }
                next.push(*row);
            }
            None => next.push(RwSignal::new_local(item)),
        }
    }
    list.set(Rc::new(next));
}

/// Drops every row whose key is not in `keep`.
fn retain_rows<T>(list: Rows<T>, keep: impl Fn(&T) -> bool)
where
    T: 'static,
{
    let rows = list.get_untracked();
    if rows.iter().all(|row| row.with_untracked(&keep)) {
        return;
    }
    let mut next = (*rows).clone();
    next.retain(|row| row.with_untracked(&keep));
    list.set(Rc::new(next));
}

/// The order the channel list is drawn in: pinned first, then live, then the
/// channel that was most recently live.
///
/// Live rows keep their relative order instead of being ranked by viewers: a
/// list that reshuffles every time a counter ticks is a list you cannot reach
/// for. The sort is stable, so everything the key does not name keeps the
/// order it arrived in (config order, until something has to move).
fn order_key(channel: &ChannelView, pins: &[u64]) -> (bool, bool, i64) {
    (
        pins.contains(&channel.id),
        channel.live,
        channel.last_live_ms.unwrap_or(i64::MIN),
    )
}

/// Whether two keys are already in display order (pinned, live, recency, all
/// descending). `reorder` checks this before sorting, so an upsert that only
/// changed a viewer count does not allocate or rebuild the key list.
fn in_order(earlier: (bool, bool, i64), later: (bool, bool, i64)) -> bool {
    (earlier.0, earlier.1, earlier.2) >= (later.0, later.1, later.2)
}

fn order_channels(
    rows: &[RwSignal<ChannelView, LocalStorage>],
    pins: &[u64],
) -> Vec<RwSignal<ChannelView, LocalStorage>> {
    let mut ordered: Vec<_> = rows
        .iter()
        .map(|row| (row.with_untracked(|channel| order_key(channel, pins)), *row))
        .collect();
    ordered.sort_by(|(left, _), (right, _)| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| right.1.cmp(&left.1))
            .then_with(|| right.2.cmp(&left.2))
    });
    ordered.into_iter().map(|(_, row)| row).collect()
}

/// The pinned channel ids in a settings document, for the ordering comparators.
fn pinned_ids(document: &ClientSettings) -> Vec<u64> {
    document
        .channels
        .iter()
        .filter(|(_, prefs)| prefs.pinned)
        .filter_map(|(id, _)| id.parse().ok())
        .collect()
}

impl UiState {
    /// The state a window starts with, before the first snapshot arrives.
    #[must_use]
    pub fn new(settings: RwSignal<ClientSettings, LocalStorage>, intents: IntentSender) -> Self {
        let state = empty_snapshot();
        let pins = pinned_ids(&settings.get_untracked());
        Self {
            conn: RwSignal::new_local(state.conn),
            error: RwSignal::new_local(state.error),
            update: RwSignal::new_local(state.update),
            channels: RwSignal::new_local(Rc::new(Vec::new())),
            settings,
            pins: RwSignal::new_local(Rc::new(pins)),
            pending: RwSignal::new_local(Rc::new(state.pending)),
            pipelines: RwSignal::new_local(Rc::new(Vec::new())),
            lanes: RwSignal::new_local(Rc::new(Vec::new())),
            started: RwSignal::new_local(None),
            media: RwSignal::new_local(None),
            arena: RwSignal::new_local(state.arena),
            downloader: RwSignal::new_local(state.downloader),
            notices: RwSignal::new_local(Rc::new(Vec::new())),
            logs: RwSignal::new_local(Rc::new(state.logs)),
            avatars: RwSignal::new_local(Rc::new(HashMap::new())),
            pair: RwSignal::new_local(PairPhase::Idle),
            app_id: Some(crate::service::APP_ID),
            avatar_requests: Rc::new(RefCell::new(HashSet::new())),
            intents,
            muted: Rc::new(RefCell::new(HashMap::new())),
            notice_seq: Rc::new(Cell::new(1)),
        }
    }

    /// Everything, from a snapshot: the first connect, or a reconnect.
    fn reset(&self, state: Snapshot) {
        self.conn.set(state.conn);
        self.error.set(state.error);
        self.update.set(state.update);
        // A snapshot is a new connection, and its receiver starts its counters
        // from zero. The old numbers described a connection that is gone.
        self.media.set(None);
        // A reconnect is the one moment to re-ask: whatever a dropped connection
        // never answered is still unanswered.
        self.avatar_requests.borrow_mut().clear();
        self.ensure_avatars(&state.channels);
        sync_rows(self.channels, state.channels, |channel| channel.id);
        self.reorder();
        self.pending.set(Rc::new(state.pending));
        sync_rows(self.pipelines, state.pipelines, |pipeline| {
            pipeline.key.clone()
        });
        sync_rows(self.lanes, state.lanes, |lane| lane.key.clone());
        self.arena.set(state.arena);
        self.downloader.set(state.downloader);
        self.logs.set(Rc::new(state.logs));
    }

    /// Re-derives the pinned set after a change to the settings document.
    fn refresh_pins(&self) {
        let pins = pinned_ids(&self.settings.get_untracked());
        self.pins.set(Rc::new(pins));
        self.reorder();
    }

    /// Pins a channel to the top, or unpins it, and keeps the file in step.
    pub fn toggle_pin(&self, id: u64) {
        crate::settings::update(self.settings, |settings| {
            settings.toggle_pin(id);
        });
        self.refresh_pins();
    }

    /// Replaces one channel's notification switches.
    pub fn set_channel_prefs(&self, id: u64, change: impl FnOnce(&mut ChannelPrefs)) {
        crate::settings::update(self.settings, |settings| {
            let mut prefs = settings.channel(id);
            change(&mut prefs);
            settings.set_channel(id, prefs);
        });
    }

    pub fn add_word(&self, word: &str) {
        let word = word.trim().to_lowercase();
        if word.is_empty() {
            return;
        }
        crate::settings::update(self.settings, |settings| {
            let words = &mut settings.notifications.filtered_words;
            if !words.iter().any(|existing| existing.to_lowercase() == word) {
                words.push(word);
            }
        });
    }

    pub fn remove_word(&self, word: &str) {
        crate::settings::update(self.settings, |settings| {
            settings
                .notifications
                .filtered_words
                .retain(|existing| existing != word);
        });
    }

    /// Makes sure every row's picture is either in this client's cache or on its
    /// way, and does nothing for one that already is.
    fn ensure_avatars(&self, views: &[ChannelView]) {
        for view in views {
            if let Some(id) = view.avatar.as_deref() {
                self.ensure_avatar(id);
            }
        }
    }

    fn ensure_avatar(&self, id: &str) {
        if self.avatars.get_untracked().contains_key(id) {
            return;
        }
        if !self.avatar_requests.borrow_mut().insert(id.to_owned()) {
            return;
        }
        // A picture an earlier run cached needs no round trip at all.
        if let Some(path) = crate::avatar::cached(id) {
            self.set_avatar(id, path);
            return;
        }
        self.intents.fetch_avatar(id);
    }

    /// Records where one avatar's bytes are: what the row's image element was
    /// waiting for, and the reason a row needs no republish to show a picture.
    fn set_avatar(&self, id: &str, path: String) {
        let mut avatars = (*self.avatars.get_untracked()).clone();
        if avatars.get(id) == Some(&path) {
            return;
        }
        avatars.insert(id.to_owned(), path);
        self.avatars.set(Rc::new(avatars));
    }

    /// The local picture for a row, when this client has it.
    fn avatar_file(&self, view: &ChannelView) -> Option<String> {
        let id = view.avatar.as_deref()?;
        self.avatars.get_untracked().get(id).cloned()
    }

    /// Turns one stream transition into a notification, or drops it.
    ///
    /// The daemon publishes every transition; the switches, the word filter and
    /// the mute window live here, which is what moving notification policy out
    /// of the daemon means in practice.
    fn on_stream_event(&self, event: StreamEvent) {
        let settings = self.settings.get_untracked();
        let prefs = settings.channel(event.view.id);
        let policy = settings.notifications;
        if event.kind == StreamEventKind::Offline {
            self.muted.borrow_mut().insert(event.view.id, event.at_ms);
            return;
        }
        let (summary, body) = match event.kind {
            StreamEventKind::Live => {
                if !prefs.notify_live {
                    return;
                }
                let body = match (event.view.game.as_deref(), event.view.title.as_deref()) {
                    (Some(game), Some(title)) => format!("{title} — {game}"),
                    (Some(game), None) => game.to_owned(),
                    (None, Some(title)) => title.to_owned(),
                    (None, None) => String::new(),
                };
                (format!("[{}] is LIVE", event.view.display_name), body)
            }
            StreamEventKind::Title | StreamEventKind::Game => {
                // Title churn mid-broadcast is noise, and so is the minutes
                // after a stream ends; a restream is a `Live` event and always
                // gets through.
                if !policy.title_changes || !prefs.notify_titles || event.view.live {
                    return;
                }
                if self.muted(
                    event.view.id,
                    event.at_ms,
                    policy.mute_after_stream_down_secs,
                ) {
                    return;
                }
                let is_title = event.kind == StreamEventKind::Title;
                let (what, value) = if is_title {
                    ("title", event.view.title.clone().unwrap_or_default())
                } else {
                    ("game", event.view.game.clone().unwrap_or_default())
                };
                if is_title
                    && !policy.filtered_words.is_empty()
                    && crate::matcher::Matcher::new(&policy.filtered_words).is_match(&value)
                {
                    return;
                }
                let body = event
                    .previous
                    .filter(|old| !old.is_empty())
                    .map_or_else(|| value.clone(), |old| format!("{old} → {value}"));
                (
                    format!("[{}] {what} changed", event.view.display_name),
                    body,
                )
            }
            StreamEventKind::Offline => return,
        };
        let id = self.notice_seq.get();
        self.notice_seq.set(id.wrapping_add(1));
        let mut notices = (*self.notices.get_untracked()).clone();
        notices.push(Notice {
            id,
            at_ms: event.at_ms,
            summary: summary.clone(),
            body: body.clone(),
        });
        if notices.len() > NOTIFICATION_CAP {
            let drop = notices.len() - NOTIFICATION_CAP;
            notices.drain(..drop);
        }
        self.notices.set(Rc::new(notices));
        let image = self.avatar_file(&event.view);
        crate::notify::show(
            &summary,
            &body,
            policy.sound,
            image.as_deref(),
            Some(&event.view.login),
        );
    }

    /// Whether a channel's title/game notices are still muted by a recent
    /// `stream-down`.
    fn muted(&self, id: u64, now_ms: i64, secs: u64) -> bool {
        let window = i64::try_from(secs).unwrap_or(i64::MAX).saturating_mul(1000);
        self.muted
            .borrow()
            .get(&id)
            .is_some_and(|at| now_ms.saturating_sub(*at) < window)
    }

    /// Puts the rows in display order. The guard matters: an upsert that does
    /// not change a row's standing must not rewrite the key list, or the whole
    /// keyed list would be diffed on every viewcount tick.
    fn reorder(&self) {
        let rows = self.channels.get_untracked();
        if rows.len() < 2 {
            return;
        }
        let pins = self.pins.get_untracked();
        let mut previous = None;
        for row in rows.iter() {
            let current = row.with_untracked(|channel| order_key(channel, &pins));
            if previous.is_some_and(|earlier| !in_order(earlier, current)) {
                let ordered = order_channels(&rows, &pins);
                self.channels.set(Rc::new(ordered));
                return;
            }
            previous = Some(current);
        }
    }

    /// One entity update, applied to the signal it names and nowhere else.
    fn apply(&self, update: Update) {
        match update {
            Update::Conn(conn) => {
                if self.conn.get_untracked() != conn {
                    self.conn.set(conn);
                }
            }
            // One row of the list, not the list: the row's own signal carries
            // the change, so the keyed list never re-runs.
            Update::ChannelUpsert(view) => {
                self.ensure_avatars(std::slice::from_ref(&view));
                upsert_row(self.channels, *view, |channel| channel.id);
                // A live flip or a new `last live` is the only thing that can
                // move a row; `reorder` is a no-op when the order holds.
                self.reorder();
            }
            Update::ChannelRemove(id) => {
                retain_rows(self.channels, |channel| channel.id != id);
            }
            Update::Pending(pending) => {
                if *self.pending.get_untracked() != pending {
                    self.pending.set(Rc::new(pending));
                }
            }
            // The config names the channels that may exist, so removing one from
            // it is the only thing that can drop a row here. A per-channel
            // preference for a channel that is no longer followed is left alone:
            // it is inert, and pruning it would lose the switches on a re-add.
            Update::Config(config) => {
                let ids: Vec<u64> = config.channels.iter().map(|channel| channel.id).collect();
                retain_rows(self.channels, |channel| ids.contains(&channel.id));
            }
            Update::PipelineUpsert(view) => {
                let recorded = view.recording || view.disk.is_some();
                let was = self
                    .pipelines
                    .get_untracked()
                    .iter()
                    .find(|row| row.with_untracked(|pipeline| pipeline.key == view.key))
                    .map(|row| {
                        row.with_untracked(|pipeline| pipeline.recording || pipeline.disk.is_some())
                    });
                upsert_row(self.pipelines, view, |pipeline| pipeline.key.clone());
                // The Recordings list filters on this, and a keyed list does not
                // watch row data: re-run it when a row joins or leaves.
                if was != Some(recorded) {
                    let rows = (*self.pipelines.get_untracked()).clone();
                    self.pipelines.set(Rc::new(rows));
                }
            }
            Update::PipelineRemove(key) => {
                retain_rows(self.pipelines, |pipeline| pipeline.key != key);
            }
            // A lane arrives as a complete row; upserting by key is the same
            // operation for the first one and for a correction.
            Update::Lane(view) => {
                upsert_row(self.lanes, view, |lane| lane.key.clone());
            }
            Update::LaneRemove(key) => {
                retain_rows(self.lanes, |lane| lane.key != key);
            }
            Update::Arena(arena) => {
                if self.arena.get_untracked() != arena {
                    self.arena.set(arena);
                }
            }
            Update::Downloader(downloader) => {
                if self.downloader.get_untracked() != downloader {
                    self.downloader.set(downloader);
                }
            }
            // An edge, not state: the daemon publishes it uncoalesced precisely
            // so this decision can see every transition.
            Update::StreamEvent(event) => self.on_stream_event(*event),
            Update::Log(entry) => {
                let mut logs = (*self.logs.get_untracked()).clone();
                // A reconnect replays the snapshot and then the updates that
                // raced it, so the same seq can arrive twice.
                if logs.last().is_some_and(|last| last.seq >= entry.seq) {
                    return;
                }
                logs.push(entry);
                if logs.len() > LOG_CAP {
                    let drop = logs.len() - LOG_CAP;
                    logs.drain(..drop);
                }
                self.logs.set(Rc::new(logs));
            }
            Update::Error(error) => self.error.set(error),
            Update::Update(update) => self.update.set(update),
            // The segment ring is queried on demand by the detail view.
            Update::Segment { .. } => {}
        }
    }

    /// Applies one event to the signals the view reads.
    pub fn apply_event(&self, event: UiEvent) {
        match event {
            UiEvent::Snapshot(state) => self.reset(*state),
            UiEvent::Update(update) => self.apply(*update),
            UiEvent::Avatar { id, path } => {
                if let Some(path) = path {
                    self.set_avatar(&id, path);
                }
            }
            UiEvent::Started { key, started } => self.started.set(Some((key, started))),
            UiEvent::Media(view) => {
                if self.media.get_untracked() != Some(view) {
                    self.media.set(Some(view));
                }
            }
            UiEvent::PairCode { sas } => self.pair.set(PairPhase::Code(sas)),
            UiEvent::Paired {
                endpoint,
                fingerprint,
                label,
            } => self.pair_succeeded(&endpoint, &fingerprint, &label),
            UiEvent::PairFailed(message) => self.pair.set(PairPhase::Failed(message)),
        }
    }

    /// Records a daemon this window just paired with.
    ///
    /// The worker has already switched to it; this is what a later start reads,
    /// so the document is written as well as updated. Nothing here can fail the
    /// pairing: a connection that could not be remembered still works until the
    /// window closes.
    fn pair_succeeded(&self, endpoint: &str, fingerprint: &str, label: &str) {
        let mut document = self.settings.get_untracked();
        document.remember_remote(endpoint, fingerprint, label);
        self.settings.set(document.clone());
        // A state a test builds has no document of its own: the assertion is
        // the signal, and no test may overwrite the person's `ui.json`.
        let written = self.app_id.map_or(Ok(()), |app_id| {
            siphon_shared::settings::save(app_id, &document)
        });
        if let Err(error) = written {
            tracing::warn!(target: "settings", "could not save settings: {error}");
        }
        self.pair.set(PairPhase::Paired(label.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use siphon_shared::riffle::ChatColor;
    use siphon_shared::riffle::LaneState;
    use zgui::reactive::Mounted;
    use zgui::reactive::install;

    use super::*;

    /// A window state over a fresh, unsaved settings document.
    fn state() -> UiState {
        let (intents, _intent_rx, _wake) = intent_channel();
        UiState::new(RwSignal::new_local(ClientSettings::default()), intents)
    }

    fn lane(channel: u64, playhead_ms: u64) -> LaneView {
        LaneView {
            key: StreamKey::new(channel, None),
            state: LaneState::Attached,
            position_ms: playhead_ms,
            playhead_ms,
            buffered_ahead_ms: 0,
            rate: 1.0,
            dropped_video: 0,
            dropped_audio: 0,
            recovering: false,
            started_ms: 0,
        }
    }

    fn view(id: u64, viewers: u32) -> ChannelView {
        ChannelView {
            id,
            login: format!("channel{id}"),
            display_name: format!("Channel {id}"),
            chat_color: ChatColor::Twitch(0x00ff_ffff),
            avatar: None,
            live: true,
            title: Some("a title".to_owned()),
            game: None,
            game_id: None,
            viewers: Some(viewers),
            collaboration_viewers: None,
            stream_start_ms: None,
            last_live_ms: None,
            stream_id: None,
            error: None,
            record: false,
        }
    }

    /// A keyed list watches its key list and nothing else, so an upsert has to
    /// reach the row through the row's own signal. Replacing the vector would
    /// leave the row drawing the value it was built with, forever.
    #[test]
    fn an_upsert_reaches_the_row_without_rebuilding_the_list() {
        install().ok();
        let scope = Mounted::new();
        scope.with(|| {
            let state = state();
            state.reset(Snapshot {
                channels: vec![view(1, 10)],
                ..Snapshot::default()
            });

            let rows = state.channels.get_untracked();
            let row = rows[0];
            state.apply(Update::ChannelUpsert(Box::new(view(1, 99))));

            assert!(
                Rc::ptr_eq(&rows, &state.channels.get_untracked()),
                "one row's data must not rebuild the list"
            );
            assert_eq!(row.get_untracked().viewers, Some(99));
        });
        scope.unmount();
    }

    /// A reconnect ends with a whole snapshot; the rows it names already exist,
    /// and a fresh signal behind the same key would keep drawing the old value.
    #[test]
    fn a_snapshot_reuses_the_rows_it_already_has() {
        install().ok();
        let scope = Mounted::new();
        scope.with(|| {
            let state = state();
            state.reset(Snapshot {
                channels: vec![view(1, 10)],
                ..Snapshot::default()
            });
            let row = state.channels.get_untracked()[0];

            state.reset(Snapshot {
                channels: vec![view(1, 20), view(2, 30)],
                ..Snapshot::default()
            });

            assert_eq!(row.get_untracked().viewers, Some(20));
            let rows = state.channels.get_untracked();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].get_untracked().id, 1);
        });
        scope.unmount();
    }

    #[test]
    fn removing_a_channel_drops_its_row() {
        install().ok();
        let scope = Mounted::new();
        scope.with(|| {
            let state = state();
            state.reset(Snapshot {
                channels: vec![view(1, 10), view(2, 20)],
                ..Snapshot::default()
            });
            state.apply(Update::ChannelRemove(1));
            let rows = state.channels.get_untracked();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].get_untracked().id, 2);
        });
        scope.unmount();
    }

    /// A pin outranks liveness, liveness outranks recency, and within the
    /// offline group the most recently live comes first.
    #[test]
    fn the_list_orders_pinned_then_live_then_most_recent() {
        install().ok();
        let scope = Mounted::new();
        scope.with(|| {
            let row = |id: u64, live: bool, last: Option<i64>| {
                let mut channel = view(id, 1);
                channel.live = live;
                channel.last_live_ms = last;
                RwSignal::new_local(channel)
            };
            let rows = vec![
                row(1, false, Some(10)),
                row(2, true, None),
                row(3, false, Some(30)),
                row(4, true, None),
                row(5, false, Some(20)),
            ];
            let ordered = order_channels(&rows, &[5]);
            let ids: Vec<u64> = ordered.iter().map(|row| row.get_untracked().id).collect();
            assert_eq!(ids, vec![5, 2, 4, 3, 1]);
        });
        scope.unmount();
    }

    /// A lane arrives whole and is keyed by its `StreamKey`: an update reaches
    /// the row, and a removal drops it.
    #[test]
    fn lane_updates_reach_the_row_and_a_removal_drops_it() {
        install().ok();
        let scope = Mounted::new();
        scope.with(|| {
            let state = state();
            state.reset(Snapshot {
                lanes: vec![lane(1, 1_000)],
                ..Snapshot::default()
            });
            let row = state.lanes.get_untracked()[0];
            state.apply(Update::Lane(lane(2, 2_000)));
            assert_eq!(state.lanes.get_untracked().len(), 2);

            state.apply(Update::Lane(lane(1, 3_000)));
            assert_eq!(row.get_untracked().playhead_ms, 3_000);

            state.apply(Update::LaneRemove(StreamKey::new(1, None)));
            let rows = state.lanes.get_untracked();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].get_untracked().key.channel, 2);
        });
        scope.unmount();
    }

    /// One id is asked for once. A viewer count that ticks must not ask again,
    /// and the answer is what a row draws, so it needs no republish.
    #[test]
    fn an_avatar_is_asked_for_once_and_then_drawn_from_the_event() {
        install().ok();
        let scope = Mounted::new();
        scope.with(|| {
            let (intents, intent_rx, _wake) = intent_channel();
            let state = UiState::new(RwSignal::new_local(ClientSettings::default()), intents);
            let id = "0".repeat(32);
            let mut row = view(1, 10);
            row.avatar = Some(id.clone());
            state.reset(Snapshot {
                channels: vec![row],
                ..Snapshot::default()
            });

            let asked = intent_rx.try_recv().unwrap().expect("a request");
            assert!(matches!(asked, Intent::Rpc(Request::GetAvatar { id: asked }) if asked == id));

            state.apply(Update::ChannelUpsert(Box::new(ChannelView {
                avatar: Some(id.clone()),
                ..view(1, 99)
            })));
            assert!(
                intent_rx.try_recv().unwrap().is_none(),
                "a tick must not ask for a picture twice"
            );

            state.apply_event(UiEvent::Avatar {
                id: id.clone(),
                path: Some("/tmp/avatar.png".to_owned()),
            });
            assert_eq!(
                state.avatars.get_untracked().get(&id).map(String::as_str),
                Some("/tmp/avatar.png")
            );
        });
        scope.unmount();
    }

    /// A state that would write the settings document, with the write taken
    /// out: a test asserts the signal and must not touch the person's file.
    fn unpersisted() -> UiState {
        UiState {
            app_id: None,
            ..state()
        }
    }

    /// The worker connects where the document says. A local document is this
    /// machine's pipe; a remembered daemon is its address and the key it must
    /// present, not the pipe.
    #[test]
    fn the_worker_follows_the_active_connection() {
        let app_id = crate::service::APP_ID;
        let local = ClientSettings::default();
        assert_eq!(
            Target::of(local.active(), app_id),
            Target::Local(siphon_shared::riffle::admin_endpoint(app_id))
        );

        let mut document = ClientSettings::default();
        let pin = "ab".repeat(32);
        document.remember_remote("home.example.net:7418", &pin, "media box");
        assert_eq!(
            Target::of(document.active(), app_id),
            Target::Remote {
                endpoint: "home.example.net:7418".to_owned(),
                pin,
            }
        );

        // And back: the local connection is still a row, and choosing it again
        // is what a window that never paired starts with.
        document.active_connection = Connection::default().id().to_owned();
        assert_eq!(
            Target::of(document.active(), app_id),
            Target::Local(siphon_shared::riffle::admin_endpoint(app_id))
        );
    }

    /// A successful pairing is written into the client document, and the daemon
    /// becomes the connection the next start connects to.
    #[test]
    fn a_paired_daemon_is_remembered_and_active() {
        install().ok();
        let scope = Mounted::new();
        scope.with(|| {
            let state = unpersisted();
            assert!(state.settings.get_untracked().active().is_local());

            let pin = "cd".repeat(32);
            state.apply_event(UiEvent::Paired {
                endpoint: "10.0.0.2:7418".to_owned(),
                fingerprint: pin.clone(),
                label: "attic".to_owned(),
            });

            let document = state.settings.get_untracked();
            assert_eq!(document.connections.len(), 2, "local plus the daemon");
            assert_eq!(document.active_connection, document.active().id());
            assert!(matches!(
                document.active(),
                Connection::Remote { endpoint, fingerprint, label, .. }
                    if endpoint == "10.0.0.2:7418"
                        && *fingerprint == pin
                        && label == "attic"
            ));
            assert_eq!(
                state.pair.get_untracked(),
                PairPhase::Paired("attic".to_owned())
            );
        });
        scope.unmount();
    }

    /// The two events the panel turns on are the worker's, and a failure has to
    /// leave the button usable again.
    #[test]
    fn the_pairing_events_drive_the_panel() {
        install().ok();
        let scope = Mounted::new();
        scope.with(|| {
            let state = unpersisted();
            state.apply_event(UiEvent::PairCode {
                sas: "7K4Q-2M9X".to_owned(),
            });
            assert_eq!(
                state.pair.get_untracked(),
                PairPhase::Code("7K4Q-2M9X".to_owned())
            );
            assert!(!state.pair.get_untracked().busy());

            state.apply_event(UiEvent::PairFailed("wrong code".to_owned()));
            assert_eq!(
                state.pair.get_untracked(),
                PairPhase::Failed("wrong code".to_owned())
            );
        });
        scope.unmount();
    }

    /// The typed code is the daemon's, as a person reads it: case, the hyphen
    /// and stray spaces are not part of the comparison.
    #[test]
    fn a_typed_code_is_compared_without_its_formatting() {
        assert!(codes_match("7K4Q-2M9X", "7K4Q-2M9X"));
        assert!(codes_match("7k4q-2m9x", "7K4Q-2M9X"));
        assert!(codes_match(" 7K4Q2M9X ", "7K4Q-2M9X"));
        assert!(!codes_match("7K4Q-2M9Y", "7K4Q-2M9X"));
        assert!(!codes_match("", "7K4Q-2M9X"));
    }
}
