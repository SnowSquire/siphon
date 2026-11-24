//! Where the daemon's output goes.
//!
//! The UI starts the daemon with no console, so without this everything it
//! prints — including the message a panic prints before `abort` — is lost. The
//! log lives in [`siphon_shared::riffle::state_dir`], next to nothing the user edits, and
//! is rotated at startup so a daemon left running for weeks cannot fill a disk.
//!
//! A console is still a console: when stdout is a terminal the same lines are
//! printed there too, which is what makes `cargo run -p daemon -- play` read
//! like a normal program.

use std::fs::File;
use std::io::IsTerminal as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use tracing_subscriber::EnvFilter;

/// A log this large is rotated to `daemon.log.old` at the next start. One
/// generation is enough to debug a crash that came with a restart.
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;

/// Installs the subscriber: every event is appended to the log, and also
/// printed when stdout is a terminal.
pub fn init(app_id: &str) {
    let console = std::io::stdout().is_terminal();
    let path = siphon_shared::riffle::log_path(app_id);
    let file = open(&path);
    let failed = file.is_none();

    let writer = Writer {
        file: Arc::new(Mutex::new(file)),
        console,
    };
    // The default is `error` only, which would make the file useless: it exists
    // to be read back after something went wrong. Color is for a terminal; the
    // file gets plain text so it can be grepped.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(console)
        .with_writer(writer)
        .init();

    if failed {
        // Nothing to fall back to: keep running, it is a log.
        eprintln!("could not open {}; output is discarded", path.display());
    }
    tracing::info!(target: "logging", path = %path.display(), "daemon log");
}

/// Opens the log for appending, rotating a full one out of the way first.
fn open(path: &Path) -> Option<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() > MAX_LOG_BYTES) {
        let _ = std::fs::rename(path, path.with_extension("log.old"));
    }
    File::options().create(true).append(true).open(path).ok()
}

/// Writes one line to the file, and to the console when there is one.
struct Writer {
    file: Arc<Mutex<Option<File>>>,
    console: bool,
}

struct Sink<'a> {
    file: MutexGuard<'a, Option<File>>,
    console: bool,
}

impl std::io::Write for Sink<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.console {
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(buf);
            let _ = stdout.flush();
        }
        // The log is best-effort: a full disk must not stop playback.
        self.file
            .as_mut()
            .map_or(Ok(buf.len()), |file| file.write(buf))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.as_mut().map_or(Ok(()), std::io::Write::flush)
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Writer {
    type Writer = Sink<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        Sink {
            file: self.file.lock().unwrap_or_else(PoisonError::into_inner),
            console: self.console,
        }
    }
}
