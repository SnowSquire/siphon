use std::thread::JoinHandle;
use std::thread::{self};
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use kanal::Receiver;
use kanal::Sender;
use thiserror::Error;
use tracing::Level;
use tracing::debug;
use tracing::debug_span;
use tracing::error;
use tracing::info;
use tracing::span;
use tracing::warn;
use ureq::Agent;

use crate::downloader::ToDownloader;
use crate::playlist::Event;
use crate::playlist::ManifestParseError;
use crate::playlist::Playlist;
use crate::segment::Reader;
use crate::segment::Stream;
use crate::utils::RetainUnordered;
use crate::utils::httpify_url;

struct PrefetchSegment {
    expected_id: u64,
    url: Box<str>,
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
    #[error("ureq threw an error")]
    Ureq(#[from] ureq::Error),
    #[error("failed to parse manifest {0}")]
    Parse(#[from] ManifestParseError),

    #[error("dispatch error")]
    Dispatch,
    #[error("send error")]
    Send(#[from] kanal::SendError),
}

pub fn spawn_scheduler(
    agent: Agent,
    track_url: Box<str>,
    tx: Sender<ToDownloader>,
    srx: Receiver<Sender<SegmentMessage>>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("scheduler".to_owned())
        .spawn(move || StreamScheduler::new(agent, track_url, tx, srx).main())
        .expect("failed to spawn scheduler")
}

/// copied and modified from twitch-hls-client directly
struct StreamScheduler {
    agent: Agent,
    stream_url: Box<str>,
    downloaders: Sender<ToDownloader>,
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
        agent: Agent,
        stream_url: Box<str>,
        downloaders: Sender<ToDownloader>,
        receive: Receiver<Sender<SegmentMessage>>,
    ) -> Self {
        Self {
            agent,
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

    pub fn main(mut self) {
        let span = span!(Level::INFO, "scheduler");
        let _guard = span.enter();

        info!("{}", self.stream_url.to_string());
        loop {
            match self.mainloop() {
                Ok(()) => {}
                Err(err) => match err {
                    SchedulerError::Offline => {
                        info!("stream ended");
                        return;
                    }
                    _ => self.reset(),
                },
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
        'readers: while let Ok(Some(tx)) = self.receive.try_recv() {
            debug!("new sender");
            if let Some(init) = &self.init {
                if tx
                    .try_send(SegmentMessage::Init {
                        reader: init.1.clone(),
                    })
                    .is_ok_and(|v| v)
                {
                } else {
                    error!(url = init.0, "failed to send init to new reader");
                    let _ = tx.close();
                    continue 'readers;
                }
            }

            // self.segments.sort_by_key(|f| f.sequence_num);
            for segment in &self.segments {
                if tx.try_send(segment.clone().into()).is_ok_and(|v| v) {
                } else {
                    error!(
                        segment_id = segment.sequence_num,
                        "failed to send init to new reader"
                    );
                    let _ = tx.close();
                    continue 'readers;
                }
            }

            self.readers.push(tx);
        }
    }

    fn mainloop(&mut self) -> Result<(), SchedulerError> {
        const MIN_EDGE_POLL: Duration = Duration::from_millis(200);
        const MAX_EDGE_POLL: Duration = Duration::from_secs(5);
        const CATCHUP_POLL: Duration = Duration::from_millis(100);

        let start = Instant::now();
        let now: u64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let span = span!(Level::INFO, "mainloop", time = now);
        let _guard = span.enter();

        let new_segments = self.reload()?;

        self.prefetches
            .retain_unordered(|p| p.expected_id >= self.segment_tail);

        let sleep_dur = if new_segments == 0 {
            (self.last_segment_dur / 4).clamp(MIN_EDGE_POLL, MAX_EDGE_POLL)
        } else {
            CATCHUP_POLL
        };
        self.handle_new_readers();

        if let Some(sleep_time) = sleep_dur.checked_sub(start.elapsed()) {
            thread::sleep(sleep_time);
        }

        Ok(())
    }

    fn dispatch_main(
        &mut self,
        url: &str,
        expected_sequence: u64,
        timestamp_ms: u64,
        duration_ms: u64,
    ) {
        let prefetch = self
            .prefetches
            .iter()
            .position(|s| s.expected_id == expected_sequence && s.url.as_ref() == url)
            .map(|idx| self.prefetches.swap_remove(idx));

        let duration = Duration::from_millis(duration_ms);
        let span = debug_span!(
            "main segment",
            sequence_id = expected_sequence,
            was_prefetched = prefetch.is_some(),
            duration = duration.as_secs_f64(),
        );

        let _guard = span.enter();
        debug!("new main segment");

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
            let (writer, reader) = Stream::new_stream();
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
                    url: httpify_url(url),
                    writer,

                    span: span.clone(),
                })
                .unwrap(); // if this send fails i genuinely have no idea how to handle it just leave it
        }

        self.readers.retain_unordered(|reader| {
            if reader.try_send(segment.clone().into()).is_ok_and(|v| v) {
                true // success, keep
            } else {
                let _ = reader.close(); // close it
                false
            }
        });

        self.last_segment_dur = duration;
    }

    fn dispatch_prefetch(&mut self, url: &str, expected_sequence: u64) -> Result<(), ()> {
        let prefetch = self
            .prefetches
            .iter()
            .find(|s| s.expected_id == expected_sequence);

        if prefetch.is_some() {
            return Err(());
        }
        let span = debug_span!("prefetch segment", sequence_id = expected_sequence);

        let (writer, reader) = Stream::new_stream();
        let url = httpify_url(url);
        self.downloaders
            .send(ToDownloader::FetchURL {
                url: url.clone(),
                writer,

                span,
            })
            .unwrap();

        self.readers.retain_unordered(|segmentreader| {
            if segmentreader
                .try_send(SegmentMessage::Prefetch {
                    expected_sequence,
                    reader: reader.clone(),
                })
                .is_ok_and(|v| v)
            {
                true
            } else {
                let _ = segmentreader.close();
                false
            }
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
            if reader
                .try_send(SegmentMessage::Discontinuity())
                .is_ok_and(|v| v)
            {
                true // success, keep
            } else {
                let _ = reader.close(); // close it
                false
            }
        });
    }

    fn apply_init(&mut self, uri: &str) -> Result<(), SchedulerError> {
        if let Some((old, _)) = &self.init {
            if old.as_str() != uri {
                debug!(old, uri, "init segment changed mid-stream");
            }
            return Ok(());
        }

        let span = debug_span!("init segment");
        let (writer, reader) = Stream::new_stream();
        self.downloaders.send(ToDownloader::FetchURL {
            url: uri.into(),
            writer,
            span,
        })?;
        self.init = Some((uri.into(), reader));

        Ok(())
    }

    fn reload(&mut self) -> Result<u64, SchedulerError> {
        let span = span!(Level::INFO, "manifest");
        let _guard = span.enter();

        let manifest = self
            .agent
            .get(self.stream_url.as_ref())
            .call()?
            .into_body()
            .read_to_string()?;

        let original_prefetch_tail = self.prefetch_tail;
        // Dropout protection repeats the final sequence at every checkpoint, so
        // only the last annotation is a meaningful canary.
        let mut last_checkpoint = None;

        for event in Playlist::new(&manifest) {
            match event? {
                Event::MediaSequence(front) => {
                    debug!("new front {front}");
                    self.segments.retain(|s| s.sequence_num >= front);
                }
                Event::Init(uri) => self.apply_init(uri)?,
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
                    );
                }
                Event::Prefetch { uri, sequence } => {
                    if sequence < self.prefetch_tail {
                        continue;
                    }

                    self.prefetch_tail = sequence + 1;
                    self.dispatch_prefetch(uri, sequence)
                        .map_err(|()| SchedulerError::Dispatch)?;
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
