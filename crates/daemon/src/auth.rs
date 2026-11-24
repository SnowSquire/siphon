//! The Twitch token on disk: `auth.json` in the state directory.
//!
//! The value is the Twitch website's session bearer, sent as `OAuth <token>`.
//! It is long-lived and has no refresh path, so there is nothing to store but
//! the value itself; the login and user id that go with it are cached in
//! `config.json` (`account`) because they are not secret and a few queries
//! need the id.
//!
//! On Windows the value is wrapped with DPAPI, so a copy of the file taken to
//! another machine — a backup, a roaming profile, a synced folder — is inert.
//! `protection` records which wrapping was used, so an unreadable blob degrades
//! to "no token" instead of a decrypt error nobody can explain.

use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::config::APP_ID;

/// The file under the state directory.
pub const FILE: &str = "auth.json";

/// Current on-disk schema.
const VERSION: u64 = 1;

/// How the token is wrapped in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Protection {
    /// Base64 of a `CryptProtectData` blob.
    Dpapi,
    /// The token itself, in a file the platform keeps at 0600.
    None,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stored {
    version: u64,
    protection: Protection,
    token: String,
}

/// Where the token lives: per-machine state, never a roaming config directory.
#[must_use]
pub fn path() -> PathBuf {
    siphon_shared::riffle::state_dir(APP_ID).join(FILE)
}

/// Reads the token, or `None` when there is no usable one.
///
/// Missing, unreadable, unparseable, newer than this build, or a blob DPAPI
/// refuses are all the same answer: ask for a new token. The daemon still
/// starts and serves everything that does not need Twitch.
#[must_use]
pub fn load() -> Option<String> {
    let path = path();
    let stored: Stored = match siphon_shared::store::read(&path) {
        Ok(Some(stored)) => stored,
        Ok(None) => return None,
        Err(error) => {
            tracing::info!(target: "auth", "no token: {error}");
            return None;
        }
    };
    if stored.version > VERSION {
        tracing::info!(target: "auth", "ignoring token file version {}", stored.version);
        return None;
    }
    let token = match stored.protection {
        Protection::None => stored.token,
        Protection::Dpapi => decode(&stored.token)
            .and_then(|blob| unprotect(&blob))
            .and_then(|bytes| String::from_utf8(bytes).map_err(|_| StoreError::Undecodable))
            .inspect_err(|error| {
                tracing::info!(target: "auth", "the stored token cannot be read: {error}");
            })
            .ok()?,
    };
    (!token.trim().is_empty()).then_some(token)
}

/// Writes the token, wrapping it with DPAPI where that exists.
///
/// A failure is returned so the caller can say so; a daemon that cannot write
/// the token still runs with the one it was given.
pub fn save(token: &str) -> Result<(), StoreError> {
    let (protection, token) = match protect(token.as_bytes()) {
        Ok(blob) => (Protection::Dpapi, encode(&blob)),
        Err(error) => {
            tracing::info!(target: "auth", "storing the token unwrapped: {error}");
            (Protection::None, token.to_owned())
        }
    };
    let stored = Stored {
        version: VERSION,
        protection,
        token,
    };
    let path = path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(StoreError::Io)?;
    }
    siphon_shared::store::write(&path, &stored)?;
    #[cfg(unix)]
    restrict(&path).map_err(StoreError::Io)?;
    tracing::info!(target: "auth", "stored the token at {}", path.display());
    Ok(())
}

/// The token an ad-hoc run (`play`, `record`) should use: the flag wins and is
/// written to the store, so a later daemon start does not need it; otherwise
/// the file is the source.
///
/// Only those two subcommands have the flag. `daemon` never takes a token on
/// the command line — a startup entry would carry it — so it calls [`load`].
/// An unreadable or missing file is `None`, not an error: the daemon serves
/// everything that does not need Twitch, and the UI says the token is absent.
#[must_use]
pub fn resolve(flag: Option<String>) -> Option<String> {
    let Some(token) = flag else {
        return load();
    };
    let token = token.trim().to_owned();
    if token.is_empty() {
        return load();
    }
    if let Err(error) = save(&token) {
        tracing::info!(target: "auth", "could not persist the token: {error}");
    }
    Some(token)
}

/// Asks Twitch who the token belongs to, for the `account` cache in the config.
///
/// `GET /oauth2/validate` with `Authorization: OAuth <token>`; it answers with
/// the login and the user id. Failure is not fatal: the id is a convenience,
/// and the token itself is untouched.
pub async fn validate(
    client: &crate::http::Client,
    token: &str,
) -> Result<siphon_shared::riffle::Account, String> {
    use sonic_rs::JsonValueTrait as _;

    const MAX_BODY: usize = 8 * 1024;
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    let authorization = format!("OAuth {token}");
    let mut response = crate::http::timeout(
        TIMEOUT,
        client.get_with(
            crate::constants::public_endpoint(),
            &[("Authorization", authorization.as_str())],
        ),
    )
    .await
    .map_err(|_| "validate timed out".to_owned())?
    .map_err(|error| error.to_string())?;
    if !response.status.is_success() {
        return Err(format!("validate returned {}", response.status));
    }
    let body = crate::http::timeout(TIMEOUT, response.body.read_to_end(MAX_BODY))
        .await
        .map_err(|_| "validate body timed out".to_owned())?
        .map_err(|error| error.to_string())?;
    let value: sonic_rs::Value = sonic_rs::from_slice(&body).map_err(|error| error.to_string())?;
    let login = value["login"]
        .as_str()
        .ok_or("validate has no login")?
        .to_owned();
    let user_id = value["user_id"]
        .as_u64()
        .or_else(|| value["user_id"].as_str().and_then(|text| text.parse().ok()))
        .ok_or("validate has no user id")?;
    Ok(siphon_shared::riffle::Account { login, user_id })
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] siphon_shared::store::StoreError),
    #[error("the stored token cannot be decoded")]
    Undecodable,
}

/// Owner-only permissions on the platforms that have them.
///
/// Windows has no mode bits to narrow: the token is wrapped with DPAPI there
/// (`protect`) and the profile's ACLs are the outer layer. The function is
/// `cfg(unix)` rather than a no-op `Ok(())` so that callers are not handed a
/// result that cannot fail.
#[cfg(unix)]
fn restrict(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
fn protect(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB;
    use windows_sys::Win32::Security::Cryptography::CRYPTPROTECT_UI_FORBIDDEN;
    use windows_sys::Win32::Security::Cryptography::CryptProtectData;

    let mut input = bytes.to_vec();
    let mut blob_in = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(input.len()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "token is too long")
        })?,
        pbData: input.as_mut_ptr(),
    };
    let mut blob_out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: both blobs describe live buffers; the call reads the input and
    // returns a freshly allocated output, freed below.
    let ok = unsafe {
        CryptProtectData(
            &raw mut blob_in,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &raw mut blob_out,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: DPAPI filled the output with `cbData` readable bytes.
    let out =
        unsafe { std::slice::from_raw_parts(blob_out.pbData, blob_out.cbData as usize).to_vec() };
    // SAFETY: the pointer came from DPAPI's allocator, which uses LocalAlloc.
    unsafe { LocalFree(blob_out.pbData.cast()) };
    Ok(out)
}

#[cfg(not(windows))]
fn protect(_bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no DPAPI on this platform",
    ))
}

#[cfg(windows)]
fn unprotect(blob: &[u8]) -> Result<Vec<u8>, StoreError> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB;
    use windows_sys::Win32::Security::Cryptography::CRYPTPROTECT_UI_FORBIDDEN;
    use windows_sys::Win32::Security::Cryptography::CryptUnprotectData;

    let mut input = blob.to_vec();
    let mut blob_in = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(input.len()).map_err(|_| StoreError::Undecodable)?,
        pbData: input.as_mut_ptr(),
    };
    let mut blob_out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as in `protect`; the description out-parameter is not requested.
    let ok = unsafe {
        CryptUnprotectData(
            &raw mut blob_in,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &raw mut blob_out,
        )
    };
    if ok == 0 {
        return Err(StoreError::Io(std::io::Error::last_os_error()));
    }
    // SAFETY: DPAPI filled the output with `cbData` readable bytes.
    let out =
        unsafe { std::slice::from_raw_parts(blob_out.pbData, blob_out.cbData as usize).to_vec() };
    // SAFETY: the pointer came from DPAPI's allocator.
    unsafe { LocalFree(blob_out.pbData.cast()) };
    Ok(out)
}

#[cfg(not(windows))]
const fn unprotect(_blob: &[u8]) -> Result<Vec<u8>, StoreError> {
    Err(StoreError::Undecodable)
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        let packed = (u32::from(chunk[0]) << 16) | (u32::from(second) << 8) | u32::from(third);
        out.push(char::from(ALPHABET[((packed >> 18) & 63) as usize]));
        out.push(char::from(ALPHABET[((packed >> 12) & 63) as usize]));
        out.push(if chunk.len() > 1 {
            char::from(ALPHABET[((packed >> 6) & 63) as usize])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(ALPHABET[(packed & 63) as usize])
        } else {
            '='
        });
    }
    out
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "the shift leaves at most seven bits, so the cast is the low byte"
)]
fn decode(text: &str) -> Result<Vec<u8>, StoreError> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return Err(StoreError::Undecodable),
        };
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_every_length() {
        for length in 0..=32u8 {
            let bytes: Vec<u8> = (0..length).map(|value| value.wrapping_mul(7)).collect();
            let text = encode(&bytes);
            assert_eq!(decode(&text).expect("decode"), bytes, "length {length}");
        }
    }

    #[test]
    fn base64_rejects_a_foreign_byte() {
        assert!(decode("AAAA*").is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn unwrapped_round_trips_through_the_file_format() {
        let stored = Stored {
            version: VERSION,
            protection: Protection::None,
            token: "abc123".to_owned(),
        };
        let json = siphon_shared::store::to_json(&stored).expect("serialize");
        let read_back: Stored = sonic_rs::from_slice(json.as_bytes()).expect("parse");
        assert_eq!(read_back.token, "abc123");
        assert_eq!(read_back.protection, Protection::None);
    }
}
