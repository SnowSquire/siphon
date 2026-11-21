#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
// The DXGI/Vulkan glue is FFI: one unsafe block per call site with one SAFETY
// comment covers the few calls it makes. API names are names, not code.
#![allow(
    clippy::multiple_unsafe_ops_per_block,
    clippy::doc_markdown,
    clippy::borrow_as_ptr,
    clippy::ptr_as_ptr,
    clippy::ptr_cast_constness,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_ptr_alignment,
    clippy::unnecessary_cast
)]

//! Siphon UI: a ZGUI window fed by the daemon RPC.
//!
//! Both platforms compose the interface into a wgpu texture and let libplacebo
//! draw it into the frame a chain acquired: a D3D12 flip chain imported into
//! Vulkan on Windows, a `VkSwapchainKHR` on ZGUI's Wayland surface on Linux.
//! Nothing above the renderer knows which chain is linked.

mod app;
mod chain;
mod css;
mod format;
mod gpu;
mod hdr;
mod placebo;
mod renderer;
mod rpc;
mod service;
mod settings;

#[cfg(windows)]
mod dxgi;
#[cfg(windows)]
mod sdr;
#[cfg(windows)]
mod tray;

#[cfg(windows)]
fn main() {
    use tracing_subscriber::EnvFilter;

    use crate::rpc::EVENT_QUEUE_CAP;
    use crate::rpc::intent_channel;
    use crate::rpc::run;
    use crate::service::APP_ID;

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    // A scriptable process-level restart, used by the tray action and tests.
    if std::env::args().any(|argument| argument == "--restart-service") {
        service::restart();
        std::process::exit(0);
    }

    // A second launch wakes the first instance and exits immediately.
    if app_single_instance::notify_if_running(APP_ID) {
        return;
    }

    // The tray first, so a second launch arriving from here on has something to
    // post its show request to.
    let tray = match tray::build() {
        Ok(tray) => Some(tray),
        Err(error) => {
            tracing::error!(target: "tray", "tray build failed: {error:?}");
            None
        }
    };
    // The primary handle must outlive the app; a second launch asks the window
    // to show itself, on the GUI thread that owns it.
    let primary = app_single_instance::start_primary(APP_ID, tray::show);

    // The tray app owns the service: if the daemon is not listening, start it.
    service::ensure_running();

    let (intents, intent_rx, wake) = intent_channel();
    let (event_tx, event_rx) = kanal::bounded(EVENT_QUEUE_CAP);
    std::thread::Builder::new()
        .name("rpc worker".to_owned())
        .spawn(move || {
            compio::runtime::Runtime::new()
                .expect("compio runtime")
                .block_on(run(intent_rx, wake, event_tx));
        })
        .expect("rpc thread");

    let result = app::run(event_rx, intents);
    drop(tray);
    drop(primary);
    if let Err(error) = result {
        tracing::error!(target: "app", "{error:?}");
    }
}

/// The window is the whole program here: closing it ends the process, and there
/// is no tray, no single-instance hand-over and no notification output.
#[cfg(not(windows))]
fn main() {
    use tracing_subscriber::EnvFilter;

    use crate::rpc::EVENT_QUEUE_CAP;
    use crate::rpc::intent_channel;
    use crate::rpc::run;

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    if std::env::args().any(|argument| argument == "--restart-service") {
        service::restart();
        std::process::exit(0);
    }

    // The UI owns the daemon it did not find listening.
    service::ensure_running();

    let (intents, intent_rx, wake) = intent_channel();
    let (event_tx, event_rx) = kanal::bounded(EVENT_QUEUE_CAP);
    std::thread::Builder::new()
        .name("rpc worker".to_owned())
        .spawn(move || {
            compio::runtime::Runtime::new()
                .expect("compio runtime")
                .block_on(run(intent_rx, wake, event_tx));
        })
        .expect("rpc thread");

    if let Err(error) = app::run(event_rx, intents) {
        tracing::error!(target: "app", "{error:?}");
    }
}
