//! Client half of the Siphon RPC: Hello, one full snapshot, then updates.
//!
//! The UI keeps one [`Client`] per backend connection. A dropped connection
//! closes the update receiver; the UI reconnects with backoff and resyncs by
//! asking the new connection for a snapshot.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use compio::net::TcpStream;
use futures_util::FutureExt as _;
use kanal::AsyncReceiver;
use kanal::Sender;
use thiserror::Error;

use crate::codec::CodecError;
use crate::codec::FrameReader;
use crate::codec::write;
use crate::protocol::ClientMessage;
use crate::protocol::MAGIC;
use crate::protocol::PROTOCOL_VERSION;
use crate::protocol::Request;
use crate::protocol::Response;
use crate::protocol::RpcError;
use crate::protocol::ServerMessage;
use crate::protocol::Snapshot;
use crate::protocol::Update;

/// How long one request may wait for its response.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
pub enum ConnectError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error("backend speaks protocol {server}, this build speaks {ours}")]
    Protocol { server: u16, ours: u16 },
    #[error("backend rejected the hello: {0}")]
    Rejected(String),
    #[error("backend sent an unexpected first message")]
    Unexpected,
}

/// One asynchronous push from the backend.
#[derive(Debug)]
pub enum ClientEvent {
    Snapshot(Box<Snapshot>),
    Update { revision: u64, update: Update },
}

type Pending = Rc<RefCell<HashMap<u64, Sender<Result<Response, RpcError>>>>>;

/// Which half of the connection woke up.
enum Selected {
    Outgoing(Result<ClientMessage, kanal::ReceiveError>),
    Incoming(Result<ServerMessage, CodecError>),
    Stop,
}

/// A connected RPC client. Requests are correlated by id; updates arrive on
/// the channel returned by [`Self::events`].
pub struct Client {
    out: Sender<ClientMessage>,
    events: AsyncReceiver<ClientEvent>,
    pending: Pending,
    next_id: Cell<u64>,
}

impl Client {
    /// Connects, performs the Hello/Welcome exchange, and consumes the first
    /// snapshot. The remaining traffic is driven by the connection task.
    pub async fn connect(
        addr: SocketAddr,
        app_version: &str,
    ) -> Result<(Self, Snapshot), ConnectError> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let (mut read, mut write_half) = stream.into_split();

        write(
            &mut write_half,
            &ClientMessage::Hello {
                magic: MAGIC,
                protocol: PROTOCOL_VERSION,
                app_version: app_version.to_owned(),
            },
        )
        .await?;

        let mut reader = FrameReader::new();
        let welcome: ServerMessage = reader.read(&mut read).await?;
        let protocol = match welcome {
            ServerMessage::Welcome {
                protocol,
                server_version: _,
            } => protocol,
            ServerMessage::Rejected(reason) => return Err(ConnectError::Rejected(reason)),
            _ => return Err(ConnectError::Unexpected),
        };
        if protocol != PROTOCOL_VERSION {
            return Err(ConnectError::Protocol {
                server: protocol,
                ours: PROTOCOL_VERSION,
            });
        }

        let first: ServerMessage = reader.read(&mut read).await?;
        let ServerMessage::Snapshot(snapshot) = first else {
            return Err(ConnectError::Unexpected);
        };
        let snapshot = *snapshot;

        let (out_tx, out_rx) = kanal::unbounded();
        let (events_tx, events_rx) = kanal::unbounded();
        let pending: Pending = Rc::new(RefCell::new(HashMap::new()));
        let reader_pending = Rc::clone(&pending);
        let writer_pending = Rc::clone(&pending);

        // Read and write are separate tasks: a select between the two would
        // drop an already-completed receive when both sides woke together,
        // losing the frame it had just consumed. A shutdown signal stops the
        // other half instead.
        let (stop_tx, stop_rx) = kanal::bounded(1);
        let reader_stop = stop_rx.clone();
        let reader_stop_tx = stop_tx.clone();
        compio::runtime::spawn(async move {
            let stop = reader_stop.to_async();
            let events = events_tx;
            loop {
                let incoming = reader.read::<ServerMessage, _>(&mut read).fuse();
                let stopped = stop.recv().fuse();
                futures_util::pin_mut!(incoming, stopped);
                let selected = futures_util::select! {
                    message = incoming => Selected::Incoming(message),
                    _ = stopped => Selected::Stop,
                };
                match selected {
                    Selected::Incoming(Ok(ServerMessage::Update { revision, update })) => {
                        let _ = events.try_send(ClientEvent::Update { revision, update });
                    }
                    Selected::Incoming(Ok(ServerMessage::Snapshot(snapshot))) => {
                        let _ = events.try_send(ClientEvent::Snapshot(snapshot));
                    }
                    Selected::Incoming(Ok(ServerMessage::Response { id, result })) => {
                        eprintln!("client: response {id}");
                        if let Some(waiter) = reader_pending.borrow_mut().remove(&id) {
                            let _ = waiter.try_send(result);
                        }
                    }
                    // The handshake already consumed the only welcome; the
                    // writer variant cannot be produced by this select.
                    Selected::Incoming(Ok(
                        ServerMessage::Welcome { .. } | ServerMessage::Rejected(_),
                    ))
                    | Selected::Outgoing(_) => {}
                    Selected::Incoming(Err(_)) | Selected::Stop => break,
                }
            }
            for (_, waiter) in reader_pending.borrow_mut().drain() {
                let _ = waiter.try_send(Err(RpcError::Disconnected));
            }
            let _ = reader_stop_tx.try_send(());
        })
        .detach();

        let writer_stop = stop_rx;
        compio::runtime::spawn(async move {
            let out = out_rx.to_async();
            let stop = writer_stop.to_async();
            loop {
                let outgoing = out.recv().fuse();
                let stopped = stop.recv().fuse();
                futures_util::pin_mut!(outgoing, stopped);
                let selected = futures_util::select! {
                    message = outgoing => Selected::Outgoing(message),
                    _ = stopped => Selected::Stop,
                };
                match selected {
                    Selected::Outgoing(Ok(message)) => {
                        if write(&mut write_half, &message).await.is_err() {
                            break;
                        }
                    }
                    Selected::Outgoing(Err(_)) | Selected::Stop => break,
                    Selected::Incoming(_) => {}
                }
            }
            for (_, waiter) in writer_pending.borrow_mut().drain() {
                let _ = waiter.try_send(Err(RpcError::Disconnected));
            }
            let _ = stop_tx.try_send(());
        })
        .detach();

        Ok((
            Self {
                out: out_tx,
                events: events_rx.to_async(),
                pending,
                next_id: Cell::new(0),
            },
            snapshot,
        ))
    }

    /// The stream of `Snapshot`/`Update` events. Closes when the backend
    /// disconnects.
    pub fn events(&self) -> AsyncReceiver<ClientEvent> {
        self.events.clone()
    }

    /// Sends one request and waits for its typed result.
    pub async fn request(&self, request: Request) -> Result<Response, RpcError> {
        let id = self.next_id.get();
        eprintln!("client: request {id}");
        self.next_id.set(id.wrapping_add(1));
        let (waiter, receiver) = kanal::bounded(1);
        self.pending.borrow_mut().insert(id, waiter);
        if self
            .out
            .try_send(ClientMessage::Request { id, request })
            .is_err()
        {
            self.pending.borrow_mut().remove(&id);
            return Err(RpcError::Disconnected);
        }
        match compio::time::timeout(REQUEST_TIMEOUT, receiver.to_async().recv()).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RpcError::Disconnected),
            Err(_) => {
                self.pending.borrow_mut().remove(&id);
                Err(RpcError::Failed("request timed out".to_owned()))
            }
        }
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::protocol::ArenaSummary;
    use crate::protocol::Config;
    use crate::protocol::ConnHealth;
    use crate::protocol::DownloaderSummary;
    use crate::protocol::ServerMessage;
    use compio::net::TcpListener;
    use std::future::Future;

    fn snapshot() -> Snapshot {
        Snapshot {
            revision: 1,
            config: Config::default(),
            conn: ConnHealth::Connecting,
            channels: Vec::new(),
            pending: Vec::new(),
            pipelines: Vec::new(),
            arena: ArenaSummary::default(),
            downloader: DownloaderSummary::default(),
            notifications: Vec::new(),
            error: None,
            update: None,
        }
    }

    async fn serve(protocol: u16) -> (SocketAddr, impl Future<Output = ()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let serve = async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut read, mut sink) = stream.into_split();
            let mut reader = FrameReader::new();
            let hello: ClientMessage = reader.read(&mut read).await.unwrap();
            assert!(matches!(hello, ClientMessage::Hello { magic: MAGIC, .. }));
            write(
                &mut sink,
                &ServerMessage::Welcome {
                    protocol,
                    server_version: "test".to_owned(),
                },
            )
            .await
            .unwrap();
            write(&mut sink, &ServerMessage::Snapshot(Box::new(snapshot())))
                .await
                .unwrap();
            // A version-mismatched client hangs up after the welcome.
            let Ok(message) = reader.read::<ClientMessage, _>(&mut read).await else {
                return;
            };
            let ClientMessage::Request { id, request } = message else {
                panic!("expected a request");
            };
            assert!(matches!(request, Request::GetArena));
            write(
                &mut sink,
                &ServerMessage::Response {
                    id,
                    result: Ok(Response::Arena(ArenaSummary::default())),
                },
            )
            .await
            .unwrap();
            write(
                &mut sink,
                &ServerMessage::Update {
                    revision: 2,
                    update: Update::Error(Some("x".to_owned())),
                },
            )
            .await
            .unwrap();
        };
        (addr, serve)
    }

    #[test]
    fn connect_resyncs_and_routes_a_request_and_an_update() {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let (addr, server) = serve(PROTOCOL_VERSION).await;
            let client = async {
                let (client, snapshot) = Client::connect(addr, "test").await.unwrap();
                assert_eq!(snapshot.revision, 1);
                let events = client.events();
                let response = client.request(Request::GetArena).await.unwrap();
                assert!(matches!(response, Response::Arena(_)));
                match events.recv().await.unwrap() {
                    ClientEvent::Update { revision, update } => {
                        assert_eq!(revision, 2);
                        assert!(matches!(update, Update::Error(Some(reason)) if reason == "x"));
                    }
                    ClientEvent::Snapshot(_) => panic!("expected an update"),
                }
            };
            futures_util::join!(server, client);
        });
    }

    #[test]
    fn connect_refuses_a_protocol_mismatch() {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let (addr, server) = serve(PROTOCOL_VERSION + 1).await;
            let client = async {
                let result = Client::connect(addr, "test").await;
                assert!(matches!(
                    result,
                    Err(ConnectError::Protocol { server: 2, .. })
                ));
            };
            futures_util::join!(server, client);
        });
    }
}
