//! Minimal RTSP server with ES-over-RTP output.
//!
//! One TS consumer demuxes the live segment stream, packetizes H.264 and AAC and
//! fans the RTP packets out to connected sessions. RTSP is only control: SDP from
//! the discovered codec configuration, one UDP or interleaved-TCP transport per
//! track, and RTCP sender reports for the NTP/RTP mapping.

mod aac;
#[cfg(test)]
mod bench;
mod h264;
mod sdp;
mod ts;
pub mod vod;

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::future::poll_fn;
use std::io::ErrorKind;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::rc::Rc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use compio::buf::IoBuf;
use compio::io::AsyncRead;
use compio::io::AsyncWriteExt;
use compio::net::TcpListener;
use compio::net::TcpStream;
use compio::net::UdpSocket;
use futures_util::future::LocalBoxFuture;
use kanal::AsyncReceiver;
use socket2::Domain;
use socket2::Protocol;
use socket2::Socket;
use socket2::Type;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::constants::DEFAULT_RTSP_PORT;
use crate::constants::MAX_CONSUMER_LAG;
use crate::constants::RTP_MAX_PAYLOAD;
use crate::http::Client;
use crate::rtsp::aac::AudioConfig;
use crate::rtsp::h264::H264Config;
use crate::rtsp::ts::Sample;
use crate::rtsp::ts::TsDemuxer;
use crate::rtsp::vod::Dvr;
use crate::scheduler::ConsumerRegistration;
use crate::scheduler::Segment;
use crate::scheduler::SegmentMessage;
use crate::spool::ConsumerWake;
use crate::spool::WakeSet;

const TRACK_VIDEO: u8 = 0;
const TRACK_AUDIO: u8 = 1;
/// The live-edge beacon track. It carries no pictures: its RTP timestamps are
/// the broadcast NPT of the current live edge, so the player's demuxer (which
/// grows `duration` from the highest packet timestamp) tracks the broadcast
/// length after DESCRIBE.
const TRACK_TIMELINE: u8 = 2;
const TRACK_COUNT: usize = 3;

/// The beacon's own SSRC, independent of the A/V tracks.
const TIMELINE_SSRC: u32 = 0x5A5A_0003;
/// AAC-LC silent frame (1024 samples, 48 kHz stereo), 6 raw bytes. The beacon
/// sends one per second; it is never audible because the real audio track is
/// the default, but a normal AAC RTP packet is what makes the demuxer emit a
/// packet and count its timestamp.
const TIMELINE_AU: &[u8] = &[0x21, 0x10, 0x04, 0x60, 0x8c, 0x1c];
/// The beacon track's RTP clock (the SDP advertises 48 kHz).
const TIMELINE_CLOCK: u64 = 48_000;

/// Outbound-queue slots kept free for the RTCP sender reports that are queued
/// between samples. They are not part of `wait_out_capacity`'s packet count, so
/// without the headroom a full queue silently drops the report.
const RTCP_HEADROOM: usize = 64;

/// One RTP/RTCP packet, shared by every session's queue.
///
/// The packetizer builds it once per access unit; each session then holds a
/// refcount instead of a copy. Cloning a `Vec` per session per packet was the
/// hottest allocation in the RTSP output (~20k/s at 16x with two viewers).
type RtpPacket = Rc<Vec<u8>>;

/// RTP timestamps wrap at 2^32, and PTS wraps at 2^33; deltas are taken modulo
/// the PTS wrap so both wrap cleanly.
const PTS_WRAP: i64 = 1 << 33;

const NTP_UNIX_OFFSET: u64 = 2_208_988_800;
const SDES_CNAME: &[u8] = b"twitch-hls-client2";

/// Identity of one RTSP presentation: a channel login and an optional
/// `STABLE-VARIANT-ID`. `None` means the first variant, like `play`'s default.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StreamKey {
    /// Twitch login, lowercased.
    pub login: String,
    /// `STABLE-VARIANT-ID` (`chunked`, `720p60`, `audio_only`, ...).
    pub quality: Option<String>,
}

/// Why a DESCRIBE could not be served. `status` is the RTSP status line.
#[derive(Clone, Copy, Debug)]
pub struct ResolveError {
    pub status: &'static str,
}

impl ResolveError {
    pub const BAD_REQUEST: Self = Self {
        status: "400 Bad Request",
    };
    pub const NOT_FOUND: Self = Self {
        status: "404 Not Found",
    };
    pub const UNAVAILABLE: Self = Self {
        status: "503 Service Unavailable",
    };
}

/// Turns a request URI into the stream its control connection talks to.
///
/// `play` serves one pre-built stream and ignores the path; `daemon` resolves
/// `/<channel>/<quality>/` through its registry.
pub trait StreamResolver {
    fn resolve<'a>(&'a self, uri: &'a str) -> LocalBoxFuture<'a, Result<Rc<Stream>, ResolveError>>;

    /// The last control connection for `stream` disconnected.
    fn release(&self, stream: &Rc<Stream>);
}

/// Split `/channel/quality/trackID=0` out of an RTSP request URI.
///
/// Path segments, never query parameters: ffmpeg appends a relative
/// `a=control:trackID=0` by string concatenation, so a query would swallow the
/// track fragment.
#[must_use]
pub fn parse_stream_key(uri: &str) -> Option<StreamKey> {
    // Both `rtsp://host/path` and an absolute path (`/path`) are accepted.
    let path = uri.split_once("://").map_or(uri, |(_, rest)| {
        rest.find('/').map_or("", |slash| &rest[slash..])
    });
    let mut segments = path.trim_start_matches('/').split('/');

    let login = segments.next()?;
    if login.is_empty()
        || login.len() > 64
        || !login.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    let quality = match segments.next() {
        Some(segment) if !segment.is_empty() && !segment.starts_with("trackID=") => {
            Some(segment.to_owned())
        }
        _ => None,
    };

    Some(StreamKey {
        login: login.to_ascii_lowercase(),
        quality,
    })
}

/// Signed PTS delta from a track epoch, handling the 33-bit wrap.
///
/// A small negative delta is a real reordering/offset, not a wrap; clamping it
/// to zero makes several frames share a timestamp. Only a delta near ±2^33 is
/// treated as the wrap.
const fn media_delta(pts: i64, base: i64) -> i64 {
    let delta = pts - base;
    if delta < -(PTS_WRAP / 2) {
        delta + PTS_WRAP
    } else if delta >= PTS_WRAP / 2 {
        delta - PTS_WRAP
    } else {
        delta
    }
}

/// How far ahead of the wall clock a sample may be scheduled before the clock is
/// re-anchored. Covers a segment that arrives early while a late segment still
/// catches up.
const MAX_LEAD: Duration = Duration::from_millis(300);

/// How far behind the wall clock playback may fall before the pacer stops
/// sleeping and just sends.
const CATCHUP: Duration = Duration::from_millis(500);

struct Track {
    payload_type: u8,
    clock_rate: u32,
    ssrc: u32,
    next_seq: u16,
    /// RTP timestamp of the last packet actually sent. Sender reports must be
    /// anchored on this exact value, never on a separately computed playhead:
    /// the two clocks only line up when the SR describes a packet that was
    /// really sent.
    last_packet_rtp: u32,
    /// Presentation time of the last packet on the shared 90 kHz PES epoch.
    last_media_90k: i64,
    packet_count: u32,
    octet_count: u32,
}

impl Track {
    fn new(payload_type: u8, clock_rate: u32, ssrc: u32) -> Self {
        Self {
            payload_type,
            clock_rate,
            ssrc,
            next_seq: u16::try_from(ssrc & 0x7FFF).expect("16-bit mask"),
            last_packet_rtp: 0,
            last_media_90k: 0,
            packet_count: 0,
            octet_count: 0,
        }
    }

    /// Build one RTP packet around `payload`, consuming a sequence number.
    ///
    /// Returned shared: the fan-out hands the same buffer to every session's
    /// queue, so the packet is allocated once per access unit and not once per
    /// session.
    fn packet(
        &mut self,
        timestamp: u32,
        media_90k: i64,
        marker: bool,
        payload: &[u8],
    ) -> RtpPacket {
        let mut packet = Vec::with_capacity(12 + payload.len());
        packet.push(0x80);
        packet.push(if marker {
            0x80 | self.payload_type
        } else {
            self.payload_type
        });
        packet.extend_from_slice(&self.next_seq.to_be_bytes());
        packet.extend_from_slice(&timestamp.to_be_bytes());
        packet.extend_from_slice(&self.ssrc.to_be_bytes());
        packet.extend_from_slice(payload);

        self.next_seq = self.next_seq.wrapping_add(1);
        self.last_packet_rtp = timestamp;
        self.last_media_90k = media_90k;
        self.packet_count = self.packet_count.wrapping_add(1);

        self.octet_count = self
            .octet_count
            .wrapping_add(u32::try_from(payload.len()).unwrap_or(u32::MAX));
        Rc::new(packet)
    }

    /// Forget the per-epoch state. Sequence numbers restart at 1 because the
    /// receiver resets its own tracking on a seek and expects the next packet
    /// after the gap to resynchronise.
    /// Forget the per-epoch timestamp state. The RTP **sequence continues**:
    /// ffmpeg resets its `s->seq` counter on a seek but keeps the RTP
    /// statistics (`max_seq`) from before it. Resetting the sequence to 1
    /// therefore looks like a 64k jump, is rejected, and produces the
    /// `bad cseq 0001 expected=0001` line before resynchronising on the second
    /// packet. Continuing gives `max_seq + 1`, which is accepted immediately.
    const fn reset_mapping(&mut self) {
        self.last_packet_rtp = 0;
        self.last_media_90k = 0;
    }

    /// RTCP sender report plus an SDES CNAME item.
    ///
    /// The NTP value is derived from the *media* clock (`origin` plus the
    /// packet's presentation time), not from the wall clock at send time. VOD is
    /// sent ahead of real time, and ffmpeg maps packet timestamps through the
    /// last SR: a wall-clock anchor would make every report pull the timeline
    /// backwards when the sender is buffering ahead of the player. A media-clock
    /// anchor makes the mapping identical between reports, so it introduces no
    /// jumps at all, and indexing it from the shared PES base keeps the two
    /// tracks on one timeline.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn sender_report(&self, origin: u64) -> Option<Vec<u8>> {
        if self.packet_count == 0 {
            return None;
        }

        let rtp = self.last_packet_rtp;
        let ntp = origin.wrapping_add(media_to_ntp(self.last_media_90k));

        let mut packet = Vec::with_capacity(44);
        // Sender report: V=2, RC=0, PT=200, length 6 (28 bytes).
        packet.extend_from_slice(&[0x80, 200, 0x00, 0x06]);
        packet.extend_from_slice(&self.ssrc.to_be_bytes());
        let ntp_high = u32::try_from(ntp >> 32).expect("NTP seconds fit in 32 bits");
        packet.extend_from_slice(&ntp_high.to_be_bytes());
        packet.extend_from_slice(&(ntp as u32).to_be_bytes());
        packet.extend_from_slice(&rtp.to_be_bytes());
        packet.extend_from_slice(&self.packet_count.to_be_bytes());
        packet.extend_from_slice(&self.octet_count.to_be_bytes());

        // SDES compound item: V=2, one item, PT=202. The RFC length field is
        // the packet size in 32-bit words minus one; getting it wrong makes the
        // receiver parse the CNAME as another RTCP packet.
        let sdes_body = 4 + 2 + SDES_CNAME.len(); // SSRC + item type/length + CNAME
        let sdes_total = 4 + sdes_body.next_multiple_of(4);
        let sdes_length = u16::try_from(sdes_total / 4 - 1).expect("small SDES");
        packet.extend_from_slice(&[0x81, 202]);
        packet.extend_from_slice(&sdes_length.to_be_bytes());
        packet.extend_from_slice(&self.ssrc.to_be_bytes());
        packet.push(1); // CNAME item
        packet.push(u8::try_from(SDES_CNAME.len()).expect("cname fits in u8"));
        packet.extend_from_slice(SDES_CNAME);
        // Pad the SDES packet to a 32-bit boundary.
        while packet.len() % 4 != 0 {
            packet.push(0);
        }
        Some(packet)
    }
}

struct Session {
    /// The stream this control connection described, if any. A connection may
    /// send OPTIONS first; DESCRIBE attaches it, and re-describing another URI
    /// moves it.
    stream: RefCell<Option<Rc<Stream>>>,
    /// The player's outbound queue: RTP, RTCP and transport setup.
    ///
    /// Deliberately an [`kanal::AsyncSender`]: it exposes `try_send`, `len`,
    /// `capacity` and `is_closed`, but *not* the blocking `send`, which parks
    /// the thread when the queue is full. Everything here shares one reactor
    /// thread, so that call can only deadlock against the writer task that
    /// would free a slot.
    out: kanal::AsyncSender<Outbound>,
    /// RTSP control bytes waiting for the writer.
    ///
    /// Responses must never be queued behind the RTP backlog: the outbound
    /// channel is deliberately filled to capacity while VOD is paced ahead of
    /// the player, and `kanal::Sender::send` *blocks the thread* when full. On
    /// this single-threaded reactor that deadlocks against the very writer
    /// task that would free a slot, freezing every RTP stream and every log
    /// line until the process is restarted. Control bytes therefore live in
    /// their own unbounded queue that the writer drains before each message.
    control: RefCell<VecDeque<Vec<u8>>>,
    /// Bit set of tracks this session has set up.
    subscribed: Cell<u8>,
    playing: Cell<bool>,
    /// The client paused. ffmpeg's seek path always sends PAUSE before the PLAY
    /// that carries the target range, which distinguishes a seek from the first
    /// PLAY (or a saved resume position in the initial PLAY).
    paused: Cell<bool>,
    /// NPT offset last advertised in a PLAY response. Kept unchanged for
    /// pause/resume so ffmpeg's `range_start_offset` does not jump. With the
    /// absolute media clock this is the playhead at the PLAY (or the seek
    /// target), which is exactly the offset ffmpeg must add to its RTP delta.
    range_start: Cell<f64>,
    /// A PLAY has already happened. The first PLAY's `npt=0` is ffmpeg's
    /// default, not a request to rewind, so it starts at the live edge; later
    /// PLAYs compare their range against the current position.
    played: Cell<bool>,
    /// Bumped on a seek. The writer drops queued packets from older epochs, so
    /// a burst buffered before the seek cannot leak into the new timeline.
    epoch: Cell<u32>,
    /// A seek is in flight: the media engine has not reset its RTP epoch yet.
    /// Suppress the fan-out until it does, or packets with the old sequence
    /// numbers and timestamps leak past the PLAY response.
    seeking: Cell<bool>,
    /// When the current seek was requested. A PLAY whose response or media
    /// engine never completes must not gate the stream forever, so the
    /// watchdog releases the session after [`SEEK_TIMEOUT`].
    seek_started: Cell<Option<Instant>>,
    /// The session has received a video keyframe in the current RTP epoch.
    /// A live RTSP session joins mid-GOP, where the decoder lacks references
    /// and reports "reference picture missing"; hold video until the next IDR.
    video_synced: Cell<bool>,
    /// Next RTP sequence number of this session's timeline beacon.
    beacon_seq: Cell<u16>,
    /// Woken whenever this session's outbound queue gains room. Replaced when
    /// the control connection attaches to a stream (DESCRIBE): before that
    /// there is nothing to wake.
    drain: RefCell<Rc<DrainSignal>>,
}

/// Whether the outbound queue has room for the media engine, without polling.
///
/// The engine must wait for every playing session to accept the next access
/// unit's packets, and polling that cost 500 wake-ups a second per blocked
/// track. Instead the engine parks on one waker: the writer bumps the
/// generation after every dequeue, and every state change that can unblock the
/// engine (pause, seek, disconnect) bumps it too. The waiter re-checks after
/// parking, so a wake can never be lost.
struct DrainSignal {
    generation: Cell<u64>,
    waker: Cell<Option<Waker>>,
}

impl DrainSignal {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            generation: Cell::new(0),
            waker: Cell::new(None),
        })
    }

    fn notify(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    const fn generation(&self) -> u64 {
        self.generation.get()
    }

    fn park(&self, context: &Context<'_>) {
        self.waker.set(Some(context.waker().clone()));
    }
}

enum Outbound {
    /// Wake the writer so it re-checks the session's control queue.
    Flush,
    Setup {
        track: u8,
        transport: Transport,
    },
    Rtp {
        epoch: u32,
        track: u8,
        data: RtpPacket,
    },
    Rtcp {
        epoch: u32,
        track: u8,
        data: RtpPacket,
    },
}

enum Transport {
    Tcp {
        rtp_channel: u8,
        rtcp_channel: u8,
    },
    Udp {
        rtp: UdpSocket,
        rtcp: UdpSocket,
        rtp_peer: SocketAddr,
        rtcp_peer: SocketAddr,
    },
}

/// Shared state between the RTSP control tasks and the media consumer.
pub struct Stream {
    /// Which `(channel, quality)` this presentation serves.
    key: StreamKey,
    /// Streamer/channel name; the SDP session name and player title.
    title: String,
    /// Set when the daemon drops the stream because its last viewer left. The
    /// scheduler, the DVR refresh and the media engine check it and stop.
    stop: Cell<bool>,
    sessions: RefCell<Vec<Rc<Session>>>,
    video_config: RefCell<Option<H264Config>>,
    audio_config: RefCell<Option<AudioConfig>>,
    ready: Cell<bool>,
    video_base: Cell<Option<i64>>,
    audio_base: Cell<Option<i64>>,
    /// NTP time corresponding to media time zero of the current RTP epoch.
    /// Sender reports map their media timestamp through this value.
    rtp_origin_ntp: Cell<u64>,
    /// Current playhead in milliseconds since the start of the broadcast.
    position_ms: Cell<u64>,
    /// The scheduler's live edge in milliseconds since the start of the
    /// broadcast, refreshed by the timeline beacon task. Independent of the
    /// media engine's playhead, so it remains the live edge during DVR replay.
    live_edge_ms: Cell<u64>,
    /// Set for the duration of one video access unit, so `send` can decide
    /// whether a session still waiting for an IDR may start.
    video_keyframe: Cell<bool>,
    /// Seek requests from RTSP control to the media engine (consumer channel).
    seek_tx: RefCell<Option<kanal::AsyncSender<SegmentMessage>>>,
    video: RefCell<Track>,
    audio: RefCell<Track>,
    /// Wakes the media engine when the outbound queues gain room.
    drain: Rc<DrainSignal>,
}

impl Stream {
    #[must_use]
    pub fn new(key: StreamKey, title: impl Into<String>) -> Self {
        Self {
            key,
            title: title.into(),
            stop: Cell::new(false),
            sessions: RefCell::new(Vec::new()),
            video_config: RefCell::new(None),
            audio_config: RefCell::new(None),
            ready: Cell::new(false),
            video_base: Cell::new(None),
            audio_base: Cell::new(None),
            rtp_origin_ntp: Cell::new(0),
            position_ms: Cell::new(0),
            live_edge_ms: Cell::new(0),
            video_keyframe: Cell::new(false),
            seek_tx: RefCell::new(None),
            video: RefCell::new(Track::new(
                sdp::VIDEO_PAYLOAD_TYPE,
                sdp::VIDEO_CLOCK_RATE,
                0x5A5A_0001,
            )),
            audio: RefCell::new(Track::new(sdp::AUDIO_PAYLOAD_TYPE, 48_000, 0x5A5A_0002)),
            drain: DrainSignal::new(),
        }
    }

    #[must_use]
    pub const fn key(&self) -> &StreamKey {
        &self.key
    }

    /// Stop every task that belongs to this stream.
    ///
    /// Closing the seek channel wakes the media engine once the scheduler has
    /// dropped its consumer sender; `stop` is checked directly while a VOD fetch
    /// or a segment read is in flight, because closing a channel cannot
    /// interrupt an in-progress await.
    pub fn stop(&self) {
        if self.stop.replace(true) {
            return;
        }
        // Dropping the last seek sender closes the consumer channel, which
        // ends `Media::run`'s `recv` even when no segment is arriving.
        self.seek_tx.borrow_mut().take();
        // A media engine parked on outbound capacity must re-check: every
        // session is gone, so it is no longer blocked.
        self.notify_drain();
    }

    #[must_use]
    pub const fn stopped(&self) -> bool {
        self.stop.get()
    }

    /// The first segment has been demuxed: DESCRIBE can build an SDP.
    fn mark_ready(&self) {
        self.ready.set(true);
    }

    async fn wait_ready(&self) -> bool {
        for _ in 0..100 {
            if self.ready.get() || self.stop.get() {
                return self.ready.get();
            }
            compio::time::sleep(Duration::from_millis(100)).await;
        }
        self.ready.get()
    }

    fn add_session(&self, session: Rc<Session>) {
        self.sessions.borrow_mut().push(session);
    }

    fn remove_session(&self, session: &Rc<Session>) {
        self.sessions
            .borrow_mut()
            .retain(|candidate| !Rc::ptr_eq(candidate, session));
        // A session that was blocking the engine is gone.
        self.notify_drain();
    }

    fn has_track(&self, track: u8) -> bool {
        match track {
            TRACK_VIDEO => self.video_config.borrow().is_some(),
            TRACK_AUDIO => self.audio_config.borrow().is_some(),
            TRACK_TIMELINE => true,
            _ => false,
        }
    }

    /// Fan one RTP packet out to every playing session subscribed to `track`.
    ///
    /// The packet is shared, not copied: every queue holds the same `Rc`, so
    /// the fan-out costs a refcount per session.
    fn send(&self, track: u8, packet: &RtpPacket) {
        let bit = 1u8 << track;
        let keyframe = self.video_keyframe.get();
        let video = 1u8 << TRACK_VIDEO;
        self.sessions.borrow_mut().retain(|session| {
            if !session.playing.get()
                || session.seeking.get()
                || session.subscribed.get() & bit == 0
            {
                return true;
            }
            // A live join starts mid-GOP, so *every* track waits for the first
            // video keyframe. Feeding a mid-GOP access unit to the decoder
            // produces "reference picture missing" until the next IDR, and
            // audio that starts seconds before video breaks ffmpeg's
            // cross-stream RTCP mapping: on the first sender report it copies
            // that stream's `rtcp_ts_offset` and `first_rtcp_ntp_time` into
            // every stream that has not seen a report yet (rtsp.c), which is
            // only valid if both tracks' first received packets are the same
            // instant. Starting audio alone made the audio timeline jump back
            // by the IDR wait, which players report as a non-monotonic DTS at
            // the join.
            if !session.video_synced.get() && session.subscribed.get() & video != 0 {
                if !keyframe {
                    return true;
                }
                session.video_synced.set(true);
            }
            let keep = Self::push_out(
                session,
                Outbound::Rtp {
                    epoch: session.epoch.get(),
                    track,
                    data: Rc::clone(packet),
                },
            );
            if !keep {
                debug!("rtsp: unregistering session, its writer is gone");
            }
            keep
        });
    }

    /// Whether any RTSP client is still connected (playing or paused).
    pub fn has_sessions(&self) -> bool {
        !self.sessions.borrow().is_empty()
    }

    /// Queue one outbound message, dropping it when the player's buffer is
    /// full. Returns `false` only when the writer is gone.
    ///
    /// A full queue must never unregister the session: `try_send` reports
    /// "full" separately from "closed", and treating the two the same silently
    /// removed a live client from the fan-out for good, so playback froze while
    /// the control plane kept answering until the player was reloaded.
    fn push_out(session: &Session, message: Outbound) -> bool {
        if session.out.is_closed() {
            return false;
        }
        // Full: the player's buffer is the back-pressure signal, not a reason
        // to stop sending it media. The packet (or report) is dropped.
        let _ = session.out.try_send(message);
        true
    }

    /// Wait until every playing session subscribed to `track` can accept
    /// `packets` more RTP packets.
    ///
    /// The bounded outbound queue is the receiver's buffer. Short VOD
    /// segments are sent as fast as that buffer drains, which is what lets a
    /// player consume faster than real time; a full queue means "slow down",
    /// not "drop the client". [`RTCP_HEADROOM`] slots stay free for the sender
    /// reports that are queued between samples: RTCP is not part of the packet
    /// count, and a rejected report is a lost NTP/RTP mapping.
    ///
    /// Event-driven: the writer bumps [`DrainSignal`] on every dequeue, and
    /// pausing/seeking/disconnecting bumps it too, so this parks instead of
    /// polling the queues.
    async fn wait_out_capacity(&self, track: u8, packets: usize) {
        if !self.out_queue_blocked(track, packets) {
            return;
        }
        let started = Instant::now();
        loop {
            self.wait_drained().await;
            if !self.out_queue_blocked(track, packets) {
                break;
            }
        }
        let waited = started.elapsed();
        if waited >= Duration::from_secs(2) {
            debug!(
                track,
                waited_ms = waited.as_millis(),
                queued = self.sessions.borrow().iter().map(|s| s.out.len()).max(),
                "rtsp: outbound queue blocked on the client"
            );
        }
    }

    /// Whether any playing session on `track` is too close to full.
    fn out_queue_blocked(&self, track: u8, packets: usize) -> bool {
        let bit = 1u8 << track;
        self.sessions.borrow().iter().any(|session| {
            !session.out.is_closed()
                && session.playing.get()
                && !session.seeking.get()
                && session.subscribed.get() & bit != 0
                && session.out.len().saturating_add(packets)
                    > session.out.capacity().saturating_sub(RTCP_HEADROOM)
        })
    }

    /// Park until something may have freed outbound capacity.
    async fn wait_drained(&self) {
        let seen = self.drain.generation();
        poll_fn(|context| {
            if self.drain.generation() != seen {
                return Poll::Ready(());
            }
            self.drain.park(context);
            // Re-check after parking: a notify between the check above and the
            // park must not be lost.
            if self.drain.generation() == seen {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
    }

    /// Something that can unblock the media engine changed state.
    fn notify_drain(&self) {
        self.drain.notify();
    }

    fn send_rtcp(&self) {
        let origin = self.rtp_origin_ntp.get();
        if let Some(report) = self.video.borrow().sender_report(origin) {
            self.send_rtcp_packet(TRACK_VIDEO, &Rc::new(report));
        }
        if let Some(report) = self.audio.borrow().sender_report(origin) {
            self.send_rtcp_packet(TRACK_AUDIO, &Rc::new(report));
        }
    }

    fn send_rtcp_packet(&self, track: u8, packet: &RtpPacket) {
        let bit = 1u8 << track;
        self.sessions.borrow_mut().retain(|session| {
            if !session.playing.get()
                || session.seeking.get()
                || session.subscribed.get() & bit == 0
            {
                return true;
            }
            let keep = Self::push_out(
                session,
                Outbound::Rtcp {
                    epoch: session.epoch.get(),
                    track,
                    data: Rc::clone(packet),
                },
            );
            if !keep {
                debug!("rtsp: unregistering session, its writer is gone");
            }
            keep
        });
    }

    #[allow(dead_code)] // Kept for the epoch bookkeeping; the media clock is absolute now.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn rtp_timestamp(pts: i64, clock_rate: u32, base: &Cell<Option<i64>>) -> u32 {
        let base = base.get().unwrap_or_else(|| {
            base.set(Some(pts));
            pts
        });
        let delta = media_delta(pts, base);
        let scaled = delta * i64::from(clock_rate) / 90_000;
        scaled as u32
    }

    fn set_position_ms(&self, position_ms: u64) {
        self.position_ms.set(position_ms);
    }

    /// Discard the per-track RTP timestamp epochs; the next samples start new
    /// ones. The RTCP sender reports must forget the old mapping too, or a report
    /// sent before the first post-seek packet pairs an old RTP timestamp with a
    /// new NTP time and players remap the whole stream incorrectly.
    fn reset_media_base(&self) {
        debug!("rtsp: reset RTP/RTCP timestamp mapping");
        self.video_base.set(None);
        self.audio_base.set(None);
        self.video.borrow_mut().reset_mapping();
        self.audio.borrow_mut().reset_mapping();
        for session in self.sessions.borrow().iter() {
            session.video_synced.set(false);
        }
    }

    /// Start the next samples from an explicit PES PTS epoch. Both tracks use
    /// the same value at a seek, so the A/V offset is preserved.
    fn set_media_base(&self, pts: i64) {
        self.video_base.set(Some(pts));
        self.audio_base.set(Some(pts));
        // The SRs of this epoch describe presentation time relative to `pts`;
        // anchor that media clock on the wall clock now. Live reports then read
        // as real time, and VOD reports stay internally consistent.
        self.rtp_origin_ntp.set(ntp_now());
        self.end_seek();
    }

    /// Release sessions held back while the media engine was resetting its RTP
    /// epoch. Anything queued in that window belongs to the old timeline.
    fn end_seek(&self) {
        for session in self.sessions.borrow().iter() {
            session.seeking.set(false);
            session.seek_started.set(None);
        }
        // Released sessions can back-pressure the engine again, and a waiter
        // that saw `seeking` must re-evaluate.
        self.notify_drain();
    }

    fn set_seek_sender(&self, sender: kanal::AsyncSender<SegmentMessage>) {
        *self.seek_tx.borrow_mut() = Some(sender);
    }

    async fn request_seek(&self, target_ms: u64) -> bool {
        let Some(sender) = self.seek_tx.borrow().clone() else {
            return false;
        };
        sender.send(SegmentMessage::Seek(target_ms)).await.is_ok()
    }

    fn set_audio_rate(&self, rate: u32) {
        let mut audio = self.audio.borrow_mut();
        if audio.clock_rate != rate {
            debug!(rate, "audio clock rate changed");
            audio.clock_rate = rate;
        }
    }

    /// Absolute NPT position of the live playhead, in seconds.
    #[allow(clippy::cast_precision_loss)] // broadcast durations are far below 2^53 ms
    fn position_seconds(&self) -> f64 {
        self.position_ms.get() as f64 / 1000.0
    }
}

/// Serve one pre-built stream on both loopback families (play mode).
///
/// Two listeners, one per family, on the same port. Both are loopback-only,
/// so the stream is never served to the network.
pub fn spawn_single(stream: Rc<Stream>) {
    compio::runtime::spawn(async move {
        let listeners = match loopback_listeners() {
            Ok(listeners) => listeners,
            Err(error) => {
                error!(%error, "failed to bind the RTSP listener");
                return;
            }
        };
        let resolver: Rc<dyn StreamResolver> = Rc::new(SingleStream { stream });
        for listener in listeners {
            let local = listener.local_addr().expect("local addr");
            println!("rtsp://{local}/");
            compio::runtime::spawn(accept_loop(listener, Rc::clone(&resolver))).detach();
        }
    })
    .detach();
}

/// Both loopback families, on the default port if it is free, else on an
/// ephemeral one.
fn loopback_listeners() -> std::io::Result<Vec<TcpListener>> {
    match bind_loopbacks(DEFAULT_RTSP_PORT) {
        Ok(listeners) => Ok(listeners),
        Err(error) => {
            warn!(
                %error,
                port = DEFAULT_RTSP_PORT,
                "rtsp: default port unavailable, binding an ephemeral pair"
            );
            bind_loopbacks(0)
        }
    }
}

/// Bind `127.0.0.1` and `::1` on one port.
///
/// v4 goes first because it picks the port when `0` asks for an ephemeral one;
/// v6 then takes the same number. A family that cannot bind at all (an IPv6
/// stack that is switched off) is skipped, but `AddrInUse` moves the whole pair
/// to another port: half a pair on the default port is worse than a full pair
/// on an ephemeral one.
fn bind_loopbacks(port: u16) -> std::io::Result<Vec<TcpListener>> {
    let v4 = bind_loopback(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
    let port = v4
        .as_ref()
        .ok()
        .and_then(|listener| listener.local_addr().ok())
        .map_or(port, |local| local.port());
    let v6 = bind_loopback(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port));
    match (v4, v6) {
        (Ok(v4), Ok(v6)) => Ok(vec![v4, v6]),
        (Ok(_v4), Err(error)) if error.kind() == ErrorKind::AddrInUse => Err(error),
        (Err(error), Ok(_)) if error.kind() == ErrorKind::AddrInUse => Err(error),
        (Ok(v4), Err(error)) => {
            warn!(%error, port, "rtsp: no IPv6 loopback listener");
            Ok(vec![v4])
        }
        (Err(error), Ok(v6)) => {
            warn!(%error, port, "rtsp: no IPv4 loopback listener");
            Ok(vec![v6])
        }
        (Err(error), Err(_)) => Err(error),
    }
}

/// Bind one loopback address.
///
/// compio's binder would do, except it sets `SO_REUSEADDR` unconditionally: on
/// Windows that lets a second process share the port instead of failing, so the
/// ephemeral fallback above would never trigger. Unix still sets it, to rebind
/// over `TIME_WAIT`.
fn bind_loopback(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let domain = match addr {
        SocketAddr::V4(_) => Domain::IPV4,
        SocketAddr::V6(_) => Domain::IPV6,
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(128)?;
    TcpListener::from_std(socket.into())
}

/// Bind `addr` and serve whatever the resolver hands out (daemon mode).
///
/// One listener: `--bind` names the exact address, so the loopback pair of
/// `spawn_single` does not apply.
pub async fn serve(resolver: Rc<dyn StreamResolver>, addr: SocketAddr) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    println!("rtsp://{local}/");
    accept_loop(listener, resolver).await;
    Ok(())
}

/// The `play` resolver: one stream, any URI.
struct SingleStream {
    stream: Rc<Stream>,
}

impl StreamResolver for SingleStream {
    fn resolve<'a>(
        &'a self,
        _uri: &'a str,
    ) -> LocalBoxFuture<'a, Result<Rc<Stream>, ResolveError>> {
        Box::pin(async { Ok(Rc::clone(&self.stream)) })
    }

    fn release(&self, _stream: &Rc<Stream>) {}
}

fn next_session_id() -> String {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let value = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{:08x}", value ^ 0x5A5A_0000)
}

async fn accept_loop(listener: TcpListener, resolver: Rc<dyn StreamResolver>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                if stream.set_nodelay(true).is_err() {
                    error!(%peer, "failed to set TCP no delay");
                    continue;
                }
                compio::runtime::spawn(control_session(stream, peer, Rc::clone(&resolver)))
                    .detach();
            }
            Err(error) => warn!(%error, "rtsp accept failed"),
        }
    }
}

async fn control_session(stream: TcpStream, peer: SocketAddr, resolver: Rc<dyn StreamResolver>) {
    let local = stream.local_addr().unwrap_or(peer);
    let (mut read, write) = stream.into_split();
    let (out_tx, out_rx) = kanal::bounded(4096);

    let session = Rc::new(Session {
        stream: RefCell::new(None),
        out: out_tx.to_async(),
        control: RefCell::new(VecDeque::new()),
        subscribed: Cell::new(0),
        playing: Cell::new(false),
        paused: Cell::new(false),
        range_start: Cell::new(0.0),
        played: Cell::new(false),
        epoch: Cell::new(0),
        seeking: Cell::new(false),
        seek_started: Cell::new(None),
        video_synced: Cell::new(false),
        beacon_seq: Cell::new(1),
        drain: RefCell::new(DrainSignal::new()),
    });
    compio::runtime::spawn(writer(write, out_rx.to_async(), Rc::clone(&session))).detach();
    let id = next_session_id();
    debug!(%peer, %id, "rtsp client connected");

    let mut buffer = Vec::new();
    while let Some(request) = read_request(&mut read, &mut buffer).await {
        if !handle_request(&resolver, &session, &id, local, peer, &request).await {
            break;
        }
    }

    // Detach from whatever stream this control connection described. The
    // daemon drops a stream as soon as its last viewer is gone.
    if let Some(stream) = session.stream.borrow_mut().take() {
        stream.remove_session(&session);
        if !stream.has_sessions() {
            resolver.release(&stream);
        }
    }
    debug!(%peer, %id, "rtsp client disconnected");
}

async fn writer(mut stream: TcpStream, rx: AsyncReceiver<Outbound>, session: Rc<Session>) {
    let mut transports: [Option<Transport>; TRACK_COUNT] = [None, None, None];
    // Interleaved framing buffer, allocated once and handed back by every
    // write. `write_all` returns the buffer it was given, so the payload is
    // copied in place instead of into a fresh `Vec` per packet.
    let mut frame = Vec::with_capacity(64 + RTP_MAX_PAYLOAD);

    'writer: loop {
        // Control bytes go first, always. A response parked behind the RTP
        // backlog would stall the player's seek for as long as its buffer
        // takes to drain.
        loop {
            let next = session.control.borrow_mut().pop_front();
            let Some(bytes) = next else { break };
            if stream.write_all(bytes).await.0.is_err() {
                break 'writer;
            }
        }

        let Ok(message) = rx.recv().await else {
            break 'writer;
        };
        // A slot just came free. Wake the media engine if it is waiting here
        // rather than on the driver.
        session.drain.borrow().notify();
        match message {
            Outbound::Flush => {}
            Outbound::Setup { track, transport } => {
                transports[usize::from(track)] = Some(transport);
            }
            Outbound::Rtp { epoch, track, data } => {
                if epoch != session.epoch.get() {
                    continue;
                }
                match &transports[usize::from(track)] {
                    Some(Transport::Tcp { rtp_channel, .. }) => {
                        match write_interleaved(&mut stream, &mut frame, *rtp_channel, &data).await
                        {
                            Ok(returned) => frame = returned,
                            Err(()) => break,
                        }
                    }
                    Some(Transport::Udp { rtp, rtp_peer, .. }) => {
                        let _ = rtp.send_to(data, *rtp_peer).await;
                    }
                    None => {}
                }
            }
            Outbound::Rtcp { epoch, track, data } => {
                if epoch != session.epoch.get() {
                    continue;
                }
                match &transports[usize::from(track)] {
                    Some(Transport::Tcp { rtcp_channel, .. }) => {
                        match write_interleaved(&mut stream, &mut frame, *rtcp_channel, &data).await
                        {
                            Ok(returned) => frame = returned,
                            Err(()) => break,
                        }
                    }
                    Some(Transport::Udp {
                        rtcp, rtcp_peer, ..
                    }) => {
                        let _ = rtcp.send_to(data, *rtcp_peer).await;
                    }
                    None => {}
                }
            }
        }
    }

    // The writer is gone, so this session can never accept another packet.
    // Dropping the receiver closes the channel, and `out_queue_blocked` then
    // ignores the session -- but a media engine already parked here has to be
    // told to re-check, or it would wait for a client that no longer exists.
    drop(rx);
    session.drain.borrow().notify();
}

/// Write one interleaved frame (`$`, channel, length, payload).
///
/// Takes and returns the session's framing buffer: `write_all` hands the buffer
/// back, so the allocation survives across packets. The payload still has to be
/// copied once to put the four-byte header in front of it, but nothing is
/// allocated.
async fn write_interleaved(
    stream: &mut TcpStream,
    frame: &mut Vec<u8>,
    channel: u8,
    data: &[u8],
) -> Result<Vec<u8>, ()> {
    let len = u16::try_from(data.len()).unwrap_or(0);
    frame.clear();
    frame.extend_from_slice(&[b'$', channel]);
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(data);

    let taken = std::mem::take(frame);
    let written = stream.write_all(taken).await;
    written.0.map_err(|_| ())?;
    Ok(written.1)
}

struct Request {
    method: String,
    uri: String,
    cseq: String,
    transport: Option<String>,
    range: Option<String>,
}

async fn read_request(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Option<Request> {
    loop {
        // Drop interleaved RTCP/receiver bytes from the client before looking for
        // the next request head.
        while buffer.first() == Some(&b'$') {
            if buffer.len() < 4 {
                break;
            }
            let len = usize::from(u16::from_be_bytes([buffer[2], buffer[3]]));
            if buffer.len() < 4 + len {
                break;
            }
            buffer.drain(..4 + len);
        }

        if let Some(end) = header_end(buffer) {
            let request = parse_request(&buffer[..end])?;
            let content_length = content_length(&buffer[..end]);
            let body_start = end + 4;
            if buffer.len() >= body_start + content_length {
                buffer.drain(..body_start + content_length);
                return Some(request);
            }
        }

        if buffer.len() > crate::constants::MAX_HEAD_BYTES {
            return None;
        }

        let chunk = vec![0u8; 2048];
        let result = stream.read(chunk).await;
        let read = result.0.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&result.1[..read]);
    }
}

fn header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_request(head: &[u8]) -> Option<Request> {
    let head = std::str::from_utf8(head).ok()?;
    let mut lines = head.split("\r\n");

    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_owned();
    let uri = request_line.next()?.to_owned();

    let mut cseq = String::from("0");
    let mut transport = None;
    let mut range = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "cseq" => value.clone_into(&mut cseq),
            "transport" => transport = Some(value.to_owned()),
            "range" => range = Some(value.to_owned()),
            _ => {}
        }
    }

    Some(Request {
        method,
        uri,
        cseq,
        transport,
        range,
    })
}

fn content_length(head: &[u8]) -> usize {
    let Ok(head) = std::str::from_utf8(head) else {
        return 0;
    };
    for line in head.split("\r\n").skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse().unwrap_or(0);
        }
    }
    0
}

async fn handle_request(
    resolver: &Rc<dyn StreamResolver>,
    session: &Rc<Session>,
    id: &str,
    local: SocketAddr,
    peer: SocketAddr,
    request: &Request,
) -> bool {
    if request.method == "OPTIONS" {
        let headers = [(
            "Public",
            "OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN, GET_PARAMETER, SET_PARAMETER",
        )];
        send_response(session, response(&request.cseq, None, &headers, ""));
        return true;
    }
    if request.method == "DESCRIBE" {
        return describe(resolver, session, local, request).await;
    }

    // Everything else acts on the stream this connection described.
    let Some(hub) = session.stream.borrow().clone() else {
        send_response(
            session,
            error_response(&request.cseq, "455 Method Not Valid in This State"),
        );
        return true;
    };

    match request.method.as_str() {
        "SETUP" => setup(&hub, session, id, peer, request).await,
        "PLAY" => {
            session.playing.set(true);

            // A PAUSE+PLAY is always a seek. So is a PLAY carrying a new range
            // while already playing: mpv sometimes seeks without pausing, and
            // requiring PAUSE made the first seek silently resume instead.
            //
            // The first PLAY starts at the stream start (the DVR's first
            // segment), not at the live edge: its real first video frame at
            // NPT 0 is what anchors the player's timeline at 0. ffmpeg's
            // `npt=0` is the default, so this is one deliberate seek rather
            // than "resume live". A saved nonzero start is honoured as-is.
            let requested = request.range.as_deref().and_then(parse_npt_start);
            let was_paused = session.paused.replace(false);
            let position = hub.position_seconds();
            let first = !session.played.get();
            let seek_requested = requested.is_some_and(|target| {
                let target_ms = seconds_to_ms(target);
                if first {
                    true
                } else {
                    was_paused || target_ms.abs_diff(seconds_to_ms(position)) > 1_500
                }
            });
            debug!(
                range = request.range.as_deref().unwrap_or("-"),
                was_paused,
                seek_requested,
                position_ms = hub.position_ms.get(),
                "rtsp: PLAY"
            );
            let mut waiting = false;
            if seek_requested && let Some(target) = requested {
                let target_ms = seconds_to_ms(target);
                if target_ms.abs_diff(hub.position_ms.get()) > 1_500 {
                    session.seeking.set(true);
                    session.seek_started.set(Some(Instant::now()));
                    if hub.request_seek(target_ms).await {
                        session.epoch.set(session.epoch.get().wrapping_add(1));
                        session.range_start.set(target);
                        waiting = true;
                    }
                }
            }
            // A PLAY that did not ask the media engine for a new timeline (first
            // play, plain resume, or a no-op seek) must not leave the session
            // gated behind `seeking`. Its NPT offset is the current playhead:
            // the media timestamps are absolute, so the player's
            // `range_start_offset` has to be re-anchored here.
            if !waiting {
                session.seeking.set(false);
                session.seek_started.set(None);
                session.range_start.set(position);
            }
            session.played.set(true);
            // `playing`/`seeking` just changed, so the engine may no longer be
            // blocked on this session's queue.
            hub.notify_drain();

            let av_info = rtp_info(&hub, &request.uri, session.subscribed.get());
            let beacon = (session.subscribed.get() & (1 << TRACK_TIMELINE) != 0)
                .then(|| timeline_info(&request.uri, session.beacon_seq.get()));
            // On a seek the A/V RTP-Info is stale (the epoch has not reset yet),
            // but the beacon lives on its own timestamps and must always be
            // advertised, or its PTS loses the session's NPT offset.
            let info = match (seek_requested, beacon) {
                (false, Some(beacon)) if !av_info.is_empty() => format!("{av_info},{beacon}"),
                (_, Some(beacon)) => beacon,
                (false, None) => av_info,
                (true, None) => String::new(),
            };
            // Keep the offset stable across pause/resume; only a seek changes it.
            let range = format!("npt={:.3}-", session.range_start.get());
            let mut headers = vec![("Range", range.as_str())];
            // RTP-Info seeds ffmpeg's `base_timestamp`. On a seek, the media
            // engine has not reset the RTP epoch yet, so the value here is the
            // old live timestamp; advertising it makes ffmpeg reject every
            // post-seek packet until the next PLAY. Omit it and let the first
            // packet establish the base.
            if !info.is_empty() {
                headers.push(("RTP-Info", info.as_str()));
            }
            send_response(session, response(&request.cseq, Some(id), &headers, ""));
            true
        }
        "PAUSE" => {
            // ffmpeg's RTSP seek sends PAUSE followed by PLAY with the target
            // range; keep the session alive and stop the RTP fan-out. Bump the
            // epoch so packets queued before the seek are dropped by the writer.
            session.playing.set(false);
            session.paused.set(true);
            session.seeking.set(true);
            session.seek_started.set(Some(Instant::now()));
            session.epoch.set(session.epoch.get().wrapping_add(1));
            // A paused session no longer back-pressures the media engine.
            hub.notify_drain();
            debug!(position_ms = hub.position_ms.get(), "rtsp: PAUSE");
            send_response(session, response(&request.cseq, Some(id), &[], ""));
            true
        }
        "TEARDOWN" => {
            send_response(session, response(&request.cseq, Some(id), &[], ""));
            false
        }
        "GET_PARAMETER" | "SET_PARAMETER" => {
            send_response(session, response(&request.cseq, Some(id), &[], ""));
            true
        }
        _ => {
            send_response(
                session,
                error_response(&request.cseq, "405 Method Not Allowed"),
            );
            true
        }
    }
}

/// Resolve the requested `(channel, quality)`, attach the connection to it and
/// answer with the SDP.
#[allow(clippy::cast_precision_loss)] // broadcast durations are far below 2^53 ms
async fn describe(
    resolver: &Rc<dyn StreamResolver>,
    session: &Rc<Session>,
    local: SocketAddr,
    request: &Request,
) -> bool {
    let stream = match resolver.resolve(&request.uri).await {
        Ok(stream) => stream,
        Err(error) => {
            debug!(uri = %request.uri, status = error.status, "rtsp: DESCRIBE failed");
            send_response(session, error_response(&request.cseq, error.status));
            return true;
        }
    };
    attach(session, resolver, &stream);

    if !stream.wait_ready().await {
        send_response(
            session,
            error_response(&request.cseq, "503 Service Unavailable"),
        );
        return true;
    }

    let video = stream.video_config.borrow();
    let audio = stream.audio_config.borrow();
    // A finite end makes the stream seekable in the player; the live edge (the
    // scheduler's, not the playhead, which may be replaying the DVR) is the
    // current end of the broadcast. The timeline beacon keeps it growing.
    let live_edge = stream.live_edge_ms.get();
    let end_ms = live_edge.max(stream.position_ms.get());
    let end = (end_ms >= 500).then(|| end_ms as f64 / 1000.0);
    let sdp = sdp::build(
        local.ip(),
        &stream.title,
        end,
        video.as_ref(),
        audio.as_ref(),
        true,
    );
    drop(video);
    drop(audio);

    let base = if request.uri.ends_with('/') {
        request.uri.clone()
    } else {
        format!("{}/", request.uri)
    };
    let headers = [
        ("Content-Type", "application/sdp"),
        ("Content-Base", base.as_str()),
    ];
    send_response(session, response(&request.cseq, None, &headers, &sdp));
    true
}

/// Move the control connection onto `stream`, detaching it from the previous
/// one. A second DESCRIBE for another URI is legal and moves the session.
fn attach(session: &Rc<Session>, resolver: &Rc<dyn StreamResolver>, stream: &Rc<Stream>) {
    let previous = session.stream.replace(Some(Rc::clone(stream)));
    if previous.as_ref().is_some_and(|old| Rc::ptr_eq(old, stream)) {
        return;
    }
    session.drain.borrow_mut().clone_from(&stream.drain);
    stream.add_session(Rc::clone(session));

    if let Some(old) = previous {
        old.remove_session(session);
        if !old.has_sessions() {
            resolver.release(&old);
        }
    }
}

#[allow(clippy::similar_names)] // rtp/rtcp are the protocol's names
async fn setup(
    hub: &Stream,
    session: &Rc<Session>,
    id: &str,
    peer: SocketAddr,
    request: &Request,
) -> bool {
    let Some(track) = track_from_uri(&request.uri) else {
        send_response(session, error_response(&request.cseq, "404 Not Found"));
        return true;
    };
    if !hub.has_track(track) {
        send_response(session, error_response(&request.cseq, "404 Not Found"));
        return true;
    }

    let Some(spec) = request.transport.as_deref().and_then(parse_transport) else {
        send_response(session, error_response(&request.cseq, "400 Bad Request"));
        return true;
    };

    let (transport, description, server_ssrc) = if spec.tcp {
        let description = format!(
            "RTP/AVP/TCP;unicast;interleaved={}-{}",
            spec.interleaved.0, spec.interleaved.1
        );
        (
            Transport::Tcp {
                rtp_channel: spec.interleaved.0,
                rtcp_channel: spec.interleaved.1,
            },
            description,
            track_ssrc(hub, track),
        )
    } else {
        let bind_ip = match peer {
            SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        };
        let Ok(rtp) = UdpSocket::bind(SocketAddr::new(bind_ip, 0)).await else {
            send_response(
                session,
                error_response(&request.cseq, "500 Internal Server Error"),
            );
            return true;
        };
        let rtp_port = rtp.local_addr().map_or(0, |addr| addr.port());
        let rtcp = match UdpSocket::bind(SocketAddr::new(bind_ip, rtp_port.saturating_add(1))).await
        {
            Ok(socket) => Ok(socket),
            // The neighbouring port is taken; the pairing is informational for
            // this server because inbound RTCP is ignored.
            Err(_) => UdpSocket::bind(SocketAddr::new(bind_ip, 0)).await,
        };
        let Ok(rtcp) = rtcp else {
            send_response(
                session,
                error_response(&request.cseq, "500 Internal Server Error"),
            );
            return true;
        };
        let rtcp_port = rtcp.local_addr().map_or(0, |addr| addr.port());

        let description = format!(
            "RTP/AVP;unicast;client_port={}-{};server_port={}-{};ssrc={:08x}",
            spec.client_port.0,
            spec.client_port.1,
            rtp_port,
            rtcp_port,
            track_ssrc(hub, track),
        );
        (
            Transport::Udp {
                rtp,
                rtcp,
                rtp_peer: SocketAddr::new(peer.ip(), spec.client_port.0),
                rtcp_peer: SocketAddr::new(peer.ip(), spec.client_port.1),
            },
            description,
            track_ssrc(hub, track),
        )
    };
    let _ = server_ssrc;

    // Setup arrives before any media for this track; the queue is empty, so
    // there is room. Never use the blocking `send` here: on a full queue it
    // parks the reactor thread and deadlocks the writer task.
    if session
        .out
        .try_send(Outbound::Setup { track, transport })
        .is_err()
    {
        return false;
    }
    session
        .subscribed
        .set(session.subscribed.get() | (1u8 << track));

    let headers = [("Transport", description.as_str())];
    send_response(session, response(&request.cseq, Some(id), &headers, ""));
    true
}

fn track_ssrc(hub: &Stream, track: u8) -> u32 {
    match track {
        TRACK_VIDEO => hub.video.borrow().ssrc,
        TRACK_AUDIO => hub.audio.borrow().ssrc,
        _ => TIMELINE_SSRC,
    }
}

fn send_response(session: &Session, bytes: Vec<u8>) {
    session.control.borrow_mut().push_back(bytes);
    // Wake the writer. The RTP queue may be full, in which case the failed
    // poke is harmless: the writer re-checks the control queue after every
    // message it does dequeue.
    let _ = session.out.try_send(Outbound::Flush);
}

fn response(cseq: &str, session: Option<&str>, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    let mut out = String::with_capacity(128 + body.len());
    out.push_str("RTSP/1.0 200 OK\r\n");
    let _ = write!(out, "CSeq: {cseq}\r\n");
    if let Some(id) = session {
        let _ = write!(out, "Session: {id};timeout=60\r\n");
    }
    for (name, value) in headers {
        let _ = write!(out, "{name}: {value}\r\n");
    }
    if !body.is_empty() {
        let _ = write!(out, "Content-Length: {}\r\n", body.len());
    }
    out.push_str("\r\n");
    out.push_str(body);
    out.into_bytes()
}

fn error_response(cseq: &str, status: &str) -> Vec<u8> {
    format!("RTSP/1.0 {status}\r\nCSeq: {cseq}\r\n\r\n").into_bytes()
}

/// Parse the start of an NPT range (`npt=12.345-`) into seconds.
fn parse_npt_start(range: &str) -> Option<f64> {
    let value = range.trim().strip_prefix("npt=")?;
    let start = value.split('-').next()?.trim();
    if start.is_empty() || start.eq_ignore_ascii_case("now") {
        return None;
    }
    start.parse().ok()
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn seconds_to_ms(seconds: f64) -> u64 {
    (seconds.max(0.0) * 1000.0) as u64
}

fn rtp_info(hub: &Stream, uri: &str, subscribed: u8) -> String {
    let base = uri.trim_end_matches('/');
    let mut entries = Vec::new();

    if subscribed & (1 << TRACK_VIDEO) != 0 {
        let video = hub.video.borrow();
        entries.push(format!(
            "{base}/trackID=0;seq={};rtptime={}",
            video.next_seq, video.last_packet_rtp
        ));
    }
    if subscribed & (1 << TRACK_AUDIO) != 0 {
        let audio = hub.audio.borrow();
        entries.push(format!(
            "{base}/trackID=1;seq={};rtptime={}",
            audio.next_seq, audio.last_packet_rtp
        ));
    }

    entries.join(",")
}

/// The beacon's `RTP-Info` entry. `rtptime=1` is the base every beacon
/// timestamp is expressed against; 0 would be ignored by ffmpeg (it treats a
/// zero base as "unset") and the first packet would then become the base.
fn timeline_info(uri: &str, seq: u16) -> String {
    let base = uri.trim_end_matches('/');
    format!("{base}/trackID=2;seq={seq};rtptime=1")
}

/// One beacon packet. `pts_ms` is the broadcast NPT it announces, and the RTP
/// timestamp is shifted by the session's advertised NPT offset so that the
/// player computes `pts = rtp - 1 + range_start = pts_ms`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss
)]
fn timeline_packet(seq: u16, pts_ms: u64, range_start: f64) -> RtpPacket {
    let ticks = (pts_ms.saturating_mul(TIMELINE_CLOCK / 1000)) as i64;
    let base = (range_start * TIMELINE_CLOCK as f64) as i64;
    let timestamp = (ticks + 1 - base) as u32;
    let payload = aac::packetize(TIMELINE_AU);

    let mut packet = Vec::with_capacity(12 + payload.len());
    packet.push(0x80);
    packet.push(0x80 | sdp::TIMELINE_PAYLOAD_TYPE); // marker
    packet.extend_from_slice(&seq.to_be_bytes());
    packet.extend_from_slice(&timestamp.to_be_bytes());
    packet.extend_from_slice(&TIMELINE_SSRC.to_be_bytes());
    packet.extend_from_slice(&payload);
    Rc::new(packet)
}

/// Queue one beacon (a new sequence number each time).
fn timeline_step(session: &Session, pts_ms: u64) {
    let seq = session.beacon_seq.get();
    session.beacon_seq.set(seq.wrapping_add(1));
    let packet = timeline_packet(seq, pts_ms, session.range_start.get());
    // The beacon is 1 packet/session/s; if the player's queue is full the
    // packet is simply dropped and the next one carries a newer edge.
    let _ = session.out.try_send(Outbound::Rtp {
        epoch: session.epoch.get(),
        track: TRACK_TIMELINE,
        data: packet,
    });
}

/// The scheduler's current live edge: the last window segment plus one nominal
/// segment. Independent of the media engine's playhead, so it stays the live
/// edge while a viewer is replaying the DVR.
fn live_edge_ms(window: &RefCell<Vec<Segment>>) -> Option<u64> {
    let window = window.borrow();
    window
        .last()
        .map(|segment| segment.timestamp_ms.saturating_add(LIVE_SEGMENT_MS))
}

/// One beacon per second for every playing session that set up the timeline
/// track.
async fn timeline_loop(hub: Rc<Stream>, window: Rc<RefCell<Vec<Segment>>>) {
    loop {
        compio::time::sleep(Duration::from_secs(1)).await;
        if hub.stopped() {
            return;
        }
        let Some(edge) = live_edge_ms(&window) else {
            continue;
        };
        hub.live_edge_ms.set(edge);
        let sessions = hub.sessions.borrow();
        for session in sessions.iter() {
            if session.subscribed.get() & (1 << TRACK_TIMELINE) != 0 {
                timeline_step(session, edge);
            }
        }
    }
}

fn track_from_uri(uri: &str) -> Option<u8> {
    if uri.contains("trackID=0") {
        Some(TRACK_VIDEO)
    } else if uri.contains("trackID=1") {
        Some(TRACK_AUDIO)
    } else if uri.contains("trackID=2") {
        Some(TRACK_TIMELINE)
    } else {
        None
    }
}

struct TransportSpec {
    tcp: bool,
    interleaved: (u8, u8),
    client_port: (u16, u16),
}

fn parse_transport(value: &str) -> Option<TransportSpec> {
    let tcp = value.to_ascii_uppercase().contains("RTP/AVP/TCP");
    let mut interleaved = (0, 1);
    let mut client_port = (0, 1);

    for attribute in value.split(';') {
        let Some((name, value)) = attribute.trim().split_once('=') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "interleaved" => {
                let (first, second) = parse_pair(value)?;
                interleaved = (u8::try_from(first).ok()?, u8::try_from(second).ok()?);
            }
            "client_port" => client_port = parse_pair(value)?,
            _ => {}
        }
    }

    Some(TransportSpec {
        tcp,
        interleaved,
        client_port,
    })
}

fn parse_pair(value: &str) -> Option<(u16, u16)> {
    let (first, second) = value.split_once('-')?;
    let first: u16 = first.trim().parse().ok()?;
    let second: u16 = second
        .trim()
        .parse()
        .unwrap_or_else(|_| first.saturating_add(1));
    Some((first, second))
}

fn ntp_now() -> u64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = elapsed.as_secs() + NTP_UNIX_OFFSET;
    let fraction = u64::try_from((u128::from(elapsed.subsec_nanos()) << 32) / 1_000_000_000)
        .unwrap_or_default();
    (seconds << 32) | fraction
}

/// Convert a 90 kHz presentation offset to NTP fixed-point units.
fn media_to_ntp(media_90k: i64) -> u64 {
    let ticks = u128::try_from(media_90k.max(0)).unwrap_or(0);
    let seconds = u64::try_from(ticks / 90_000).unwrap_or(u64::MAX);
    let fraction = u64::try_from(((ticks % 90_000) << 32) / 90_000).unwrap_or(0);
    (seconds << 32).wrapping_add(fraction)
}

/// Sleep until `pts` is due, anchored to the wall clock at the first sample.
///
/// A stream that arrives faster than real time is clamped to at most [`MAX_LEAD`]
/// ahead, so the mock at 8x is sent in bursts instead of being starved; a live
/// stream is paced at exactly 1x.
/// Maps PES timestamps of the current segment onto the broadcast NPT.
#[derive(Default)]
struct Timeline {
    base_ms: u64,
    first_pts: Option<i64>,
}

impl Timeline {
    const fn start(&mut self, base_ms: u64) {
        self.base_ms = base_ms;
        self.first_pts = None;
    }

    fn position_ms(&mut self, pts: i64) -> u64 {
        let first = *self.first_pts.get_or_insert(pts);
        let delta_ms = (pts - first).max(0) / 90; // 90 kHz ticks -> ms
        self.base_ms
            .saturating_add(u64::try_from(delta_ms).unwrap_or(0))
    }
}

struct Pacer {
    base_pts: Option<i64>,
    base_wall: Instant,
    /// Media seconds sent per wall-clock second. Live is 1x; VOD sends ahead of
    /// real time so a player can buffer and consume faster than 1x.
    speed: f64,
}

impl Pacer {
    fn new() -> Self {
        Self::with_speed(1.0)
    }

    fn with_speed(speed: f64) -> Self {
        Self {
            base_pts: None,
            base_wall: Instant::now(),
            speed,
        }
    }

    const fn reset(&mut self) {
        self.base_pts = None;
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    async fn wait(&mut self, pts: i64) {
        let now = Instant::now();
        let Some(base) = self.base_pts else {
            self.base_pts = Some(pts);
            self.base_wall = now;
            return;
        };

        let delta = media_delta(pts, base).max(0) as u128;
        if delta > 10 * 90_000 {
            // A jump of more than ten seconds is a discontinuity, not jitter.
            self.base_pts = Some(pts);
            self.base_wall = now;
            return;
        }

        let offset = Duration::from_micros(u64::try_from(delta * 1_000_000 / 90_000).unwrap_or(0))
            .div_f64(self.speed);
        let target = self.base_wall + offset;
        if target > now + MAX_LEAD {
            self.base_wall -= target.duration_since(now).saturating_sub(MAX_LEAD);
        } else if now > target + CATCHUP {
            self.base_wall += now.duration_since(target).saturating_sub(CATCHUP);
        }

        let target = self.base_wall + offset;
        let now = Instant::now();
        if target > now {
            compio::time::sleep(target - now).await;
        }
    }
}

/// Largest DVR segment body accepted from the VOD playlist.
const MAX_VOD_SEGMENT: usize = 64 * 1024 * 1024;
/// VOD is sent ahead of real time so players can buffer and consume faster than
/// 1x. The bounded outbound queue ultimately throttles this to the client's
/// drain rate; the high cap just fills the player's cache quickly after a seek.
const VOD_SPEED: f64 = 16.0;
/// Catch-up pace while replaying the live window after a DVR seek.
const HANDOVER_SPEED: f64 = 4.0;
/// Nominal live segment duration, used to decide whether a live-window segment
/// still overlaps the DVR playhead.
const LIVE_SEGMENT_MS: u64 = 2_000;
/// How far a live segment may start before the DVR playhead and still be the
/// one to resume from. The last DVR sample ends a frame or two before the group
/// boundary, so require the *start* to be within this window.
const LIVE_START_TOLERANCE_MS: u64 = 200;
/// How long a seek waits for the background task to publish the first DVR
/// timeline. Discovery normally finishes in well under a second; a join-time
/// watch-later seek must not be turned into "play live" because of a slow start.
const DVR_WAIT: Duration = Duration::from_secs(5);

/// Register the demuxing consumer with the scheduler's fan-out.
pub fn spawn_media(
    hub: Rc<Stream>,
    client: Rc<Client>,
    dvr: Rc<Dvr>,
    wakes: &Rc<WakeSet>,
    add_output: &kanal::Sender<ConsumerRegistration>,
    live_window: Rc<RefCell<Vec<Segment>>>,
    reader_wake: &Rc<ConsumerWake>,
) {
    let (tx, rx) = kanal::bounded(MAX_CONSUMER_LAG);
    // The RTSP control plane sends seeks through the same ordered consumer
    // channel: kanal async receives must not be used in a select.
    hub.set_seek_sender(tx.clone().to_async());
    // A live RTSP session starts at the live edge: replaying the window would
    // put it seconds behind the pacer and the bounded queue would drop it.
    if let Err(error) = add_output.send(ConsumerRegistration {
        tx,
        backlog: false,
        lossy: true,
    }) {
        error!(%error, "failed to register rtsp consumer");
        return;
    }
    // The scheduler's registration latch parks instead of polling; without
    // this poke a new stream waits out a whole manifest poll interval.
    reader_wake.wake();

    let wake = ConsumerWake::new();
    wakes.register(&wake);

    compio::runtime::spawn(watchdog(Rc::clone(&hub))).detach();
    compio::runtime::spawn(timeline_loop(Rc::clone(&hub), Rc::clone(&live_window))).detach();
    compio::runtime::spawn(async move {
        Media::new(hub, client, dvr, rx.to_async(), wake, live_window)
            .run()
            .await;
    })
    .detach();
}

/// How long a session may stay gated on a seek before the stream is released.
/// A response or media engine that never completes the seek must degrade to a
/// glitch, not to a dead stream that only a reload recovers from.
const SEEK_TIMEOUT: Duration = Duration::from_secs(5);

/// Release sessions whose seek never completed, and say why.
///
/// This also doubles as a liveness check: the whole reactor runs on one
/// thread, so a log line that never appears means some task is spinning or
/// blocking the thread rather than waiting on the driver.
async fn watchdog(hub: Rc<Stream>) {
    loop {
        compio::time::sleep(Duration::from_secs(1)).await;
        if hub.stopped() {
            return;
        }
        let mut stalled = Vec::new();
        for session in hub.sessions.borrow().iter() {
            let Some(started) = session.seek_started.get() else {
                continue;
            };
            if started.elapsed() >= SEEK_TIMEOUT {
                stalled.push((
                    session.seeking.get(),
                    session.playing.get(),
                    session.out.len(),
                    session.out.capacity(),
                    session.range_start.get(),
                    session.epoch.get(),
                ));
            }
        }
        if stalled.is_empty() {
            continue;
        }
        warn!(
            sessions = ?stalled,
            position_ms = hub.position_ms.get(),
            ready = hub.ready.get(),
            "rtsp: seek did not complete, releasing gated sessions"
        );
        hub.end_seek();
    }
}

enum VodOutcome {
    Seek(u64),
    Live,
}

/// Where the media engine is in the DVR -> live transition.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Handover {
    /// Normal live playback.
    None,
    /// The DVR ended; replay the live playlist window from the playhead.
    Replaying,
    /// The window has been replayed; drain the segments queued during VOD.
    Queue,
}

struct Media {
    hub: Rc<Stream>,
    client: Rc<Client>,
    dvr: Rc<Dvr>,
    rx: AsyncReceiver<SegmentMessage>,
    wake: Rc<ConsumerWake>,
    demuxer: TsDemuxer,
    pacer: Pacer,
    timeline: Timeline,
    samples: Vec<Sample>,
    last_sequence: u64,
    last_rtcp: Instant,
    /// Newest live segment seen during normal live playback.
    pending_live: Option<Segment>,
    /// Live segments received while playing VOD, in broadcast order. At the
    /// handover they are replayed from the first one after the DVR playhead, so
    /// the transition starts at the beginning of the next live/DVR group
    /// instead of jumping to the newest segment.
    handover: VecDeque<Segment>,
    handover_state: Handover,
    /// A VOD seek actually played DVR segments (as opposed to a target in the
    /// live window). Used to report the VOD -> live boundary once.
    vod_played: bool,
    /// Scheduler snapshot of the live playlist window, used to bridge the DVR
    /// lag at the handover.
    live_window: Rc<RefCell<Vec<Segment>>>,
    /// A discontinuity happened in the live stream while playing VOD.
    live_discontinuity: bool,
}

impl Media {
    fn new(
        hub: Rc<Stream>,
        client: Rc<Client>,
        dvr: Rc<Dvr>,
        rx: AsyncReceiver<SegmentMessage>,
        wake: Rc<ConsumerWake>,
        live_window: Rc<RefCell<Vec<Segment>>>,
    ) -> Self {
        Self {
            hub,
            client,
            dvr,
            rx,
            wake,
            demuxer: TsDemuxer::new(),
            pacer: Pacer::new(),
            timeline: Timeline::default(),
            samples: Vec::new(),
            last_sequence: 0,
            last_rtcp: Instant::now(),
            pending_live: None,
            handover: VecDeque::new(),
            handover_state: Handover::None,
            vod_played: false,
            live_window,
            live_discontinuity: false,
        }
    }

    async fn run(mut self) {
        loop {
            // The daemon drops a stream when its last viewer leaves; nothing
            // waits for the channel to close first because a VOD fetch or a
            // stalled segment can hold it open for seconds.
            if self.hub.stopped() {
                debug!("rtsp: media engine stopped");
                return;
            }
            if self.handover_state != Handover::None {
                // The DVR playlist lags the live edge by roughly one group.
                // Replay the live window from the DVR playhead so playback
                // continues with the first live segment after the last DVR
                // group instead of jumping to the newest one.
                if self.handover_state == Handover::Replaying {
                    self.handover_state = Handover::Queue;
                    if let Some(seek) = self.replay_live_window().await {
                        self.seek(seek).await;
                        continue;
                    }
                }
                // Drop live segments that end before the DVR playhead. The
                // first kept one starts the next live segment after the last
                // DVR group, so the switch does not skip a group's worth of
                // content (or jump the timestamps).
                while self.handover.front().is_some_and(|segment| {
                    segment.timestamp_ms.saturating_add(LIVE_SEGMENT_MS)
                        < self.hub.position_ms.get()
                }) {
                    self.handover.pop_front();
                }
                if let Some(segment) = self.handover.pop_front() {
                    debug!(
                        sequence = segment.sequence_num,
                        timestamp_ms = segment.timestamp_ms,
                        playhead = self.hub.position_ms.get(),
                        "rtsp: handover play"
                    );
                    if let Some(seek) = self.play_live(SegmentMessage::Segment(segment)).await {
                        self.seek(seek).await;
                    }
                    continue;
                }
                self.handover_state = Handover::None;
                self.pacer = Pacer::new();
            }

            if let Some(segment) = self.pending_live.take() {
                if let Some(seek) = self.play_live(SegmentMessage::Segment(segment)).await {
                    self.seek(seek).await;
                }
                continue;
            }

            let Ok(message) = self.rx.recv().await else {
                return;
            };
            match message {
                SegmentMessage::Seek(target) => self.seek(target).await,
                message => {
                    if let Some(seek) = self.play_live(message).await {
                        self.seek(seek).await;
                    }
                }
            }
        }
    }

    async fn seek(&mut self, mut target: u64) {
        // Live segments received during the seek/VOD are kept in order. At the
        // handover they are replayed from the first one after the DVR playhead,
        // which is the start of the next live segment below the DVR grouping.
        self.handover.clear();
        self.handover_state = Handover::Replaying;
        self.vod_played = false;
        self.pending_live = None;
        // The DVR task discovers and refreshes the playlist in parallel. On a
        // fresh start a seek (typically mpv's watch-later resume) can arrive
        // before the first fetch completes; wait for it rather than snapping
        // back to live. Newer seeks and live traffic are handled meanwhile.
        let deadline = Instant::now() + DVR_WAIT;
        while self.dvr.is_empty() {
            if Instant::now() >= deadline {
                debug!("rtsp: no DVR timeline, playing live");
                break;
            }
            if let Some(seek) = drain_messages(
                &self.rx,
                &mut self.pending_live,
                &mut self.handover,
                self.handover_state != Handover::None,
                &mut self.last_sequence,
                &mut self.live_discontinuity,
            ) {
                target = seek;
            }
            compio::time::sleep(Duration::from_millis(50)).await;
        }

        // Send the first RTCP report of the new epoch right after the first
        // sample, so the receiver switches to the fresh media-clock mapping
        // before it has buffered any meaningful amount of audio or video.
        self.last_rtcp = Instant::now()
            .checked_sub(Duration::from_secs(10))
            .unwrap_or_else(Instant::now);
        while let VodOutcome::Seek(next) = self.play_vod(target).await {
            target = next;
        }
        // No DVR timeline (or nothing to play): release the sessions that the
        // seek gated; they continue live from the current epoch.
        self.hub.end_seek();

        if self.vod_played && self.hub.has_sessions() {
            // The DVR ended and the media engine is switching this client to
            // the live edge. Report the boundary and how far the DVR trailed
            // live (the first live-window segment after the DVR playhead).
            let playhead = self.hub.position_ms.get();
            let (live_start, live_edge, sequence) = {
                let window = self.live_window.borrow();
                let start = window.iter().find(|segment| {
                    segment.timestamp_ms.saturating_add(LIVE_START_TOLERANCE_MS) >= playhead
                });
                start.map_or((playhead, playhead, 0), |segment| {
                    (
                        segment.timestamp_ms,
                        window.last().map_or(segment.timestamp_ms, |last| {
                            last.timestamp_ms.saturating_add(LIVE_SEGMENT_MS)
                        }),
                        segment.sequence_num,
                    )
                })
            };
            info!(
                target_ms = target,
                playhead_ms = playhead,
                live_start_ms = live_start,
                live_edge_ms = live_edge,
                dvr_lag_ms = live_edge.saturating_sub(playhead),
                sequence,
                "rtsp: client crossed VOD -> live boundary"
            );
        }

        // The DVR and live PES timestamps are continuous (the DVR files are
        // concatenated live segments), so the handover queue is replayed in
        // order. Give it a catch-up pace, then settle back to 1x live.
        debug!(
            target_ms = target,
            vod_played = self.vod_played,
            handover = ?self.handover_state,
            handover_segments = self.handover.len(),
            pending_live = self.pending_live.is_some(),
            playhead_ms = self.hub.position_ms.get(),
            "rtsp: seek finished"
        );
        if self.handover_state == Handover::None {
            self.pacer = Pacer::new();
        } else {
            self.pacer = Pacer::with_speed(HANDOVER_SPEED);
        }
    }

    /// Replay the live playlist window from the current broadcast playhead.
    ///
    /// The DVR playlist ends about one group (~10 s) behind the live edge, so
    /// jumping straight to the newest live spool skips that group. The live
    /// playlist still lists it; those spools are alive in the scheduler's
    /// window, so replay them in order before the queued live segments.
    async fn replay_live_window(&mut self) -> Option<u64> {
        let playhead = self.hub.position_ms.get();
        let window = self.live_window.borrow().clone();
        let start = window.iter().position(|segment| {
            segment.timestamp_ms.saturating_add(LIVE_START_TOLERANCE_MS) >= playhead
        })?;
        self.last_sequence = window[start].sequence_num.saturating_sub(1);
        debug!(
            playhead,
            window = window.len(),
            start_ms = window[start].timestamp_ms,
            newest_ms = window.last().map(|segment| segment.timestamp_ms),
            sequence = window[start].sequence_num,
            "rtsp: replaying live window at DVR handover"
        );

        // Follow the window as the scheduler refreshes it. A one-shot snapshot
        // ends before the segments that arrive while the replay is running, and
        // a message the bounded queue could not take during a download stall
        // would then leave a hole. Re-reading the window heals both.
        loop {
            let window = self.live_window.borrow().clone();
            let segment = window
                .into_iter()
                .find(|segment| segment.sequence_num > self.last_sequence)?;
            debug!(
                sequence = segment.sequence_num,
                timestamp_ms = segment.timestamp_ms,
                "rtsp: live window segment"
            );
            if let Some(seek) = self.play_live(SegmentMessage::Segment(segment)).await {
                return Some(seek);
            }
        }
    }

    /// Handle one scheduler message. Returns a seek target if the control plane
    /// interrupted playback.
    async fn play_live(&mut self, message: SegmentMessage) -> Option<u64> {
        match message {
            SegmentMessage::Segment(segment) => self.play_live_segment(segment).await,
            SegmentMessage::Discontinuity() => {
                debug!("rtsp: discontinuity");
                self.last_sequence = 0;
                self.demuxer.reset();
                self.pacer.reset();
                None
            }
            // The scheduler starts downloading prefetch segments before they
            // are listed as complete; playing them immediately removes the
            // prefetch lead from the live latency. The matching `Segment`
            // message is skipped by the sequence filter.
            SegmentMessage::Prefetch {
                expected_sequence,
                timestamp_ms,
                reader,
            } => {
                debug!(
                    sequence = expected_sequence,
                    timestamp_ms, "rtsp: prefetch segment"
                );
                self.play_live_segment(Segment {
                    timestamp_ms,
                    sequence_num: expected_sequence,
                    reader,
                })
                .await
            }
            // TS has no init segment; fMP4 is not supported by this output yet.
            SegmentMessage::Init { .. } => None,
            SegmentMessage::Seek(target) => Some(target),
        }
    }

    async fn play_live_segment(&mut self, segment: Segment) -> Option<u64> {
        if segment.sequence_num <= self.last_sequence {
            return None;
        }
        self.last_sequence = segment.sequence_num;
        if self.live_discontinuity {
            self.live_discontinuity = false;
            self.demuxer.reset();
            self.pacer.reset();
        }
        self.timeline.start(segment.timestamp_ms);

        let mut reader = segment.reader;
        while let Some(view) = reader.next_view(&self.wake).await {
            if let Some(seek) = drain_messages(
                &self.rx,
                &mut self.pending_live,
                &mut self.handover,
                self.handover_state != Handover::None,
                &mut self.last_sequence,
                &mut self.live_discontinuity,
            ) {
                return Some(seek);
            }
            self.demuxer.feed(view.as_init(), &mut self.samples);
            sync_config(&self.hub, &self.demuxer);
            if self.hub.video_base.get().is_none() {
                let base = self
                    .samples
                    .iter()
                    .map(|sample| match sample {
                        Sample::Video(video) => video.pts,
                        Sample::Audio(audio) => audio.pts,
                    })
                    .min();
                if let Some(base) = base {
                    self.hub.set_media_base(base);
                }
            }
            for sample in self.samples.drain(..) {
                if let Some(seek) = drain_messages(
                    &self.rx,
                    &mut self.pending_live,
                    &mut self.handover,
                    self.handover_state != Handover::None,
                    &mut self.last_sequence,
                    &mut self.live_discontinuity,
                ) {
                    return Some(seek);
                }
                emit_sample(&self.hub, &mut self.pacer, &mut self.timeline, sample).await;
                maybe_rtcp(&self.hub, &mut self.last_rtcp);
            }
        }

        self.demuxer.finish_segment(&mut self.samples);
        sync_config(&self.hub, &self.demuxer);
        for sample in self.samples.drain(..) {
            if let Some(seek) = drain_messages(
                &self.rx,
                &mut self.pending_live,
                &mut self.handover,
                self.handover_state != Handover::None,
                &mut self.last_sequence,
                &mut self.live_discontinuity,
            ) {
                return Some(seek);
            }
            emit_sample(&self.hub, &mut self.pacer, &mut self.timeline, sample).await;
            maybe_rtcp(&self.hub, &mut self.last_rtcp);
        }
        self.hub.mark_ready();
        None
    }

    /// Play the DVR timeline from `target_ms` until the live edge or a seek.
    async fn play_vod(&mut self, target_ms: u64) -> VodOutcome {
        // A seek can land outside the DVR range (typically at the live edge).
        // Start it on a fresh RTP epoch either way: otherwise the first
        // live-window frame is timestamped from the old VOD base and the
        // player waits on a position far in the future.
        self.demuxer = TsDemuxer::new();
        self.pacer = Pacer::with_speed(VOD_SPEED);
        self.timeline = Timeline::default();
        self.samples.clear();
        self.hub.reset_media_base();
        self.hub.set_position_ms(target_ms);

        let Some((mut url, mut start_ms, mut duration_ms)) = self.dvr.segment_at(target_ms) else {
            // The target is outside the DVR timeline (usually the live window).
            debug!(
                target_ms,
                dvr_end_ms = self.dvr.end_ms(),
                "rtsp: VOD target outside timeline, handing over to live"
            );
            return VodOutcome::Live;
        };
        debug!(target_ms, dvr_end_ms = self.dvr.end_ms(), "rtsp: VOD seek");

        let mut first = true;
        let mut prefetched: Option<(Vec<u8>, u64)> = None;

        loop {
            if self.hub.stopped() {
                return VodOutcome::Live;
            }
            if let Some(seek) = drain_messages(
                &self.rx,
                &mut self.pending_live,
                &mut self.handover,
                self.handover_state != Handover::None,
                &mut self.last_sequence,
                &mut self.live_discontinuity,
            ) {
                return VodOutcome::Seek(seek);
            }

            // Nobody is watching any more: stop pulling the archive.
            if !self.hub.has_sessions() {
                debug!("rtsp: VOD stopped: no sessions");
                return VodOutcome::Live;
            }

            let bytes = if let Some((bytes, start)) = prefetched.take() {
                debug_assert_eq!(start, start_ms);
                bytes
            } else {
                debug!(url = %url, start_ms, "rtsp: VOD fetching segment");
                let Some(bytes) = self.fetch_segment(&url).await else {
                    warn!(url = %url, "rtsp: VOD segment fetch failed");
                    return VodOutcome::Live;
                };
                bytes
            };
            self.vod_played = true;

            self.samples.clear();
            self.demuxer.feed(&bytes, &mut self.samples);
            self.demuxer.finish_segment(&mut self.samples);
            sync_config(&self.hub, &self.demuxer);

            let start_index = if first {
                vod_start_index(&self.samples, start_ms, target_ms)
            } else {
                0
            };
            if first {
                // Anchor the RTP epochs on the earliest PES time we will
                // actually send. Otherwise a later sample can precede the base
                // and the player sees duplicate or wrapped timestamps.
                let base = self
                    .samples
                    .iter()
                    .skip(start_index)
                    .map(|sample| match sample {
                        Sample::Video(video) => video.pts,
                        Sample::Audio(audio) => audio.pts,
                    })
                    .min();
                if let Some(base) = base {
                    self.hub.set_media_base(base);
                }
            } else if self.hub.video_base.get().is_none() {
                // The first segment had nothing to send (an ad, or a truncated
                // fetch). The session is still gated on the seek; anchor on the
                // first segment that does carry samples or it stays silent.
                let base = self
                    .samples
                    .iter()
                    .map(|sample| match sample {
                        Sample::Video(video) => video.pts,
                        Sample::Audio(audio) => audio.pts,
                    })
                    .min();
                if let Some(base) = base {
                    self.hub.set_media_base(base);
                }
            }
            let skipped_ms = samples_skipped_ms(&self.samples, start_index);
            let first_video_pts = self.samples.iter().find_map(|sample| match sample {
                Sample::Video(video) => Some(video.pts),
                Sample::Audio(_) => None,
            });
            let chosen_pts = self
                .samples
                .get(start_index)
                .and_then(|sample| match sample {
                    Sample::Video(video) => Some(video.pts),
                    Sample::Audio(_) => None,
                });
            let last_video_pts = self.samples.iter().rev().find_map(|sample| match sample {
                Sample::Video(video) => Some(video.pts),
                Sample::Audio(_) => None,
            });
            debug!(
                start_ms,
                target_ms,
                first_video_pts,
                chosen_pts,
                last_video_pts,
                skipped_ms,
                samples = self.samples.len(),
                "rtsp: VOD segment loaded"
            );
            self.timeline.start(start_ms.saturating_add(skipped_ms));
            first = false;

            for sample in self.samples.drain(start_index..) {
                if let Some(seek) = drain_messages(
                    &self.rx,
                    &mut self.pending_live,
                    &mut self.handover,
                    self.handover_state != Handover::None,
                    &mut self.last_sequence,
                    &mut self.live_discontinuity,
                ) {
                    return VodOutcome::Seek(seek);
                }
                emit_sample(&self.hub, &mut self.pacer, &mut self.timeline, sample).await;
                maybe_rtcp(&self.hub, &mut self.last_rtcp);
            }
            self.hub.mark_ready();

            // The refresh task replaces the whole playlist, so the next
            // segment is looked up by broadcast time, never by list index.
            let next_position = start_ms.saturating_add(duration_ms);
            let Some((next_url, next_start, next_duration)) = self.dvr.segment_at(next_position)
            else {
                // Reached the published DVR end: hand over to live.
                debug!(
                    next_position,
                    dvr_end_ms = self.dvr.end_ms(),
                    "rtsp: VOD reached published end"
                );
                return VodOutcome::Live;
            };
            if next_start <= start_ms {
                // The refreshed playlist has not moved past this segment yet.
                debug!(start_ms, next_start, "rtsp: VOD playlist has not advanced");
                return VodOutcome::Live;
            }
            url = next_url;
            start_ms = next_start;
            duration_ms = next_duration;
        }
    }

    async fn fetch_segment(&self, url: &str) -> Option<Vec<u8>> {
        let mut response = self.client.get(url).await.ok()?;
        if !response.status.is_success() {
            return None;
        }
        response.body.read_to_end(MAX_VOD_SEGMENT).await.ok()
    }
}

/// Drain queued live messages. During VOD the segments are kept in order for
/// the handover; during live only the newest one matters. Returns the latest
/// seek request.
fn drain_messages(
    rx: &AsyncReceiver<SegmentMessage>,
    pending_live: &mut Option<Segment>,
    handover: &mut VecDeque<Segment>,
    queue: bool,
    last_sequence: &mut u64,
    live_discontinuity: &mut bool,
) -> Option<u64> {
    let mut seek = None;
    while let Ok(Some(message)) = rx.try_recv() {
        match message {
            SegmentMessage::Segment(segment) => {
                if queue {
                    push_handover(handover, segment);
                } else if pending_live
                    .as_ref()
                    .is_none_or(|pending| segment.sequence_num > pending.sequence_num)
                {
                    *pending_live = Some(segment);
                }
            }
            SegmentMessage::Discontinuity() => {
                *last_sequence = 0;
                *live_discontinuity = true;
            }
            SegmentMessage::Seek(target) => seek = Some(target),
            SegmentMessage::Prefetch {
                expected_sequence,
                timestamp_ms,
                reader,
            } => {
                let candidate = Segment {
                    timestamp_ms,
                    sequence_num: expected_sequence,
                    reader,
                };
                if queue {
                    push_handover(handover, candidate);
                } else if pending_live
                    .as_ref()
                    .is_none_or(|pending| candidate.sequence_num > pending.sequence_num)
                {
                    *pending_live = Some(candidate);
                }
            }
            SegmentMessage::Init { .. } => {}
        }
    }
    seek
}

/// Keep roughly the DVR lag's worth of live segments for the handover.
///
/// Prefetch and playlist messages can arrive out of order for the same window
/// (a prefetch tag for the next segment may precede the main dispatch of the
/// current one), so insert by sequence. Appending would let the queue play
/// 14201 before a late 14200, and the sequence filter would then drop 14200.
fn push_handover(handover: &mut VecDeque<Segment>, segment: Segment) {
    const MAX_HANDOVER: usize = 12;
    match handover
        .iter()
        .position(|queued| queued.sequence_num >= segment.sequence_num)
    {
        // The same segment was already queued from the other message kind.
        Some(index) if handover[index].sequence_num == segment.sequence_num => return,
        Some(index) => handover.insert(index, segment),
        None => handover.push_back(segment),
    }
    if handover.len() > MAX_HANDOVER {
        handover.pop_front();
    }
}

fn maybe_rtcp(hub: &Stream, last_rtcp: &mut Instant) {
    if last_rtcp.elapsed() >= Duration::from_secs(1) {
        hub.send_rtcp();
        *last_rtcp = Instant::now();
    }
}

/// Index into a demuxed DVR segment at which playback should start so that the
/// first video sample is a keyframe at (or as close as possible before) `target`.
fn vod_start_index(samples: &[Sample], segment_start_ms: u64, target_ms: u64) -> usize {
    let Some(first_pts) = samples.iter().find_map(|sample| match sample {
        Sample::Video(video) => Some(video.pts),
        Sample::Audio(_) => None,
    }) else {
        return 0;
    };

    let target_offset =
        i64::try_from(target_ms.saturating_sub(segment_start_ms)).unwrap_or(i64::MAX);
    let target_pts = first_pts.saturating_add(target_offset.saturating_mul(90));

    let mut chosen = None;
    for (index, sample) in samples.iter().enumerate() {
        let Sample::Video(video) = sample else {
            continue;
        };
        if !video.keyframe {
            continue;
        }
        if video.pts <= target_pts {
            chosen = Some(index);
        } else {
            if chosen.is_none() {
                chosen = Some(index);
            }
            break;
        }
    }
    chosen.unwrap_or(0)
}

/// Milliseconds between the first video sample and the chosen start index.
fn samples_skipped_ms(samples: &[Sample], index: usize) -> u64 {
    let first_pts = samples.iter().find_map(|sample| match sample {
        Sample::Video(video) => Some(video.pts),
        Sample::Audio(_) => None,
    });
    let chosen_pts = samples.get(index).and_then(|sample| match sample {
        Sample::Video(video) => Some(video.pts),
        Sample::Audio(_) => None,
    });
    match (first_pts, chosen_pts) {
        (Some(first), Some(chosen)) => u64::try_from((chosen - first).max(0) / 90).unwrap_or(0),
        _ => 0,
    }
}

fn sync_config(hub: &Stream, demuxer: &TsDemuxer) {
    if demuxer.video_config().is_ready() {
        let mut slot = hub.video_config.borrow_mut();
        if slot.as_ref() != Some(demuxer.video_config()) {
            *slot = Some(demuxer.video_config().clone());
        }
    }
    if let Some(audio) = demuxer.audio_config() {
        hub.set_audio_rate(audio.sample_rate);
        let mut slot = hub.audio_config.borrow_mut();
        if slot.as_ref() != Some(audio) {
            *slot = Some(audio.clone());
        }
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
async fn emit_sample(hub: &Stream, pacer: &mut Pacer, timeline: &mut Timeline, sample: Sample) {
    match sample {
        Sample::Video(video) => {
            if video.keyframe {
                debug!(pts = video.pts, dts = video.dts, "rtsp: video keyframe");
            }
            // Pace on DTS: that is the order the packets leave the decoder.
            // VOD playback skips the clock and fills the receiver's queue
            // instead, so it can run ahead of real time.
            pacer.wait(video.dts).await;
            let position = timeline.position_ms(video.dts);
            hub.set_position_ms(position);

            // The media clock is absolute broadcast NPT, not an epoch offset.
            // The player's start time is the beacon's 0, so the position it
            // displays is this value directly.
            let timestamp = position.saturating_mul(90) as u32;
            let media = position.saturating_mul(90) as i64;
            hub.wait_out_capacity(TRACK_VIDEO, video_packet_count(&video.nals))
                .await;

            hub.video_keyframe.set(video.keyframe);
            let mut scratch = Vec::new();
            h264::packetize(
                &video.data,
                &video.nals,
                RTP_MAX_PAYLOAD,
                &mut scratch,
                |payload, marker| {
                    let packet = hub
                        .video
                        .borrow_mut()
                        .packet(timestamp, media, marker, payload);
                    hub.send(TRACK_VIDEO, &packet);
                },
            );
            hub.video_keyframe.set(false);
        }
        Sample::Audio(audio) => {
            let clock_rate = hub.audio.borrow().clock_rate;
            pacer.wait(audio.pts).await;
            let position = timeline.position_ms(audio.pts);
            hub.set_position_ms(position);

            let timestamp = (position.saturating_mul(u64::from(clock_rate)) / 1000) as u32;
            let media = position.saturating_mul(90) as i64;
            hub.wait_out_capacity(TRACK_AUDIO, 1).await;

            let payload = aac::packetize(&audio.frame);
            let packet = hub
                .audio
                .borrow_mut()
                .packet(timestamp, media, true, &payload);
            hub.send(TRACK_AUDIO, &packet);
        }
    }
}

/// Upper bound on the RTP packets one access unit will produce, used to reserve
/// room in the receiver queues before packetizing.
fn video_packet_count(nals: &[(usize, usize)]) -> usize {
    let fragment = RTP_MAX_PAYLOAD.saturating_sub(2).max(1);
    nals.iter()
        .map(|&(start, end)| (end - start).div_ceil(fragment).max(1))
        .sum::<usize>()
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_play_listeners_share_a_port() {
        compio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(async {
                // Port 0: the test must not fight a running `play`.
                let listeners = bind_loopbacks(0).expect("bind");
                assert!(!listeners.is_empty());
                let port = listeners[0].local_addr().expect("local addr").port();
                for listener in &listeners {
                    assert_eq!(listener.local_addr().expect("local addr").port(), port);
                    let local = listener.local_addr().expect("local addr");
                    let (connected, accepted) =
                        futures_util::future::join(TcpStream::connect(local), listener.accept())
                            .await;
                    connected.expect("connect");
                    accepted.expect("accept");
                }
            });
    }

    #[test]
    fn parses_udp_and_tcp_transports() {
        let udp = parse_transport("RTP/AVP;unicast;client_port=5000-5001").expect("udp");
        assert!(!udp.tcp);
        assert_eq!(udp.client_port, (5000, 5001));

        let tcp = parse_transport("RTP/AVP/TCP;unicast;interleaved=2-3").expect("tcp");
        assert!(tcp.tcp);
        assert_eq!(tcp.interleaved, (2, 3));
    }

    #[test]
    fn resolves_control_urls_to_tracks() {
        assert_eq!(
            track_from_uri("rtsp://127.0.0.1:8554/twitch/trackID=0"),
            Some(TRACK_VIDEO)
        );
        assert_eq!(
            track_from_uri("rtsp://127.0.0.1:8554/twitch/trackID=1"),
            Some(TRACK_AUDIO)
        );
        assert_eq!(track_from_uri("rtsp://127.0.0.1:8554/twitch"), None);
    }

    #[test]
    fn parses_the_stream_key_from_the_request_uri() {
        let key = parse_stream_key("rtsp://127.0.0.1:7417/Sunnys/720p60/").expect("key");
        assert_eq!(key.login, "sunnys");
        assert_eq!(key.quality.as_deref(), Some("720p60"));

        // A missing quality means the first variant.
        let key = parse_stream_key("rtsp://127.0.0.1:7417/sunnys/").expect("key");
        assert_eq!(key.quality, None);

        // The track control URI of a stream without a quality is not a quality.
        let key = parse_stream_key("rtsp://127.0.0.1:7417/sunnys/trackID=0").expect("key");
        assert_eq!(key.quality, None);

        // Absolute paths are accepted too.
        assert_eq!(
            parse_stream_key("/sunnys/audio_only").expect("key"),
            StreamKey {
                login: "sunnys".to_owned(),
                quality: Some("audio_only".to_owned()),
            }
        );

        assert!(parse_stream_key("rtsp://127.0.0.1:7417/").is_none());
        assert!(parse_stream_key("").is_none());
    }

    #[test]
    fn sender_report_is_a_valid_compound_packet() {
        let mut track = Track::new(sdp::VIDEO_PAYLOAD_TYPE, 90_000, 0x1234_5678);
        track.packet(0, 0, true, &[1, 2, 3]);

        let report = track.sender_report(0).expect("report");
        // Sender report: 28 bytes, length field 6.
        assert_eq!(report[1], 200);
        assert_eq!(u16::from_be_bytes([report[2], report[3]]), 6);
        // SDES follows at 28 bytes and must consume the rest of the compound.
        assert_eq!(report[29], 202);
        let sdes_len = u16::from_be_bytes([report[30], report[31]]);
        assert_eq!(usize::from(sdes_len) * 4 + 4, report.len() - 28);
    }

    #[test]
    fn reset_mapping_keeps_the_rtp_sequence_running() {
        let mut track = Track::new(sdp::VIDEO_PAYLOAD_TYPE, 90_000, 0x1234_5678);
        track.packet(0, 0, true, &[1]);
        let expected = track.next_seq;
        track.reset_mapping();
        assert_eq!(track.next_seq, expected, "a seek must not restart at 1");

        let packet = track.packet(0, 0, true, &[1]);
        assert_eq!(u16::from_be_bytes([packet[2], packet[3]]), expected);
    }

    #[test]
    fn timeline_maps_pes_time_onto_the_broadcast() {
        let mut timeline = Timeline::default();
        timeline.start(10_000);
        assert_eq!(timeline.position_ms(90_000), 10_000);
        // 90 kHz: 90 000 ticks is one second.
        assert_eq!(timeline.position_ms(180_000), 11_000);
    }

    #[test]
    fn parses_a_request_head() {
        let head = b"DESCRIBE rtsp://127.0.0.1:8554/twitch RTSP/1.0\r\nCSeq: 7\r\nTransport: RTP/AVP;unicast\r\n";
        let request = parse_request(head).expect("request");
        assert_eq!(request.method, "DESCRIBE");
        assert_eq!(request.uri, "rtsp://127.0.0.1:8554/twitch");
        assert_eq!(request.cseq, "7");
        assert!(request.transport.is_some());
    }
}
