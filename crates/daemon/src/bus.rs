//! Entity-level update bus between the app/registry and RPC connections.
//!
//! Publishing never keeps a stream alive: subscribers are weak, and a slow
//! connection is closed instead of being queued indefinitely. The UI resyncs
//! from a snapshot after reconnecting.

use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;
use std::rc::Weak;

use siphon_rpc::Update;

/// How many updates a connection may fall behind before it is disconnected.
const BACKLOG: usize = 64;

/// One published change with the revision it belongs to.
#[derive(Debug, Clone)]
pub struct Broadcast {
    pub revision: u64,
    pub update: Update,
}

/// The receiving end of one connection's subscription.
pub struct Subscriber {
    tx: kanal::Sender<Broadcast>,
}

pub struct Bus {
    revision: Cell<u64>,
    subscribers: RefCell<Vec<Weak<Subscriber>>>,
}

impl Bus {
    #[must_use]
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            revision: Cell::new(0),
            subscribers: RefCell::new(Vec::new()),
        })
    }

    /// Current revision a snapshot should carry.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision.get()
    }

    /// Registers one connection. The caller keeps the `Subscriber` alive for
    /// the lifetime of the connection; the bus only holds a weak reference.
    #[must_use]
    pub fn subscribe(self: &Rc<Self>) -> (Rc<Subscriber>, kanal::Receiver<Broadcast>) {
        let (tx, rx) = kanal::bounded(BACKLOG);
        let subscriber = Rc::new(Subscriber { tx });
        self.subscribers
            .borrow_mut()
            .push(Rc::downgrade(&subscriber));
        (subscriber, rx)
    }

    /// Publishes one complete entity update. Bounded subscribers that are
    /// full are closed: the UI reconnects and asks for a snapshot.
    pub fn publish(&self, update: Update) {
        let revision = self.revision.get().wrapping_add(1);
        self.revision.set(revision);
        let broadcast = Broadcast { revision, update };
        self.subscribers.borrow_mut().retain(|weak| {
            let Some(subscriber) = weak.upgrade() else {
                return false;
            };
            match subscriber.tx.try_send(broadcast.clone()) {
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
    use siphon_rpc::ConnHealth;

    use super::*;

    #[test]
    fn publishing_reaches_every_live_subscriber() {
        let bus = Bus::new();
        let (first, first_rx) = bus.subscribe();
        let (second, second_rx) = bus.subscribe();
        bus.publish(Update::Conn(ConnHealth::Connected));
        assert_eq!(bus.revision(), 1);
        assert_eq!(first_rx.try_recv().unwrap().unwrap().revision, 1);
        assert_eq!(second_rx.try_recv().unwrap().unwrap().revision, 1);
        drop(first);
        drop(second);
        bus.publish(Update::Conn(ConnHealth::Connecting));
        assert_eq!(bus.revision(), 2);
    }
}
