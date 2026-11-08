use std::rc::Rc;
use std::time::Duration;

use kanal::AsyncReceiver;
use thiserror::Error;
use tracing::Instrument;
use tracing::Span;
use tracing::debug;
use tracing::warn;

use crate::constants::ARENA_BACKOFF;
use crate::constants::ARENA_RETRIES;
use crate::constants::DOWNLOAD_RETRIES;
use crate::http::Client;
use crate::spool::ArenaFull;
use crate::spool::Writer;

pub enum ToDownloader {
    FetchURL {
        url: Rc<str>,
        writer: Writer,
        span: Span,
    },
}

pub fn spawn_downloaders<const N: usize>(client: &Rc<Client>, rx: &kanal::Receiver<ToDownloader>) {
    for _ in 0..N {
        let rx = rx.clone().to_async();
        let client = Rc::clone(client);
        compio::runtime::spawn(async move {
            worker(rx, &client).await;
        })
        .detach();
    }
}

async fn worker(rx: AsyncReceiver<ToDownloader>, client: &Rc<Client>) {
    while let Ok(message) = rx.recv().await {
        let ToDownloader::FetchURL {
            url,
            mut writer,
            span,
        } = message;

        let mut attempt = 0;
        loop {
            match fetch(client, &url, &mut writer, &span).await {
                Ok(()) => break,
                Err(error) => {
                    // Retrying would append to a partially written segment.
                    if !writer.is_empty() {
                        warn!(%url, %error, "download failed after writing bytes, giving up");
                        break;
                    }
                    attempt += 1;
                    if attempt > DOWNLOAD_RETRIES {
                        warn!(%url, %error, attempt, "download failed, giving up");
                        break;
                    }
                    let backoff = Duration::from_millis(500 * 2u64.pow(attempt - 1));
                    warn!(%url, %error, attempt, ?backoff, "download failed, retrying");
                    compio::time::sleep(backoff).await;
                }
            }
        }
    }
}

#[derive(Debug, Error)]
enum FetchError {
    #[error(transparent)]
    Http(#[from] crate::http::Error),
    #[error(transparent)]
    Arena(#[from] ArenaFull),
}

async fn fetch(
    client: &Rc<Client>,
    url: &str,
    writer: &mut Writer,
    span: &Span,
) -> Result<(), FetchError> {
    let started = std::time::Instant::now();
    let mut response = client.get(url).instrument(span.clone()).await?;
    let head_ms = started.elapsed().as_millis();
    let mut reads = 0usize;
    let mut first_read_ms = 0u128;
    let mut last_read = started;
    let mut max_gap_ms = 0u128;
    let mut max_read = 0usize;
    let mut slow_gaps = 0usize;
    let mut arena_waits = 0;

    loop {
        let dst = match writer.tail() {
            Ok(dst) => dst,
            Err(error) => {
                arena_waits += 1;
                if arena_waits > ARENA_RETRIES {
                    return Err(error.into());
                }
                compio::time::sleep(ARENA_BACKOFF).await;
                continue;
            }
        };

        let read = response.body.read(dst).instrument(span.clone()).await?;
        if read == 0 {
            debug!(
                url = %url.rsplit('/').next().unwrap_or_default().get(..8).unwrap_or(""),
                head_ms,
                reads,
                first_read_ms,
                max_gap_ms,
                max_read,
                slow_gaps,
                total_ms = started.elapsed().as_millis(),
                "body end"
            );
            return Ok(());
        }
        reads += 1;
        if reads == 1 {
            first_read_ms = started.elapsed().as_millis();
        }
        let gap = last_read.elapsed().as_millis();
        max_gap_ms = max_gap_ms.max(gap);
        if gap > 100 {
            slow_gaps += 1;
        }
        last_read = std::time::Instant::now();
        max_read = max_read.max(read);
        writer.commit(read);
    }
}
