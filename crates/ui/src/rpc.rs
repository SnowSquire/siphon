//! The UI-side RPC worker: connect, forward snapshots and updates, send intents.
//!
//! The worker runs on its own compio thread. The UI thread only renders the
//! latest snapshot and sends [`UiCommand`]s; it never blocks on the socket.

#![allow(clippy::future_not_send)]

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;

use siphon_rpc::ArenaSummary;
use siphon_rpc::Client;
use siphon_rpc::ClientEvent;
use siphon_rpc::Config;
use siphon_rpc::ConnHealth;
use siphon_rpc::DownloaderSummary;
use siphon_rpc::Request;
use siphon_rpc::Snapshot;
use siphon_rpc::Update;

/// One UI action.
#[derive(Debug)]
pub enum UiCommand {
    Request(Request),
}

/// One snapshot or entity update for the UI thread.
#[derive(Debug, Clone)]
pub enum UiEvent {
    Snapshot(Box<Snapshot>),
    Update(Update),
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

/// The UI's sending end for intents.
#[derive(Clone)]
pub struct IntentSender {
    tx: kanal::Sender<UiCommand>,
    wake: Arc<Wake>,
}

impl IntentSender {
    /// Queues an intent and wakes the worker. The command stays in the channel
    /// until the worker drains it, so a wake that races an update cannot drop a
    /// user action.
    pub fn send(&self, command: UiCommand) {
        let _ = self.tx.try_send(command);
        self.wake.wake();
    }
}

/// Creates the intent channel, its sender and the shared wake.
#[must_use]
pub fn intent_channel() -> (IntentSender, kanal::Receiver<UiCommand>, Arc<Wake>) {
    let (tx, rx) = kanal::unbounded();
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

/// A fresh snapshot for the first frame.
#[must_use]
pub fn empty_snapshot() -> Snapshot {
    Snapshot {
        revision: 0,
        config: Config::default(),
        conn: ConnHealth::Connecting,
        channels: Vec::new(),
        pending: Vec::new(),
        pipelines: Vec::new(),
        arena: ArenaSummary::default(),
        downloader: DownloaderSummary::default(),
        notifications: Vec::new(),
        error: None,
        update: None,
    }
}

/// Runs forever, reconnecting with backoff. One connection at a time.
pub async fn run(
    intents: kanal::Receiver<UiCommand>,
    wake: Arc<Wake>,
    events: kanal::Sender<UiEvent>,
) {
    let endpoint = siphon_rpc::admin_endpoint(crate::service::APP_ID);
    tracing::info!(target: "rpc", %endpoint, "admin endpoint");
    let mut backoff = Duration::from_secs(1);
    loop {
        let connected = match Client::connect(&endpoint, env!("CARGO_PKG_VERSION")).await {
            Ok((client, snapshot)) => {
                backoff = Duration::from_secs(1);
                let _ = events.try_send(UiEvent::Snapshot(Box::new(snapshot)));
                serve_connection(client, &intents, &wake, &events).await
            }
            Err(error) => {
                let _ = events.try_send(UiEvent::Update(Update::Error(Some(format!(
                    "backend: {error}"
                )))));
                false
            }
        };
        if !connected {
            let _ = events.try_send(UiEvent::Update(Update::Error(Some(
                "backend disconnected".to_owned(),
            ))));
        }
        compio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(2));
    }
}

/// Serves one connection; returns false when the UI went away.
async fn serve_connection(
    client: Client,
    intents: &kanal::Receiver<UiCommand>,
    wake: &Arc<Wake>,
    events: &kanal::Sender<UiEvent>,
) -> bool {
    let client = Rc::new(client);
    let stream = client.events();
    let queue: Rc<RefCell<VecDeque<ClientEvent>>> = Rc::new(RefCell::new(VecDeque::new()));
    let closed = Rc::new(Cell::new(false));

    // The forwarder owns the async receive; the worker drains its queue. A
    // select between the two receivers would drop a completed receive.
    let feed_queue = Rc::clone(&queue);
    let feed_closed = Rc::clone(&closed);
    let feed_wake = Arc::clone(wake);
    compio::runtime::spawn(async move {
        while let Ok(event) = stream.recv().await {
            feed_queue.borrow_mut().push_back(event);
            feed_wake.wake();
        }
        feed_closed.set(true);
        feed_wake.wake();
    })
    .detach();

    loop {
        wait(wake, || {
            closed.get() || !queue.borrow().is_empty() || !intents.is_empty()
        })
        .await;

        let disconnected = closed.get() && queue.borrow().is_empty();

        while let Ok(Some(command)) = intents.try_recv() {
            let UiCommand::Request(request) = command;
            if let Err(error) = client.request(request).await {
                let _ = events.try_send(UiEvent::Update(Update::Error(Some(error.to_string()))));
            }
        }

        loop {
            let event = queue.borrow_mut().pop_front();
            let Some(event) = event else { break };
            match event {
                ClientEvent::Snapshot(snapshot) => {
                    let _ = events.try_send(UiEvent::Snapshot(snapshot));
                }
                ClientEvent::Update { update, .. } => {
                    let _ = events.try_send(UiEvent::Update(update));
                }
            }
        }

        if disconnected {
            return true;
        }
        if (intents.is_closed() && intents.is_empty()) || events.is_closed() {
            return false;
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

/// Applies one entity update to a snapshot. Idempotent: applying it twice is
/// harmless.
pub fn apply_update(state: &mut Snapshot, update: Update) {
    match update {
        Update::Config(config) => {
            state.config = config;
            state.channels.retain(|view| {
                state
                    .config
                    .channels
                    .iter()
                    .any(|channel| channel.id == view.id)
            });
        }
        Update::Conn(conn) => state.conn = conn,
        Update::ChannelUpsert(view) => {
            if let Some(existing) = state.channels.iter_mut().find(|row| row.id == view.id) {
                *existing = view;
            } else {
                state.channels.push(view);
            }
        }
        Update::ChannelRemove(id) => state.channels.retain(|row| row.id != id),
        Update::Pending(pending) => state.pending = pending,
        Update::PipelineUpsert(pipeline) => {
            if let Some(existing) = state
                .pipelines
                .iter_mut()
                .find(|row| row.key == pipeline.key)
            {
                *existing = pipeline;
            } else {
                state.pipelines.push(pipeline);
            }
        }
        Update::PipelineRemove(key) => state.pipelines.retain(|row| row.key != key),
        Update::Segment { .. } => {}
        Update::Arena(arena) => state.arena = arena,
        Update::Downloader(downloader) => state.downloader = downloader,
        Update::Notification(notification) => {
            if let Some(existing) = state
                .notifications
                .iter_mut()
                .find(|row| row.id == notification.id)
            {
                *existing = notification;
            } else {
                state.notifications.push(notification);
            }
        }
        Update::NotificationDismissed(id) => {
            state.notifications.retain(|row| row.id != id);
        }
        Update::Error(error) => state.error = error,
        Update::Update(update) => state.update = update,
    }
}
