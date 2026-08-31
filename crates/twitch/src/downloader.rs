use curl::easy::Easy;
use kanal::Receiver;

use std::io;
use std::thread::sleep;
use std::time::Duration;
use thiserror::Error;
use tracing::{Span, debug, info, trace, warn};
use ureq::Agent;

use crate::downloader::ToDownloader::FetchURL;
use crate::segment::writer::SegmentWriter;

const MAX_RETRIES: u32 = 4;

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
                let mut attempt = 0;

                loop {
                    match launch_download(url.as_ref(), &mut writer, &mut handle) {
                        Ok(()) => break,
                        Err(e) => {
                            attempt += 1;
                            if attempt > MAX_RETRIES {
                                warn!(
                                    "download of {} failed after {} attempts: {}. giving up",
                                    url, attempt, e
                                );
                                writer.close();
                                break;
                            }
                            let backoff = Duration::from_millis(500 * 2u64.pow(attempt - 1));
                            warn!(
                                "download of {} failed (attempt {}/{}): {}. retrying in {:?}",
                                url, attempt, MAX_RETRIES, e, backoff
                            );
                            sleep(backoff);
                        }
                    }
                }
            }
        }
    }
}

#[derive(Error, Debug)]
enum DownloadError {
    #[error("curl error: {0}")]
    Curl(#[from] curl::Error),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

fn launch_download(
    url: &str,
    writer: &mut SegmentWriter,
    handle: &mut Easy,
) -> Result<(), DownloadError> {
    info!("starting download");
    handle.reset();
    handle.url(url)?;

    let total_size = &mut 0usize;

    let result = {
        let mut transfer = handle.transfer();

        let write_result = transfer.write_function(|src| {
            let mut remaining = src;

            while !remaining.is_empty() {
                let dst = writer.writable_slice();

                let to_write = remaining.len().min(dst.len());

                unsafe {
                    std::ptr::copy_nonoverlapping(
                        remaining.as_ptr(),
                        dst.as_mut_ptr() as *mut u8,
                        to_write,
                    );
                    writer.commit(to_write);
                }
                remaining = &remaining[to_write..];
            }

            *total_size += src.len();
            trace!("added {} bytes", src.len());
            writer.publish();
            Ok(src.len())
        });

        match write_result {
            Ok(()) => transfer.perform(),
            Err(e) => Err(e.into()),
        }
    };

    result?;
    writer.close();
    debug!("finished download with {} bytes", total_size);
    Ok(())
}
