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
use crate::riffle::codec::FrameReader;
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

/// A single-waiter, signalled wake so a request queued between the drain and
/// the park is not missed.
struct Wake {
    waker: Cell<Option<Waker>>,
    signalled: Cell<bool>,
}

impl Wake {
    const fn new() -> Self {
        Self {
            waker: Cell::new(None),
            signalled: Cell::new(false),
        }
    }

    fn wake(&self) {
        self.signalled.set(true);
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    fn park(&self, context: &std::task::Context<'_>) {
        self.waker.set(Some(context.waker().clone()));
    }

    const fn take_signal(&self) -> bool {
        self.signalled.replace(false)
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
}

impl Client {
    /// Connects, performs the Hello/Welcome exchange, and consumes the first
    /// snapshot. The remaining traffic is driven by the connection task.
    pub async fn connect(
        endpoint: &str,
        app_version: &str,
    ) -> Result<(Self, Snapshot), ConnectError> {
        let mut stream = endpoint::connect(endpoint).await?;

        write(
            &mut stream,
            &ClientMessage::Hello {
                magic: MAGIC,
                protocol: PROTOCOL_VERSION,
                app_version: app_version.to_owned(),
            },
        )
        .await?;

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
        let task_pending = Rc::clone(&pending);
        let task_out = Rc::clone(&out);
        compio::runtime::spawn(async move {
            drive(stream, reader, task_out, task_pending, events_tx).await;
        })
        .detach();

        Ok((
            Self {
                out,
                events: events_rx.to_async(),
                pending,
                next_id: Cell::new(0),
            },
            snapshot,
        ))
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

async fn drive<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    mut reader: FrameReader,
    out: Rc<Outbox>,
    pending: Pending,
    events: Sender<ClientEvent>,
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

        let incoming = reader.read::<ServerMessage, _>(&mut stream).fuse();
        let woken = wait_wake(&out.wake).fuse();
        futures_util::pin_mut!(incoming, woken);
        // The read is polled first: a completed frame is never dropped for a
        // queued request. The wake waiter is non-consuming.
        let selected = futures_util::select_biased! {
            message = incoming => message,
            () = woken => continue,
        };
        match selected {
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
