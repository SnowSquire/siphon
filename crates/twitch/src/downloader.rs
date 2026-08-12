use curl::easy::Easy;
use kanal::Receiver;
use std::io::{self, Read, Write};
use thiserror::Error;
use tracing::{Span, debug, info, trace};
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
    let mut handle = Easy::new();
    _ = handle.accept_encoding("");

    for msg in rx {
        match msg {
            FetchURL {
                url,
                mut writer,
                span,
            } => {
                let _guard = span.enter();
                let _ = launch_download(url.as_ref(), &mut writer, &mut handle);
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
    handle: &mut Easy,
) -> Result<(), DownloadError> {
    info!("starting download");
    handle.url(url);
    let mut total_size = 0;
    let result = {
        let mut transfer = handle.transfer();

        transfer
            .write_function(|src| {
                let mut remaining = src;

                while !remaining.is_empty() {
                    let mut dst = writer.writable_slice();

                    let n = dst.write(remaining).unwrap();

                    writer.commit(n);
                    remaining = &remaining[n..];
                }
                total_size += src.len();
                trace!("added {} bytes", src.len());
                writer.publish();
                Ok(src.len())
            })
            .unwrap();

        transfer.perform()
    }
    .unwrap();
    writer.close();
    debug!("finished download with {} bytes", total_size);
    Ok(())
}
