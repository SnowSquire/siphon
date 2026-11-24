//! The QUIC carrier for the control plane, pinned by the daemon's key.
//!
//! The pipe is the local transport; this is the one for another machine. One
//! bidirectional stream per connection carries exactly the frames the pipe
//! carries, so the handshake, the snapshot and the intents do not know which
//! carrier they are on.
//!
//! There is no CA and no `WebPKI`. The daemon makes one self-signed certificate
//! on first use and the pin is the SHA-256 of its `SubjectPublicKeyInfo` — the
//! key, not the certificate wrapper, so re-issuing the certificate does not
//! break a client. The client refuses a key it was not given; a missing pin is
//! an error, never "connect anyway". Signature verification is still the real
//! TLS one, against the presented certificate.
//!
//! Pairing (a typed code, client certificates and `peers.json`) is the next
//! task. The seam is [`Identity`] — the one key the daemon presents — and
//! [`connect`]'s pin parameter: a future pairing flow replaces where the pin
//! comes from, not how it is checked.

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use compio::buf::BufResult;
use compio::buf::IoBuf;
use compio::buf::IoBufMut;
use compio::io::AsyncRead;
use compio::io::AsyncWrite;
use compio::net::ToSocketAddrsAsync;
use compio_quic::Endpoint;
use compio_quic::Incoming;
use compio_quic::RecvStream;
use compio_quic::SendStream;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::client::danger::ServerCertVerified;
use rustls::client::danger::ServerCertVerifier;
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::PrivateKeyDer;
use rustls::pki_types::PrivatePkcs8KeyDer;
use rustls::pki_types::ServerName;
use rustls::pki_types::UnixTime;
use thiserror::Error;
use tracing::info;

/// The protocol both sides speak, and the only one offered.
pub const ALPN: &[u8] = b"riffle/1";

/// The SNI the client sends. Nothing is named: the pin decides, and a name
/// would only be another thing an operator has to get right.
const SERVER_NAME: &str = "siphon";

/// The certificate on disk. DER, not PEM: nothing here needs to read a PEM
/// wrapper, and the pin is over bytes either way.
const CERT_FILE: &str = "quic-cert.der";

/// The private key on disk, PKCS#8 DER, 0600 on Unix.
const KEY_FILE: &str = "quic-key.der";

/// How long a handshake may take before the client gives up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A daemon's pinned identity: the certificate it presents and the key.
pub struct Identity {
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
    pin: String,
}

impl Identity {
    /// Loads the daemon's certificate and key, generating them on first use.
    ///
    /// The files are stable across restarts: a new key is a new pin, which
    /// every client would have to be told about. A corrupt or mismatched pair
    /// is therefore an error rather than a silent regeneration — delete both
    /// files to start over, deliberately.
    pub fn load_or_create(dir: &Path) -> Result<Self, Error> {
        let cert_path = dir.join(CERT_FILE);
        let key_path = dir.join(KEY_FILE);
        let existing = match (std::fs::read(&cert_path), std::fs::read(&key_path)) {
            (Ok(cert), Ok(key)) => Some((cert, key)),
            _ => None,
        };
        match existing {
            Some((cert, key)) => Self::from_der(cert.into(), key.into()),
            None => Self::generate(&cert_path, &key_path),
        }
    }

    fn from_der(
        cert: CertificateDer<'static>,
        key: PrivatePkcs8KeyDer<'static>,
    ) -> Result<Self, Error> {
        let pin = pin_of(&cert)?;
        Ok(Self { cert, key, pin })
    }

    fn generate(cert_path: &Path, key_path: &Path) -> Result<Self, Error> {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_owned()])
                .map_err(|error| Error::Identity(error.to_string()))?;
        let cert = cert.der().clone();
        let pin = pin_of(&cert)?;
        let key = PrivatePkcs8KeyDer::from(signing_key.serialize_der());
        write_private(key_path, key.secret_pkcs8_der())?;
        write_private(cert_path, cert.as_ref())?;
        info!(
            target: "rpc",
            path = %cert_path.display(),
            "quic: generated a self-signed identity"
        );
        Ok(Self { cert, key, pin })
    }

    /// The SPKI SHA-256 as lowercase hex: what a client pins.
    #[must_use]
    pub fn pin(&self) -> &str {
        &self.pin
    }
}

/// Parses a pin: exactly 32 bytes as 64 hex characters, with an optional
/// `sha256:` prefix (the spelling a UI may write next to the key).
fn parse_pin(pin: &str) -> Result<[u8; 32], Error> {
    let pin = pin.trim();
    if pin.is_empty() {
        return Err(Error::PinMissing);
    }
    let hex = pin.strip_prefix("sha256:").unwrap_or(pin);
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return Err(Error::PinMalformed(pin.to_owned()));
    }
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
        let pair = std::str::from_utf8(pair).map_err(|_| Error::PinMalformed(pin.to_owned()))?;
        *slot = u8::from_str_radix(pair, 16).map_err(|_| Error::PinMalformed(pin.to_owned()))?;
    }
    Ok(out)
}

/// The SPKI's SHA-256, from a certificate anybody presented.
fn pin_of(cert: &CertificateDer<'_>) -> Result<String, Error> {
    let parsed = rustls::server::ParsedCertificate::try_from(cert)?;
    Ok(lower_hex(&sha256(
        parsed.subject_public_key_info().as_ref(),
    )))
}

/// SHA-256 of the SPKI. The hash comes from the TLS provider already in use,
/// so there is no second digest implementation to keep right.
fn sha256(bytes: &[u8]) -> [u8; 32] {
    let suite = rustls::crypto::ring::cipher_suite::TLS13_AES_128_GCM_SHA256
        .tls13()
        .expect("the static suite is TLS 1.3");
    let digest = suite.common.hash_provider.hash(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

/// Lowercase hex, the spelling a pin is written in.
fn lower_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Writes one file with the owner-only permissions a key needs, through a
/// temporary name so a crash never leaves a half-written key.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("der.tmp");
    std::fs::write(&temporary, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&temporary, path)
}

/// The QUIC server configuration: TLS 1.3, our certificate, ALPN `riffle/1`,
/// no client certificate yet and no 0-RTT.
fn server_config(identity: &Identity) -> Result<compio_quic::ServerConfig, Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(Error::Tls)?
        .with_no_client_auth()
        .with_single_cert(
            vec![identity.cert.clone()],
            PrivateKeyDer::Pkcs8(identity.key.clone_key()),
        )
        .map_err(Error::Tls)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.max_early_data_size = 0;
    // No resumption and no 0-RTT, for the same reason the client refuses
    // them: a resumed handshake never presents the certificate.
    tls.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    tls.send_tls13_tickets = 0;
    let crypto = compio_quic::crypto::rustls::QuicServerConfig::try_from(tls)
        .map_err(|error| Error::Config(error.to_string()))?;
    Ok(compio_quic::ServerConfig::with_crypto(Arc::new(crypto)))
}

/// The verifier's record of why a handshake was refused.
///
/// rustls reports a certificate refusal as a TLS alert, not as the value the
/// verifier returned, so the typed answer is kept beside the handshake and
/// read when it fails. The lock is rustls' `Send + Sync` requirement, not a
/// shared reactor resource: everything here runs on the one compio thread.
#[derive(Debug, Default)]
struct Refusal(Mutex<Option<Mismatch>>);

#[derive(Debug)]
struct Mismatch {
    expected: String,
    presented: String,
}

impl Refusal {
    fn record(&self, expected: String, presented: String) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(Mismatch {
                expected,
                presented,
            });
        }
    }

    fn take(&self) -> Option<Mismatch> {
        self.0.lock().ok().and_then(|mut slot| slot.take())
    }
}

/// Accepts one certificate only if its SPKI hashes to the pin.
#[derive(Debug)]
struct Pinned {
    pin: [u8; 32],
    algorithms: WebPkiSupportedAlgorithms,
    refusal: Arc<Refusal>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let parsed = rustls::server::ParsedCertificate::try_from(end_entity)?;
        let presented = sha256(parsed.subject_public_key_info().as_ref());
        if presented != self.pin {
            let expected = lower_hex(&self.pin);
            let seen = lower_hex(&presented);
            self.refusal.record(expected.clone(), seen.clone());
            return Err(rustls::Error::General(format!(
                "siphon: the pinned key is {expected}, the daemon presented {seen}"
            )));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

fn client_config(pin: [u8; 32]) -> Result<(compio_quic::ClientConfig, Arc<Refusal>), Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let refusal = Arc::new(Refusal::default());
    let verifier = Arc::new(Pinned {
        pin,
        algorithms: provider.signature_verification_algorithms,
        refusal: Arc::clone(&refusal),
    });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(Error::Tls)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    // No resumption and no 0-RTT: both would skip the certificate on exactly
    // the reconnects the pin exists to check.
    tls.enable_early_data = false;
    tls.resumption = rustls::client::Resumption::disabled();
    let crypto = compio_quic::crypto::rustls::QuicClientConfig::try_from(tls)
        .map_err(|error| Error::Config(error.to_string()))?;
    Ok((compio_quic::ClientConfig::new(Arc::new(crypto)), refusal))
}

/// A server endpoint bound to one of `network.listen`.
pub struct Listener {
    endpoint: Endpoint,
}

impl Listener {
    /// Binds one address and is ready to accept.
    pub async fn bind(addr: impl ToSocketAddrsAsync, identity: &Identity) -> Result<Self, Error> {
        let config = server_config(identity)?;
        let endpoint = Endpoint::server(addr, config).await?;
        Ok(Self { endpoint })
    }

    /// The address the OS actually bound (the port is ephemeral in tests).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// The next handshake that reached us. `None` means the endpoint closed.
    pub async fn incoming(&self) -> Option<Incoming> {
        self.endpoint.wait_incoming().await
    }
}

/// Completes one incoming handshake and takes its control stream.
///
/// One bidirectional stream is the control plane. More streams and datagrams
/// belong to the media lane that does not exist yet, so they are ignored.
pub async fn establish(incoming: Incoming) -> Result<Stream, Error> {
    let connection = incoming
        .await
        .map_err(|error| Error::Connect(error.to_string()))?;
    let (send, recv) = connection
        .accept_bi()
        .await
        .map_err(|error| Error::Stream(error.to_string()))?;
    Ok(Stream {
        endpoint: None,
        send,
        recv,
    })
}

/// Opens the client side: resolve, handshake against the pin, one stream.
///
/// `pin` is the daemon's SPKI SHA-256 as 64 hex characters (either case, with
/// or without a `sha256:` prefix). It is a parameter here on purpose: where a
/// UI keeps it (`ui.json`'s `Connection::Remote`) is the pairing task's
/// business, and a missing pin is [`Error::PinMissing`] rather than a
/// connection.
pub async fn connect(addr: &str, pin: &str) -> Result<Stream, Error> {
    let pin = parse_pin(pin)?;
    let (config, refusal) = client_config(pin)?;
    let remote = resolve(addr).await?;
    let bind = if remote.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let endpoint = Endpoint::client(bind).await?;
    let connecting = endpoint
        .connect(remote, SERVER_NAME, Some(config))
        .map_err(|error| Error::Connect(error.to_string()))?;
    let connection = match compio::time::timeout(CONNECT_TIMEOUT, connecting).await {
        Ok(Ok(connection)) => connection,
        Ok(Err(error)) => {
            return Err(match refusal.take() {
                Some(Mismatch {
                    expected,
                    presented,
                }) => Error::PinMismatch {
                    expected,
                    presented,
                },
                None => Error::Connect(error.to_string()),
            });
        }
        Err(_) => return Err(Error::Timeout),
    };
    let (send, recv) = connection
        .open_bi()
        .map_err(|error| Error::Stream(error.to_string()))?;
    Ok(Stream {
        endpoint: Some(endpoint),
        send,
        recv,
    })
}

async fn resolve(addr: &str) -> Result<SocketAddr, Error> {
    addr.to_socket_addrs_async()
        .await?
        .next()
        .ok_or(Error::Unresolved)
}

/// One bidirectional QUIC stream carrying control frames.
///
/// The write and read halves are separate QUIC stream handles, which is what
/// lets the daemon arm a read and a write at the same time; [`Self::split`]
/// hands them out separately.
pub struct Stream {
    /// Client-side only: the endpoint owns the UDP socket, so it must outlive
    /// the connection. A server's [`Listener`] already does that.
    endpoint: Option<Endpoint>,
    send: SendStream,
    recv: RecvStream,
}

impl Stream {
    /// Consumes the stream into the two halves the daemon's loop drives.
    #[must_use]
    pub fn split(self) -> (Reader, Writer) {
        (
            Reader {
                _endpoint: self.endpoint,
                recv: self.recv,
            },
            Writer { send: self.send },
        )
    }
}

impl AsyncRead for Stream {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.recv.read(buf).await
    }
}

impl AsyncWrite for Stream {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        self.send.write(buf).await
    }

    async fn flush(&mut self) -> io::Result<()> {
        self.send.flush().await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        self.send.shutdown().await
    }
}

/// The read half. Its own handle, so it can be armed while a write is in
/// flight.
pub struct Reader {
    _endpoint: Option<Endpoint>,
    recv: RecvStream,
}

impl AsyncRead for Reader {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.recv.read(buf).await
    }
}

/// The write half.
pub struct Writer {
    send: SendStream,
}

impl AsyncWrite for Writer {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        self.send.write(buf).await
    }

    async fn flush(&mut self) -> io::Result<()> {
        self.send.flush().await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        self.send.shutdown().await
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("tls: {0}")]
    Tls(#[from] rustls::Error),
    #[error("quic: {0}")]
    Config(String),
    #[error("identity: {0}")]
    Identity(String),
    #[error("the daemon presented {presented}, but the pinned key is {expected}")]
    PinMismatch { expected: String, presented: String },
    #[error("a pin is required to connect to a remote daemon")]
    PinMissing,
    #[error("the pin must be 64 hex characters, got {0:?}")]
    PinMalformed(String),
    #[error("connect: {0}")]
    Connect(String),
    #[error("the handshake timed out")]
    Timeout,
    #[error("the address did not resolve")]
    Unresolved,
    #[error("stream: {0}")]
    Stream(String),
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::path::PathBuf;

    use futures_util::future::join;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("siphon-quic-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_identity_is_stable_and_the_key_is_owner_only() {
        let dir = scratch("identity");
        let first = Identity::load_or_create(&dir).expect("first identity");
        let second = Identity::load_or_create(&dir).expect("second identity");
        assert_eq!(first.pin(), second.pin());
        assert_eq!(first.pin().len(), 64);
        assert!(
            first
                .pin()
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let mode = std::fs::metadata(dir.join(KEY_FILE))
                .expect("key metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "the QUIC key must not be group/world readable");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_wrong_pin_is_refused_with_the_typed_error() {
        let dir = scratch("mismatch");
        let runtime = compio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let identity = Identity::load_or_create(&dir).expect("identity");
            let listener = Listener::bind("127.0.0.1:0", &identity)
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("local addr").to_string();
            let wrong = "00".repeat(32);
            // The server has to complete its side of the handshake, or the
            // client would time out before it ever sees the certificate.
            let accept = async {
                if let Some(incoming) = listener.incoming().await {
                    let _ = incoming.await;
                }
            };
            let (client, ()) = join(connect(&addr, &wrong), accept).await;
            match client {
                Ok(_) => panic!("a wrong pin must not produce a stream"),
                Err(Error::PinMismatch {
                    expected,
                    presented,
                }) => {
                    assert_eq!(expected, wrong);
                    assert_eq!(presented, identity.pin());
                }
                Err(other) => panic!("expected a pin mismatch, got {other}"),
            }
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_or_malformed_pin_never_reaches_the_network() {
        let runtime = compio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            match connect("127.0.0.1:1", "").await {
                Err(Error::PinMissing) => {}
                Ok(_) => panic!("an empty pin must not connect"),
                Err(other) => panic!("expected a missing pin, got {other}"),
            }
            match connect("127.0.0.1:1", "not-a-pin").await {
                Err(Error::PinMalformed(_)) => {}
                Ok(_) => panic!("a malformed pin must not connect"),
                Err(other) => panic!("expected a malformed pin, got {other}"),
            }
        });
    }

    #[test]
    fn a_pinned_connection_carries_a_frame() {
        use crate::riffle::codec;
        use crate::riffle::protocol::ClientMessage;
        use crate::riffle::protocol::MAGIC;
        use crate::riffle::protocol::PROTOCOL_VERSION;

        let dir = scratch("frame");
        let runtime = compio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let identity = Identity::load_or_create(&dir).expect("identity");
            // The `sha256:` spelling is what a UI may persist beside the key.
            let pin = format!("sha256:{}", identity.pin());
            let listener = Listener::bind("127.0.0.1:0", &identity)
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("local addr").to_string();
            let hello = ClientMessage::Hello {
                magic: MAGIC,
                protocol: PROTOCOL_VERSION,
                app_version: "test".to_owned(),
            };
            // A bidirectional stream is not announced to the peer until it
            // carries bytes, so the client writes its first frame before the
            // server is expected to have a stream to accept.
            let client_fut = async {
                let mut stream = connect(&addr, &pin).await.expect("client");
                codec::write(&mut stream, &hello)
                    .await
                    .expect("write hello");
                stream
            };
            let accept = async { establish(listener.incoming().await.expect("incoming")).await };
            let (mut client, server) = join(client_fut, accept).await;
            let mut server = server.expect("server");

            let mut reader = codec::FrameReader::new();
            let received: ClientMessage = reader.read(&mut server).await.expect("read hello");
            assert!(matches!(
                received,
                ClientMessage::Hello { magic, protocol, .. }
                    if magic == MAGIC && protocol == PROTOCOL_VERSION
            ));

            let welcome = crate::riffle::protocol::ServerMessage::Welcome {
                protocol: PROTOCOL_VERSION,
                server_version: "test".to_owned(),
            };
            codec::write(&mut server, &welcome)
                .await
                .expect("write welcome");
            let mut reader = codec::FrameReader::new();
            let received: crate::riffle::protocol::ServerMessage =
                reader.read(&mut client).await.expect("read welcome");
            assert!(matches!(
                received,
                crate::riffle::protocol::ServerMessage::Welcome { .. }
            ));
        });
        let _ = std::fs::remove_dir_all(dir);
    }
}
