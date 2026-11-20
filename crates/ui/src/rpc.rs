//! The UI-side RPC worker: connect, forward snapshots and updates, send intents.
//!
//! The worker runs on its own compio thread. The UI thread only renders the
//! latest snapshot and sends [`Request`]s; it never blocks on the socket.

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

use siphon_shared::rpc::ArenaSummary;
use siphon_shared::rpc::ChannelView;
use siphon_shared::rpc::Client;
use siphon_shared::rpc::ClientEvent;
use siphon_shared::rpc::ConnHealth;
use siphon_shared::rpc::DownloaderSummary;
use siphon_shared::rpc::LogEntry;
use siphon_shared::rpc::NotificationView;
use siphon_shared::rpc::PipelineView;
pub use siphon_shared::rpc::Request;
use siphon_shared::rpc::Snapshot;
use siphon_shared::rpc::Update;
use siphon_shared::rpc::UpdateView;
use zgui::reactive::LocalStorage;
use zgui::reactive::RwSignal;
use zgui::reactive::prelude::*;

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
    tx: kanal::Sender<Request>,
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
        let queued = matches!(self.tx.try_send(request), Ok(true));
        self.wake.wake();
        queued
    }
}

/// Creates the intent channel, its sender and the shared wake.
#[must_use]
pub fn intent_channel() -> (IntentSender, kanal::Receiver<Request>, Arc<Wake>) {
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
    /// The backend went away; reconnect.
    BackendLost,
}

/// Runs forever, reconnecting with backoff. One connection at a time.
pub async fn run(
    intents: kanal::Receiver<Request>,
    wake: Arc<Wake>,
    events: kanal::Sender<UiEvent>,
) {
    let endpoint = siphon_shared::rpc::admin_endpoint(crate::service::APP_ID);
    tracing::info!(target: "rpc", %endpoint, "admin endpoint");
    let mut backoff = Duration::from_secs(1);
    loop {
        let failure = match Client::connect(&endpoint, env!("CARGO_PKG_VERSION")).await {
            Ok((client, snapshot)) => {
                backoff = Duration::from_secs(1);
                let _ = events.try_send(UiEvent::Snapshot(Box::new(snapshot)));
                match serve_connection(client, &intents, &wake, &events).await {
                    End::UiGone => return,
                    End::BackendLost => Some("backend disconnected".to_owned()),
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

/// Serves one connection until the backend or the UI goes away.
async fn serve_connection(
    client: Client,
    intents: &kanal::Receiver<Request>,
    wake: &Arc<Wake>,
    events: &kanal::Sender<UiEvent>,
) -> End {
    let client = Rc::new(client);
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

    loop {
        wait(wake, || {
            closed.get() || !queue.borrow().is_empty() || !intents.is_empty()
        })
        .await;

        let disconnected = closed.get() && queue.borrow().is_empty();

        while let Ok(Some(request)) = intents.try_recv() {
            // Fire and forget: a slow handler must not stall the update stream,
            // and the ack would be discarded anyway. A rejected intent arrives
            // as an `Update`.
            if let Err(error) = client.send(request) {
                let _ = events.try_send(UiEvent::Update(Box::new(Update::Error(Some(
                    error.to_string(),
                )))));
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
            return End::BackendLost;
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
    pub pending: RwSignal<Rc<Vec<String>>, LocalStorage>,
    pub pipelines: RwSignal<Rc<Vec<RwSignal<PipelineView, LocalStorage>>>, LocalStorage>,
    pub arena: RwSignal<ArenaSummary, LocalStorage>,
    pub downloader: RwSignal<DownloaderSummary, LocalStorage>,
    pub notifications: RwSignal<Rc<Vec<NotificationView>>, LocalStorage>,
    pub logs: RwSignal<Rc<Vec<LogEntry>>, LocalStorage>,
    pub words: RwSignal<Rc<Vec<String>>, LocalStorage>,
    pub title_changes: RwSignal<bool, LocalStorage>,
    pub sound: RwSignal<bool, LocalStorage>,
}

/// A list of rows, each behind its own signal.
///
/// The keyed list watches the *key list* and nothing else, so an update to one
/// row's data has to travel through that row's signal: rewriting the vector
/// would rebuild every row, and rebuilding only the changed one is not
/// something a keyed list does for you. Mutating a row signal touches no list.
type Rows<T> = RwSignal<Rc<Vec<RwSignal<T, LocalStorage>>>, LocalStorage>;

/// Replaces one row's data, or appends a row for a key not seen before.
fn upsert_row<T, K>(list: &Rows<T>, item: T, key: impl Fn(&T) -> K)
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
fn sync_rows<T, K>(list: &Rows<T>, incoming: Vec<T>, key: impl Fn(&T) -> K)
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
fn retain_rows<T>(list: &Rows<T>, keep: impl Fn(&T) -> bool)
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

impl UiState {
    /// The state a window starts with, before the first snapshot arrives.
    #[must_use]
    pub fn new() -> Self {
        let state = empty_snapshot();
        Self {
            conn: RwSignal::new_local(state.conn),
            error: RwSignal::new_local(state.error),
            update: RwSignal::new_local(state.update),
            channels: RwSignal::new_local(Rc::new(Vec::new())),
            pending: RwSignal::new_local(Rc::new(state.pending)),
            pipelines: RwSignal::new_local(Rc::new(Vec::new())),
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
        sync_rows(&self.channels, state.channels, |channel| channel.id);
        self.pending.set(Rc::new(state.pending));
        sync_rows(&self.pipelines, state.pipelines, |pipeline| {
            pipeline.key.clone()
        });
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
            // One row of the list, not the list: the row's own signal carries
            // the change, so the keyed list never re-runs.
            Update::ChannelUpsert(view) => upsert_row(&self.channels, view, |channel| channel.id),
            Update::ChannelRemove(id) => {
                retain_rows(&self.channels, |channel| channel.id != id);
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
                let ids: Vec<u64> = config.channels.iter().map(|channel| channel.id).collect();
                retain_rows(&self.channels, |channel| ids.contains(&channel.id));
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
                upsert_row(&self.pipelines, view, |pipeline| pipeline.key.clone());
                // The Recordings list filters on this, and a keyed list does not
                // watch row data: re-run it when a row joins or leaves.
                if was != Some(recorded) {
                    let rows = (*self.pipelines.get_untracked()).clone();
                    self.pipelines.set(Rc::new(rows));
                }
            }
            Update::PipelineRemove(key) => {
                retain_rows(&self.pipelines, |pipeline| pipeline.key != key);
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

    /// Applies one event to the signals the view reads.
    pub fn apply_event(&self, event: UiEvent) {
        match event {
            UiEvent::Snapshot(state) => self.reset(*state),
            UiEvent::Update(update) => self.apply(*update),
        }
    }
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use zgui::reactive::Mounted;
    use zgui::reactive::install;

    use super::*;

    fn view(id: u64, viewers: u32) -> ChannelView {
        ChannelView {
            id,
            login: format!("channel{id}"),
            display_name: format!("Channel {id}"),
            chat_color: "#ffffff".to_owned(),
            profile_image_url: String::new(),
            avatar_path: None,
            live: true,
            title: Some("a title".to_owned()),
            game: None,
            game_id: None,
            viewers: Some(viewers),
            collaboration_viewers: None,
            stream_start_ms: None,
            stream_id: None,
            notify_live: true,
            notify_titles: true,
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
            let state = UiState::new();
            state.reset(Snapshot {
                channels: vec![view(1, 10)],
                ..Snapshot::default()
            });

            let rows = state.channels.get_untracked();
            let row = rows[0];
            state.apply(Update::ChannelUpsert(view(1, 99)));

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
            let state = UiState::new();
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
            let state = UiState::new();
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
}
