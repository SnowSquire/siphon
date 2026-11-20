#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
// This crate is FFI glue over FFmpeg, Vulkan, DXGI and libplacebo's own headers.
//
// One `unsafe` block per call site carries one SAFETY comment covering the few
// calls it makes, which reads better than splitting every block per operation.
// Handles are 64-bit pointers and C enums are `int`, so the integer casts are
// exact on the only target this builds for. The API names are names, not code.
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

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod cli;
#[cfg(windows)]
mod dxgi;
#[cfg(windows)]
mod gpu;
#[cfg(windows)]
mod hw;
#[cfg(windows)]
mod placebo;
#[cfg(windows)]
mod player;
#[cfg(windows)]
mod renderer;
#[cfg(windows)]
mod sdr;
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
