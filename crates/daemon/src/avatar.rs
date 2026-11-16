//! Channel avatars, cached once per Twitch picture.
//!
//! The UI's image element reads a local path, not a URL: ZGUI decodes a file
//! on a worker and never speaks HTTP. The daemon already has the TLS client and
//! the GQL quota, so it downloads each avatar once into the app's data directory
//! and hands the UI the path. The file name comes from the URL, which is stable
//! per user, so a restart is one metadata check and no network.

use std::path::PathBuf;

use crate::http::Client;

const AVATAR_DIR: &str = "avatars";

/// The file an avatar URL is cached as, if the URL has a safe last segment.
#[must_use]
pub fn cached_path(url: &str) -> Option<PathBuf> {
    let name = url.rsplit('/').next()?.split('?').next()?;
    let name = name
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
        .collect::<String>();
    if name.is_empty() || name.len() > 128 {
        return None;
    }
    let dir = dirs::data_local_dir()?
        .join(crate::config::APP_ID)
        .join(AVATAR_DIR);
    Some(dir.join(name))
}

/// Returns the local path of an avatar, downloading it first when missing.
///
/// `None` is a failed or impossible download; the UI then keeps the initials
/// its avatar draws behind the picture.
pub async fn ensure(client: &Client, url: &str) -> Option<String> {
    let path = cached_path(url)?;
    if compio::fs::metadata(&path).await.is_err()
        && let Err(error) = crate::gql::fetch_file(client, url, &path).await
    {
        tracing::info!(target: "avatar", "could not cache {url}: {error}");
        return None;
    }
    path.into_os_string().into_string().ok()
}
