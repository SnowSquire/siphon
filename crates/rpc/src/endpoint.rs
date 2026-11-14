//! Where the daemon's admin surface lives.
//!
//! Windows uses a named pipe, Unix a domain socket. Neither is a TCP port, so
//! the admin surface can never follow `--bind 0.0.0.0`, and the OS access
//! check is the process user's.

#[cfg(unix)]
use std::path::PathBuf;

/// The endpoint for one app id: a pipe name on Windows, a socket path on Unix.
#[must_use]
pub fn admin_endpoint(app_id: &str) -> String {
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\siphon-{app_id}")
    }
    #[cfg(unix)]
    {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        dir.join(format!("siphon-{app_id}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

/// Whether a daemon is listening on `endpoint`, without blocking.
///
/// Used by the UI before it spawns the daemon and by the restart path while it
/// waits for the old process to release the endpoint.
#[must_use]
pub fn probe(endpoint: &str) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Pipes::WaitNamedPipeW;

        let name: Vec<u16> = endpoint.encode_utf16().chain([0]).collect();
        // A zero timeout only checks whether an instance is available now.
        // SAFETY: `name` is nul-terminated and live for the call.
        unsafe { WaitNamedPipeW(name.as_ptr(), 0) != 0 }
    }
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(endpoint).is_ok()
    }
}

/// Opens the client side of `endpoint`.
#[cfg(windows)]
pub(crate) async fn connect(
    endpoint: &str,
) -> std::io::Result<compio::fs::named_pipe::NamedPipeClient> {
    compio::fs::named_pipe::ClientOptions::new()
        .open(endpoint)
        .await
}

/// Opens the client side of `endpoint`.
#[cfg(unix)]
pub(crate) async fn connect(endpoint: &str) -> std::io::Result<compio::net::UnixStream> {
    compio::net::UnixStream::connect(endpoint).await
}
