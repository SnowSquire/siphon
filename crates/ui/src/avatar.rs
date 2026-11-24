//! The client's avatar cache: pictures fetched from the daemon by content id.
//!
//! ZGUI's image element reads a local file, and the daemon may be on another
//! machine, so the only path the window can be handed is one of its own. A row
//! carries the avatar's content id (`ChannelView::avatar`); this module turns
//! that id into a file here, fetching the bytes over the control plane when the
//! picture is not already cached.
//!
//! The name *is* the id, so two channels with the same picture share one file
//! and nothing has to be invalidated: a changed picture is a changed id. No
//! extension is kept — ZGUI's decoder guesses the format from the bytes.

// compio is thread-per-core: this runs on the RPC worker thread, so the future
// is deliberately !Send.
#![allow(clippy::future_not_send)]

use std::path::PathBuf;

use siphon_shared::riffle::Client;
use siphon_shared::riffle::Request;
use siphon_shared::riffle::Response;

use crate::service::APP_ID;

/// Where this client keeps the pictures it has fetched.
///
/// On the daemon's own machine this is the same directory the daemon caches
/// into, which is harmless: the file is named by its contents, so either side
/// may have put it there.
const AVATAR_DIR: &str = "avatars";

/// Length of an avatar id: 128 bits of hash as lowercase hex.
const ID_LEN: usize = 32;

fn dir() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join(APP_ID).join(AVATAR_DIR))
}

/// The local file one avatar id is cached as, when `id` can name one.
///
/// The id arrives from the daemon and becomes a file name, so only the hash
/// alphabet is accepted: no separator and no dot, hence no way out of the
/// directory.
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

/// The local path of a picture this client already has, if any.
///
/// One metadata check per id, not per frame: a row asks once when its id first
/// appears, so a cached picture is drawn without a round trip.
#[must_use]
pub fn cached(id: &str) -> Option<String> {
    let path = path_for(id)?;
    std::fs::metadata(&path).ok()?;
    path.into_os_string().into_string().ok()
}

/// Fetches one picture into this client's cache, returning its local path.
///
/// Runs on the RPC worker, so the frame loop never waits on a picture. `None` is
/// a request that failed or a body that could not be written; the row keeps its
/// initials.
pub async fn fetch(client: &Client, id: &str) -> Option<String> {
    let path = path_for(id)?;
    if compio::fs::metadata(&path).await.is_ok() {
        return path.into_os_string().into_string().ok();
    }
    let bytes = match client
        .request(Request::GetAvatar { id: id.to_owned() })
        .await
    {
        Ok(Response::Avatar(bytes)) => bytes,
        Ok(_) => {
            tracing::info!(target: "avatar", "backend answered {id} with the wrong response");
            return None;
        }
        Err(error) => {
            tracing::info!(target: "avatar", "could not fetch {id}: {error}");
            return None;
        }
    };
    if let Some(dir) = path.parent()
        && let Err(error) = compio::fs::create_dir_all(dir).await
    {
        tracing::info!(target: "avatar", "could not create {dir:?}: {error}");
        return None;
    }
    if let Err(error) = compio::fs::write(&path, bytes).await.0 {
        tracing::info!(target: "avatar", "could not cache {id}: {error}");
        return None;
    }
    path.into_os_string().into_string().ok()
}
