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
use siphon_rpc::ChannelView;
use siphon_rpc::Client;
use siphon_rpc::ClientEvent;
use siphon_rpc::Config;
use siphon_rpc::ConnHealth;
use siphon_rpc::DownloaderSummary;
use siphon_rpc::LogEntry;
use siphon_rpc::NotificationView;
use siphon_rpc::PipelineView;
use siphon_rpc::Request;
use siphon_rpc::Snapshot;
use siphon_rpc::Update;
use siphon_rpc::UpdateView;
use zgui::reactive::LocalStorage;
use zgui::reactive::RwSignal;
use zgui::reactive::prelude::*;

/// One UI action.
#[derive(Debug)]
pub enum UiCommand {
    Request(Request),
}

/// One snapshot or entity update for the UI thread.
#[derive(Debug, Clone)]
pub enum UiEvent {
    Snapshot(Box<Snapshot>),
    Update(Box<Update>),
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

/// How many notifications and log lines the window keeps.
const NOTIFICATION_CAP: usize = 64;
const LOG_CAP: usize = 512;

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
        logs: Vec::new(),
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
                let _ = events.try_send(UiEvent::Update(Box::new(Update::Error(Some(format!(
                    "backend: {error}"
                ))))));
                false
            }
        };
        if !connected {
            let _ = events.try_send(UiEvent::Update(Box::new(Update::Error(Some(
                "backend disconnected".to_owned(),
            )))));
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
                let _ = events.try_send(UiEvent::Update(Box::new(Update::Error(Some(
                    error.to_string(),
                )))));
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
    pub channels: RwSignal<Rc<Vec<ChannelView>>, LocalStorage>,
    pub pending: RwSignal<Rc<Vec<String>>, LocalStorage>,
    pub pipelines: RwSignal<Rc<Vec<PipelineView>>, LocalStorage>,
    pub arena: RwSignal<ArenaSummary, LocalStorage>,
    pub downloader: RwSignal<DownloaderSummary, LocalStorage>,
    pub notifications: RwSignal<Rc<Vec<NotificationView>>, LocalStorage>,
    pub logs: RwSignal<Rc<Vec<LogEntry>>, LocalStorage>,
    pub words: RwSignal<Rc<Vec<String>>, LocalStorage>,
    pub title_changes: RwSignal<bool, LocalStorage>,
    pub sound: RwSignal<bool, LocalStorage>,
}

impl UiState {
    /// The state a window starts with, before the first snapshot arrives.
    #[must_use]
    pub fn new() -> Self {
        let state = empty_snapshot();
        Self {
            conn: RwSignal::new_local(state.conn),
            error: RwSignal::new_local(state.error),
            update: RwSignal::new_local(state.update),
            channels: RwSignal::new_local(Rc::new(state.channels)),
            pending: RwSignal::new_local(Rc::new(state.pending)),
            pipelines: RwSignal::new_local(Rc::new(state.pipelines)),
            arena: RwSignal::new_local(state.arena),
            downloader: RwSignal::new_local(state.downloader),
            notifications: RwSignal::new_local(Rc::new(state.notifications)),
            logs: RwSignal::new_local(Rc::new(state.logs)),
            words: RwSignal::new_local(Rc::new(state.config.filtered_words)),
            title_changes: RwSignal::new_local(state.config.notify_title_changes),
            sound: RwSignal::new_local(state.config.sound),
        }
    }

    /// Everything, from a snapshot: the first connect, or a reconnect.
    fn reset(&self, state: Snapshot) {
        self.conn.set(state.conn);
        self.error.set(state.error);
        self.update.set(state.update);
        self.channels.set(Rc::new(state.channels));
        self.pending.set(Rc::new(state.pending));
        self.pipelines.set(Rc::new(state.pipelines));
        self.arena.set(state.arena);
        self.downloader.set(state.downloader);
        self.notifications.set(Rc::new(state.notifications));
        self.logs.set(Rc::new(state.logs));
        self.words.set(Rc::new(state.config.filtered_words));
        self.title_changes.set(state.config.notify_title_changes);
        self.sound.set(state.config.sound);
    }

    /// One entity update, applied to the signal it names and nowhere else.
    fn apply(&self, update: Update) {
        match update {
            Update::Conn(conn) => {
                if self.conn.get_untracked() != conn {
                    self.conn.set(conn);
                }
            }
            // One row of the list, not the list: the clone is what the keyed
            // list needs to iterate, and it is the only allocation this costs.
            Update::ChannelUpsert(view) => {
                let mut channels = (*self.channels.get_untracked()).clone();
                if let Some(existing) = channels.iter_mut().find(|row| row.id == view.id) {
                    *existing = view;
                } else {
                    channels.push(view);
                }
                self.channels.set(Rc::new(channels));
            }
            Update::ChannelRemove(id) => {
                let mut channels = (*self.channels.get_untracked()).clone();
                channels.retain(|row| row.id != id);
                self.channels.set(Rc::new(channels));
            }
            Update::Pending(pending) => {
                if *self.pending.get_untracked() != pending {
                    self.pending.set(Rc::new(pending));
                }
            }
            // The config names the channels that may exist, so removing one from
            // it is the only thing that can drop a row here.
            Update::Config(config) => {
                if self.title_changes.get_untracked() != config.notify_title_changes {
                    self.title_changes.set(config.notify_title_changes);
                }
                if self.sound.get_untracked() != config.sound {
                    self.sound.set(config.sound);
                }
                if *self.words.get_untracked() != config.filtered_words {
                    self.words.set(Rc::new(config.filtered_words));
                }
                let mut channels = (*self.channels.get_untracked()).clone();
                let before = channels.len();
                channels.retain(|row| config.channels.iter().any(|channel| channel.id == row.id));
                if channels.len() != before {
                    self.channels.set(Rc::new(channels));
                }
            }
            Update::PipelineUpsert(view) => {
                let mut pipelines = (*self.pipelines.get_untracked()).clone();
                if let Some(existing) = pipelines.iter_mut().find(|row| row.key == view.key) {
                    *existing = view;
                } else {
                    pipelines.push(view);
                }
                self.pipelines.set(Rc::new(pipelines));
            }
            Update::PipelineRemove(key) => {
                let mut pipelines = (*self.pipelines.get_untracked()).clone();
                pipelines.retain(|row| row.key != key);
                self.pipelines.set(Rc::new(pipelines));
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
            Update::Notification(notification) => {
                let mut notifications = (*self.notifications.get_untracked()).clone();
                if !notifications
                    .iter()
                    .any(|existing| existing.id == notification.id)
                {
                    notifications.push(notification);
                    if notifications.len() > NOTIFICATION_CAP {
                        let drop = notifications.len() - NOTIFICATION_CAP;
                        notifications.drain(..drop);
                    }
                    self.notifications.set(Rc::new(notifications));
                }
            }
            Update::NotificationDismissed(id) => {
                let mut notifications = (*self.notifications.get_untracked()).clone();
                let before = notifications.len();
                notifications.retain(|notification| notification.id != id);
                if notifications.len() != before {
                    self.notifications.set(Rc::new(notifications));
                }
            }
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

    /// Drains the worker's channel. Answers whether anything landed, which is what
    /// decides whether the frame owes anything at all.
    pub fn drain(&self, events: &kanal::Receiver<UiEvent>) -> bool {
        let mut landed = false;
        while let Ok(Some(event)) = events.try_recv() {
            match event {
                UiEvent::Snapshot(state) => self.reset(*state),
                UiEvent::Update(update) => self.apply(*update),
            }
            landed = true;
        }
        landed
    }
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}
