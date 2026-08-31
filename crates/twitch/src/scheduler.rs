use std::{
    num::{ParseFloatError, ParseIntError},
    ptr, thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use kanal::{Receiver, Sender};
use thiserror::Error;
use tracing::{Level, debug, debug_span, error, info, span};
use ureq::Agent;

use crate::{
    downloader::ToDownloader,
    scheduler::ManifestParseError::Split,
    segment::{SegmentStream, reader::SegmentReader},
};

struct PrefetchSegment {
    expected_id: u64,
    url: Box<str>,
    reader: SegmentReader,
}
#[derive(Clone, Debug)]
pub struct Segment {
    pub timestamp_ms: u64,
    pub sequence_num: u64,
    pub reader: SegmentReader,
}

#[derive(Debug)]
pub enum SegmentMessage {
    Segment(Segment),
    Prefetch {
        expected_sequence: u64,
        reader: SegmentReader,
    },
    Discontinuity(),
    Init {
        reader: SegmentReader,
    },
}

impl From<Segment> for SegmentMessage {
    fn from(value: Segment) -> Self {
        Self::Segment(value)
    }
}

#[derive(Error, Debug)]
enum ManifestParseError {
    #[error("invalid integer: {0}")]
    Int(#[from] ParseIntError),
    #[error("invalid float: {0}")]
    Float(#[from] ParseFloatError),
    #[error("unable to split as expected")]
    Split(),
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
    #[error("discontinuity error")]
    Discontinuity,
    #[error("send error")]
    Send(#[from] kanal::SendError),
    #[error("writer error")]
    Write,
}

/// copied and modified from twitch-hls-client directly
pub struct StreamScheduler {
    agent: Agent,
    stream_url: Box<str>,
    downloaders: Sender<ToDownloader>,
    receive: Receiver<Sender<SegmentMessage>>,

    /// hls fmp4 init header, doesn't reqire a discontinuity
    init: Option<(String, SegmentReader)>,

    segments: Vec<Segment>,
    prefetches: Vec<PrefetchSegment>,

    readers: Vec<Sender<SegmentMessage>>,

    segment_tail: u64,
    prefetch_tail: u64,
    last_segment_dur: Duration,
}

impl StreamScheduler {
    pub fn new(
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

            init: Option::None,

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
                Ok(_) => {}
                Err(err) => match err {
                    SchedulerError::Offline => {
                        info!("stream ended");
                        return;
                    }
                    _ => self.reset(),
                },
            };
        }
    }

    fn reset(&mut self) {
        self.segment_tail = 0;
        self.prefetch_tail = 0;
        self.last_segment_dur = Duration::from_secs(5);
        self.init = Option::None;
        self.segments.clear();
        self.prefetches.clear();
    }

    fn handle_new_readers(&mut self) {
        'readers: while let Ok(Some(tx)) = self.receive.try_recv() {
            debug!("new sender");
            if let Some(init) = &self.init {
                match tx.try_send(SegmentMessage::Init {
                    reader: init.1.clone(),
                }) {
                    Ok(true) => {}
                    Ok(false) | Err(_) => {
                        error!(url = init.0, "failed to send init to new reader");
                        let _ = tx.close();
                        continue 'readers;
                    }
                }
            }

            // self.segments.sort_by_key(|f| f.sequence_num);
            for segment in &self.segments {
                match tx.try_send(segment.clone().into()) {
                    Ok(true) => {}
                    Ok(false) | Err(_) => {
                        error!(
                            segment_id = segment.sequence_num,
                            "failed to send init to new reader"
                        );
                        let _ = tx.close();
                        continue 'readers;
                    }
                }
            }

            self.readers.push(tx);
        }
    }
    fn mainloop(&mut self) -> Result<(), SchedulerError> {
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

        const MIN_EDGE_POLL: Duration = Duration::from_millis(200);
        const MAX_EDGE_POLL: Duration = Duration::from_secs(5);
        const CATCHUP_POLL: Duration = Duration::from_millis(100);

        let sleep_dur = match new_segments == 0 {
            true => (self.last_segment_dur / 4).clamp(MIN_EDGE_POLL, MAX_EDGE_POLL),
            false => CATCHUP_POLL,
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
        infoline: &str,
        timestamp_ms: u64,
    ) -> Result<Duration, Box<dyn std::error::Error>> {
        let prefetch = self
            .prefetches
            .iter()
            .position(|s| s.expected_id == expected_sequence && s.url.as_ref() == url)
            .map(|idx| self.prefetches.swap_remove(idx));

        let is_ad = infoline.contains('|');

        let duration =
            Duration::try_from_secs_f64(infoline.split_once(',').map(|d| d.0).unwrap().parse()?)?;
        let span = debug_span!(
            "main segment",
            sequence_id = expected_sequence,
            was_prefetched = prefetch.is_some(),
            %is_ad,
            duration = duration.as_secs_f64(),
        );

        let _guard = span.enter();
        debug!("new main segment");
        if is_ad {
            return Ok(duration);
        };

        match prefetch {
            Some(prefetch) => {
                let segment = Segment {
                    sequence_num: expected_sequence,
                    reader: prefetch.reader,
                    timestamp_ms,
                };
                self.segments.push(segment.clone());

                self.readers.retain_unordered(|reader| {
                    match reader.try_send(segment.clone().into()) {
                        Ok(true) => true, // success, keep
                        Ok(false) | Err(_) => {
                            let _ = reader.close(); // close it
                            false
                        }
                    }
                });
            }
            None => {
                let (writer, segmentreader) = SegmentStream::new();
                let segment = Segment {
                    sequence_num: expected_sequence,
                    reader: segmentreader,
                    timestamp_ms,
                };
                self.segments.push(segment.clone());

                self.downloaders
                    .send(ToDownloader::FetchURL {
                        url: as_http(url),
                        writer,

                        span: span.clone(),
                    })
                    .unwrap(); // if this send fails i genuinely have no idea how to handle it just leave it
                self.readers.retain_unordered(|reader| {
                    match reader.try_send(segment.clone().into()) {
                        Ok(true) => true, // success, keep
                        Ok(false) | Err(_) => {
                            let _ = reader.close(); // close it
                            false
                        }
                    }
                });
            }
        }

        self.last_segment_dur = duration;
        Ok(duration)
    }

    fn dispatch_prefetch(&mut self, url: &str, expected_sequence: u64) -> Result<(), ()> {
        let prefetch = self
            .prefetches
            .iter()
            .find(|s| s.expected_id == expected_sequence);

        if prefetch.is_some() {
            return Err(());
        };
        let span = debug_span!("prefetch segment", sequence_id = expected_sequence);

        let (writer, reader) = SegmentStream::new();
        let url = as_http(url);
        self.downloaders
            .send(ToDownloader::FetchURL {
                url: url.clone(),
                writer,

                span: span,
            })
            .unwrap();

        self.readers.retain_unordered(|segmentreader| {
            match segmentreader.try_send(SegmentMessage::Prefetch {
                expected_sequence,
                reader: reader.clone(),
            }) {
                Ok(true) => true,
                Ok(false) | Err(_) => {
                    let _ = segmentreader.close();
                    false
                }
            }
        });
        self.prefetches.push(PrefetchSegment {
            expected_id: expected_sequence,
            url: url,
            reader: reader,
        });

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

        if manifest
            .lines()
            .next()
            .is_some_and(|l| l.trim() == "#EXT-X-ENDLIST")
        {
            info!("stream offline");
            return Err(SchedulerError::Offline {});
        }

        let original_prefetch_tail = self.prefetch_tail;
        let mut sequence_number = 0;

        let mut lines = manifest.lines();

        let mut segment_time_ms = 0;
        while let Some(line) = lines.next() {
            let Some((tag, value)) = line.split_once(':') else {
                continue;
            };

            match tag {
                "#EXT-X-TWITCH-ELAPSED-SECS" => {
                    segment_time_ms = parse_millis(value).ok_or(SchedulerError::Parse(Split()))?;
                }
                "#EXT-X-MEDIA-SEQUENCE" => {
                    let front: u64 = value.parse().map_err(ManifestParseError::Int)?;

                    sequence_number = front;
                    debug!("new front {}", sequence_number);

                    self.segments.retain(|s| s.sequence_num >= front);
                }
                "#EXT-X-DISCONTINUITY" => return Err(SchedulerError::Discontinuity),
                "#EXT-X-MAP" => {
                    //#EXT-X-MAP:URI="main.mp4",BYTERANGE="560@0", this code doesn't handle having multiple values after the URI, should probably write a better m3u8 parser
                    let new_init = value
                        .split_once('=')
                        .ok_or_else(ManifestParseError::Split)?
                        .1
                        .trim_matches('"');

                    match &self.init {
                        Some(old_init) => {
                            if new_init != old_init.0 {
                                // TODO: log
                            }
                        }

                        None => {
                            let span = debug_span!("init segment");
                            let (writer, reader) = SegmentStream::new();
                            self.downloaders.send(ToDownloader::FetchURL {
                                url: new_init.into(),
                                writer: writer,
                                span: span.clone(),
                            })?;
                            self.init = Some((new_init.into(), reader))
                        }
                    }
                }
                "#EXTINF" => {
                    if sequence_number >= self.segment_tail
                        && let Some(url) = lines.next()
                    {
                        self.segment_tail = sequence_number + 1;
                        self.prefetch_tail = self.prefetch_tail.max(self.segment_tail);

                        self.dispatch_main(url, sequence_number, value, segment_time_ms)
                            .map_err(|_| SchedulerError::Dispatch)?;
                    }
                    sequence_number += 1;
                    let segment_len = parse_millis(
                        value
                            .split_once(',')
                            .ok_or(SchedulerError::Parse(ManifestParseError::Split()))?
                            .0,
                    )
                    .ok_or(SchedulerError::Parse(Split()))?;
                    segment_time_ms += segment_len
                }
                "#EXT-X-TWITCH-PREFETCH" | "#EXT-X-PREFETCH" => {
                    if sequence_number >= self.prefetch_tail {
                        self.prefetch_tail = sequence_number + 1;

                        self.dispatch_prefetch(value, sequence_number)
                            .map_err(|_| SchedulerError::Dispatch)?;
                    }
                    sequence_number += 1;
                }
                _ => (),
            }
        }

        // info!(new_segments = self.prefetch_tail - original_prefetch_tail, %mains, %prefetches);
        Ok(self.prefetch_tail - original_prefetch_tail)
    }
}

pub trait RetainUnordered<T> {
    /// Keeps elements for which `f` returns `true`; order of survivors is not preserved.
    fn retain_unordered<F>(&mut self, f: F)
    where
        F: FnMut(&T) -> bool;
}

impl<T> RetainUnordered<T> for Vec<T> {
    fn retain_unordered<F>(&mut self, mut f: F)
    where
        F: FnMut(&T) -> bool,
    {
        let mut i = 0;
        let mut n = self.len();
        while i < n {
            if f(&self[i]) {
                i += 1;
            } else {
                n -= 1;
                self.swap(i, n);
            }
        }
        self.truncate(n);
    }
}

fn as_http(url: &str) -> Box<str> {
    if let Some(rest) = url.strip_prefix("https://") {
        const HTTP_PREFIX: &[u8; 7] = b"http://";
        let mut buffer = Box::<[u8]>::new_uninit_slice(url.len() - 1);

        let dest = buffer.as_mut_ptr().cast::<u8>();
        unsafe {
            ptr::copy_nonoverlapping(HTTP_PREFIX.as_ptr(), dest, HTTP_PREFIX.len());
            ptr::copy_nonoverlapping(rest.as_ptr(), dest.add(HTTP_PREFIX.len()), rest.len());
            std::mem::transmute::<Box<[u8]>, Box<str>>(buffer.assume_init())
        }
    } else {
        Box::from(url)
    }
}

fn parse_millis(s: &str) -> Option<u64> {
    let (secs, frac) = s.split_once('.').unwrap_or((s, "0"));
    let frac = if frac.is_empty() { "0" } else { frac };

    let digits = frac.len().min(3) as u32;
    let frac_ms: u64 = frac.get(..digits as usize)?.parse().ok()?;

    Some(secs.parse::<u64>().ok()? * 1000 + frac_ms * 10u64.pow(3 - digits))
}
