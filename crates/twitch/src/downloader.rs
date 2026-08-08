use std::io::{self, Read};

use kanal::Receiver;
use thiserror::Error;
use tracing::{Span, debug, info};
use ureq::Agent;

use crate::{
    constants::{MPEG_TS_PACKET_SIZE, round_chunk_size},
    downloader::ToDownloader::FetchURL,
    stream_writer::SegmentWriter,
};

pub enum ToDownloader {
    FetchURL {
        url: Box<str>,
        writer: SegmentWriter,
        span: Span,
    },
}
pub fn download_worker(rx: Receiver<ToDownloader>, agent: Agent) {
    for msg in rx {
        match msg {
            FetchURL {
                url,
                mut writer,
                span,
            } => {
                let _guard = span.enter();
                let _ = launch_download(url.as_ref(), &mut writer, &agent);
            }
        }
    }
}
#[derive(Error, Debug)]
enum DownloadError {
    #[error("ureq threw an error")]
    Ureq(#[from] ureq::Error),
    #[error("ureq threw an error")]
    Io(#[from] io::Error),
}

fn launch_download(
    url: &str,
    writer: &mut SegmentWriter,
    agent: &Agent,
) -> Result<(), DownloadError> {
    info!("starting download");
    let mut reader = agent.get(url).call()?.into_body().into_reader();
    let mut uncommitedbytes = 0;
    loop {
        let readbytes = {
            let dst = writer.writable_slice();
            reader.read(dst)?
        };

        if readbytes == 0 {
            writer.commit(uncommitedbytes);
            break;
        }
        uncommitedbytes += readbytes;
        if uncommitedbytes >= MPEG_TS_PACKET_SIZE.into() {
            let rounded = round_chunk_size(uncommitedbytes);

            writer.commit(rounded);
            uncommitedbytes -= rounded;
        }
    }
    Ok(())
}
