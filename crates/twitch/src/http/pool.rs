use std::collections::VecDeque;
use std::time::Duration;
use std::time::Instant;

use crate::http::transport::Conn;

/// Key a connection is pooled under. TLS and plaintext to the same host must never
/// share a socket.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PoolKey {
    pub tls: bool,
    pub host: String,
    pub port: u16,
}

impl PoolKey {
    pub const fn new(tls: bool, host: String, port: u16) -> Self {
        Self { tls, host, port }
    }
}

struct Idle {
    key: PoolKey,
    conn: Conn,
    since: Instant,
}

/// Idle keep-alive connections.
///
/// Sized from the transport probe: a live prefetch owns one connection for its
/// whole body (~2 s), 2 covers the overlap with catch-up, and the manifest origin
/// gets its own key so it never competes with segment sockets.
pub struct Pool {
    idle: VecDeque<Idle>,
    max_idle: usize,
    max_idle_per_host: usize,
    max_idle_age: Duration,
}

impl Pool {
    pub const fn new() -> Self {
        Self {
            idle: VecDeque::new(),
            max_idle: 8,
            max_idle_per_host: 4,
            max_idle_age: Duration::from_secs(30),
        }
    }

    /// Take the most recently returned idle connection for `key`.
    pub fn take(&mut self, key: &PoolKey) -> Option<Conn> {
        self.prune();
        let position = self.idle.iter().rposition(|idle| &idle.key == key)?;
        self.idle.remove(position).map(|idle| idle.conn)
    }

    /// Return a connection to the pool. Callers must only do this for a body that
    /// ended cleanly on a connection the server did not ask to close.
    pub fn put(&mut self, key: PoolKey, conn: Conn) {
        self.prune();
        self.idle.push_back(Idle {
            key,
            conn,
            since: Instant::now(),
        });
        self.trim();
    }

    fn prune(&mut self) {
        let now = Instant::now();
        self.idle
            .retain(|idle| now.duration_since(idle.since) < self.max_idle_age);
    }

    fn trim(&mut self) {
        while self.idle.len() > self.max_idle {
            self.idle.pop_front();
        }

        // Oldest-first eviction per host.
        let mut per_host: Vec<(PoolKey, usize)> = Vec::new();
        let mut evict = Vec::new();
        for (index, idle) in self.idle.iter().enumerate() {
            match per_host.iter_mut().find(|(key, _)| key == &idle.key) {
                Some((_, count)) => {
                    *count += 1;
                    if *count > self.max_idle_per_host {
                        evict.push(index);
                    }
                }
                None => per_host.push((idle.key.clone(), 1)),
            }
        }
        for index in evict.into_iter().rev() {
            self.idle.remove(index);
        }
    }
}

impl Default for Pool {
    fn default() -> Self {
        Self::new()
    }
}
