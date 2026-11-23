//! Build identity baked in at compile time.
//!
//! The release workflow exports `SIPHON_COMMIT` from the checked-out commit
//! before cargo runs. A local build without it reports `local`.

/// The commit the binaries were built from.
pub const COMMIT: &str = match option_env!("SIPHON_COMMIT") {
    Some(commit) => commit,
    None => "local",
};

/// [`COMMIT`] shortened for display; `COMMIT` itself keeps the full SHA.
#[must_use]
pub fn short_commit() -> &'static str {
    COMMIT.get(..12).unwrap_or(COMMIT)
}

/// `<version> (<short commit>)`, for help and log output.
#[must_use]
pub fn version_with_commit() -> String {
    format!("{} ({})", env!("CARGO_PKG_VERSION"), short_commit())
}
