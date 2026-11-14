//! Daemon mode: one long-lived RTSP server whose streams are created on
//! demand from the request URI.
//!
//! Nothing remote is fetched at startup. `DESCRIBE /<channel>/<quality>/`
//! resolves the channel (one GQL variant fetch per login), picks the variant
//! and builds that variant's pipeline: `Stream`, `WakeSet`, consumer
//! registration channel, `Dvr`, media engine and scheduler. The last control
//! connection for a stream drops it again, along with the whole pipeline.
//!
//! `URI`: path segments, never query parameters. ffmpeg appends a relative
//! `a=control:trackID=0` by string concatenation, so a query would swallow the
//! track fragment.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::poll_fn;
use std::path::PathBuf;
use std::rc::Rc;
use std::task::Poll;
use std::task::Waker;

use futures_util::future::LocalBoxFuture;
use kanal::AsyncSender;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::arena::ArenaSet;
use crate::constants::DEBUG_CHANNEL_TITLE;
use crate::constants::DEBUG_DVR_URL;
use crate::disk::DiskOptions;
use crate::disk::DiskOutput;
use crate::downloader::ToDownloader;
use crate::http::Client;
use crate::notify::Notifier;
use crate::rtsp::ResolveError;
use crate::rtsp::Stream;
use crate::rtsp::StreamKey;
use crate::rtsp::StreamResolver;
use crate::rtsp::dvr::Dvr;
use crate::rtsp::parse_stream_key;
use crate::scheduler::ConsumerRegistration;
use crate::scheduler::StreamScheduler;
use crate::spool::ConsumerWake;
use crate::spool::WakeSet;
use crate::variant::Variant;
use crate::variant::VariantPlaylistError;
use crate::variant::choose_variant;
use crate::variant::debug_variant;
use crate::variant::get_variants;

/// Options shared by every stream the daemon creates.
pub struct Options {
    pub client_id: Option<String>,
    pub auth_token: Option<String>,
    pub low_latency: bool,
    pub debug: bool,
    pub max_streams: usize,
    pub output: Option<PathBuf>,
    pub notify: Option<Notifier>,
    pub keep_runs: bool,
}

/// Everything the daemon shares between streams.
pub struct Registry {
    client: Rc<Client>,
    arena: Rc<ArenaSet>,
    downloaders: AsyncSender<ToDownloader>,
    options: Options,
    /// Resolved logins, kept while any stream of that channel exists.
    channels: RefCell<HashMap<String, Rc<Channel>>>,
    /// Live streams, by `(login, quality)`.
    streams: RefCell<HashMap<StreamKey, Rc<StreamRuntime>>>,
    /// Serialises resolution per channel and per stream key.
    gate: Gate,
    /// Disk recordings that finished, so the stream entry can be released.
    finished: kanal::Sender<StreamKey>,
    finished_rx: RefCell<Option<kanal::Receiver<StreamKey>>>,
}

/// A channel's cached resolution: the title, the variant list and Twitch's
/// broadcast id (the disk output's stream id).
struct Channel {
    title: String,
    variants: Vec<Variant>,
    broadcast_id: u64,
}

/// One running pipeline. Dropping the registration sender and setting the stop
/// flags is what tears the scheduler, media engine and DVR refresh down.
struct StreamRuntime {
    stream: Rc<Stream>,
    dvr: Rc<Dvr>,
    registration: RefCell<Option<kanal::Sender<ConsumerRegistration>>>,
    /// The scheduler parks on this latch instead of polling; a closed
    /// registration channel is only noticed when the task is polled again.
    reader_wake: Rc<ConsumerWake>,
    disk: RefCell<Option<Rc<DiskOutput>>>,
    /// A recording keeps the entry (and its URLs) alive without a viewer.
    recording: Cell<bool>,
}

impl StreamRuntime {
    fn stop(&self) {
        self.stream.stop();
        self.dvr.stop();
        self.reader_wake.wake();
        self.registration.borrow_mut().take();
        if let Some(disk) = self.disk.borrow().as_ref() {
            disk.stop();
        }
    }

    const fn is_recording(&self) -> bool {
        self.recording.get()
    }
}

impl Registry {
    #[must_use]
    pub fn new(
        client: Rc<Client>,
        arena: Rc<ArenaSet>,
        downloaders: AsyncSender<ToDownloader>,
        options: Options,
    ) -> Self {
        let (finished_tx, finished_rx) = kanal::unbounded();
        Self {
            client,
            arena,
            downloaders,
            options,
            channels: RefCell::new(HashMap::new()),
            streams: RefCell::new(HashMap::new()),
            gate: Gate::default(),
            finished: finished_tx,
            finished_rx: RefCell::new(Some(finished_rx)),
        }
    }

    /// Drain the recordings that finished. One task for the whole registry.
    pub fn watch_finished(self: &Rc<Self>) {
        let Some(rx) = self.finished_rx.borrow_mut().take() else {
            return;
        };
        let registry = Rc::clone(self);
        compio::runtime::spawn(async move {
            let rx = rx.to_async();
            while let Ok(key) = rx.recv().await {
                registry.finish_stream(&key);
            }
        })
        .detach();
    }

    async fn resolve_uri(&self, uri: &str) -> Result<Rc<Stream>, ResolveError> {
        let key = parse_stream_key(uri).ok_or(ResolveError::BAD_REQUEST)?;

        if let Some(runtime) = self.streams.borrow().get(&key) {
            return Ok(Rc::clone(&runtime.stream));
        }

        let gate_key = format!(
            "stream:{}:{}",
            key.login,
            key.quality.as_deref().unwrap_or_default()
        );
        let _guard = self.gate.acquire(&gate_key).await;

        // Another DESCRIBE may have created it while this one waited.
        if let Some(runtime) = self.streams.borrow().get(&key) {
            return Ok(Rc::clone(&runtime.stream));
        }

        let channel = self.channel(&key.login).await?;
        let Some(variant) = choose_variant(&channel.variants, key.quality.as_deref()) else {
            debug!(
                login = %key.login,
                quality = key.quality.as_deref().unwrap_or("<first>"),
                "daemon: no such variant"
            );
            return Err(ResolveError::NOT_FOUND);
        };

        self.ensure_capacity()?;
        // `spawn_stream` is synchronous, so no other task can take the last
        // slot between the capacity check and the insert.
        let runtime = self.spawn_stream(&key, &channel, variant);
        self.streams
            .borrow_mut()
            .insert(key.clone(), Rc::clone(&runtime));
        // The stream entry exists before its recorder can finish, so a fast
        // (or fatal) completion cannot be missed by the cleanup watcher.
        if let Some(disk) = runtime.disk.borrow().clone() {
            let finished = self.finished.clone();
            let stream_key = key.clone();
            compio::runtime::spawn(async move {
                if let Some(error) = disk.wait().await {
                    error!(%error, "daemon: fatal disk output error");
                    std::process::exit(1);
                }
                let _ = finished.send(stream_key);
            })
            .detach();
        }
        Ok(Rc::clone(&runtime.stream))
    }

    /// Get or build the channel entry. One GQL fetch per login, shared by every
    /// quality requested while the channel has streams.
    async fn channel(&self, login: &str) -> Result<Rc<Channel>, ResolveError> {
        if let Some(channel) = self.channels.borrow().get(login) {
            return Ok(Rc::clone(channel));
        }
        let _guard = self.gate.acquire(&format!("channel:{login}")).await;
        if let Some(channel) = self.channels.borrow().get(login) {
            return Ok(Rc::clone(channel));
        }

        let channel = if self.options.debug {
            Rc::new(Channel {
                title: DEBUG_CHANNEL_TITLE.to_owned(),
                variants: vec![debug_variant()],
                broadcast_id: 0,
            })
        } else {
            let resolved = get_variants(
                &self.client,
                self.options.client_id.as_deref(),
                self.options.auth_token.as_deref(),
                login,
                self.options.low_latency,
            )
            .await;
            match resolved {
                Ok((variants, broadcast_id)) => Rc::new(Channel {
                    title: login.to_owned(),
                    variants,
                    broadcast_id,
                }),
                Err(error) => {
                    // Offline is a 404; anything else (GQL, TLS, usher) may be
                    // transient, so tell the client to try again.
                    let status = match &error {
                        VariantPlaylistError::NoVariants(_) => ResolveError::NOT_FOUND,
                        _ => ResolveError::UNAVAILABLE,
                    };
                    warn!(%error, login, status = status.status, "daemon: channel resolution failed");
                    return Err(status);
                }
            }
        };

        self.channels
            .borrow_mut()
            .insert(login.to_owned(), Rc::clone(&channel));
        Ok(channel)
    }

    /// Drop idle streams, then refuse a new one when the limit is reached.
    ///
    /// A stream with no attached control connections is eligible for an
    /// immediate drop (a player that vanished without TEARDOWN is reclaimed
    /// here). A stream someone is watching, or that is being recorded, is never
    /// evicted.
    fn ensure_capacity(&self) -> Result<(), ResolveError> {
        let mut idle = Vec::new();
        self.streams.borrow_mut().retain(|key, runtime| {
            if runtime.stream.has_sessions() || runtime.is_recording() {
                return true;
            }
            idle.push((key.clone(), Rc::clone(runtime)));
            false
        });
        for (key, runtime) in idle {
            info!(
                login = %key.login,
                quality = key.quality.as_deref().unwrap_or("<first>"),
                "daemon: dropping idle stream"
            );
            runtime.stop();
            self.prune_channel(&key.login);
        }

        if self.streams.borrow().len() < self.options.max_streams {
            Ok(())
        } else {
            Err(ResolveError::UNAVAILABLE)
        }
    }

    fn spawn_stream(
        &self,
        key: &StreamKey,
        channel: &Channel,
        variant: &Variant,
    ) -> Rc<StreamRuntime> {
        let stream = Rc::new(Stream::new(key.clone(), channel.title.clone()));

        let dvr = Dvr::new(self.options.debug.then(|| DEBUG_DVR_URL.to_owned()));
        dvr.set_live_variant(&variant.id);
        crate::rtsp::dvr::spawn(
            Rc::clone(&self.client),
            (!self.options.debug).then(|| key.login.clone()),
            Rc::clone(&dvr),
        );

        let wakes = WakeSet::new();
        let live_window = Rc::new(RefCell::new(Vec::new()));
        let (registration, receive) = kanal::unbounded();
        let reader_wake = ConsumerWake::new();

        let disk = self.options.output.as_ref().map(|output| {
            Rc::new(crate::disk::spawn_disk_output(
                DiskOptions {
                    output: output.clone(),
                    stream_id: channel.broadcast_id,
                    keep_runs: self.options.keep_runs,
                    notifier: self.options.notify.clone(),
                    dvr: Rc::clone(&dvr),
                    client: Rc::clone(&self.client),
                },
                &wakes,
                &self.downloaders,
                &registration,
            ))
        });

        crate::rtsp::spawn_media(
            Rc::clone(&stream),
            Rc::clone(&self.client),
            Rc::clone(&dvr),
            &wakes,
            &registration,
            Rc::clone(&live_window),
            &reader_wake,
        );

        let scheduler = StreamScheduler::new(
            Rc::clone(&self.client),
            Rc::clone(&self.arena),
            wakes,
            variant.url.clone(),
            self.downloaders.clone(),
            receive,
            Rc::clone(&reader_wake),
        )
        .with_live_window(live_window);
        compio::runtime::spawn(scheduler.main()).detach();

        let runtime = Rc::new(StreamRuntime {
            stream,
            dvr,
            registration: RefCell::new(Some(registration)),
            reader_wake,
            recording: Cell::new(disk.is_some()),
            disk: RefCell::new(disk),
        });

        info!(
            login = %key.login,
            quality = key.quality.as_deref().unwrap_or("<first>"),
            variant = %variant.id,
            "daemon: stream started"
        );

        runtime
    }

    /// The disk recorder for `key` finished. Release the stream when nobody is
    /// watching it any more.
    fn finish_stream(&self, key: &StreamKey) {
        let runtime = self.streams.borrow().get(key).cloned();
        let Some(runtime) = runtime else {
            return;
        };
        runtime.recording.set(false);
        runtime.disk.borrow_mut().take();
        if runtime.stream.has_sessions() {
            return;
        }
        let removed = self.streams.borrow_mut().remove(key);
        if let Some(runtime) = removed {
            info!(
                login = %key.login,
                quality = key.quality.as_deref().unwrap_or("<first>"),
                "daemon: recording finished, stream stopped"
            );
            runtime.stop();
            self.prune_channel(&key.login);
        }
    }

    /// The last viewer left. Drop the stream, and the channel entry with it
    /// when this was that channel's last stream: a reconnect then re-resolves
    /// the variant list instead of playing a stale one.
    fn release_stream(&self, key: &StreamKey) {
        let removed = {
            let mut streams = self.streams.borrow_mut();
            let Some(runtime) = streams.get(key) else {
                return;
            };
            // A recording keeps the stream entry alive without a viewer.
            if runtime.stream.has_sessions() || runtime.is_recording() {
                return;
            }
            streams.remove(key)
        };
        let Some(runtime) = removed else {
            return;
        };
        info!(
            login = %key.login,
            quality = key.quality.as_deref().unwrap_or("<first>"),
            "daemon: stream stopped, last viewer left"
        );
        runtime.stop();
        self.prune_channel(&key.login);
    }

    fn prune_channel(&self, login: &str) {
        if self
            .streams
            .borrow()
            .keys()
            .any(|stream| stream.login == login)
        {
            return;
        }
        if self.channels.borrow_mut().remove(login).is_some() {
            debug!(login, "daemon: channel entry dropped");
        }
    }
}

impl StreamResolver for Registry {
    fn resolve<'a>(&'a self, uri: &'a str) -> LocalBoxFuture<'a, Result<Rc<Stream>, ResolveError>> {
        Box::pin(self.resolve_uri(uri))
    }

    fn release(&self, stream: &Rc<Stream>) {
        self.release_stream(stream.key());
    }
}

/// One async mutex per key, without three dependency crates.
///
/// Resolution is rare (one DESCRIBE per stream, one GQL round trip per channel),
/// so a keyed gate is enough: waiters are woken all at once and re-check, which
/// happens at most twice per resolution.
#[derive(Default)]
struct Gate {
    busy: RefCell<HashSet<String>>,
    waiters: RefCell<Vec<Waker>>,
}

struct GateGuard<'a> {
    gate: &'a Gate,
    key: String,
}

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        self.gate.busy.borrow_mut().remove(&self.key);
        for waker in self.gate.waiters.borrow_mut().drain(..) {
            waker.wake();
        }
    }
}

impl Gate {
    async fn acquire(&self, key: &str) -> GateGuard<'_> {
        loop {
            if self.busy.borrow_mut().insert(key.to_owned()) {
                return GateGuard {
                    gate: self,
                    key: key.to_owned(),
                };
            }
            poll_fn(|context| {
                // Re-check before parking so a release cannot be lost.
                if !self.busy.borrow().contains(key) {
                    return Poll::Ready(());
                }
                self.waiters.borrow_mut().push(context.waker().clone());
                Poll::Pending
            })
            .await;
        }
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::future::Future;
    use std::time::Duration;

    use futures_util::future::Either;
    use futures_util::future::select;
    use futures_util::pin_mut;

    use super::*;

    fn run<F: Future>(future: F) -> F::Output {
        compio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(future)
    }

    #[test]
    fn the_gate_serialises_the_same_key_only() {
        let gate = Gate::default();
        run(async {
            let first = gate.acquire("a").await;
            let other = gate.acquire("b").await;
            drop(other);

            // The same key must wait until the first guard is dropped.
            let wait = gate.acquire("a");
            let timeout = compio::time::sleep(Duration::from_millis(20));
            pin_mut!(wait, timeout);
            assert!(
                matches!(select(wait, timeout).await, Either::Right(_)),
                "a second resolution of the same key must wait"
            );

            drop(first);
            let second = gate.acquire("a").await;
            drop(second);
        });
    }

    #[test]
    fn quality_selects_the_variant_and_none_takes_the_first() {
        let variants = vec![
            debug_variant(),
            Variant {
                id: "audio_only".into(),
                ..debug_variant()
            },
        ];
        assert_eq!(
            choose_variant(&variants, Some("audio_only")).map(|variant| variant.id.as_ref()),
            Some("audio_only")
        );
        assert_eq!(
            choose_variant(&variants, None).map(|variant| variant.id.as_ref()),
            Some("1080p")
        );
        assert!(choose_variant(&variants, Some("720p60")).is_none());
    }
}
