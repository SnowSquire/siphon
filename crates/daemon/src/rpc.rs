//! The admin RPC listener: one reader and one writer task per connection.
//!
//! A connection subscribes to the update bus, receives `Welcome` and one full
//! snapshot, then forwards requests to the app. The writer coalesces updates
//! by entity and disconnects a client that falls too far behind.

use std::collections::HashMap;
use std::io;

use compio::io::AsyncRead;
use compio::io::AsyncWrite;
use siphon_rpc::ClientMessage;
use siphon_rpc::FrameReader;
use siphon_rpc::MAGIC;
use siphon_rpc::PROTOCOL_VERSION;
use siphon_rpc::ServerMessage;
use siphon_rpc::Update;
use siphon_rpc::codec;
use tracing::info;
#[cfg(unix)]
use tracing::warn;

use crate::app::AppHandle;
use crate::bus::Broadcast;
use crate::bus::Outbound;

/// Serves the admin endpoint forever.
#[cfg(windows)]
pub async fn serve(endpoint: String, handle: AppHandle) -> io::Result<()> {
    use compio::fs::named_pipe::ServerOptions;

    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&endpoint)?;
    info!(%endpoint, "rpc: listening");
    loop {
        server.connect().await?;
        let connected = server;
        // Create the next instance before handing this one to a task, so a
        // client never finds the pipe briefly absent.
        server = ServerOptions::new().create(&endpoint)?;
        compio::runtime::spawn(connection(connected, handle.clone())).detach();
    }
}

/// Serves the admin endpoint forever.
#[cfg(unix)]
pub async fn serve(endpoint: String, handle: AppHandle) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    use compio::net::UnixListener;

    if siphon_rpc::probe(&endpoint) {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "admin socket is already in use",
        ));
    }
    let _ = std::fs::remove_file(&endpoint);
    let listener = UnixListener::bind(&endpoint).await?;
    std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600))?;
    info!(%endpoint, "rpc: listening");
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                compio::runtime::spawn(connection(stream, handle.clone())).detach();
            }
            Err(error) => warn!(%error, "rpc: accept failed"),
        }
    }
}

async fn connection<S: AsyncRead + AsyncWrite + Clone + Unpin + 'static>(
    stream: S,
    handle: AppHandle,
) {
    let (mut read, mut write) = (stream.clone(), stream);
    let mut reader = FrameReader::new();
    let hello: ClientMessage = match reader.read(&mut read).await {
        Ok(message) => message,
        Err(error) => {
            info!(%error, "rpc: handshake read failed");
            return;
        }
    };
    let ClientMessage::Hello {
        magic,
        protocol,
        app_version,
    } = hello
    else {
        return;
    };
    if magic != MAGIC || protocol != PROTOCOL_VERSION {
        let reason = format!("protocol {protocol} (backend speaks {PROTOCOL_VERSION})");
        let _ = codec::write(&mut write, &ServerMessage::Rejected(reason)).await;
        return;
    }
    info!(%app_version, "rpc: client connected");

    let bus = handle.bus();
    let (subscriber, outbound) = bus.subscribe();
    let Some(snapshot) = handle.snapshot().await else {
        return;
    };
    if codec::write(
        &mut write,
        &ServerMessage::Welcome {
            protocol: PROTOCOL_VERSION,
            server_version: env!("CARGO_PKG_VERSION").to_owned(),
        },
    )
    .await
    .is_err()
    {
        return;
    }
    if codec::write(&mut write, &ServerMessage::Snapshot(Box::new(snapshot)))
        .await
        .is_err()
    {
        return;
    }

    compio::runtime::spawn(writer(write, outbound)).detach();

    // The reader owns the last strong reference to the subscriber. Closing it
    // on exit stops the writer after it drains, even if a request is in flight.
    loop {
        match reader.read::<ClientMessage, _>(&mut read).await {
            Ok(ClientMessage::Request { id, request }) => {
                handle.request(id, request, &subscriber);
            }
            Ok(ClientMessage::Hello { .. }) => {}
            Err(error) => {
                info!(%error, "rpc: client disconnected");
                subscriber.close();
                return;
            }
        }
    }
}

/// Coalesces updates by entity so one slow UI frame carries the latest value
/// of each row instead of every intermediate step.
#[derive(Default)]
struct Coalescer {
    entries: Vec<(u64, Update)>,
    index: HashMap<Key, usize>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Key {
    Config,
    Conn,
    Pending,
    Channel(u64),
    Pipeline(String, Option<String>),
    Arena,
    Downloader,
    Notification(u64),
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
                pipeline.key.login.clone(),
                pipeline.key.quality.clone(),
            )),
            Update::PipelineRemove(key) => {
                Some(Self::Pipeline(key.login.clone(), key.quality.clone()))
            }
            Update::Arena(_) => Some(Self::Arena),
            Update::Downloader(_) => Some(Self::Downloader),
            Update::Notification(notification) => Some(Self::Notification(notification.id)),
            Update::NotificationDismissed(id) => Some(Self::Notification(*id)),
            Update::Error(_) => Some(Self::Error),
            Update::Update(_) => Some(Self::Update),
            // The segment ring and the event log are append-only; coalescing
            // would lose entries.
            Update::Segment { .. } | Update::Log(_) => None,
        }
    }
}

impl Coalescer {
    fn push(&mut self, broadcast: Broadcast) {
        if let Some(key) = Key::of(&broadcast.update) {
            if let Some(&position) = self.index.get(&key) {
                self.entries[position] = (broadcast.revision, broadcast.update);
                return;
            }
            self.index.insert(key, self.entries.len());
        }
        self.entries.push((broadcast.revision, broadcast.update));
    }

    fn finish(mut self) -> Vec<(u64, Update)> {
        self.entries.sort_by_key(|(revision, _)| *revision);
        self.entries
    }
}

async fn writer<W: AsyncWrite + Unpin>(mut write: W, outbound_rx: kanal::Receiver<Outbound>) {
    let outbound = outbound_rx.clone().to_async();
    loop {
        let Ok(first) = outbound.recv().await else {
            return;
        };
        // One queue and one receive future: draining synchronously cannot drop
        // a completed receive the way a select between two queues could.
        let mut controls = Vec::new();
        let mut coalescer = Coalescer::default();
        match first {
            Outbound::Control(message) => controls.push(message),
            Outbound::Update(broadcast) => coalescer.push(broadcast),
        }
        while let Ok(Some(item)) = outbound_rx.try_recv() {
            match item {
                Outbound::Control(message) => controls.push(message),
                Outbound::Update(broadcast) => coalescer.push(broadcast),
            }
        }
        for message in controls {
            if codec::write(&mut write, &message).await.is_err() {
                return;
            }
        }
        for (revision, update) in coalescer.finish() {
            if codec::write(&mut write, &ServerMessage::Update { revision, update })
                .await
                .is_err()
            {
                return;
            }
        }
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::Duration;

    use siphon_rpc::Channel;
    use siphon_rpc::Client;
    use siphon_rpc::ClientEvent;
    use siphon_rpc::Config;
    use siphon_rpc::Request;
    use siphon_rpc::Response;
    use siphon_rpc::Update;

    use super::*;
    use crate::app::App;
    use crate::app::AppInit;
    use crate::arena::ArenaSet;
    use crate::bus::Bus;
    use crate::http::Client as HttpClient;
    use crate::metrics::DownloaderMetrics;
    use crate::registry::Options as RegistryOptions;
    use crate::registry::Registry;

    fn scratch_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("siphon-rpc-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn next_config(events: &kanal::AsyncReceiver<ClientEvent>) {
        for _ in 0..50 {
            let event = compio::time::timeout(Duration::from_millis(200), events.recv())
                .await
                .ok()
                .and_then(Result::ok);
            if let Some(ClientEvent::Update { update, .. }) = event
                && matches!(*update, Update::Config(_))
            {
                return;
            }
        }
        panic!("config update did not arrive");
    }

    async fn next_pipeline(events: &kanal::AsyncReceiver<ClientEvent>, login: &str, removed: bool) {
        for _ in 0..50 {
            let event = compio::time::timeout(Duration::from_millis(200), events.recv())
                .await
                .ok()
                .and_then(Result::ok);
            let Some(event) = event else {
                continue;
            };
            let ClientEvent::Update { update, .. } = event else {
                continue;
            };
            match *update {
                Update::PipelineUpsert(view) if !removed && view.key.login == login => return,
                Update::PipelineRemove(key) if removed && key.login == login => return,
                _ => {}
            }
        }
        panic!("pipeline update for {login} did not arrive");
    }

    #[test]
    fn monitor_add_watch_and_stop_round_trip_over_rpc() {
        let dir = scratch_dir();
        let runtime = compio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let http = Rc::new(HttpClient::new());
            let (downloaders_tx, downloaders_rx) = kanal::bounded(32);
            let downloader_metrics = Rc::new(DownloaderMetrics::new(32));
            crate::downloader::spawn_downloaders::<1>(&http, &downloaders_rx, &downloader_metrics);

            let bus = Bus::new();
            let registry = Rc::new(Registry::new(
                Rc::clone(&http),
                Rc::new(ArenaSet::new()),
                downloaders_tx.to_async(),
                downloader_metrics,
                RegistryOptions {
                    client_id: None,
                    auth_token: None,
                    low_latency: true,
                    debug: true,
                    max_streams: 4,
                    output: Some(dir.clone()),
                    notify: None,
                    keep_runs: false,
                },
                Rc::clone(&bus),
            ));
            registry.watch_finished();

            let (events_tx, events_rx) = kanal::unbounded();
            let wake = crate::spool::ConsumerWake::new();
            let sender = crate::app::EventSender::new(events_tx, Rc::clone(&wake));
            let handle = AppHandle::new(sender.clone(), Rc::clone(&bus));
            let app = App::new(AppInit {
                config: Config {
                    channels: vec![Channel {
                        login: "sunnys".to_owned(),
                        id: 7,
                        display_name: Some("Sunnys".to_owned()),
                        record: false,
                        ..Channel::default()
                    }],
                    ..Config::default()
                },
                config_path: dir.join("config.json"),
                client: Rc::clone(&http),
                registry,
                bus,
                events: events_rx,
                sender,
                wake,
                client_id: None,
                debug: true,
            });
            compio::runtime::spawn(app.run()).detach();

            let endpoint = siphon_rpc::admin_endpoint(&format!(
                "test-{}-{}",
                std::process::id(),
                dir.file_name().unwrap().to_string_lossy()
            ));
            let serve_endpoint = endpoint.clone();
            compio::runtime::spawn(serve(serve_endpoint, handle)).detach();
            for _ in 0..50 {
                if siphon_rpc::probe(&endpoint) {
                    break;
                }
                compio::time::sleep(Duration::from_millis(20)).await;
            }

            let (client, snapshot) = Client::connect(&endpoint, "test").await.unwrap();
            assert_eq!(snapshot.channels.len(), 1);
            let events = client.events();
            // A second UI must receive the same entity updates.
            let (second, second_snapshot) = Client::connect(&endpoint, "test-2").await.unwrap();
            assert_eq!(second_snapshot.channels.len(), 1);
            let second_events = second.events();

            let response = client
                .request(Request::SetRecord {
                    channel: 7,
                    record: true,
                })
                .await
                .unwrap();
            assert!(matches!(response, Response::Ack));
            next_pipeline(&events, "sunnys", false).await;
            next_config(&second_events).await;

            let response = client
                .request(Request::GetDisk(siphon_rpc::StreamKey::new("sunnys", None)))
                .await
                .unwrap();
            assert!(matches!(response, Response::Disk(_)));

            let response = client
                .request(Request::PipelineStop {
                    key: siphon_rpc::StreamKey::new("sunnys", None),
                    force: true,
                })
                .await
                .unwrap();
            assert!(matches!(response, Response::Ack));
            next_pipeline(&events, "sunnys", true).await;

            // A one-shot recording starts without a persisted monitor and can
            // be stopped again with the same lifecycle.
            let key = siphon_rpc::StreamKey::new("sunnys", None);
            let response = client
                .request(Request::RecordOnce {
                    login: "sunnys".to_owned(),
                    quality: None,
                })
                .await
                .unwrap();
            assert!(matches!(response, Response::Ack));
            next_pipeline(&events, "sunnys", false).await;

            let response = client
                .request(Request::DisconnectViewers(key.clone()))
                .await
                .unwrap();
            assert!(matches!(response, Response::Ack));

            let response = client
                .request(Request::PipelineStop { key, force: false })
                .await
                .unwrap();
            assert!(matches!(response, Response::Ack));
            next_pipeline(&events, "sunnys", true).await;
        });
        let _ = std::fs::remove_dir_all(dir);
    }
}
