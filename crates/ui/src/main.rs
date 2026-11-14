#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
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

//! Siphon UI: a ZGUI window presented through DXGI, fed by the daemon RPC.
//!
//! Windows-only: the DXGI flip chain, the native tray and the service control
//! are all Win32.

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod dxgi;
#[cfg(windows)]
mod format;
#[cfg(windows)]
mod gpu;
#[cfg(windows)]
mod placebo;
#[cfg(windows)]
mod pump;
#[cfg(windows)]
mod renderer;
#[cfg(windows)]
mod rpc;
#[cfg(windows)]
mod sdr;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod tray;

#[cfg(windows)]
fn main() {
    use tracing_subscriber::EnvFilter;

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

    // The tray app owns the service: if the daemon is not listening, start it.
    service::ensure_running();

    let (intents, intent_rx, wake) = intent_channel();
    let (event_tx, event_rx) = kanal::unbounded();
    std::thread::Builder::new()
        .name("rpc worker".to_owned())
        .spawn(move || {
            compio::runtime::Runtime::new()
                .expect("compio runtime")
                .block_on(run(intent_rx, wake, event_tx));
        })
        .expect("rpc thread");

    let (tray_tx, tray_rx) = kanal::unbounded();
    let tray = match tray::build(tray_tx) {
        Ok(tray) => Some(tray),
        Err(error) => {
            tracing::error!(target: "tray", "tray build failed: {error:#}");
            None
        }
    };

    let result = app::run(event_rx, tray_rx, intents);
    drop(tray);
    if let Err(error) = result {
        tracing::error!(target: "app", "{error:#}");
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Siphon UI is Windows-only");
}
