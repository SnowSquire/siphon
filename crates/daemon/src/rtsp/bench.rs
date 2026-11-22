//! Per-stream CPU cost of the media pipeline, measured on the mock's captured
//! segments.
//!
//! It runs exactly what one broadcast costs the client between the downloader
//! and the session queues: TS demuxing, access-unit assembly, NAL splitting,
//! H.264/AAC packetization and the per-packet RTP buffer. The pacer and the
//! fan-out are left out (they are clock- and session-bound, not per-stream
//! media work), as are the HTTP fetch and the spool copy.
//!
//! `#[ignore]`d because it loads the whole ~556 MB segment set into memory; run it
//! with:
//!
//! ```text
//! cargo test -p daemon --release -- --ignored --nocapture throughput
//! ```
//!
//! For a Linux test box, cross-compile instead of building there: zig supplies
//! the musl toolchain and linker, so the target needs no compiler at all.
//!
//! ```text
//! cargo zigbuild --release --tests -p daemon --target x86_64-unknown-linux-musl
//! scp target/x86_64-unknown-linux-musl/release/deps/daemon-* host:~/bench/
//! ssh host 'cd ~/bench && taskset -c 2 ./daemon-* throughput --ignored --nocapture'
//! ```
//!
//! Run it from a directory that holds `captures/servermock/` (the binary carries
//! the build host's paths), or point `TWITCH_BENCH_SEGMENTS` at the fixture
//! directory. The fixtures are built, never committed:
//! `pnpm -C servermock run build:fixtures`.
//!
//! `media_seconds / wall_seconds` is the number of 1x streams one core can
//! carry, i.e. the per-stream CPU share is `1 / that`.
//!
//! The number is per core; on an SMT pair run it pinned to one logical CPU to
//! keep the scheduler from moving it to a sibling mid-pass.

use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use sonic_rs::JsonContainerTrait;
use sonic_rs::JsonValueTrait;
use sonic_rs::pointer;

use crate::constants::RTP_MAX_PAYLOAD;
use crate::rtsp::Track;
use crate::rtsp::aac;
use crate::rtsp::h264;
use crate::rtsp::sdp;
use crate::rtsp::ts::SampleStore;
use crate::rtsp::ts::TsDemuxer;

/// The self-contained fixture directory: `manifest.json`, `segments.json`,
/// `chunks/`, `live/`.
///
/// A cross-compiled binary carries the *host's* `CARGO_MANIFEST_DIR`, so the
/// directory is resolved at run time: `TWITCH_BENCH_SEGMENTS` first, then the
/// checkout the binary was built in, then `./captures/servermock` (running from
/// a copy of the repo, e.g. the remote test box).
fn mock_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TWITCH_BENCH_SEGMENTS") {
        return Some(PathBuf::from(dir));
    }

    [
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../captures/servermock"),
        PathBuf::from("captures/servermock"),
    ]
    .into_iter()
    .find(|dir| dir.join("segments.json").is_file())
}

/// `(filename, length_ms)` in playlist order, from the fixture directory's own
/// `segments.json`.
fn segment_list(dir: &Path) -> Option<Vec<(String, f64)>> {
    let json = fs::read_to_string(dir.join("segments.json")).ok()?;
    let value = sonic_rs::from_str::<sonic_rs::Value>(&json).ok()?;

    let mut list = Vec::new();
    for entry in value.as_array()? {
        list.push((
            entry.pointer(pointer!["filename"])?.as_str()?.to_owned(),
            entry.pointer(pointer!["length_ms"])?.as_f64()?,
        ));
    }

    (!list.is_empty()).then_some(list)
}

#[derive(Default)]
struct Totals {
    access_units: u64,
    audio_frames: u64,
    packets: u64,
    payload: u64,
    bytes: u64,
}

struct Timing {
    demux: Duration,
    packetize: Duration,
    totals: Totals,
}

/// The per-broadcast half of the media engine: one demuxer, one sample buffer
/// and the two packetizers, reused across segments like `MediaEngine` does.
struct Pipeline {
    demuxer: TsDemuxer,
    samples: SampleStore,
    video: Track,
    audio: Track,
    scratch: Vec<u8>,
    totals: Totals,
}

impl Pipeline {
    fn new() -> Self {
        Self {
            demuxer: TsDemuxer::new(),
            samples: SampleStore::new(),
            video: Track::new(sdp::VIDEO_PAYLOAD_TYPE, sdp::VIDEO_CLOCK_RATE, 0x1234_5678),
            audio: Track::new(sdp::AUDIO_PAYLOAD_TYPE, 48_000, 0x1234_5679),
            scratch: Vec::new(),
            totals: Totals::default(),
        }
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn segment(&mut self, bytes: &[u8], demux: &mut Duration, packetize: &mut Duration) {
        self.totals.bytes += bytes.len() as u64;

        let start = Instant::now();
        self.demuxer.feed(bytes, &mut self.samples);
        self.demuxer.finish_segment(&mut self.samples);
        *demux += start.elapsed();

        let start = Instant::now();
        for index in 0..self.samples.samples().len() {
            let sample = self.samples.samples()[index];
            if sample.is_video() {
                self.totals.access_units += 1;
                // The media base would come from the first sample; any fixed
                // value keeps the arithmetic identical.
                let timestamp = black_box(sample.pts()) as u32;
                let media = black_box(sample.dts());
                let totals = &mut self.totals;
                h264::packetize(
                    self.samples.data(sample),
                    self.samples.nals(sample),
                    RTP_MAX_PAYLOAD,
                    &mut self.scratch,
                    |payload, marker| {
                        let packet = self.video.packet(timestamp, media, marker, payload);
                        totals.packets += 1;
                        totals.payload += payload.len() as u64;
                        black_box(&packet);
                    },
                );
            } else {
                self.totals.audio_frames += 1;
                let payload = aac::packetize(self.samples.data(sample));
                let packet = self.audio.packet(
                    black_box(sample.pts()) as u32,
                    black_box(sample.pts()),
                    true,
                    &payload,
                );
                self.totals.packets += 1;
                self.totals.payload += payload.len() as u64;
                black_box(&packet);
            }
        }
        self.samples.clear();
        *packetize += start.elapsed();
    }
}

fn pass(segments: &[Vec<u8>]) -> Timing {
    let mut pipeline = Pipeline::new();
    let mut demux = Duration::ZERO;
    let mut packetize = Duration::ZERO;

    for bytes in segments {
        pipeline.segment(bytes, &mut demux, &mut packetize);
    }

    Timing {
        demux,
        packetize,
        totals: pipeline.totals,
    }
}

#[test]
#[ignore = "reads the whole fixture set (~3 GB) into memory"]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn throughput() {
    let Some(dir) = mock_dir() else {
        eprintln!("throughput: no captures/servermock/segments.json, skipping");
        return;
    };
    let Some(list) = segment_list(&dir) else {
        eprintln!("throughput: unreadable {}", dir.display());
        return;
    };
    if !dir.join(&list[0].0).exists() {
        eprintln!(
            "throughput: segments missing in {} (build them with `pnpm -C servermock run build:fixtures`), skipping",
            dir.display()
        );
        return;
    }

    let media_ms: f64 = list.iter().map(|(_, length)| *length).sum();
    let media = Duration::from_secs_f64(media_ms / 1000.0);

    // Everything is read up front: the per-stream CPU cost is the media work,
    // and a download is somebody else's syscall. Keeping the bytes resident
    // also makes repeated passes comparable.
    let segments: Vec<Vec<u8>> = list
        .iter()
        .map(|(filename, _)| fs::read(dir.join(filename)).expect("segment file"))
        .collect();

    // Warm the allocator so the measured pass is steady.
    let warm = pass(&segments);
    black_box(&warm.totals);

    let start = Instant::now();
    let timing = pass(&segments);
    let wall = start.elapsed();

    let realtime = media.as_secs_f64() / wall.as_secs_f64();
    let demux_share = timing.demux.as_secs_f64() / wall.as_secs_f64() * 100.0;
    let packetize_share = timing.packetize.as_secs_f64() / wall.as_secs_f64() * 100.0;
    let totals = &timing.totals;

    eprintln!(
        "throughput: {} segments, {:.1} s of 1080p60 media ({:.1} MiB)",
        list.len(),
        media.as_secs_f64(),
        totals.bytes as f64 / (1024.0 * 1024.0),
    );
    eprintln!(
        "throughput: media {:.1} s in wall {:.3} s -> {realtime:.1}x realtime on one core",
        media.as_secs_f64(),
        wall.as_secs_f64(),
    );
    eprintln!(
        "throughput: {:.2}% of one core per stream, i.e. {:.0} concurrent live streams per core",
        100.0 / realtime,
        realtime,
    );
    eprintln!(
        "throughput: demux {:.3} s ({demux_share:.0}%), packetize+rtp {:.3} s ({packetize_share:.0}%)",
        timing.demux.as_secs_f64(),
        timing.packetize.as_secs_f64(),
    );
    eprintln!(
        "throughput: {} AUs, {} AAC frames, {} RTP packets ({:.1}/AU), {:.2} MB/s payload",
        totals.access_units,
        totals.audio_frames,
        totals.packets,
        totals.packets as f64 / totals.access_units.max(1) as f64,
        totals.payload as f64 / wall.as_secs_f64() / 1_000_000.0,
    );
}
