//! Entity-level update bus between the daemon's spawned tasks and its loop.
//!
//! Producers are spawned components (registry, metrics, disk); the only consumer
//! is the daemon loop, which drains the queue and encodes it into each connected
//! UI. There is one queue, not one per connection: a slow connection is detected
//! by its own outbound buffer and dropped, and the UI resyncs from a snapshot.
//!
//! Updates are coalesced by entity on the way in, so a burst of changes to one
//! row costs the loop one frame carrying its latest value.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::num::NonZeroU64;
use std::rc::Rc;
use std::task::Poll;

use siphon_shared::riffle::LogEntry;
use siphon_shared::riffle::LogLevel;
use siphon_shared::riffle::Update;

use crate::constants::LOG_CAP;
use crate::spool::ConsumerWake;

/// One published change with the revision it belongs to.
#[derive(Debug, Clone)]
pub struct Broadcast {
    pub revision: u64,
    pub update: Update,
}

pub struct Bus {
    revision: Cell<u64>,
    /// Coalesced updates waiting for the daemon loop.
    pending: RefCell<Coalescer>,
    logs: RefCell<VecDeque<LogEntry>>,
    next_log: Cell<u64>,
    /// Wakes the loop when an update lands.
    wake: RefCell<Rc<ConsumerWake>>,
}

impl Bus {
    #[must_use]
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            revision: Cell::new(0),
            pending: RefCell::new(Coalescer::default()),
            logs: RefCell::new(VecDeque::with_capacity(LOG_CAP)),
            next_log: Cell::new(1),
            wake: RefCell::new(ConsumerWake::new()),
        })
    }

    /// Current revision a snapshot should carry.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision.get()
    }

    /// Appends one event with no stream identity.
    pub fn log(&self, level: LogLevel, target: &str, message: impl Into<String>) {
        self.log_channel(level, target, message, None, None);
    }

    /// Appends one event and queues it for every connected UI.
    ///
    /// The ring is updated before the publish, so a UI that reconnects between
    /// the two sees the line exactly once.
    pub fn log_channel(
        &self,
        level: LogLevel,
        target: &str,
        message: impl Into<String>,
        login: Option<String>,
        stream_id: Option<NonZeroU64>,
    ) {
        let seq = self.next_log.get();
        self.next_log.set(seq.wrapping_add(1));
        let entry = LogEntry {
            seq,
            at_ms: crate::time::now_ms(),
            level,
            target: target.to_owned(),
            message: message.into(),
            login,
            stream_id,
        };
        {
            let mut logs = self.logs.borrow_mut();
            if logs.len() == LOG_CAP {
                logs.pop_front();
            }
            logs.push_back(entry.clone());
        }
        self.publish(Update::Log(entry));
    }

    /// The event ring, oldest first.
    #[must_use]
    pub fn logs(&self) -> Vec<LogEntry> {
        self.logs.borrow().iter().cloned().collect()
    }

    /// Queues one complete entity update, coalescing it with an earlier value of
    /// the same entity that the loop has not drained yet.
    pub fn publish(&self, update: Update) {
        let revision = self.revision.get().wrapping_add(1);
        self.revision.set(revision);
        self.pending
            .borrow_mut()
            .push(Broadcast { revision, update });
        self.wake.borrow().wake();
    }

    /// Takes everything queued since the last drain, oldest revision first.
    #[must_use]
    pub fn take_pending(&self) -> Vec<Broadcast> {
        self.pending.borrow_mut().finish()
    }

    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.pending.borrow().entries.is_empty()
    }

    /// Resolves when an update is waiting. The loop re-arms this after draining.
    pub async fn wait(self: &Rc<Self>) {
        poll_fn(|context| {
            if self.has_pending() {
                return Poll::Ready(());
            }
            self.wake.borrow().park(context);
            if self.has_pending() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
}

/// Coalesces updates by entity so one frame carries the latest value of each
/// row instead of every intermediate step.
#[derive(Default)]
struct Coalescer {
    entries: Vec<Broadcast>,
    index: HashMap<Key, usize>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Key {
    Config,
    Conn,
    Pending,
    Channel(u64),
    Pipeline(u64, Option<String>),
    Arena,
    Downloader,
    Error,
    Update,
}

impl Key {
    fn of(update: &Update) -> Option<Self> {
        match update {
            Update::Config(_) => Some(Self::Config),
            Update::Conn(_) => Some(Self::Conn),
            Update::Pending(_) => Some(Self::Pending),
            Update::ChannelUpsert(channel) => Some(Self::Channel(channel.id)),
            Update::ChannelRemove(id) => Some(Self::Channel(*id)),
            Update::PipelineUpsert(pipeline) => Some(Self::Pipeline(
                pipeline.key.channel,
                pipeline.key.quality.clone(),
            )),
            Update::PipelineRemove(key) => Some(Self::Pipeline(key.channel, key.quality.clone())),
            Update::Arena(_) => Some(Self::Arena),
            Update::Downloader(_) => Some(Self::Downloader),
            Update::Error(_) => Some(Self::Error),
            Update::Update(_) => Some(Self::Update),
            // The segment ring, the event log and stream events are
            // append-only; coalescing would lose entries, and for a stream
            // event the transition *is* the value. Lanes are per connection and
            // written straight to the owning connection's outbound queue, so
            // they never reach the bus at all; naming them here means a future
            // publisher cannot coalesce one connection's row with another's.
            Update::Segment { .. }
            | Update::Log(_)
            | Update::StreamEvent(_)
            | Update::Lane(_)
            | Update::LaneRemove(_) => None,
        }
    }
}

impl Coalescer {
    fn push(&mut self, broadcast: Broadcast) {
        if let Some(key) = Key::of(&broadcast.update) {
            if let Some(&position) = self.index.get(&key) {
                self.entries[position] = broadcast;
                return;
            }
            self.index.insert(key, self.entries.len());
        }
        self.entries.push(broadcast);
    }

    fn finish(&mut self) -> Vec<Broadcast> {
        let mut entries = std::mem::take(&mut self.entries);
        self.index.clear();
        entries.sort_by_key(|broadcast| broadcast.revision);
        entries
    }
}

#[cfg(test)]
mod tests {
    use siphon_shared::riffle::ConnHealth;

    use super::*;

    #[test]
    fn one_entity_keeps_only_its_latest_value() {
        let bus = Bus::new();
        bus.publish(Update::Conn(ConnHealth::Connected));
        bus.publish(Update::Conn(ConnHealth::Connecting));
        let pending = bus.take_pending();
        assert_eq!(pending.len(), 1);
        assert!(matches!(
            pending[0].update,
            Update::Conn(ConnHealth::Connecting)
        ));
        assert_eq!(pending[0].revision, 2);
    }

    #[test]
    fn append_only_updates_are_not_coalesced() {
        let bus = Bus::new();
        bus.log(LogLevel::Info, "t", "one");
        bus.log(LogLevel::Info, "t", "two");
        let pending = bus.take_pending();
        assert_eq!(pending.len(), 2);
        assert!(!bus.has_pending());
    }

    #[test]
    fn revisions_are_monotonic_across_drains() {
        let bus = Bus::new();
        bus.publish(Update::Conn(ConnHealth::Connected));
        assert_eq!(bus.take_pending()[0].revision, 1);
        bus.publish(Update::Conn(ConnHealth::Connecting));
        assert_eq!(bus.take_pending()[0].revision, 2);
    }
}
