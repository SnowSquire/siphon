//! Entity-level update bus between the app/registry and RPC connections.
//!
//! Publishing never keeps a stream alive: subscribers are weak, and a slow
//! connection is closed instead of being queued indefinitely. The UI resyncs
//! from a snapshot after reconnecting.
//!
//! Control frames and updates share one bounded queue. A writer that selected
//! between two receive queues could drop an already-completed receive when both
//! became ready, losing a response or an update; one queue removes that choice.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::num::NonZeroU64;
use std::rc::Rc;
use std::rc::Weak;

use siphon_shared::rpc::LogEntry;
use siphon_shared::rpc::LogLevel;
use siphon_shared::rpc::ServerMessage;
use siphon_shared::rpc::Update;

/// How many frames a connection may fall behind before it is disconnected.
const BACKLOG: usize = 64;

/// How many event-log lines a snapshot carries.
///
/// The ring is bounded because the daemon outlives any UI: a full history is a
/// leak that only looks like a feature.
pub const LOG_CAP: usize = 512;

/// One published change with the revision it belongs to.
#[derive(Debug, Clone)]
pub struct Broadcast {
    pub revision: u64,
    pub update: Update,
}

/// One frame for a connection: a request response or a published update.
#[derive(Debug, Clone)]
pub enum Outbound {
    Control(ServerMessage),
    Update(Broadcast),
}

/// The sending end of one connection's queue.
pub struct Subscriber {
    tx: kanal::Sender<Outbound>,
}

impl Subscriber {
    /// Queues a request response. Returns false when the connection is gone or
    /// too far behind; the client will resync.
    #[must_use]
    pub fn control(&self, message: ServerMessage) -> bool {
        let sent = matches!(self.tx.try_send(Outbound::Control(message)), Ok(true));
        if sent {
        } else {
            let _ = self.tx.close();
        }
        sent
    }

    /// Closes the queue, which stops the writer after it drains.
    pub fn close(&self) {
        let _ = self.tx.close();
    }
}

pub struct Bus {
    revision: Cell<u64>,
    subscribers: RefCell<Vec<Weak<Subscriber>>>,
    logs: RefCell<VecDeque<LogEntry>>,
    next_log: Cell<u64>,
}

impl Bus {
    #[must_use]
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            revision: Cell::new(0),
            subscribers: RefCell::new(Vec::new()),
            logs: RefCell::new(VecDeque::with_capacity(LOG_CAP)),
            next_log: Cell::new(1),
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

    /// Appends one event and sends it to every connected UI.
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

    /// Registers one connection. The caller keeps the `Subscriber` alive for
    /// the lifetime of the connection; the bus only holds a weak reference.
    #[must_use]
    pub fn subscribe(self: &Rc<Self>) -> (Rc<Subscriber>, kanal::Receiver<Outbound>) {
        let (tx, rx) = kanal::bounded(BACKLOG);
        let subscriber = Rc::new(Subscriber { tx });
        self.subscribers
            .borrow_mut()
            .push(Rc::downgrade(&subscriber));
        (subscriber, rx)
    }

    /// Publishes one complete entity update. Bounded subscribers that are full
    /// are closed: the UI reconnects and asks for a snapshot.
    pub fn publish(&self, update: Update) {
        let revision = self.revision.get().wrapping_add(1);
        self.revision.set(revision);
        let broadcast = Broadcast { revision, update };
        self.subscribers.borrow_mut().retain(|weak| {
            let Some(subscriber) = weak.upgrade() else {
                return false;
            };
            match subscriber.tx.try_send(Outbound::Update(broadcast.clone())) {
                Ok(true) => true,
                Ok(false) => {
                    tracing::warn!("rpc: dropping a lagging UI connection");
                    let _ = subscriber.tx.close();
                    false
                }
                Err(_) => false,
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use siphon_shared::rpc::ConnHealth;

    use super::*;

    fn revision(frame: Option<Outbound>) -> Option<u64> {
        match frame {
            Some(Outbound::Update(broadcast)) => Some(broadcast.revision),
            Some(Outbound::Control(_)) | None => None,
        }
    }

    #[test]
    fn publishing_reaches_every_live_subscriber() {
        let bus = Bus::new();
        let (first, first_rx) = bus.subscribe();
        let (second, second_rx) = bus.subscribe();
        bus.publish(Update::Conn(ConnHealth::Connected));
        assert_eq!(bus.revision(), 1);
        assert_eq!(revision(first_rx.try_recv().unwrap()), Some(1));
        assert_eq!(revision(second_rx.try_recv().unwrap()), Some(1));
        drop(first);
        drop(second);
        bus.publish(Update::Conn(ConnHealth::Connecting));
        assert_eq!(bus.revision(), 2);
    }

    #[test]
    fn control_and_updates_share_one_ordered_queue() {
        let bus = Bus::new();
        let (subscriber, rx) = bus.subscribe();
        assert!(subscriber.control(ServerMessage::Rejected("x".to_owned())));
        bus.publish(Update::Conn(ConnHealth::Connected));
        assert!(matches!(
            rx.try_recv().unwrap(),
            Some(Outbound::Control(ServerMessage::Rejected(_)))
        ));
        assert_eq!(revision(rx.try_recv().unwrap()), Some(1));
    }
}
