//! Non-blocking operator notifications.
//!
//! The disk output sends messages about conditions that contradict the
//! assumptions about Twitch (a skipped DVR chunk, an alignment mismatch, a DVR
//! playlist that appears too late). Sending must never block the reactor, so
//! `notify` is a channel send and a detached task performs the HTTP request.

use std::rc::Rc;

use tracing::debug;
use tracing::warn;

use crate::http::Client;

/// A cloned handle to the notifier task's queue.
#[derive(Clone)]
pub struct Notifier {
    tx: kanal::Sender<String>,
}

impl Notifier {
    /// `url` is a webhook; the body is `{"content": "..."}`, which Discord and
    /// most compatible endpoints accept.
    #[must_use]
    pub fn new(client: Rc<Client>, url: String) -> Self {
        let (tx, rx) = kanal::unbounded();
        compio::runtime::spawn(async move {
            let rx = rx.to_async();
            while let Ok(message) = rx.recv().await {
                let Ok(body) = sonic_rs::to_vec(&sonic_rs::json!({ "content": message })) else {
                    continue;
                };
                match client
                    .post(&url, "application/json", &body, &[])
                    .await
                {
                    Ok(mut response) => {
                        if !response.status.is_success() {
                            warn!(status = %response.status, "notify: webhook rejected the message");
                            continue;
                        }
                        // Drain the body so the pooled connection is reusable.
                        let _ = response.body.read_to_end(4096).await;
                        debug!("notify: sent");
                    }
                    Err(error) => warn!(%error, "notify: webhook request failed"),
                }
            }
        })
        .detach();
        Self { tx }
    }

    pub fn notify(&self, message: impl Into<String>) {
        let _ = self.tx.try_send(message.into());
    }
}
