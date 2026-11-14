#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod cli;
#[cfg(windows)]
mod composite;
#[cfg(windows)]
mod dxgi;
#[cfg(windows)]
mod pattern;
#[cfg(windows)]
mod renderer;
#[cfg(windows)]
mod video;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    cli::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("video is a Windows-only prototype");
}
