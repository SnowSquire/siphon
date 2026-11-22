//! Channel avatars, cached once per Twitch picture.
//!
//! The UI's image element reads a local path, not a URL: ZGUI decodes a file
//! on a worker and never speaks HTTP. The daemon already has the TLS client and
//! the GQL quota, so it downloads each avatar once into the app's data directory
//! and hands the UI the path. The file name comes from the URL, which is stable
//! per user, so a restart is one metadata check and no network.

use std::path::PathBuf;
use std::time::Duration;

use crate::http::Client;

const AVATAR_DIR: &str = "avatars";

/// Delays before the retry attempts of one avatar whose first download failed.
///
/// A connect can lose a race with a slow DNS answer or a CDN edge — a 5s
/// connect timeout on one attempt reports `http: timed out: io`, and the same
/// URL answers in 40ms a moment later. Without this a channel keeps its initials
/// for the whole session, because the only other attempt is the next Hermes
/// reconnect.
const AVATAR_RETRIES: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(5)];

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
    if compio::fs::metadata(&path).await.is_ok() {
        return path.into_os_string().into_string().ok();
    }
    for attempt in 0..=AVATAR_RETRIES.len() {
        if attempt > 0 {
            compio::time::sleep(AVATAR_RETRIES[attempt - 1]).await;
        }
        match crate::gql::fetch_file(client, url, &path).await {
            Ok(()) => {
                if attempt > 0 {
                    tracing::info!(
                        target: "avatar",
                        "cached {url} on attempt {}",
                        attempt + 1
                    );
                }
                return path.into_os_string().into_string().ok();
            }
            Err(error) => {
                if attempt == AVATAR_RETRIES.len() {
                    tracing::info!(target: "avatar", "could not cache {url}: {error}");
                }
            }
        }
    }
    None
}
