use std::rc::Rc;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use kanal::AsyncSender;
use kanal::Receiver;
use kanal::Sender;
use thiserror::Error;
use tracing::Level;
use tracing::debug;
use tracing::debug_span;
use tracing::info;
use tracing::span;
use tracing::warn;

use crate::arena::ArenaSet;
use crate::constants::MAX_MANIFEST;
use crate::downloader::ToDownloader;
use crate::http::Client;
use crate::playlist::Event;
use crate::playlist::ManifestParseError;
use crate::playlist::Playlist;
use crate::spool::ArenaFull;
use crate::spool::Reader;
use crate::spool::Spool;
use crate::spool::WakeSet;
use crate::utils::RetainUnordered;

struct PrefetchSegment {
    expected_id: u64,
    url: Rc<str>,
    reader: Reader,
}

#[derive(Clone, Debug)]
pub struct Segment {
    pub timestamp_ms: u64,
    pub sequence_num: u64,
    pub reader: Reader,
}

#[derive(Debug)]
pub enum SegmentMessage {
    Segment(Segment),
    Prefetch {
        expected_sequence: u64,
        reader: Reader,
    },
    Discontinuity(),
    Init {
        reader: Reader,
    },
}

impl From<Segment> for SegmentMessage {
    fn from(value: Segment) -> Self {
        Self::Segment(value)
    }
}

#[derive(Error, Debug)]
enum SchedulerError {
    #[error("manifest ended")]
    Offline,
    #[error("http error: {0}")]
    Http(#[from] crate::http::Error),
    #[error("manifest is not valid utf-8")]
    Utf8,
    #[error("failed to parse manifest: {0}")]
    Parse(#[from] ManifestParseError),
    #[error("slot arena exhausted")]
    Arena(#[from] ArenaFull),
    #[error("dispatch error")]
    Dispatch,
    #[error("send error: {0}")]
    Send(#[from] kanal::SendError),
}

/// copied and modified from twitch-hls-client directly
pub struct StreamScheduler {
    client: Rc<Client>,
    arena: Rc<ArenaSet>,
    wakes: Rc<WakeSet>,
    stream_url: Box<str>,
    downloaders: AsyncSender<ToDownloader>,
    receive: Receiver<Sender<SegmentMessage>>,

    /// hls fmp4 init header, doesn't reqire a discontinuity
    init: Option<(String, Reader)>,

    segments: Vec<Segment>,
    prefetches: Vec<PrefetchSegment>,

    readers: Vec<Sender<SegmentMessage>>,

    segment_tail: u64,
    prefetch_tail: u64,
    last_segment_dur: Duration,
}

impl StreamScheduler {
    pub const fn new(
        client: Rc<Client>,
        arena: Rc<ArenaSet>,
        wakes: Rc<WakeSet>,
        stream_url: Box<str>,
        downloaders: AsyncSender<ToDownloader>,
        receive: Receiver<Sender<SegmentMessage>>,
    ) -> Self {
        Self {
            client,
            arena,
            wakes,
            stream_url,
            downloaders,
            receive,

            init: None,

            segments: Vec::new(),
            prefetches: Vec::new(),

            readers: Vec::new(),

            segment_tail: 0,
            prefetch_tail: 0,
            last_segment_dur: Duration::from_secs(5),
        }
    }

    pub async fn main(mut self) {
        let span = span!(Level::INFO, "scheduler");
        let _ = &span;
        info!("{}", self.stream_url);

        loop {
            match self.mainloop().await {
                Ok(()) => {}
                Err(SchedulerError::Offline) => {
                    info!("stream ended");
                    return;
                }
                Err(error) => {
                    warn!(%error, "scheduler reset");
                    self.reset();
                }
            }
        }
    }

    fn reset(&mut self) {
        self.segment_tail = 0;
        self.prefetch_tail = 0;
        self.last_segment_dur = Duration::from_secs(5);
        self.init = None;
        self.segments.clear();
        self.prefetches.clear();
    }

    fn handle_new_readers(&mut self) {
        const BACKLOG: usize = crate::constants::MAX_CONSUMER_LAG - 2;

        'readers: while let Ok(Some(tx)) = self.receive.try_recv() {
            debug!("new sender");
            if let Some(init) = &self.init
                && !push(
                    &tx,
                    SegmentMessage::Init {
                        reader: init.1.clone(),
                    },
                    "new reader init",
                )
            {
                continue 'readers;
            }

            // A late joiner gets the newest part of the window, not all of it:
            // the whole backlog has to fit in the bounded queue alongside live
            // segments, or every new client would be dropped immediately.
            let skip = self.segments.len().saturating_sub(BACKLOG);
            for segment in self.segments.iter().skip(skip) {
                if !push(&tx, segment.clone().into(), "new reader backlog") {
                    continue 'readers;
                }
            }

            self.readers.push(tx);
        }
    }

    async fn mainloop(&mut self) -> Result<(), SchedulerError> {
        const MIN_EDGE_POLL: Duration = Duration::from_millis(200);
        const MAX_EDGE_POLL: Duration = Duration::from_secs(5);
        const CATCHUP_POLL: Duration = Duration::from_millis(100);

        let start = Instant::now();
        let now: u64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let span = span!(Level::INFO, "mainloop", time = now);
        let _ = &span;

        let new_segments = self.reload().await?;

        self.prefetches
            .retain_unordered(|p| p.expected_id >= self.segment_tail);

        let sleep_dur = if new_segments == 0 {
            (self.last_segment_dur / 4).clamp(MIN_EDGE_POLL, MAX_EDGE_POLL)
        } else {
            CATCHUP_POLL
        };
        self.handle_new_readers();

        if let Some(sleep_time) = sleep_dur.checked_sub(start.elapsed()) {
            compio::time::sleep(sleep_time).await;
        }

        Ok(())
    }

    async fn dispatch_main(
        &mut self,
        url: &str,
        expected_sequence: u64,
        timestamp_ms: u64,
        duration_ms: u64,
    ) -> Result<(), SchedulerError> {
        let prefetch = self
            .prefetches
            .iter()
            .position(|s| s.expected_id == expected_sequence && s.url.as_ref() == url)
            .map(|idx| self.prefetches.swap_remove(idx));

        debug!(
            sequence = expected_sequence,
            prefetched = prefetch.is_some(),
            "segment dispatched"
        );

        let duration = Duration::from_millis(duration_ms);
        let span = debug_span!(
            "main segment",
            sequence_id = expected_sequence,
            was_prefetched = prefetch.is_some(),
            duration = duration.as_secs_f64(),
        );

        let (segment, writer) = if let Some(prefetch) = prefetch {
            (
                Segment {
                    sequence_num: expected_sequence,
                    reader: prefetch.reader,
                    timestamp_ms,
                },
                None,
            )
        } else {
            let (writer, reader) = Spool::open(&self.arena, &self.wakes)?;
            (
                Segment {
                    sequence_num: expected_sequence,
                    reader,
                    timestamp_ms,
                },
                Some(writer),
            )
        };
        self.segments.push(segment.clone());

        if let Some(writer) = writer {
            self.downloaders
                .send(ToDownloader::FetchURL {
                    url: Rc::from(url),
                    writer,
                    span: span.clone(),
                })
                .await?;
        }

        self.readers.retain_unordered(|reader| {
            if reader
                .try_send(segment.clone().into())
                .is_ok_and(|sent| sent)
            {
                true // success, keep
            } else {
                let _ = reader.close(); // close it
                false
            }
        });

        self.last_segment_dur = duration;

        Ok(())
    }

    async fn dispatch_prefetch(
        &mut self,
        url: &str,
        expected_sequence: u64,
    ) -> Result<(), SchedulerError> {
        let prefetch = self
            .prefetches
            .iter()
            .find(|s| s.expected_id == expected_sequence);

        if prefetch.is_some() {
            return Err(SchedulerError::Dispatch);
        }
        debug!(sequence = expected_sequence, "prefetch dispatched");

        let (writer, reader) = Spool::open(&self.arena, &self.wakes)?;
        let span = debug_span!("prefetch segment", sequence_id = expected_sequence);
        let url = Rc::<str>::from(url);

        self.downloaders
            .send(ToDownloader::FetchURL {
                url: url.clone(),
                writer,
                span,
            })
            .await?;

        self.readers.retain_unordered(|consumer| {
            push(
                consumer,
                SegmentMessage::Prefetch {
                    expected_sequence,
                    reader: reader.clone(),
                },
                "prefetch",
            )
        });
        self.prefetches.push(PrefetchSegment {
            expected_id: expected_sequence,
            url,
            reader,
        });

        Ok(())
    }

    fn broadcast_discontinuity(&mut self) {
        self.readers.retain_unordered(|reader| {
            push(reader, SegmentMessage::Discontinuity(), "discontinuity")
        });
    }

    async fn apply_init(&mut self, uri: &str) -> Result<(), SchedulerError> {
        if let Some((old, _)) = &self.init {
            if old.as_str() != uri {
                debug!(old, uri, "init segment changed mid-stream");
            }
            return Ok(());
        }

        let span = debug_span!("init segment");
        let (writer, reader) = Spool::open(&self.arena, &self.wakes)?;
        self.downloaders
            .send(ToDownloader::FetchURL {
                url: uri.into(),
                writer,
                span,
            })
            .await?;
        self.init = Some((uri.into(), reader));

        Ok(())
    }

    async fn reload(&mut self) -> Result<u64, SchedulerError> {
        let mut response = self.client.get(&self.stream_url).await?;
        let body = response.body.read_to_end(MAX_MANIFEST).await?;
        let manifest = std::str::from_utf8(&body).map_err(|_| SchedulerError::Utf8)?;

        let original_prefetch_tail = self.prefetch_tail;
        // Dropout protection repeats the final sequence at every checkpoint, so
        // only the last annotation is a meaningful canary.
        let mut last_checkpoint = None;

        for event in Playlist::new(manifest) {
            match event? {
                Event::MediaSequence(front) => {
                    debug!("new front {front}");
                    self.segments.retain(|s| s.sequence_num >= front);
                }
                Event::Init(uri) => self.apply_init(uri).await?,
                Event::Discontinuity => self.broadcast_discontinuity(),
                Event::Checkpoint { annotated, next } => last_checkpoint = Some((annotated, next)),
                Event::Ended => {
                    info!("stream offline");
                    return Err(SchedulerError::Offline {});
                }
                Event::Media(media) => {
                    // Ads consume sequence numbers but are never downloaded.
                    if media.is_ad || media.sequence < self.segment_tail {
                        continue;
                    }

                    self.segment_tail = media.sequence + 1;
                    self.prefetch_tail = self.prefetch_tail.max(self.segment_tail);
                    self.dispatch_main(
                        media.uri,
                        media.sequence,
                        media.timestamp_ms,
                        media.duration_ms,
                    )
                    .await?;
                }
                Event::Prefetch { uri, sequence } => {
                    if sequence < self.prefetch_tail {
                        continue;
                    }

                    self.prefetch_tail = sequence + 1;
                    self.dispatch_prefetch(uri, sequence).await?;
                }
            }
        }

        if let Some((annotated, next)) = last_checkpoint
            && annotated > next
        {
            warn!(
                tls = annotated,
                sequence = next,
                "live sequence annotation ahead of positional sequence"
            );
        }

        Ok(self.prefetch_tail - original_prefetch_tail)
    }
}

/// Hand one message to a consumer, dropping it when it has gone away or fallen
/// too far behind. Bounded consumer queues are the lag policy: a full queue is a
/// client that cannot keep up with the live window (see `MAX_CONSUMER_LAG`).
fn push(reader: &Sender<SegmentMessage>, message: SegmentMessage, reason: &'static str) -> bool {
    match reader.try_send(message) {
        Ok(true) => true,
        Ok(false) => {
            warn!(
                reason,
                queued = crate::constants::MAX_CONSUMER_LAG,
                "dropping consumer: lagging behind"
            );
            let _ = reader.close();
            false
        }
        Err(error) => {
            debug!(%error, reason, "dropping consumer: channel closed");
            let _ = reader.close();
            false
        }
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;

    #[test]
    fn a_full_consumer_queue_is_dropped() {
        let (tx, rx) = kanal::bounded(1);

        assert!(push(&tx, SegmentMessage::Discontinuity(), "first"));
        // No receiver poll happened, so the queue is full: the consumer is dropped.
        assert!(!push(&tx, SegmentMessage::Discontinuity(), "second"));

        // Closing discards whatever was queued, so the consumer sees a closed
        // channel immediately.
        assert!(tx.try_send(SegmentMessage::Discontinuity()).is_err());
        assert!(rx.try_recv().is_err());
    }
}
