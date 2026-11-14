use std::path::PathBuf;

use clap::Parser;

use crate::app;
use crate::dxgi::HdrMode;

#[derive(Debug, Parser)]
#[command(about = "FFplay-like prototype: ZGUI UI, DXGI presentation and Vulkan video")]
struct Args {
    /// Media file to play. Not decoded yet; the UI shows the path.
    #[arg(long)]
    file: Option<PathBuf>,

    /// Present in HDR when the display supports it.
    #[arg(long, value_enum, default_value_t = HdrMode::Auto)]
    hdr: HdrMode,
}

pub fn run() -> anyhow::Result<()> {
    let args = Args::parse();
    app::run(args.file, args.hdr)
}
