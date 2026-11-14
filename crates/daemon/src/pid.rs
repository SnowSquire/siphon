//! PID file for the UI's process-level restart.
//!
//! The UI cannot rely on RPC to restart a daemon that is frozen, so it records
//! the PID here at startup and terminates the process directly.

use std::path::PathBuf;

/// `%LOCALAPPDATA%/<app id>/daemon.pid` (or the platform data dir).
#[must_use]
pub fn path(app_id: &str) -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(app_id)
        .join("daemon.pid")
}

pub fn write(app_id: &str) -> std::io::Result<()> {
    let path = path(app_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, std::process::id().to_string())
}

pub fn remove(app_id: &str) {
    let _ = std::fs::remove_file(path(app_id));
}
