//! Finding, starting and restarting the daemon process.
//!
//! A frozen daemon cannot answer an RPC restart, so this path is strictly
//! process-level: the daemon records its PID, the UI terminates that process
//! (or falls back to the image name) and starts a fresh one.

use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;

#[cfg(debug_assertions)]
pub const APP_ID: &str = "com.iken.siphon.debug";
#[cfg(not(debug_assertions))]
pub const APP_ID: &str = "com.iken.siphon";

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The RTSP listener the daemon binds alongside its admin port.
const RTSP: &str = "127.0.0.1:7417";

fn endpoint() -> String {
    siphon_shared::riffle::admin_endpoint(APP_ID)
}

/// Whether the daemon's admin endpoint accepts a connection.
#[must_use]
pub fn reachable() -> bool {
    siphon_shared::riffle::probe(&endpoint())
}

fn port_open(address: &str) -> bool {
    let Ok(address) = address.parse() else {
        return false;
    };
    std::net::TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_ok()
}

fn healthy() -> bool {
    reachable() && port_open(RTSP)
}

/// Starts the daemon when the UI comes up and nothing is listening yet.
pub fn ensure_running() {
    if !reachable() {
        start();
    }
}

/// Force-restarts the daemon. Runs on its own thread: the UI thread must never
/// block, and a frozen daemon may take a few seconds to go away.
pub fn restart() {
    tracing::info!(target: "service", "restarting the daemon");
    terminate();
    wait_ports_closed(40);
    if reachable() || port_open(RTSP) {
        tracing::warn!(target: "service", "daemon still listening, killing by name");
        kill_by_name();
        wait_ports_closed(20);
    }
    // A listening socket can linger after the process is killed, so retry the
    // spawn until both ports answer.
    for attempt in 1..=3 {
        start();
        for _ in 0..40 {
            if healthy() {
                tracing::info!(target: "service", "daemon is up");
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        tracing::warn!(target: "service", "daemon did not come up (attempt {attempt})");
    }
    tracing::error!(target: "service", "daemon did not come up after restart");
}

fn wait_ports_closed(iterations: usize) {
    for _ in 0..iterations {
        if !reachable() && !port_open(RTSP) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Spawns the daemon next to this executable (or `SIPHON_DAEMON`).
pub fn start() {
    let path = daemon_path();
    let mut command = Command::new(&path);
    command.arg("daemon").stdin(Stdio::null());
    inherit_log(&mut command);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;

        command.creation_flags(CREATE_NO_WINDOW);
    }
    // Its own process group, so a Ctrl-C on the UI — or the terminal that started
    // it being closed — is not also a signal to the daemon this UI started.
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt as _;

        command.process_group(0);
    }
    match command.spawn() {
        Ok(child) => tracing::info!(
            target: "service",
            "started {} (pid {})",
            path.display(),
            child.id()
        ),
        Err(error) => tracing::error!(
            target: "service",
            "failed to start {}: {error}",
            path.display()
        ),
    }
}

/// Points the child's stdout and stderr at the daemon's log file.
///
/// The daemon logs through tracing, which writes to that file itself; this
/// catches what bypasses it — the message a panic prints before `abort`, and
/// anything a startup warning writes before the subscriber exists.
fn inherit_log(command: &mut Command) {
    let path = siphon_shared::riffle::log_path(APP_ID);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path);
    match opened {
        Ok(file) => {
            let copy = file.try_clone();
            command.stdout(Stdio::from(file));
            match copy {
                Ok(copy) => {
                    command.stderr(Stdio::from(copy));
                }
                Err(error) => {
                    tracing::warn!(target: "service", %error, "daemon stderr is discarded");
                    command.stderr(Stdio::null());
                }
            }
        }
        Err(error) => {
            tracing::warn!(
                target: "service",
                path = %path.display(),
                %error,
                "cannot open the daemon log; its output is discarded"
            );
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
}

fn terminate() {
    let Some(pid) = read_pid() else {
        tracing::warn!(target: "service", "no daemon pid file");
        kill_by_name();
        return;
    };
    if terminate_pid(pid) {
        tracing::info!(target: "service", "terminated daemon pid {pid}");
    } else {
        tracing::warn!(target: "service", "could not terminate pid {pid}");
        kill_by_name();
    }
}

fn pid_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_ID)
        .join("daemon.pid")
}

fn read_pid() -> Option<u32> {
    std::fs::read_to_string(pid_path())
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(windows)]
fn terminate_pid(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::OpenProcess;
    use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;
    use windows_sys::Win32::System::Threading::PROCESS_TERMINATE;
    use windows_sys::Win32::System::Threading::QueryFullProcessImageNameW;
    use windows_sys::Win32::System::Threading::TerminateProcess;

    // SAFETY: the returned handle is owned by this function and closed below.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        return false;
    }
    let mut buffer = [0u16; 1024];
    let mut length = u32::try_from(buffer.len()).unwrap_or(0);
    // SAFETY: the buffer is live and `length` starts at its capacity.
    let read =
        unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &raw mut length) };
    let path = String::from_utf16_lossy(&buffer[..usize::try_from(length).unwrap_or(0)]);
    // A stale PID may have been reused: only kill the daemon image.
    let is_daemon = read != 0 && path.to_ascii_lowercase().ends_with("siphon-daemon.exe");
    if !is_daemon {
        // SAFETY: the handle is valid and owned here.
        unsafe { CloseHandle(handle) };
        return false;
    }
    // SAFETY: the handle is valid and owned here.
    let terminated = unsafe { TerminateProcess(handle, 1) };
    // SAFETY: the handle is valid and owned here.
    unsafe { CloseHandle(handle) };
    terminated != 0
}

#[cfg(not(windows))]
fn terminate_pid(pid: u32) -> bool {
    // A stale PID may have been reused: only stop the daemon image. `/proc`
    // names the executable the pid is actually running, symlink and all.
    let exe = std::fs::read_link(format!("/proc/{pid}/exe"));
    let is_daemon =
        exe.is_ok_and(|path| path.file_name().is_some_and(|name| name == "siphon-daemon"));
    if !is_daemon {
        return false;
    }
    Command::new("kill")
        .arg(pid.to_string())
        .status()
        .is_ok_and(|status| status.success())
}

fn kill_by_name() {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;

        let _ = Command::new("taskkill")
            .args(["/IM", "siphon-daemon.exe", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        // `-x` matches the process name exactly, so nothing else named with
        // "daemon" in it is a candidate; this is the force path, hence `-9`.
        let _ = Command::new("pkill")
            .args(["-9", "-x", "siphon-daemon"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn daemon_path() -> PathBuf {
    if let Some(path) = std::env::var_os("SIPHON_DAEMON") {
        return PathBuf::from(path);
    }
    let name = if cfg!(windows) {
        "siphon-daemon.exe"
    } else {
        "siphon-daemon"
    };
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
        .unwrap_or_else(|| PathBuf::from(name))
}
