use std::array;
use std::io::Read;
use std::io::{self};
use std::thread::JoinHandle;
use std::thread::sleep;
use std::time::Duration;

#[cfg(feature = "curl-download")]
use curl::easy::Easy;
use kanal::Receiver;
use thiserror::Error;
use tracing::Span;
use tracing::info;
use tracing::trace;
use tracing::warn;
use ureq::Agent;

use crate::downloader::ToDownloader::FetchURL;
use crate::segment::SegmentWriter;

const MAX_RETRIES: u32 = 4;

pub enum ToDownloader {
    FetchURL {
        url: Box<str>,
        writer: SegmentWriter,
        span: Span,
    },
}

pub fn spawn_downloaders<const N: usize>(
    agent: &Agent,
    rx: kanal::Receiver<ToDownloader>,
) -> [JoinHandle<()>; N] {
    array::from_fn(|index| {
        let rx = rx.clone();
        let agent = agent.clone();
        std::thread::Builder::new()
            .name(format!("downloader {index}"))
            .spawn(move || download_worker(rx, agent))
            .expect("failed to spawn downloader thread")
    })
}

pub fn download_worker(rx: Receiver<ToDownloader>, agent: Agent) {
    #[cfg(feature = "curl-download")]
    let mut handle = {
        let mut handle = Easy::new();
        _ = handle.accept_encoding("");
        handle
    };

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
                    #[cfg(feature = "curl-download")]
                    let result = launch_download_curl(&mut handle, url.as_ref(), &mut writer);

                    #[cfg(not(feature = "curl-download"))]
                    let result = launch_download_ureq(&agent, url.as_ref(), &mut writer);

                    match result {
                        Ok(()) => break,
                        Err(e) => {
                            attempt += 1;
                            if attempt > MAX_RETRIES {
                                warn!(
                                    "download of {} failed after {} attempts: {}. giving up",
                                    url, attempt, e
                                );
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
    #[cfg(feature = "curl-download")]
    #[error("curl error: {0}")]
    Curl(#[from] curl::Error),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("ureq error: {0}")]
    Ureq(#[from] ureq::Error),
}

fn launch_download_ureq(
    agent: &Agent,
    url: &str,
    writer: &mut SegmentWriter,
) -> Result<(), DownloadError> {
    let mut reader = agent
        .get(url)
        .header("Accept-Encoding", "")
        .call()?
        .into_body()
        .into_reader();

    loop {
        let dst = unsafe { writer.writable_slice().assume_init_mut() };
        let written = reader.read(dst)?;
        if written == 0 {
            break;
        }
        unsafe { writer.commit(written) };

        writer.publish();
    }

    Ok(())
}

#[cfg(feature = "curl-download")]
fn launch_download_curl(
    handle: &mut Easy,
    url: &str,
    writer: &mut SegmentWriter,
) -> Result<(), DownloadError> {
    info!("starting download");
    handle.reset();
    handle.url(url)?;

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

    Ok(())
}
