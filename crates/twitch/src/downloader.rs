use std::array;
use std::io::Read;
use std::io::{self};
use std::thread::JoinHandle;
use std::thread::sleep;
use std::time::Duration;

use kanal::Receiver;
use thiserror::Error;
use tracing::Span;
use tracing::warn;
use ureq::Agent;

use crate::downloader::ToDownloader::FetchURL;
use crate::segment::Writer;

const MAX_RETRIES: u32 = 4;

pub enum ToDownloader {
    FetchURL {
        url: Box<str>,
        writer: Writer,
        span: Span,
    },
}

pub fn spawn_downloaders<const N: usize>(
    agent: &Agent,
    rx: &Receiver<ToDownloader>,
) -> [JoinHandle<()>; N] {
    array::from_fn(|index| {
        let rx = rx.clone();
        let agent = agent.clone();
        std::thread::Builder::new()
            .name(format!("downloader {index}"))
            .spawn(move || download_worker(rx, &agent))
            .expect("failed to spawn downloader thread")
    })
}

pub fn download_worker(rx: Receiver<ToDownloader>, agent: &Agent) {
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
                    let result = launch_download_ureq(agent, url.as_ref(), &mut writer);

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
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("ureq error: {0}")]
    Ureq(#[from] ureq::Error),
}

fn launch_download_ureq(
    agent: &Agent,
    url: &str,
    writer: &mut Writer,
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
