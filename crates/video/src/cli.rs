use std::path::PathBuf;

use clap::Parser;

use crate::app;

#[derive(Debug, Parser)]
#[command(about = "FFplay-like prototype: ZGUI UI, DXGI presentation and Vulkan video")]
struct Args {
    /// Media file to play. Not decoded yet; the UI shows the path.
    #[arg(long)]
    file: Option<PathBuf>,
}

pub fn run() -> anyhow::Result<()> {
    let args = Args::parse();
    app::run(args.file)
}
