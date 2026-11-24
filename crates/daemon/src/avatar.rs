//! Channel avatars, cached once per picture and named by its content.
//!
//! The UI's image element reads a local file, not a URL, and the client may be
//! on another machine, so the picture cannot cross the RPC as a path. The
//! daemon downloads each avatar once into the app's data directory under the
//! hash of its bytes and publishes that id in the row; the client fetches the
//! bytes by id and caches them itself.
//!
//! Content naming is what makes that cheap on both sides: the same picture is
//! one file however many channels show it, and a repaint never depends on which
//! URL the older row came from. A URL marker beside the picture records which
//! id its profile URL produced, so a restart is a couple of metadata checks and
//! no network.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use crate::http::Client;

const AVATAR_DIR: &str = "avatars";

/// Suffix of the file recording which id a profile URL produced.
const URL_MARKER: &str = ".url";

/// Length of a content id: 128 bits of hash as lowercase hex.
const ID_LEN: usize = 32;

/// Delays before the retry attempts of one avatar whose first download failed.
///
/// A connect can lose a race with a slow DNS answer or a CDN edge — a 5s
/// connect timeout on one attempt reports `http: timed out: io`, and the same
/// URL answers in 40ms a moment later. Without this a channel keeps its initials
/// for the whole session, because the only other attempt is the next Hermes
/// reconnect.
const AVATAR_RETRIES: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(5)];

/// The content id of a picture, from its bytes.
///
/// xxh3-128, the hash the recorder already uses for the same job (noticing that
/// bytes changed), as lowercase hex. It is an identity within this cache, not a
/// signature: nothing trusts it enough for collision resistance to matter.
#[must_use]
pub fn id(bytes: &[u8]) -> String {
    format!("{:032x}", xxhash_rust::xxh3::xxh3_128(bytes))
}

/// The directory avatars are cached in.
fn dir() -> Option<PathBuf> {
    Some(
        dirs::data_local_dir()?
            .join(crate::config::APP_ID)
            .join(AVATAR_DIR),
    )
}

/// The file an avatar id is cached as, if `id` can be one.
///
/// The id in a request comes from a client and becomes a path, so only the hash
/// alphabet is accepted: no separator, no dot, no way out of the directory. The
/// URL markers live in the same directory and are rejected by the length alone.
#[must_use]
pub fn path_for(id: &str) -> Option<PathBuf> {
    if id.len() != ID_LEN
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    Some(dir()?.join(id))
}

/// The marker recording which id a profile URL produced.
fn url_marker(url: &str) -> Option<PathBuf> {
    let key = format!("{:032x}", xxhash_rust::xxh3::xxh3_128(url.as_bytes()));
    Some(dir()?.join(format!("{key}{URL_MARKER}")))
}

/// Reads a marker, but only when the picture it names is still there.
async fn read_marker(marker: &Path) -> Option<String> {
    let id = compio::fs::read(marker).await.ok()?;
    let id = std::str::from_utf8(&id).ok()?.trim();
    let path = path_for(id)?;
    compio::fs::metadata(&path).await.ok()?;
    Some(id.to_owned())
}

/// Writes the picture under its content id and records the URL, returning the id.
///
/// The file first and the marker second: a marker without its picture would
/// claim a picture for a URL that then has none. Both writes are idempotent, so
/// two channels sharing a picture may race without harm.
async fn store(marker: &Path, bytes: Vec<u8>) -> std::io::Result<String> {
    let id = id(&bytes);
    let path = path_for(&id)
        .ok_or_else(|| std::io::Error::other(format!("avatar id {id:?} is not a file name")))?;
    for file in [&path, marker] {
        if let Some(dir) = file.parent() {
            compio::fs::create_dir_all(dir).await?;
        }
    }
    if compio::fs::metadata(&path).await.is_err() {
        compio::fs::write(&path, bytes).await.0?;
    }
    compio::fs::write(marker, id.as_bytes().to_vec()).await.0?;
    Ok(id)
}

/// Returns the content id of a channel's picture, downloading it first when the
/// URL is not in the cache.
///
/// `None` is a failed or impossible download; the client then keeps the initials
/// its avatar draws behind the picture.
pub async fn ensure(client: &Client, url: &str) -> Option<String> {
    let marker = url_marker(url)?;
    if let Some(id) = read_marker(&marker).await {
        return Some(id);
    }
    for attempt in 0..=AVATAR_RETRIES.len() {
        if attempt > 0 {
            compio::time::sleep(AVATAR_RETRIES[attempt - 1]).await;
        }
        // The same cap the RPC serves under: caching a picture that cannot be
        // framed would only report a failure later.
        match crate::gql::fetch_bytes(client, url, siphon_shared::riffle::MAX_AVATAR).await {
            Ok(bytes) => match store(&marker, bytes).await {
                Ok(id) => {
                    if attempt > 0 {
                        tracing::info!(
                            target: "avatar",
                            "cached {url} on attempt {}",
                            attempt + 1
                        );
                    }
                    return Some(id);
                }
                Err(error) => {
                    tracing::info!(target: "avatar", "could not cache {url}: {error}");
                    return None;
                }
            },
            Err(error) => {
                if attempt == AVATAR_RETRIES.len() {
                    tracing::info!(target: "avatar", "could not cache {url}: {error}");
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The id in a request becomes a file name, so nothing but a hash may pass.
    #[test]
    fn only_a_hash_names_a_cache_file() {
        assert!(path_for(&"a".repeat(ID_LEN)).is_some());
        assert!(path_for("../avatar").is_none());
        assert!(path_for(&"A".repeat(ID_LEN)).is_none(), "hex is lowercase");
        assert!(path_for(&"g".repeat(ID_LEN)).is_none());
        assert!(path_for(&"a".repeat(ID_LEN - 1)).is_none());
        assert!(path_for(&format!("{}.url", "a".repeat(ID_LEN))).is_none());
    }

    /// What `id` produces has to be what `path_for` accepts, or a picture would
    /// be cached under a name it cannot be served from.
    #[test]
    fn an_id_is_the_bytes_hash_and_can_name_its_file() {
        let hash = id(b"a picture");
        assert_eq!(hash.len(), ID_LEN);
        assert!(path_for(&hash).is_some());
        assert_eq!(hash, id(b"a picture"));
        assert_ne!(hash, id(b"another picture"));
    }
}
