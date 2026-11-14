use std::rc::Rc;
use std::time::Duration;

use kanal::AsyncReceiver;
use siphon_rpc::FetchOutcome as SegmentOutcome;
use siphon_rpc::SegmentRecord;
use thiserror::Error;
use tracing::Instrument;
use tracing::Span;
use tracing::debug;
use tracing::warn;

use crate::constants::ARENA_BACKOFF;
use crate::constants::ARENA_RETRIES;
use crate::constants::DOWNLOAD_RETRIES;
use crate::http::Client;
use crate::http::StatusCode;
use crate::metrics::DownloaderMetrics;
use crate::metrics::FetchStats;
use crate::spool::ArenaFull;
use crate::spool::Writer;

#[derive(Debug, Error)]
enum FetchError {
    #[error(transparent)]
    Http(#[from] crate::http::Error),
    #[error(transparent)]
    Arena(#[from] ArenaFull),
    #[error("http status {0}")]
    Status(StatusCode),
}

impl FetchError {
    /// Whether another attempt could plausibly succeed.
    fn retryable(&self) -> bool {
        match self {
            Self::Http(error) => error.retryable(),
            // `fetch` already waited `ARENA_RETRIES * ARENA_BACKOFF` for a free
            // slot, so another round would repeat the wait and starve again.
            Self::Arena(_) => false,
            Self::Status(status) => {
                status.is_server_error()
                    || *status == StatusCode::REQUEST_TIMEOUT
                    || *status == StatusCode::TOO_MANY_REQUESTS
            }
        }
    }
}

pub enum ToDownloader {
    FetchURL {
        url: Rc<str>,
        writer: Writer,
        span: Span,
        /// Receives the terminal outcome. The disk output needs to distinguish a
        /// completed chunk from a fetch that gave up, because a failed DVR chunk
        /// is skipped instead of being recorded in the canonical index.
        done: Option<kanal::Sender<FetchOutcome>>,
        /// Live telemetry for the segment ring; DVR chunks and prefetches that
        /// are not part of the live pipeline pass `None`.
        stats: Option<FetchStats>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchOutcome {
    Complete,
    Failed,
}

pub fn spawn_downloaders<const N: usize>(
    client: &Rc<Client>,
    rx: &kanal::Receiver<ToDownloader>,
    metrics: &Rc<DownloaderMetrics>,
) {
    for _ in 0..N {
        let sync_rx = rx.clone();
        let rx = rx.clone().to_async();
        let client = Rc::clone(client);
        let metrics = Rc::clone(metrics);
        compio::runtime::spawn(async move {
            worker(rx, &sync_rx, &client, &metrics).await;
        })
        .detach();
    }
}

async fn worker(
    rx: AsyncReceiver<ToDownloader>,
    queued_rx: &kanal::Receiver<ToDownloader>,
    client: &Rc<Client>,
    metrics: &Rc<DownloaderMetrics>,
) {
    while let Ok(message) = rx.recv().await {
        metrics
            .queued
            .set(u32::try_from(queued_rx.len()).unwrap_or(u32::MAX));
        let ToDownloader::FetchURL {
            url,
            mut writer,
            span,
            done,
            stats,
        } = message;
        metrics.in_flight.set(metrics.in_flight.get() + 1);
        let started = std::time::Instant::now();

        let mut attempt = 0;
        let mut outcome = FetchOutcome::Complete;
        loop {
            match fetch(client, &url, &mut writer, &span).await {
                Ok(()) => break,
                Err(error) => {
                    // Retrying would append to a partially written segment.
                    if !writer.is_empty() {
                        warn!(%url, %error, "download failed after writing bytes, giving up");
                        outcome = FetchOutcome::Failed;
                        break;
                    }
                    // A terminal failure (bad URL, 404, certificate) fails the
                    // same way every time; retrying only delays the next segment.
                    if !error.retryable() {
                        warn!(%url, %error, "download failed, giving up");
                        outcome = FetchOutcome::Failed;
                        break;
                    }
                    attempt += 1;
                    metrics.retries.set(metrics.retries.get() + 1);
                    if attempt > DOWNLOAD_RETRIES {
                        warn!(%url, %error, attempt, "download failed, giving up");
                        outcome = FetchOutcome::Failed;
                        break;
                    }
                    let backoff = Duration::from_millis(500 * 2u64.pow(attempt - 1));
                    warn!(%url, %error, attempt, ?backoff, "download failed, retrying");
                    compio::time::sleep(backoff).await;
                }
            }
        }
        metrics
            .in_flight
            .set(metrics.in_flight.get().saturating_sub(1));
        if let Some(stats) = stats {
            stats.metrics.push_segment(SegmentRecord {
                sequence: stats.sequence,
                timestamp_ms: stats.timestamp_ms,
                duration_ms: stats.duration_ms,
                pdt_ms: stats.pdt_ms,
                bytes: u64::try_from(writer.published()).unwrap_or(u64::MAX),
                elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                retries: attempt,
                outcome: match outcome {
                    FetchOutcome::Complete => SegmentOutcome::Complete,
                    FetchOutcome::Failed => SegmentOutcome::Failed,
                },
            });
        }
        if let Some(done) = done {
            let _ = done.send(outcome);
        }
    }
}

async fn fetch(
    client: &Rc<Client>,
    url: &str,
    writer: &mut Writer,
    span: &Span,
) -> Result<(), FetchError> {
    let started = std::time::Instant::now();
    let mut response = client.get(url).instrument(span.clone()).await?;
    // Without this, a 404/503 error page would be published as segment bytes.
    if !response.status.is_success() {
        return Err(FetchError::Status(response.status));
    }
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
                writer.note_arena_wait();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_transient_failures_only() {
        assert!(FetchError::Status(StatusCode::INTERNAL_SERVER_ERROR).retryable());
        assert!(FetchError::Status(StatusCode::SERVICE_UNAVAILABLE).retryable());
        assert!(FetchError::Status(StatusCode::TOO_MANY_REQUESTS).retryable());
        assert!(FetchError::Status(StatusCode::REQUEST_TIMEOUT).retryable());

        assert!(!FetchError::Status(StatusCode::NOT_FOUND).retryable());
        assert!(!FetchError::Status(StatusCode::FORBIDDEN).retryable());
        assert!(!FetchError::Arena(ArenaFull).retryable());
    }
}
