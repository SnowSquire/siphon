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
use siphon_shared::rpc::PipelineView;
pub use siphon_shared::rpc::Request;
use siphon_shared::rpc::Snapshot;
use siphon_shared::rpc::StreamEvent;
use siphon_shared::rpc::StreamEventKind;
use siphon_shared::rpc::Update;
use siphon_shared::rpc::UpdateView;
use siphon_shared::settings::ChannelPrefs;
use siphon_shared::settings::ClientSettings;
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
    /// The client's own document: display, notification policy, per-channel
    /// switches, pins and connections. One handle, so one writer.
    pub settings: RwSignal<ClientSettings, LocalStorage>,
    /// Channel ids this client keeps at the top of the list, mirrored from
    /// `settings` for the ordering comparators.
    pub pins: RwSignal<Rc<Vec<u64>>, LocalStorage>,
    pub pending: RwSignal<Rc<Vec<String>>, LocalStorage>,
    pub pipelines: RwSignal<Rc<Vec<RwSignal<PipelineView, LocalStorage>>>, LocalStorage>,
    pub arena: RwSignal<ArenaSummary, LocalStorage>,
    pub downloader: RwSignal<DownloaderSummary, LocalStorage>,
    /// The notification centre. Client state: it does not survive a window
    /// restart, and nothing in the daemon knows it exists.
    pub notices: RwSignal<Rc<Vec<Notice>>, LocalStorage>,
    pub logs: RwSignal<Rc<Vec<LogEntry>>, LocalStorage>,
    /// When each channel was last seen going offline, for the title/game mute.
    muted: Rc<RefCell<std::collections::HashMap<u64, i64>>>,
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
    pub fn new(settings: RwSignal<ClientSettings, LocalStorage>) -> Self {
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
            arena: RwSignal::new_local(state.arena),
            downloader: RwSignal::new_local(state.downloader),
            notices: RwSignal::new_local(Rc::new(Vec::new())),
            logs: RwSignal::new_local(Rc::new(state.logs)),
            muted: Rc::new(RefCell::new(std::collections::HashMap::new())),
            notice_seq: Rc::new(Cell::new(1)),
        }
    }

    /// Everything, from a snapshot: the first connect, or a reconnect.
    fn reset(&self, state: Snapshot) {
        self.conn.set(state.conn);
        self.error.set(state.error);
        self.update.set(state.update);
        sync_rows(self.channels, state.channels, |channel| channel.id);
        self.reorder();
        self.pending.set(Rc::new(state.pending));
        sync_rows(self.pipelines, state.pipelines, |pipeline| {
            pipeline.key.clone()
        });
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
        crate::notify::show(
            &summary,
            &body,
            policy.sound,
            event.view.avatar_path.as_deref(),
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
        }
    }
}

#[cfg(test)]
mod tests {
    use siphon_shared::rpc::ChatColor;
    use zgui::reactive::Mounted;
    use zgui::reactive::install;

    use super::*;

    /// A window state over a fresh, unsaved settings document.
    fn state() -> UiState {
        UiState::new(RwSignal::new_local(ClientSettings::default()))
    }

    fn view(id: u64, viewers: u32) -> ChannelView {
        ChannelView {
            id,
            login: format!("channel{id}"),
            display_name: format!("Channel {id}"),
            chat_color: ChatColor::Twitch(0x00ff_ffff),
            avatar_path: None,
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
}
