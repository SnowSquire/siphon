//! The QUIC carrier for the control plane: mutually authenticated, pinned by
//! both keys and paired with a typed code.
//!
//! The pipe is the local transport; this is the one for another machine. One
//! bidirectional stream per connection carries exactly the frames the pipe
//! carries, so the handshake, the snapshot and the intents do not know which
//! carrier they are on.
//!
//! The same connection also carries QUIC DATAGRAM frames ([`Datagrams`]), the
//! media lane's transport: the stream is the control plane and must not be
//! behind a video access unit, and datagrams are unreliable and unordered by
//! design. DATAGRAM is a transport parameter (`max_datagram_frame_size`), so
//! both endpoints set it in their [`TransportConfig`]; a peer that never
//! advertises it gets [`DatagramError::Unsupported`] rather than a silent
//! drop.
//!
//! There is no CA and no `WebPKI`. Each side makes one self-signed certificate
//! on first use and the pin is the SHA-256 of its `SubjectPublicKeyInfo` — the
//! key, not the certificate wrapper, so re-issuing the certificate does not
//! break the other side. The client refuses a daemon key it was not given, and
//! the daemon accepts only client keys in [`Peers`] (`peers.json`). A missing
//! pin is an error, never "connect anyway"; signature verification is still the
//! real TLS one, against the presented certificate.
//!
//! A key that is in nobody's file cannot connect at all, so the daemon has one
//! exception: while a [`PairingWindow`] window is open it also accepts a client that
//! is not in the allowlist. That connection is *provisional*: it may send one
//! `PairConfirm` carrying the short authentication string both sides derive
//! from the handshake ([`Session::sas`]), and the daemon adds the key to
//! `peers.json` if the strings match and the window was approved. The window is
//! a file because `siphon-daemon pair` is a second process: it opens the
//! window, the daemon publishes the attempt's code there, and the operator's
//! `y/n` is written back for the daemon to read.
//!
//! The files are read and written off the handshake path. rustls calls the
//! verifier synchronously, on the reactor thread, so it consults a
//! [`TrustSnapshot`] in memory and nothing else; the daemon's loop re-reads
//! `peers.json` and `pairing.json` with `compio::fs` and publishes a new
//! snapshot when they change.

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use compio::buf::BufResult;
use compio::buf::IoBuf;
use compio::buf::IoBufMut;
use compio::buf::bytes::Bytes;
use compio::io::AsyncRead;
use compio::io::AsyncWrite;
use compio::net::ToSocketAddrsAsync;
use compio_quic::Connection;
use compio_quic::Endpoint;
use compio_quic::Incoming;
use compio_quic::RecvStream;
use compio_quic::SendDatagramError;
use compio_quic::SendStream;
use compio_quic::TransportConfig;
use rustls::DistinguishedName;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::client::danger::ServerCertVerified;
use rustls::client::danger::ServerCertVerifier;
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::PrivateKeyDer;
use rustls::pki_types::PrivatePkcs8KeyDer;
use rustls::pki_types::ServerName;
use rustls::pki_types::UnixTime;
use rustls::server::danger::ClientCertVerified;
use rustls::server::danger::ClientCertVerifier;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;
use tracing::info;
use tracing::warn;

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

/// The UI's own certificate and key. Separate names from the daemon's so a
/// build that keeps both directories in the same place cannot mix them up.
const CLIENT_CERT_FILE: &str = "quic-client-cert.der";
const CLIENT_KEY_FILE: &str = "quic-client-key.der";

/// The daemon's allowlist of client keys, beside its identity.
pub const PEERS_FILE: &str = "peers.json";

/// The pairing window, beside the identity: the one piece of pairing state
/// that has to cross a process boundary.
pub const PAIRING_FILE: &str = "pairing.json";

/// How long a pairing window stays open.
pub const PAIRING_WINDOW: Duration = Duration::from_secs(120);

/// The TLS exporter label and context both sides call with. Any fixed pair
/// works; changing either is a protocol break, because the two codes would no
/// longer be equal.
const SAS_LABEL: &[u8] = b"EXPORTER-siphon-riffle";
const SAS_CONTEXT: &[u8] = b"riffle/1 pairing";

/// Crockford's base32: no `I`, `L`, `O` or `U`, so a code read aloud or typed
/// from a terminal cannot become a different code.
const SAS_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// How long a handshake may take before the client gives up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// What both endpoints advertise and buffer for DATAGRAM, in bytes.
///
/// quinn derives the transport parameter `max_datagram_frame_size` from this
/// number, capped at `u16::MAX` (RFC 9221), so both sides advertise 65535 and
/// the budget a datagram actually has is the path MTU, which
/// [`Datagrams::max_size`] reports. This is a *buffer*, not one frame: quinn
/// drops the oldest unread datagrams once this much is queued, which is the
/// right failure for a lane that is already lossy.
const DATAGRAM_RECEIVE_BUFFER: usize = 256 * 1024;

/// The transport parameters both endpoints use.
///
/// DATAGRAM is negotiated here and not in the TLS config: it is a transport
/// parameter, and quinn advertises `max_datagram_frame_size` only when a
/// receive buffer is set. Setting it on both sides is what makes
/// [`Datagrams::max_size`] `Some` after the handshake.
fn transport_config() -> Arc<TransportConfig> {
    let mut transport = TransportConfig::default();
    transport.datagram_receive_buffer_size(Some(DATAGRAM_RECEIVE_BUFFER));
    Arc::new(transport)
}

/// A daemon's pinned identity: the certificate it presents and the key.
///
/// The same type is the client's identity; only the file names and the
/// certificate's common name differ, because the pin is the key and nothing
/// here reads a name.
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
        Self::load_or_create_in(dir, CERT_FILE, KEY_FILE, SERVER_NAME)
    }

    /// Loads the client's certificate and key, generating them on first use.
    ///
    /// This is the key a daemon remembers in `peers.json`, so it must live
    /// somewhere stable and private: the UI's own app data directory, with the
    /// same 0600/DPAPI treatment as the daemon's. Regenerating it is a fresh
    /// pairing on every daemon this client talks to.
    pub fn load_or_create_client(dir: &Path) -> Result<Self, Error> {
        Self::load_or_create_in(dir, CLIENT_CERT_FILE, CLIENT_KEY_FILE, "siphon-ui")
    }

    fn load_or_create_in(
        dir: &Path,
        cert_file: &str,
        key_file: &str,
        common_name: &str,
    ) -> Result<Self, Error> {
        let cert_path = dir.join(cert_file);
        let key_path = dir.join(key_file);
        let existing = match (std::fs::read(&cert_path), std::fs::read(&key_path)) {
            (Ok(cert), Ok(key)) => Some((cert, key)),
            _ => None,
        };
        match existing {
            Some((cert, key)) => Self::from_der(cert.into(), key.into()),
            None => Self::generate(&cert_path, &key_path, common_name),
        }
    }

    fn from_der(
        cert: CertificateDer<'static>,
        key: PrivatePkcs8KeyDer<'static>,
    ) -> Result<Self, Error> {
        let pin = pin_of(&cert)?;
        Ok(Self { cert, key, pin })
    }

    fn generate(cert_path: &Path, key_path: &Path, common_name: &str) -> Result<Self, Error> {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec![common_name.to_owned()])
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

    /// The SPKI SHA-256 as lowercase hex: what the other side pins.
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

/// Unix milliseconds, for the timestamps these files record.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// One client the daemon accepts: an entry in `peers.json`.
///
/// The field names are the file's own, in snake case, because an operator
/// editing this file by hand is the supported revocation path — and the
/// supported way of adding a key whose owner is not around to pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    /// Stable within the file: the first 16 hex characters of `spki`, so an
    /// operator never has to invent one and a rewrite keeps it.
    pub id: String,
    /// What the client called itself when it paired. Display only.
    pub name: String,
    /// The SPKI SHA-256 as 64 lowercase hex characters.
    pub spki: String,
    /// Unix milliseconds.
    pub added_ms: u64,
    /// Unix milliseconds, refreshed at most once a minute per peer.
    pub last_seen_ms: u64,
}

/// The on-disk shape. `peers` is required: a misspelt key must be a parse
/// error, not a daemon that silently trusts nobody.
#[derive(Debug, Default, Serialize, Deserialize)]
struct PeersFile {
    peers: Vec<Peer>,
}

/// The daemon's allowlist of client keys: `peers.json`.
///
/// Revocation is deleting an entry, and the daemon's refresh picks that up, so
/// it takes effect within a second and needs no restart. The other direction
/// (adding a key by hand) is supported for the same reason.
#[derive(Debug)]
pub struct Peers {
    path: PathBuf,
    peers: Vec<Peer>,
}

impl Peers {
    /// Reads the allowlist.
    ///
    /// A missing file is an empty allowlist, which is what a daemon that has
    /// never paired anybody has. A malformed one is an error: a silent empty
    /// list looks exactly like "everything was revoked" and would take an hour
    /// to diagnose, and a startup error names the file instead.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, Error> {
        let path = path.into();
        let file = crate::store::read::<PeersFile>(&path)?;
        Self::from_file(path, file)
    }

    /// [`Self::load`] with `compio::fs`: the way anything on the reactor reads
    /// this file.
    pub async fn load_async(path: impl Into<PathBuf>) -> Result<Self, Error> {
        let path = path.into();
        let file = crate::store::read_async::<PeersFile>(&path).await?;
        Self::from_file(path, file)
    }

    fn from_file(path: PathBuf, file: Option<PeersFile>) -> Result<Self, Error> {
        let Some(file) = file else {
            return Ok(Self {
                path,
                peers: Vec::new(),
            });
        };
        // Normalize every pin so a hand-written `sha256:` prefix — the spelling
        // `ui.json` stores — matches a presented one, and reject an entry that
        // could never match rather than carrying it around.
        let mut peers = Vec::with_capacity(file.peers.len());
        for mut peer in file.peers {
            peer.spki = lower_hex(&parse_pin(&peer.spki)?);
            peers.push(peer);
        }
        Ok(Self { path, peers })
    }

    /// The file this allowlist is read from and written to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every entry, in file order.
    #[must_use]
    pub fn entries(&self) -> &[Peer] {
        &self.peers
    }

    /// Whether one presented key is allowed to connect.
    #[must_use]
    pub fn allows(&self, spki: &str) -> bool {
        self.peers.iter().any(|peer| peer.spki == spki)
    }

    /// Adds a key, or refreshes the name of one that is already allowed.
    pub fn add(&mut self, name: &str, spki: &str) -> Result<(), Error> {
        self.insert(name, spki)?;
        Self::write(&self.path, &self.peers)
    }

    /// [`Self::add`] with `compio::fs`.
    pub async fn add_async(&mut self, name: &str, spki: &str) -> Result<(), Error> {
        self.insert(name, spki)?;
        Self::write_async(&self.path, &self.peers).await
    }

    /// Adds one entry to this in-memory list without writing it.
    fn insert(&mut self, name: &str, spki: &str) -> Result<(), Error> {
        let spki = lower_hex(&parse_pin(spki)?);
        let now = now_ms();
        if let Some(index) = self.peers.iter().position(|peer| peer.spki == spki) {
            let peer = &mut self.peers[index];
            name.clone_into(&mut peer.name);
            peer.last_seen_ms = now;
            return Ok(());
        }
        let id = spki.get(..16).unwrap_or(&spki).to_owned();
        self.peers.push(Peer {
            id,
            name: name.to_owned(),
            spki,
            added_ms: now,
            last_seen_ms: now,
        });
        Ok(())
    }

    /// Records that one peer connected, at most once a minute.
    ///
    /// The throttle is what keeps a reconnect loop from rewriting the file on
    /// every retry: the field is "roughly when it last connected", not a log.
    pub fn touch(&mut self, spki: &str) -> Result<(), Error> {
        if self.mark_seen(spki) {
            Self::write(&self.path, &self.peers)?;
        }
        Ok(())
    }

    /// [`Self::touch`] with `compio::fs`.
    pub async fn touch_async(&mut self, spki: &str) -> Result<(), Error> {
        if self.mark_seen(spki) {
            Self::write_async(&self.path, &self.peers).await?;
        }
        Ok(())
    }

    /// Whether `spki` was due a `last_seen_ms`, and stamps it when it was.
    fn mark_seen(&mut self, spki: &str) -> bool {
        const TOUCH_MS: u64 = 60_000;
        let now = now_ms();
        let Some(peer) = self.peers.iter_mut().find(|peer| peer.spki == spki) else {
            return false;
        };
        if now.saturating_sub(peer.last_seen_ms) < TOUCH_MS {
            return false;
        }
        peer.last_seen_ms = now;
        true
    }

    fn write(path: &Path, peers: &[Peer]) -> Result<(), Error> {
        crate::store::write(
            path,
            &PeersFile {
                peers: peers.to_vec(),
            },
        )?;
        Ok(())
    }

    async fn write_async(path: &Path, peers: &[Peer]) -> Result<(), Error> {
        crate::store::write_async(
            path,
            &PeersFile {
                peers: peers.to_vec(),
            },
        )
        .await?;
        Ok(())
    }
}

/// The one attempt a pairing window is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingAttempt {
    /// The code the daemon derived for this handshake, as `XXX-XXX`.
    pub sas: String,
    /// The client's key, waiting to be added.
    pub spki: String,
    pub at_ms: u64,
}

/// The operator's answer from `siphon-daemon pair`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingDecision {
    /// The attempt this answers. A decision for a different code is stale and
    /// is ignored, so an old `y` cannot admit a later attempt.
    pub sas: String,
    pub approve: bool,
}

/// A pairing window: open until `expires_ms`, for one attempt.
///
/// This is a file rather than daemon state because the two halves are two
/// processes. `siphon-daemon pair` creates it, the daemon's refresh reads it
/// into the verifier's snapshot so an unknown key is admitted while it is open,
/// the daemon writes the attempt's code into it, and `pair` writes the
/// operator's answer back. The window is the only thing that lets an unknown key
/// get as far as a provisional connection, so an expired window is the same as
/// no window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingWindow {
    /// Unix milliseconds. The window is open while this is in the future.
    pub expires_ms: u64,
    /// The attempt the daemon saw, once one has connected.
    pub attempt: Option<PairingAttempt>,
    /// The operator's answer, once `pair` has asked.
    pub decision: Option<PairingDecision>,
}

impl PairingWindow {
    /// Opens a fresh window, replacing any previous one.
    pub fn open(path: impl Into<PathBuf>, window: Duration) -> Result<Self, Error> {
        let path = path.into();
        let expires_ms = now_ms().saturating_add(u64::try_from(window.as_millis()).unwrap_or(0));
        let pairing = Self {
            expires_ms,
            attempt: None,
            decision: None,
        };
        pairing.save(&path)?;
        Ok(pairing)
    }

    /// Reads the window. A malformed or absent file is no window at all: this
    /// is ephemeral state, and failing closed is the only safe default.
    #[must_use]
    pub fn read(path: &Path) -> Option<Self> {
        crate::store::read(path).ok().flatten()
    }

    /// [`Self::read`] with `compio::fs`, for the reactor. Same rule: anything
    /// unreadable is no window.
    pub async fn read_async(path: &Path) -> Option<Self> {
        crate::store::read_async(path).await.ok().flatten()
    }

    /// Records one attempt's code, clearing any answer for an earlier one.
    ///
    /// "One attempt at a time" is the file's own shape: a second handshake
    /// during the same window replaces the attempt, and its code is what the
    /// daemon will compare against — so the earlier approval cannot be reused.
    pub fn record_attempt(path: &Path, sas: &str, spki: &str) -> Result<Self, Error> {
        let Some(mut pairing) = Self::read(path) else {
            return Err(Error::Pairing("no pairing window is open".to_owned()));
        };
        pairing.set_attempt(sas, spki);
        pairing.save(path)?;
        Ok(pairing)
    }

    /// [`Self::record_attempt`] with `compio::fs`.
    pub async fn record_attempt_async(path: &Path, sas: &str, spki: &str) -> Result<Self, Error> {
        let Some(mut pairing) = Self::read_async(path).await else {
            return Err(Error::Pairing("no pairing window is open".to_owned()));
        };
        pairing.set_attempt(sas, spki);
        pairing.save_async(path).await?;
        Ok(pairing)
    }

    fn set_attempt(&mut self, sas: &str, spki: &str) {
        self.attempt = Some(PairingAttempt {
            sas: sas.to_owned(),
            spki: spki.to_owned(),
            at_ms: now_ms(),
        });
        self.decision = None;
    }

    /// Records the operator's answer for one attempt.
    pub fn decide(path: &Path, sas: &str, approve: bool) -> Result<Self, Error> {
        let Some(mut pairing) = Self::read(path) else {
            return Err(Error::Pairing("no pairing window is open".to_owned()));
        };
        pairing.decision = Some(PairingDecision {
            sas: sas.to_owned(),
            approve,
        });
        pairing.save(path)?;
        Ok(pairing)
    }

    /// Ends the window now, keeping the record of what happened in it.
    pub fn close(path: &Path) -> Result<(), Error> {
        if let Some(mut pairing) = Self::read(path) {
            pairing.expires_ms = now_ms();
            pairing.save(path)?;
        }
        Ok(())
    }

    /// [`Self::close`] with `compio::fs`.
    pub async fn close_async(path: &Path) -> Result<(), Error> {
        if let Some(mut pairing) = Self::read_async(path).await {
            pairing.expires_ms = now_ms();
            pairing.save_async(path).await?;
        }
        Ok(())
    }

    /// Whether the window still admits an unknown key.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.expires_ms > now_ms()
    }

    /// How long the window has left; zero once it has expired.
    #[must_use]
    pub fn remaining(&self) -> Duration {
        Duration::from_millis(self.expires_ms.saturating_sub(now_ms()))
    }

    /// Whether the operator approved exactly this code.
    #[must_use]
    pub fn approved(&self, sas: &str) -> bool {
        self.decision
            .as_ref()
            .is_some_and(|decision| decision.approve && decision.sas == sas)
    }

    fn save(&self, path: &Path) -> Result<(), Error> {
        crate::store::write(path, self)?;
        Ok(())
    }

    async fn save_async(&self, path: &Path) -> Result<(), Error> {
        crate::store::write_async(path, self).await?;
        Ok(())
    }
}

/// Where one daemon's client trust lives **on disk**: its allowlist and its
/// pairing window.
///
/// Both processes (`daemon` and `pair`) build this from the same state
/// directory, which is the whole of how they share it. Startup, `pair` and the
/// daemon's refresh are the only readers; the QUIC verifier never sees these
/// paths, only the [`TrustSnapshot`] the daemon publishes from them.
#[derive(Debug, Clone)]
pub struct Trust {
    peers: PathBuf,
    pairing: PathBuf,
}

impl Trust {
    #[must_use]
    pub fn new(state_dir: impl AsRef<Path>) -> Self {
        let dir = state_dir.as_ref();
        Self {
            peers: dir.join(PEERS_FILE),
            pairing: dir.join(PAIRING_FILE),
        }
    }

    #[must_use]
    pub fn peers(&self) -> &Path {
        &self.peers
    }

    #[must_use]
    pub fn pairing(&self) -> &Path {
        &self.pairing
    }

    /// Both files as one snapshot, read synchronously.
    ///
    /// This is for startup and `siphon-daemon pair`, neither of which is on the
    /// reactor. A malformed `peers.json` is an error here, which is what makes
    /// it a startup error; the reactor uses [`Self::refresh`].
    pub fn snapshot(&self) -> Result<TrustSnapshot, Error> {
        let peers = Peers::load(&self.peers)?;
        Ok(TrustSnapshot::new(
            peers.entries(),
            PairingWindow::read(&self.pairing).as_ref(),
        ))
    }

    /// Re-reads both files with `compio::fs` and publishes the result into
    /// `shared`, returning whether it changed.
    ///
    /// # Why the verifier does not do this
    ///
    /// rustls calls `verify_client_cert` synchronously on the reactor thread,
    /// so reading a file there is exactly the blocking I/O the daemon must not
    /// do. The verifier therefore reads the snapshot and nothing else, and this
    /// is what keeps it honest: an operator's edit to `peers.json` — including
    /// deleting a device to revoke it — takes effect on the next refresh, once
    /// a second, instead of being re-read at every handshake or needing a
    /// restart.
    ///
    /// A malformed `peers.json` is an error and leaves the previous snapshot in
    /// place: somebody halfway through editing the file must not silently
    /// revoke every client. The startup check is where it is fatal.
    pub async fn refresh(&self, shared: &SharedTrust) -> Result<bool, Error> {
        let peers = Peers::load_async(&self.peers).await?;
        let window = PairingWindow::read_async(&self.pairing).await;
        Ok(shared.replace(TrustSnapshot::new(peers.entries(), window.as_ref())))
    }
}

/// The trust state as the verifier sees it: the allowed keys and whether a
/// pairing window is open, as of the last refresh.
///
/// A snapshot is comparable, so a refresh that parsed the same files into the
/// same value is a no-op rather than a wake-up for nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustSnapshot {
    /// Every allowed client key, normalized to 64 lowercase hex characters.
    peers: Vec<String>,
    /// When the pairing window closes, in Unix milliseconds, if one was open
    /// and unanswered at the last refresh.
    pairing_open_until_ms: Option<u64>,
}

impl TrustSnapshot {
    fn new(peers: &[Peer], window: Option<&PairingWindow>) -> Self {
        Self {
            peers: peers.iter().map(|peer| peer.spki.clone()).collect(),
            pairing_open_until_ms: window
                .filter(|window| window.is_open() && window.decision.is_none())
                .map(|window| window.expires_ms),
        }
    }

    /// Whether the allowlist names this key.
    #[must_use]
    pub fn allows(&self, spki: &str) -> bool {
        self.peers.iter().any(|peer| peer == spki)
    }

    /// When the pairing window closes, if it is still open.
    ///
    /// The snapshot cannot be re-read from inside the verifier, so expiry is
    /// decided here, against the clock, rather than being baked in at refresh
    /// time: a window that ran out between two refreshes is closed.
    #[must_use]
    pub fn pairing_open_until_ms(&self) -> Option<u64> {
        self.pairing_open_until_ms.filter(|until| *until > now_ms())
    }

    /// Whether an unknown key would be admitted for one pairing attempt.
    #[must_use]
    pub fn pairing_open(&self) -> bool {
        self.pairing_open_until_ms().is_some()
    }
}

/// The live [`TrustSnapshot`] the QUIC verifier consults.
///
/// This is the only way a handshake reaches trust state, and it is a lock plus
/// a comparison: no filesystem, no `.await`, nothing that could block the
/// reactor. The mutex is rustls' `Send + Sync` requirement, not a shared
/// reactor resource — the verifier and the refresh both run on the one compio
/// thread, the lock is uncontended, and it is never held across an `.await`.
#[derive(Debug, Clone, Default)]
pub struct SharedTrust(Arc<Mutex<TrustSnapshot>>);

impl SharedTrust {
    #[must_use]
    pub fn new(snapshot: TrustSnapshot) -> Self {
        Self(Arc::new(Mutex::new(snapshot)))
    }

    /// What the snapshot says about one presented key.
    fn judge(&self, spki: &str) -> Verdict {
        // A poisoned lock means something else panicked, and every panic is
        // fatal here, so there is no healthy state to recover to: fail closed.
        let Ok(snapshot) = self.0.lock() else {
            return Verdict::Refused;
        };
        if snapshot.allows(spki) {
            Verdict::Allowed
        } else if snapshot.pairing_open() {
            Verdict::Pairing
        } else {
            Verdict::Refused
        }
    }

    /// Publishes a new snapshot, returning whether it differs from the old one.
    fn replace(&self, snapshot: TrustSnapshot) -> bool {
        let Ok(mut slot) = self.0.lock() else {
            return false;
        };
        if *slot == snapshot {
            return false;
        }
        *slot = snapshot;
        true
    }
}

/// What one snapshot says about a presented key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// The key is in the allowlist.
    Allowed,
    /// Not in the allowlist, but a pairing window is open.
    Pairing,
    /// Neither: the handshake is refused.
    Refused,
}

/// The short authentication string for one handshake, as `XXX-XXX`.
///
/// # How it is derived, and what that detects
///
/// The seed is the TLS exporter (`export_keying_material`), which is bound to
/// the whole handshake transcript — both certificates, both key shares, both
/// Finished messages. The two pinned keys are mixed in as well, so the string
/// is a function of exactly the keys a person pins, and an active MITM cannot
/// make the two sides agree: it terminates TLS on each leg, so the daemon's
/// exporter seed and the client's differ, and the codes differ. A man in the
/// middle relays, but the comparison is what he cannot relay.
///
/// A derivation of the form `truncate(H(H(server_spki) || H(client_spki)))`
/// — the documented fallback if the carrier ever stopped exposing an exporter
/// — would be computed from the public keys alone. It would still differ
/// between an honest pair and a swapped key, but it would be *identical* on
/// both of a MITM's legs, so it detects a wrong key and never an active
/// attacker. The exporter is what makes the code proof rather than comparison.
fn sas(seed: &[u8], server_spki: &str, client_spki: &str) -> String {
    let mut material = Vec::with_capacity(seed.len() + server_spki.len() + client_spki.len());
    material.extend_from_slice(seed);
    material.extend_from_slice(server_spki.as_bytes());
    material.extend_from_slice(client_spki.as_bytes());
    let digest = sha256(&material);
    let packed = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    let mut out = String::with_capacity(7);
    for index in 0..6u32 {
        if index == 3 {
            out.push('-');
        }
        let shift = 25 - index * 5;
        let slot = usize::try_from((packed >> shift) & 0x1f).unwrap_or(0);
        out.push(char::from(SAS_ALPHABET[slot]));
    }
    out
}

/// The code for this connection, from whichever side is asking.
///
/// Both sides call the same exporter with the same label, so they get the same
/// seed; the pin order is fixed (daemon first) so the two arguments agree.
fn session_sas(connection: &Connection, local_pin: &str, peer_pin: &str) -> Result<String, Error> {
    let mut seed = [0u8; 32];
    connection
        .export_keying_material(&mut seed, SAS_LABEL, SAS_CONTEXT)
        .map_err(|_| Error::NoExporter)?;
    let (server_pin, client_pin) = if connection.side().is_server() {
        (local_pin, peer_pin)
    } else {
        (peer_pin, local_pin)
    };
    Ok(sas(&seed, server_pin, client_pin))
}

/// The peer's SPKI pin, from the certificate chain the handshake exchanged.
fn peer_pin(connection: &Connection) -> Result<String, Error> {
    let chain = connection.peer_identity().ok_or(Error::NoPeerCertificate)?;
    let cert = chain.first().ok_or(Error::NoPeerCertificate)?;
    pin_of(cert)
}

/// Accepts a client certificate only if the snapshot allows its SPKI, or while
/// the snapshot says a pairing window is open.
///
/// The window's exception is the only way a key ever gets into `peers.json`:
/// without it the daemon could never accept a client it does not already know,
/// so pairing would be impossible rather than merely closed.
///
/// rustls calls this synchronously, on the reactor thread, once per handshake,
/// so the verifier touches no files: it takes [`SharedTrust`]'s lock, compares
/// the presented pin against the snapshot and returns. The daemon's loop keeps
/// that snapshot current ([`Trust::refresh`]), which is what makes an edit to
/// `peers.json` — including a revocation — take effect within a second.
#[derive(Debug)]
struct Allowlisted {
    trust: SharedTrust,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for Allowlisted {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        // There is no CA to name: the pin is the key, not an issuer.
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let presented =
            pin_of(end_entity).map_err(|error| rustls::Error::General(error.to_string()))?;
        match self.trust.judge(&presented) {
            Verdict::Allowed => Ok(ClientCertVerified::assertion()),
            Verdict::Pairing => {
                // The listener decides whether this connection is provisional;
                // the verifier only lets it through so the typed code can be
                // exchanged.
                info!(target: "rpc", pin = %presented, "quic: unknown client admitted by the pairing window");
                Ok(ClientCertVerified::assertion())
            }
            Verdict::Refused => {
                warn!(target: "rpc", pin = %presented, "quic: refused a client that is not in peers.json");
                Err(rustls::Error::General(format!(
                    "siphon: {presented} is not a paired client"
                )))
            }
        }
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

/// The QUIC server configuration: TLS 1.3, our certificate, a client
/// certificate the snapshot must allow, ALPN `riffle/1`, no 0-RTT.
fn server_config(
    identity: &Identity,
    trust: SharedTrust,
) -> Result<compio_quic::ServerConfig, Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(Allowlisted {
        trust,
        algorithms: provider.signature_verification_algorithms,
    });
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(Error::Tls)?
        .with_client_cert_verifier(verifier)
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
    let mut config = compio_quic::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(transport_config());
    Ok(config)
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

fn client_config(
    pin: [u8; 32],
    identity: &Identity,
) -> Result<(compio_quic::ClientConfig, Arc<Refusal>), Error> {
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
        .with_client_auth_cert(
            vec![identity.cert.clone()],
            PrivateKeyDer::Pkcs8(identity.key.clone_key()),
        )
        .map_err(Error::Tls)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    // No resumption and no 0-RTT: both would skip the certificate on exactly
    // the reconnects the pin exists to check.
    tls.enable_early_data = false;
    tls.resumption = rustls::client::Resumption::disabled();
    let crypto = compio_quic::crypto::rustls::QuicClientConfig::try_from(tls)
        .map_err(|error| Error::Config(error.to_string()))?;
    let mut config = compio_quic::ClientConfig::new(Arc::new(crypto));
    config.transport_config(transport_config());
    Ok((config, refusal))
}

/// A server endpoint bound to one of `network.listen`.
pub struct Listener {
    endpoint: Endpoint,
}

impl Listener {
    /// Binds one address and is ready to accept.
    ///
    /// `trust` is the live snapshot, not the files: a window opened after the
    /// listener started admits one pairing attempt as soon as the daemon's next
    /// refresh publishes it, and no handshake ever waits on a disk read.
    pub async fn bind(
        addr: impl ToSocketAddrsAsync,
        identity: &Identity,
        trust: SharedTrust,
    ) -> Result<Self, Error> {
        let config = server_config(identity, trust)?;
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

/// Why one datagram was not queued.
///
/// Datagrams are unreliable, so `Ok` from [`Datagrams::send`] means "queued",
/// never "delivered"; these are the three ways the connection refuses one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DatagramError {
    /// The payload is over the budget [`Datagrams::max_size`] reports right now.
    #[error("the datagram is {len} bytes, this connection carries at most {max}")]
    TooLarge { len: usize, max: usize },
    /// DATAGRAM was not negotiated: the peer did not advertise
    /// `max_datagram_frame_size`, or local support is off. Never a silent drop.
    #[error("the connection does not carry datagrams")]
    Unsupported,
    /// The connection is gone, so nothing can be queued any more.
    #[error("the connection is closed")]
    Closed,
}

/// The datagram half of one connection: the media lane's carrier.
///
/// A cloneable handle over the connection the control stream rides on. quinn's
/// `send_datagram` and `recv_datagram` both take `&self` and own the connection
/// internally, so there is no `Rc<RefCell<..>>` here and no borrow to hold
/// across an `.await`: a clone of [`Connection`] is all the ownership the
/// channel needs. Sharing the connection is the point — media datagrams and
/// control frames are multiplexed by QUIC, so a full video access unit cannot
/// delay a `Request`.
#[derive(Debug, Clone)]
pub struct Datagrams {
    connection: Connection,
}

impl Datagrams {
    /// Queues one application datagram.
    ///
    /// `payload` is copied: quinn frees a datagram only once it has transmitted
    /// it, so it cannot borrow the caller's buffer. Use [`Self::max_size`] to
    /// size a fragment before this is called; the check here is the last word.
    pub fn send(&self, payload: &[u8]) -> Result<(), DatagramError> {
        let len = payload.len();
        self.connection
            .send_datagram(Bytes::copy_from_slice(payload))
            .map_err(|error| match error {
                SendDatagramError::TooLarge => DatagramError::TooLarge {
                    len,
                    max: self.connection.max_datagram_size().unwrap_or(0),
                },
                SendDatagramError::UnsupportedByPeer | SendDatagramError::Disabled => {
                    DatagramError::Unsupported
                }
                SendDatagramError::ConnectionLost(_) => DatagramError::Closed,
            })
    }

    /// The largest payload [`Self::send`] accepts right now, or `None` when the
    /// peer did not advertise DATAGRAM.
    ///
    /// The number is already the application payload budget: quinn subtracts
    /// the DATAGRAM frame header and this path's QUIC and IP overhead from the
    /// smaller of the peer's `max_datagram_frame_size` and the current path
    /// MTU. It moves with path-MTU discovery, so read it when a fragment is
    /// sized rather than caching it for the life of the connection.
    #[must_use]
    pub fn max_size(&self) -> Option<usize> {
        self.connection.max_datagram_size()
    }

    /// The next datagram, or `None` once the connection is closed.
    ///
    /// Nothing is invented on a timeout and nothing is reordered: a lost
    /// datagram simply never appears here.
    pub async fn recv(&self) -> Option<Vec<u8>> {
        self.connection
            .recv_datagram()
            .await
            .ok()
            .map(|bytes| bytes.to_vec())
    }
}

/// One established connection: the control stream plus what the handshake
/// proves about the peer.
///
/// The pin is the peer's key and the code is the pairing string for *this*
/// handshake. A normal connection only needs the pin (to look it up in the
/// allowlist); a provisional one sends the code back.
pub struct Session {
    datagrams: Datagrams,
    stream: Stream,
    pin: String,
    sas: String,
}

impl Session {
    /// The peer's SPKI SHA-256 as lowercase hex.
    #[must_use]
    pub fn pin(&self) -> &str {
        &self.pin
    }

    /// The code both sides derived. Compare it, then either send it back as a
    /// `PairConfirm` or ignore it.
    #[must_use]
    pub fn sas(&self) -> &str {
        &self.sas
    }

    /// The control stream, for a peer whose key is already trusted.
    ///
    /// This consumes the datagram handle; call [`Self::datagrams`] first if the
    /// same connection is also going to carry media.
    #[must_use]
    pub fn into_stream(self) -> Stream {
        self.stream
    }

    /// The datagram channel alone, for a caller that hands the stream to
    /// somebody else and keeps sending media.
    #[must_use]
    pub fn datagrams(&self) -> Datagrams {
        self.datagrams.clone()
    }

    /// Queues one media datagram on this connection.
    pub fn send_datagram(&self, payload: &[u8]) -> Result<(), DatagramError> {
        self.datagrams.send(payload)
    }

    /// The largest datagram payload this connection accepts, or `None` when the
    /// peer did not advertise DATAGRAM.
    #[must_use]
    pub fn max_datagram_size(&self) -> Option<usize> {
        self.datagrams.max_size()
    }

    /// The next datagram from the peer, or `None` once the connection closed.
    ///
    /// Takes `&self`: the receive side is a handle into the connection, not
    /// state of this session's, so a read can be armed while the same session
    /// is still writing frames.
    pub async fn read_datagram(&self) -> Option<Vec<u8>> {
        self.datagrams.recv().await
    }
}

/// A completed handshake whose control stream has not been taken yet.
///
/// Split from [`Session`] because the daemon has to decide what the connection
/// may be — and, for an unknown key, publish the pairing code — *before* it
/// waits for the first frame. A client that is pairing sends nothing until the
/// operator has compared the code, so waiting for the stream first would
/// deadlock the one thing pairing needs.
pub struct Handshake {
    connection: Connection,
    pin: String,
    sas: String,
}

impl Handshake {
    /// The peer's SPKI SHA-256 as lowercase hex.
    #[must_use]
    pub fn pin(&self) -> &str {
        &self.pin
    }

    /// The code both sides derived for this handshake.
    #[must_use]
    pub fn sas(&self) -> &str {
        &self.sas
    }

    /// Waits for the control stream and takes both halves of it.
    ///
    /// quinn does not announce a bidirectional stream until it carries bytes,
    /// so this completes when the peer writes its first frame (`Hello`, or
    /// `PairConfirm` on a provisional connection).
    pub async fn stream(self) -> Result<Session, Error> {
        let (send, recv) = self
            .connection
            .accept_bi()
            .await
            .map_err(|error| Error::Stream(error.to_string()))?;
        Ok(Session {
            datagrams: Datagrams {
                connection: self.connection,
            },
            stream: Stream {
                endpoint: None,
                send,
                recv,
            },
            pin: self.pin,
            sas: self.sas,
        })
    }
}

/// Completes one incoming handshake: certificate, exporter, both pins.
///
/// One bidirectional stream is the control plane and the connection's datagram
/// channel is the media lane's; the [`Connection`] is kept in the [`Session`]
/// so both are reachable from one handle.
pub async fn accept(incoming: Incoming, identity: &Identity) -> Result<Handshake, Error> {
    let connection = incoming
        .await
        .map_err(|error| Error::Connect(error.to_string()))?;
    let pin = peer_pin(&connection)?;
    let sas = session_sas(&connection, identity.pin(), &pin)?;
    Ok(Handshake {
        connection,
        pin,
        sas,
    })
}

/// Completes one incoming handshake and takes its control stream.
///
/// The short form of [`accept`] followed by [`Handshake::stream`], for a caller
/// that admits every connection the handshake allowed.
pub async fn establish(incoming: Incoming, identity: &Identity) -> Result<Session, Error> {
    accept(incoming, identity).await?.stream().await
}

/// Opens the client side: resolve, handshake against the pin, one stream.
///
/// The returned [`Session`] carries both halves of the connection: the control
/// stream and the datagram channel.
///
/// `pin` is the daemon's SPKI SHA-256 as 64 hex characters (either case, with
/// or without a `sha256:` prefix). It is a parameter here on purpose: where a
/// UI keeps it (`ui.json`'s `Connection::Remote`) is the pairing screen's
/// business, and a missing pin is [`Error::PinMissing`] rather than a
/// connection. `identity` is this client's key, which the daemon either has in
/// `peers.json` or is about to add.
pub async fn connect(addr: &str, pin: &str, identity: &Identity) -> Result<Session, Error> {
    let pin = parse_pin(pin)?;
    let (config, refusal) = client_config(pin, identity)?;
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
    let peer = peer_pin(&connection)?;
    let sas = session_sas(&connection, identity.pin(), &peer)?;
    let (send, recv) = connection
        .open_bi()
        .map_err(|error| Error::Stream(error.to_string()))?;
    Ok(Session {
        datagrams: Datagrams { connection },
        stream: Stream {
            endpoint: Some(endpoint),
            send,
            recv,
        },
        pin: peer,
        sas,
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
    #[error("store: {0}")]
    Store(#[from] crate::store::StoreError),
    #[error("quic: {0}")]
    Config(String),
    #[error("identity: {0}")]
    Identity(String),
    #[error("pairing: {0}")]
    Pairing(String),
    #[error("the daemon presented {presented}, but the pinned key is {expected}")]
    PinMismatch { expected: String, presented: String },
    #[error("a pin is required to connect to a remote daemon")]
    PinMissing,
    #[error("the pin must be 64 hex characters, got {0:?}")]
    PinMalformed(String),
    #[error("the peer presented no certificate")]
    NoPeerCertificate,
    #[error("the connection exposed no TLS exporter")]
    NoExporter,
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

    /// An allowlist with one key in it, at the path a daemon would read, and
    /// the snapshot a listener would consult.
    fn allow(dir: &Path, pin: &str) -> SharedTrust {
        let trust = Trust::new(dir);
        let mut peers = Peers::load(trust.peers()).expect("empty allowlist");
        peers.add("test client", pin).expect("add the client");
        SharedTrust::new(trust.snapshot().expect("snapshot"))
    }

    /// A verifier reading one snapshot, so a test can ask it directly instead
    /// of going through a handshake.
    fn verifier(trust: SharedTrust) -> Allowlisted {
        Allowlisted {
            trust,
            algorithms: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        }
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

    /// The UI's identity is the one a daemon remembers, so it is stable too and
    /// its key has the same permissions as the daemon's.
    #[test]
    fn the_client_identity_is_stable_and_owner_only() {
        let dir = scratch("client-identity");
        let first = Identity::load_or_create_client(&dir).expect("first client identity");
        let second = Identity::load_or_create_client(&dir).expect("second client identity");
        assert_eq!(first.pin(), second.pin());
        assert_ne!(
            first.pin(),
            Identity::load_or_create(&dir)
                .expect("daemon identity")
                .pin(),
            "the daemon and the client must not share a key"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let mode = std::fs::metadata(dir.join(CLIENT_KEY_FILE))
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
            let client_identity = Identity::load_or_create_client(&dir.join("ui")).expect("client");
            // The client's key is allowed, so the refusal can only be the pin:
            // otherwise the server's client-certificate check would fail first
            // and the client would see a plain handshake error.
            let trust = allow(&dir, client_identity.pin());
            let listener = Listener::bind("127.0.0.1:0", &identity, trust)
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("local addr").to_string();
            let wrong = "00".repeat(32);
            // The server has to complete its side of the handshake, or the
            // client would time out before it ever sees the certificate.
            let accept = async {
                if let Some(incoming) = listener.incoming().await {
                    let _ = establish(incoming, &identity).await;
                }
            };
            let (client, ()) = join(connect(&addr, &wrong, &client_identity), accept).await;
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
        let dir = scratch("no-pin");
        let identity = Identity::load_or_create_client(&dir).expect("client identity");
        let runtime = compio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            match connect("127.0.0.1:1", "", &identity).await {
                Err(Error::PinMissing) => {}
                Ok(_) => panic!("an empty pin must not connect"),
                Err(other) => panic!("expected a missing pin, got {other}"),
            }
            match connect("127.0.0.1:1", "not-a-pin", &identity).await {
                Err(Error::PinMalformed(_)) => {}
                Ok(_) => panic!("a malformed pin must not connect"),
                Err(other) => panic!("expected a malformed pin, got {other}"),
            }
        });
        let _ = std::fs::remove_dir_all(dir);
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
            let client_identity = Identity::load_or_create_client(&dir.join("ui")).expect("client");
            let trust = allow(&dir, client_identity.pin());
            // The `sha256:` spelling is what a UI may persist beside the key.
            let pin = format!("sha256:{}", identity.pin());
            let listener = Listener::bind("127.0.0.1:0", &identity, trust)
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
                let mut session = connect(&addr, &pin, &client_identity)
                    .await
                    .expect("client");
                codec::write(&mut session.stream, &hello)
                    .await
                    .expect("write hello");
                session
            };
            let accept =
                async { establish(listener.incoming().await.expect("incoming"), &identity).await };
            let (mut client, server) = join(client_fut, accept).await;
            let mut server = server.expect("server");
            // Both sides of one handshake derive the same code, which is the
            // whole pairing proof.
            assert_eq!(client.sas(), server.sas());
            assert_eq!(server.pin(), client_identity.pin());
            assert_eq!(client.pin(), identity.pin());

            let mut reader = codec::FrameReader::new();
            let received: ClientMessage =
                reader.read(&mut server.stream).await.expect("read hello");
            assert!(matches!(
                received,
                ClientMessage::Hello { magic, protocol, .. }
                    if magic == MAGIC && protocol == PROTOCOL_VERSION
            ));

            let welcome = crate::riffle::protocol::ServerMessage::Welcome {
                protocol: PROTOCOL_VERSION,
                server_version: "test".to_owned(),
            };
            codec::write(&mut server.stream, &welcome)
                .await
                .expect("write welcome");
            let mut reader = codec::FrameReader::new();
            let received: crate::riffle::protocol::ServerMessage =
                reader.read(&mut client.stream).await.expect("read welcome");
            assert!(matches!(
                received,
                crate::riffle::protocol::ServerMessage::Welcome { .. }
            ));
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    /// No allowlist entry and no window: the client certificate is refused
    /// during the handshake, so the daemon never gets a connection to serve.
    ///
    /// The assertion is on the server side because a QUIC client can finish its
    /// own half of the handshake before the server's verdict arrives; what is
    /// deterministic is that the server never completes it.
    #[test]
    fn an_unpaired_client_is_refused_by_the_handshake() {
        let dir = scratch("refused");
        let runtime = compio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let identity = Identity::load_or_create(&dir).expect("identity");
            let client_identity = Identity::load_or_create_client(&dir.join("ui")).expect("client");
            // No allowlist entry and no window: an empty snapshot, which is
            // exactly what a daemon that has never paired anybody has.
            let trust = SharedTrust::default();
            let listener = Listener::bind("127.0.0.1:0", &identity, trust)
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("local addr").to_string();
            let accept = async {
                match listener.incoming().await {
                    Some(incoming) => establish(incoming, &identity).await.map(|_| ()),
                    None => Err(Error::Connect("the endpoint closed".to_owned())),
                }
            };
            let (client, server) =
                join(connect(&addr, identity.pin(), &client_identity), accept).await;
            assert!(
                server.is_err(),
                "an unpaired key must not complete its handshake"
            );
            drop(client);
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The code is a function of the exporter seed and both keys, which is what
    /// makes two MITM legs disagree.
    #[test]
    fn the_code_is_deterministic_and_bound_to_both_keys() {
        let seed = [7u8; 32];
        let daemon = "aa".repeat(32);
        let client = "bb".repeat(32);
        let first = sas(&seed, &daemon, &client);
        assert_eq!(first, sas(&seed, &daemon, &client));
        assert_eq!(first.len(), 7);
        assert_eq!(first.as_bytes()[3], b'-');
        assert!(
            first
                .chars()
                .filter(|c| *c != '-')
                .all(|c| SAS_ALPHABET.contains(&u8::try_from(c).unwrap_or(0)))
        );

        // Either key, either order, and the seed itself: all four change it.
        assert_ne!(first, sas(&seed, &daemon, &"cc".repeat(32)));
        assert_ne!(first, sas(&seed, &"cc".repeat(32), &client));
        assert_ne!(first, sas(&seed, &client, &daemon));
        assert_ne!(first, sas(&[8u8; 32], &daemon, &client));
    }

    #[test]
    fn the_allowlist_round_trips_and_revokes_by_deleting_an_entry() {
        let dir = scratch("peers");
        let trust = Trust::new(&dir);
        assert_eq!(
            Peers::load(trust.peers())
                .expect("a missing file is an empty allowlist")
                .entries(),
            []
        );

        let key = "ab".repeat(32);
        Peers::load(trust.peers())
            .expect("empty")
            .add("laptop", &key)
            .expect("add");
        let peers = Peers::load(trust.peers()).expect("read back");
        assert!(peers.allows(&key));
        assert_eq!(peers.entries()[0].spki, key);
        assert_eq!(peers.entries()[0].id, key[..16]);
        assert!(peers.entries()[0].added_ms > 0);
        assert!(!peers.allows(&"cd".repeat(32)));
        // The file is the operator's, so it is the documented shape.
        let text = std::fs::read_to_string(trust.peers()).expect("read");
        assert!(text.contains("\"peers\""));
        assert!(text.contains("\"last_seen_ms\""));
        assert!(text.contains("\"added_ms\""));

        std::fs::write(trust.peers(), r#"{"peers":[]}"#).expect("revoke");
        assert!(!Peers::load(trust.peers()).expect("reload").allows(&key));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_malformed_allowlist_is_an_error_not_an_empty_list() {
        let dir = scratch("peers-broken");
        let trust = Trust::new(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        for broken in [
            r#"{"peer":[]}"#,
            r#"{"peers":[{"id":"a","name":"b","spki":"not-a-pin","added_ms":0,"last_seen_ms":0}]}"#,
            "not json",
        ] {
            std::fs::write(trust.peers(), broken).expect("write");
            assert!(
                Peers::load(trust.peers()).is_err(),
                "{broken} must be rejected"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_window_expires_and_only_approves_the_code_it_was_asked_about() {
        let dir = scratch("window");
        let trust = Trust::new(&dir);
        let window = PairingWindow::open(trust.pairing(), Duration::from_secs(1)).expect("open");
        assert!(window.is_open());

        PairingWindow::record_attempt(trust.pairing(), "ABC-123", &"ee".repeat(32))
            .expect("attempt");
        let attempt = PairingWindow::read(trust.pairing()).expect("read").attempt;
        assert_eq!(attempt.expect("attempt").sas, "ABC-123");

        PairingWindow::decide(trust.pairing(), "ABC-123", true).expect("decide");
        let pairing = PairingWindow::read(trust.pairing()).expect("read");
        assert!(pairing.approved("ABC-123"));
        assert!(
            !pairing.approved("XYZ-789"),
            "a different attempt is not approved"
        );

        // A second attempt during the same window clears the first answer, so
        // an old `y` cannot admit it.
        PairingWindow::record_attempt(trust.pairing(), "XYZ-789", &"ff".repeat(32))
            .expect("attempt");
        let pairing = PairingWindow::read(trust.pairing()).expect("read");
        assert!(!pairing.approved("XYZ-789"));
        assert!(!pairing.approved("ABC-123"));

        PairingWindow::close(trust.pairing()).expect("close");
        let pairing = PairingWindow::read(trust.pairing()).expect("read");
        assert!(!pairing.is_open());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The verifier answers from the snapshot and the snapshot follows the
    /// files: a refresh admits a key that was added and refuses one that was
    /// revoked, without a restart in between.
    #[test]
    fn a_refresh_changes_what_the_verifier_admits() {
        let dir = scratch("refresh");
        let runtime = compio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let trust = Trust::new(&dir);
            let client = Identity::load_or_create_client(&dir.join("ui")).expect("client");
            let shared = SharedTrust::default();
            let allowed = |shared: &SharedTrust| {
                verifier(shared.clone())
                    .verify_client_cert(&client.cert, &[], UnixTime::now())
                    .is_ok()
            };
            assert!(!allowed(&shared), "an empty snapshot admits nobody");

            Peers::load(trust.peers())
                .expect("empty allowlist")
                .add("laptop", client.pin())
                .expect("add the client");
            assert!(
                trust.refresh(&shared).await.expect("refresh"),
                "the files changed"
            );
            assert!(allowed(&shared), "the refresh admitted the new peer");

            std::fs::write(trust.peers(), r#"{"peers":[]}"#).expect("revoke");
            assert!(
                trust.refresh(&shared).await.expect("refresh"),
                "the files changed"
            );
            assert!(!allowed(&shared), "the refresh revoked the peer");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A malformed `peers.json` is a refresh error that keeps the last good
    /// snapshot: an operator halfway through an edit must not revoke everybody.
    #[test]
    fn a_malformed_allowlist_keeps_the_last_good_snapshot() {
        let dir = scratch("refresh-broken");
        let runtime = compio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let trust = Trust::new(&dir);
            let client = Identity::load_or_create_client(&dir.join("ui")).expect("client");
            Peers::load(trust.peers())
                .expect("empty allowlist")
                .add("laptop", client.pin())
                .expect("add the client");
            let shared = SharedTrust::default();
            assert!(trust.refresh(&shared).await.expect("refresh"));
            assert_eq!(shared.judge(client.pin()), Verdict::Allowed);

            std::fs::write(trust.peers(), "not json").expect("write a broken file");
            assert!(
                trust.refresh(&shared).await.is_err(),
                "a malformed allowlist is an error, not an empty one"
            );
            assert_eq!(
                shared.judge(client.pin()),
                Verdict::Allowed,
                "the previous snapshot survived the broken file"
            );
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The client half of one loopback connection, with a `Hello` written so the
    /// server's `accept_bi` completes; the server has to be driven alongside.
    async fn client_session(addr: &str, pin: &str, identity: &Identity) -> Session {
        use crate::riffle::codec;
        use crate::riffle::protocol::ClientMessage;
        use crate::riffle::protocol::MAGIC;
        use crate::riffle::protocol::PROTOCOL_VERSION;

        let mut session = connect(addr, pin, identity).await.expect("client");
        codec::write(
            &mut session.stream,
            &ClientMessage::Hello {
                magic: MAGIC,
                protocol: PROTOCOL_VERSION,
                app_version: "test".to_owned(),
            },
        )
        .await
        .expect("write hello");
        session
    }

    /// One loopback connection with both ends established.
    async fn pair(dir: &Path) -> (Session, Session) {
        let identity = Identity::load_or_create(dir).expect("identity");
        let client_identity = Identity::load_or_create_client(&dir.join("ui")).expect("client");
        let trust = allow(dir, client_identity.pin());
        let listener = Listener::bind("127.0.0.1:0", &identity, trust)
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr").to_string();
        let client = client_session(&addr, identity.pin(), &client_identity);
        let accept = async {
            establish(listener.incoming().await.expect("incoming"), &identity)
                .await
                .expect("server")
        };
        join(client, accept).await
    }

    /// A datagram leaves one end and arrives at the other byte for byte, in both
    /// directions, and the budget each side reports is a plausible one.
    #[test]
    fn datagrams_cross_the_loopback_in_both_directions() {
        let dir = scratch("datagram");
        let runtime = compio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let (client, server) = pair(&dir).await;
            // Both sides advertised DATAGRAM, so the budget is the path's
            // payload allowance: over a kilobyte on any usable MTU, under one
            // QUIC frame (RFC 9221 caps the transport parameter at u16).
            for budget in [client.max_datagram_size(), server.max_datagram_size()] {
                let budget = budget.expect("the peer advertised DATAGRAM");
                assert!(
                    (1024..=usize::from(u16::MAX)).contains(&budget),
                    "implausible datagram budget: {budget}"
                );
            }

            client
                .send_datagram(b"an access unit")
                .expect("client send");
            assert_eq!(
                server.read_datagram().await.as_deref(),
                Some(b"an access unit".as_slice())
            );
            server.send_datagram(b"and back").expect("server send");
            assert_eq!(
                client.read_datagram().await.as_deref(),
                Some(b"and back".as_slice())
            );
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    /// One byte over the advertised budget is a typed error rather than a panic,
    /// and the connection survives it.
    #[test]
    fn an_oversized_datagram_is_refused_and_the_connection_survives() {
        let dir = scratch("datagram-large");
        let runtime = compio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let (client, server) = pair(&dir).await;
            let max = client.max_datagram_size().expect("budget");
            let oversized = vec![0u8; max + 1];
            match client.send_datagram(&oversized) {
                Err(DatagramError::TooLarge { len, max: reported }) => {
                    assert_eq!(len, max + 1);
                    assert_eq!(reported, max);
                }
                other => panic!("expected TooLarge, got {other:?}"),
            }
            client
                .send_datagram(b"still here")
                .expect("a small datagram still goes");
            assert_eq!(
                server.read_datagram().await.as_deref(),
                Some(b"still here".as_slice())
            );
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A peer that does not advertise DATAGRAM is `Unsupported`, not a silent
    /// drop: the transport parameter, not the TLS config, is what decides.
    #[test]
    fn a_peer_without_datagram_support_is_unsupported() {
        let dir = scratch("datagram-unsupported");
        let runtime = compio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let identity = Identity::load_or_create(&dir).expect("identity");
            let client_identity = Identity::load_or_create_client(&dir.join("ui")).expect("client");
            let trust = allow(&dir, client_identity.pin());
            // The same server with DATAGRAM turned off: `None` advertises
            // nothing, so the client is never told it may send a datagram.
            let mut config = server_config(&identity, trust).expect("server config");
            let mut transport = TransportConfig::default();
            transport.datagram_receive_buffer_size(None);
            config.transport_config(Arc::new(transport));
            let endpoint = Endpoint::server("127.0.0.1:0", config).await.expect("bind");
            let addr = endpoint.local_addr().expect("local addr").to_string();
            let client = client_session(&addr, identity.pin(), &client_identity);
            let accept = async {
                establish(endpoint.wait_incoming().await.expect("incoming"), &identity)
                    .await
                    .expect("server")
            };
            let (client, _server) = join(client, accept).await;

            assert_eq!(
                client.max_datagram_size(),
                None,
                "the peer advertised nothing"
            );
            match client.send_datagram(b"anyone there") {
                Err(DatagramError::Unsupported) => {}
                other => panic!("expected Unsupported, got {other:?}"),
            }
        });
        let _ = std::fs::remove_dir_all(dir);
    }
}
