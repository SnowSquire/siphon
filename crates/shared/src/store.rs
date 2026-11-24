//! One versioned JSON file on disk, and where the app keeps them.
//!
//! The daemon's config and a client's settings are different documents — one
//! per backend, one per attached UI — but they are the same kind of file, so
//! locating, reading and writing them lives here rather than once per side.

use std::path::Path;
use std::path::PathBuf;

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] sonic_rs::Error),
}

/// The configuration directory of one app id: the files a person may edit.
#[must_use]
pub fn config_dir(app_id: &str) -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(app_id)
}

/// One file under [`config_dir`].
#[must_use]
pub fn path(app_id: &str, file: &str) -> PathBuf {
    config_dir(app_id).join(file)
}

/// Reads and parses one file, or `None` when it is not there.
///
/// A missing file is not an error: every caller has a default, and a first run
/// is the most common reason for one to be missing.
pub fn read<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StoreError::Io(error)),
    };
    Ok(Some(sonic_rs::from_slice(&bytes)?))
}

/// Reads and parses one file with `compio::fs`, for callers on the daemon's
/// reactor: the blocking [`read`] would stop the one thread everything runs on.
// `!Send` is the whole architecture: compio's I/O futures are not `Send`, and
// every caller is a task on the one reactor thread.
#[allow(clippy::future_not_send)]
pub async fn read_async<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    let bytes = match compio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StoreError::Io(error)),
    };
    Ok(Some(sonic_rs::from_slice(&bytes)?))
}

/// Serialises a value in the spelling every file here uses.
pub fn to_json<T: Serialize>(value: &T) -> Result<String, StoreError> {
    Ok(sonic_rs::to_string_pretty(value)?)
}

/// Writes one file, creating its directory, through a temporary name so a
/// reader never sees half a document.
///
/// Synchronous on purpose: the files are a few hundred bytes and the callers
/// that use this are not on the daemon's reactor.
pub fn write<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let json = to_json(value)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, json)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// [`write`] with `compio::fs`, with the same temporary name: the reactor must
/// not block, and a reader must never see half a document.
// `!Send` for the same reason as [`read_async`].
#[allow(clippy::future_not_send)]
pub async fn write_async<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let json = to_json(value)?;
    if let Some(parent) = path.parent() {
        compio::fs::create_dir_all(parent).await?;
    }
    let temporary = path.with_extension("json.tmp");
    compio::fs::write(&temporary, json).await.0?;
    compio::fs::rename(&temporary, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("siphon-store-{}-{name}.json", std::process::id()))
    }

    #[test]
    fn a_missing_file_reads_as_nothing() {
        let path = scratch("absent");
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            read::<u32>(&path).expect("a missing file is not an error"),
            None
        );
    }

    #[test]
    fn a_written_file_reads_back() {
        let path = scratch("round-trip");
        write(&path, &42u32).expect("write");
        assert_eq!(read::<u32>(&path).expect("read"), Some(42));
        std::fs::remove_file(&path).expect("cleanup");
    }
}
