#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Siphon UI: a Slint window and tray rendered from the daemon's RPC.

mod app;
mod format;
mod rpc;
mod service;
mod ui;

use std::sync::Arc;

use slint::ComponentHandle as _;
use tracing_subscriber::EnvFilter;

use crate::app::ViewParams;
use crate::app::build;
use crate::rpc::SharedSnapshot;
use crate::rpc::TrayAction;
use crate::rpc::empty_snapshot;
use crate::rpc::intent_channel;
use crate::rpc::run;
use crate::service::APP_ID;

fn main() {
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
    let (shared, gui_rx) = SharedSnapshot::pair(empty_snapshot());

    let work = Arc::clone(&shared);
    std::thread::Builder::new()
        .name("rpc worker".to_owned())
        .spawn(move || {
            compio::runtime::Runtime::new()
                .expect("compio runtime")
                .block_on(run(intent_rx, wake, work));
        })
        .expect("rpc thread");

    // Second launches wake the window through the frame channel the foreground
    // pump awaits, so showing works even while hidden.
    let wake_shared = Arc::clone(&shared);
    let single = app_single_instance::start_primary(APP_ID, move || {
        tracing::info!(target: "single", "wake signal received");
        wake_shared.notify_tray(TrayAction::Show);
    });

    let (ui, guards) = build(ViewParams {
        intents,
        shared,
        gui_rx,
        single,
    });
    ui.run().expect("slint event loop");
    drop(guards);
}
