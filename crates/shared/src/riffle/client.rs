//! Client half of the Siphon RPC: Hello, one full snapshot, then updates.
//!
//! One task owns the connection and both directions. Request sends never
//! consume a completed socket read: a queued request only wakes the task, and
//! the biased select polls the read first. Dropping the wake waiter loses
//! nothing; dropping a pending read consumes no bytes.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::rc::Rc;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;

use compio::io::AsyncRead;
use compio::io::AsyncWrite;
use futures_util::FutureExt as _;
use kanal::AsyncReceiver;
use kanal::Sender;
use thiserror::Error;

use crate::riffle::codec::CodecError;
use crate::riffle::codec::Frame;
use crate::riffle::codec::FrameClass;
use crate::riffle::codec::FrameReader;
use crate::riffle::codec::decode;
use crate::riffle::codec::write;
use crate::riffle::endpoint;
use crate::riffle::protocol::ClientMessage;
use crate::riffle::protocol::MAGIC;
use crate::riffle::protocol::PROTOCOL_VERSION;
use crate::riffle::protocol::Request;
use crate::riffle::protocol::Response;
use crate::riffle::protocol::RpcError;
use crate::riffle::protocol::ServerMessage;
use crate::riffle::protocol::Snapshot;
use crate::riffle::protocol::Update;
use crate::riffle::quic;

/// How long one request may wait for its response.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
pub enum ConnectError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error("backend speaks protocol {server}, this build speaks {ours}")]
    Protocol { server: u16, ours: u16 },
    #[error("backend rejected the hello: {0}")]
    Rejected(String),
    #[error("backend sent an unexpected first message")]
    Unexpected,
    #[error(transparent)]
    Quic(#[from] quic::Error),
}

/// One asynchronous push from the backend.
#[derive(Debug)]
pub enum ClientEvent {
    Snapshot(Box<Snapshot>),
    /// One entity update. Boxed: it is much larger than a snapshot handle, and
    /// the worker keeps a queue of them.
    Update(Box<Update>),
}

type Pending = Rc<RefCell<HashMap<u64, Sender<Result<Response, RpcError>>>>>;

/// A single-waiter, signaled wake so a request queued between the drain and
/// the park is not missed.
struct Wake {
    waker: Cell<Option<Waker>>,
    signaled: Cell<bool>,
}

impl Wake {
    const fn new() -> Self {
        Self {
            waker: Cell::new(None),
            signaled: Cell::new(false),
        }
    }

    fn wake(&self) {
        self.signaled.set(true);
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    fn park(&self, context: &std::task::Context<'_>) {
        self.waker.set(Some(context.waker().clone()));
    }

    const fn take_signal(&self) -> bool {
        self.signaled.replace(false)
    }
}

/// What the UI writes and the connection task drains.
struct Outbox {
    queue: RefCell<VecDeque<ClientMessage>>,
    wake: Rc<Wake>,
    closed: Cell<bool>,
}

/// A connected RPC client. Requests are correlated by id; updates arrive on the
/// channel returned by [`Self::events`].
pub struct Client {
    out: Rc<Outbox>,
    events: AsyncReceiver<ClientEvent>,
    pending: Pending,
    next_id: Cell<u64>,
    datagrams: Option<quic::Datagrams>,
    /// Class-1 media frames of a pipe connection, for [`Self::frames`].
    frames: Option<AsyncReceiver<Frame>>,
}

impl Client {
    /// Connects over the local pipe/socket, performs the Hello/Welcome
    /// exchange, and consumes the first snapshot. The remaining traffic is
    /// driven by the connection task.
    pub async fn connect(
        endpoint: &str,
        app_version: &str,
    ) -> Result<(Self, Snapshot), ConnectError> {
        let stream = endpoint::connect(endpoint).await?;
        Self::connect_over(stream, Self::hello(app_version), None).await
    }

    /// Connects over QUIC, checking the daemon's SPKI against `pin`.
    ///
    /// The pin is the one piece of trust a remote connection has; where a UI
    /// keeps it is the pairing screen's business, and it comes from
    /// [`Pairing::pin`] on the first connection. `identity` is this client's
    /// key, which the daemon has in `peers.json` — or will, if this is the
    /// handshake that pairs it, in which case use [`Pairing`] instead.
    ///
    /// The connection's datagram channel survives this call: it is available
    /// from [`Self::datagrams`] for the media lane.
    pub async fn connect_quic(
        endpoint: &str,
        pin: &str,
        identity: &quic::Identity,
        app_version: &str,
    ) -> Result<(Self, Snapshot), ConnectError> {
        let session = quic::connect(endpoint, pin, identity).await?;
        let datagrams = session.datagrams();
        Self::connect_over(
            session.into_stream(),
            Self::hello(app_version),
            Some(datagrams),
        )
        .await
    }

    fn hello(app_version: &str) -> ClientMessage {
        ClientMessage::Hello {
            magic: MAGIC,
            protocol: PROTOCOL_VERSION,
            app_version: app_version.to_owned(),
        }
    }

    /// The handshake both carriers share. From here on the transport is just
    /// bytes: framing, the snapshot and the update task do not know which one
    /// they are on.
    async fn connect_over<S: AsyncRead + AsyncWrite + Unpin + 'static>(
        mut stream: S,
        first: ClientMessage,
        datagrams: Option<quic::Datagrams>,
    ) -> Result<(Self, Snapshot), ConnectError> {
        write(&mut stream, &first).await?;

        let mut reader = FrameReader::new();
        let welcome: ServerMessage = reader.read(&mut stream).await?;
        let protocol = match welcome {
            ServerMessage::Welcome {
                protocol,
                server_version: _,
            } => protocol,
            ServerMessage::Rejected(reason) => return Err(ConnectError::Rejected(reason)),
            _ => return Err(ConnectError::Unexpected),
        };
        if protocol != PROTOCOL_VERSION {
            return Err(ConnectError::Protocol {
                server: protocol,
                ours: PROTOCOL_VERSION,
            });
        }

        let first: ServerMessage = reader.read(&mut stream).await?;
        let ServerMessage::Snapshot(snapshot) = first else {
            return Err(ConnectError::Unexpected);
        };
        let snapshot = *snapshot;

        let (events_tx, events_rx) = kanal::unbounded();
        let pending: Pending = Rc::new(RefCell::new(HashMap::new()));
        let out = Rc::new(Outbox {
            queue: RefCell::new(VecDeque::new()),
            wake: Rc::new(Wake::new()),
            closed: Cell::new(false),
        });
        // A pipe carries media as class-1 frames on the control stream; QUIC
        // carries it as datagrams, so only the pipe needs the frame channel.
        // The assembler is its own task, and a media burst must never hold up
        // a control event behind it.
        let (frames_tx, frames) = if datagrams.is_none() {
            let (tx, rx) = kanal::unbounded();
            (Some(tx), Some(rx.to_async()))
        } else {
            (None, None)
        };
        let task_pending = Rc::clone(&pending);
        let task_out = Rc::clone(&out);
        compio::runtime::spawn(async move {
            drive(stream, reader, task_out, task_pending, events_tx, frames_tx).await;
        })
        .detach();

        Ok((
            Self {
                out,
                events: events_rx.to_async(),
                pending,
                next_id: Cell::new(0),
                datagrams,
                frames,
            },
            snapshot,
        ))
    }

    /// The datagram channel of a QUIC connection, or `None` on the pipe.
    ///
    /// A QUIC connection keeps the handle it handshook with, so the media lane
    /// and the control plane ride one connection. The pipe has no datagram
    /// channel and does not need one: its media is class-1 frames on the same
    /// stream ([`Self::frames`]).
    #[must_use]
    pub const fn datagrams(&self) -> Option<&quic::Datagrams> {
        self.datagrams.as_ref()
    }

    /// The class-1 media frames of a pipe connection, or `None` on QUIC.
    ///
    /// Each frame is one object in the datagram form, so the UI's assembler
    /// takes its payload exactly as it takes a datagram. Single consumer: the
    /// media task owns it for the life of the connection.
    #[must_use]
    pub const fn frames(&self) -> Option<&AsyncReceiver<Frame>> {
        self.frames.as_ref()
    }

    /// The stream of `Snapshot`/`Update` events. Closes when the backend
    /// disconnects.
    ///
    /// Borrowed, not cloned: the stream is single-consumer, and handing out a
    /// clone would silently split it between two callers.
    pub const fn events(&self) -> &AsyncReceiver<ClientEvent> {
        &self.events
    }

    /// Queues one request without waiting for its ack.
    ///
    /// Intents are fire-and-forget: the state change arrives as an `Update`,
    /// and a rejected intent is reported the same way, so nothing on the UI
    /// path should block on the ack.
    ///
    /// # Errors
    ///
    /// `Disconnected` when the socket has already closed.
    pub fn send(&self, request: Request) -> Result<(), RpcError> {
        if self.out.closed.get() {
            return Err(RpcError::Disconnected);
        }
        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1));
        self.out
            .queue
            .borrow_mut()
            .push_back(ClientMessage::Request { id, request });
        self.out.wake.wake();
        Ok(())
    }

    /// Sends one request and waits for its typed result.
    pub async fn request(&self, request: Request) -> Result<Response, RpcError> {
        if self.out.closed.get() {
            return Err(RpcError::Disconnected);
        }
        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1));
        let (waiter, receiver) = kanal::bounded(1);
        self.pending.borrow_mut().insert(id, waiter);
        self.out
            .queue
            .borrow_mut()
            .push_back(ClientMessage::Request { id, request });
        self.out.wake.wake();
        match compio::time::timeout(REQUEST_TIMEOUT, receiver.to_async().recv()).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RpcError::Disconnected),
            Err(_) => {
                self.pending.borrow_mut().remove(&id);
                Err(RpcError::Failed("request timed out".to_owned()))
            }
        }
    }
}

/// The client side of one pairing attempt.
///
/// Split from [`Client`] because the code has to be shown before anything is
/// sent: the connection is completed and both sides have derived the string,
/// the operator compares this one with the daemon's terminal, and only then
/// does [`Self::confirm`] send it back. Dropping this closes the provisional
/// connection, and the daemon forgets it.
pub struct Pairing {
    stream: quic::Session,
}

impl Pairing {
    /// Completes the handshake with a daemon whose pairing window is open.
    ///
    /// Nothing is sent yet, so the daemon is waiting on a provisional
    /// connection that has said nothing.
    pub async fn begin(
        endpoint: &str,
        pin: &str,
        identity: &quic::Identity,
    ) -> Result<Self, ConnectError> {
        Ok(Self {
            stream: quic::connect(endpoint, pin, identity).await?,
        })
    }

    /// The six characters to compare with the daemon's.
    #[must_use]
    pub fn sas(&self) -> &str {
        self.stream.sas()
    }

    /// The daemon's key: what `ui.json`'s `Connection::Remote::fingerprint`
    /// keeps once this succeeds.
    #[must_use]
    pub fn pin(&self) -> &str {
        self.stream.pin()
    }

    /// Sends the code. The daemon answers `Welcome` and the snapshot when it
    /// matches, and drops the connection when it does not.
    pub async fn confirm(self, name: &str) -> Result<(Client, Snapshot), ConnectError> {
        let sas = self.stream.sas().to_owned();
        let datagrams = self.stream.datagrams();
        let stream = self.stream.into_stream();
        Client::connect_over(
            stream,
            ClientMessage::PairConfirm {
                sas,
                name: name.to_owned(),
            },
            Some(datagrams),
        )
        .await
    }
}

async fn drive<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    mut reader: FrameReader,
    out: Rc<Outbox>,
    pending: Pending,
    events: Sender<ClientEvent>,
    frames: Option<Sender<Frame>>,
) {
    loop {
        loop {
            let message = out.queue.borrow_mut().pop_front();
            let Some(message) = message else { break };
            if write(&mut stream, &message).await.is_err() {
                fail(&out, &pending);
                return;
            }
        }
        if events.is_closed() {
            break;
        }

        let incoming = reader.read_frame(&mut stream).fuse();
        let woken = wait_wake(&out.wake).fuse();
        futures_util::pin_mut!(incoming, woken);
        // The read is polled first: a completed frame is never dropped for a
        // queued request. The wake waiter is non-consuming.
        let selected = futures_util::select_biased! {
            message = incoming => message,
            () = woken => continue,
        };
        let Ok(frame) = selected else { break };
        match frame.class() {
            // Media is not a message and never becomes one: it goes to the
            // assembler, whole, on its own task.
            FrameClass::Media => {
                if let Some(frames) = &frames {
                    let _ = frames.try_send(frame);
                }
            }
            FrameClass::Control => match decode::<ServerMessage>(frame.payload()) {
                Ok(ServerMessage::Update(update)) => {
                    let _ = events.try_send(ClientEvent::Update(Box::new(update)));
                }
                Ok(ServerMessage::Snapshot(snapshot)) => {
                    let _ = events.try_send(ClientEvent::Snapshot(snapshot));
                }
                Ok(ServerMessage::Response { id, result }) => {
                    if let Some(waiter) = pending.borrow_mut().remove(&id) {
                        let _ = waiter.try_send(result);
                    }
                }
                // The handshake already consumed the only welcome.
                Ok(ServerMessage::Welcome { .. } | ServerMessage::Rejected(_)) => {}
                Err(_) => break,
            },
        }
    }
    fail(&out, &pending);
}

fn fail(out: &Outbox, pending: &Pending) {
    out.closed.set(true);
    out.wake.wake();
    for (_, waiter) in pending.borrow_mut().drain() {
        let _ = waiter.try_send(Err(RpcError::Disconnected));
    }
}

async fn wait_wake(wake: &Wake) {
    poll_fn(|context| {
        if wake.take_signal() {
            return Poll::Ready(());
        }
        wake.park(context);
        if wake.take_signal() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}
