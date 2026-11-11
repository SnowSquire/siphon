//! Minimal RTSP server with ES-over-RTP output.
//!
//! One TS consumer demuxes the live segment stream, packetizes H.264 and AAC and
//! fans the RTP packets out to connected sessions. RTSP is only control: SDP from
//! the discovered codec configuration, one UDP or interleaved-TCP transport per
//! track, and RTCP sender reports for the NTP/RTP mapping.

mod aac;
mod h264;
mod sdp;
mod ts;
mod vod;

use std::cell::Cell;
use std::cell::RefCell;
use std::fmt::Write as _;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::rc::Rc;
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
use kanal::AsyncReceiver;
use tracing::debug;
use tracing::error;
use tracing::warn;

use crate::constants::DEFAULT_RTSP_PORT;
use crate::constants::MAX_CONSUMER_LAG;
use crate::constants::MAX_MANIFEST;
use crate::constants::RTP_MAX_PAYLOAD;
use crate::http::Client;
use crate::rtsp::aac::AudioConfig;
use crate::rtsp::h264::H264Config;
use crate::rtsp::ts::Sample;
use crate::rtsp::ts::TsDemuxer;
use crate::rtsp::vod::VodTimeline;
use crate::scheduler::ConsumerRegistration;
use crate::scheduler::Segment;
use crate::scheduler::SegmentMessage;
use crate::spool::ConsumerWake;
use crate::spool::WakeSet;

const TRACK_VIDEO: u8 = 0;
const TRACK_AUDIO: u8 = 1;
const TRACK_COUNT: usize = 2;

/// RTP timestamps wrap at 2^32, and PTS wraps at 2^33; deltas are taken modulo
/// the PTS wrap so both wrap cleanly.
const PTS_WRAP: i64 = 1 << 33;

const NTP_UNIX_OFFSET: u64 = 2_208_988_800;
const SDES_CNAME: &[u8] = b"twitch-hls-client2";

/// PTS delta from a shared epoch, handling the 33-bit wrap.
///
/// Audio PES packets at a seek keyframe can be a few frames ahead of the video
/// base, which would otherwise become a near-2^33 delta and a wrapped RTP
/// timestamp. Treat small negative offsets as zero; only large negative values
/// are a real wrap.
fn media_delta(pts: i64, base: i64) -> i64 {
    let delta = pts - base;
    if (-10 * 90_000..0).contains(&delta) {
        0
    } else {
        delta.rem_euclid(PTS_WRAP)
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
    last_rtp: u32,
    last_ntp: u64,
    /// Wall-clock instant [`Self::last_ntp`] was sampled at. The sender report
    /// extrapolates both clocks from here, like gortsplib's `rtpsender`.
    last_system: Instant,
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
            last_rtp: 0,
            last_ntp: 0,
            last_system: Instant::now(),
            packet_count: 0,
            octet_count: 0,
        }
    }

    /// Build one RTP packet around `payload`, consuming a sequence number.
    fn packet(&mut self, timestamp: u32, marker: bool, payload: &[u8], ntp: u64) -> Vec<u8> {
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
        self.last_rtp = timestamp;
        self.last_ntp = ntp;
        self.last_system = Instant::now();
        self.packet_count = self.packet_count.wrapping_add(1);

        self.octet_count = self
            .octet_count
            .wrapping_add(u32::try_from(payload.len()).unwrap_or(u32::MAX));
        packet
    }

    /// Forget the packet timestamp used by RTCP sender reports while keeping
    /// the cumulative packet/octet counters.
    fn reset_mapping(&mut self) {
        self.last_rtp = 0;
        self.last_ntp = ntp_now();
        self.last_system = Instant::now();
    }

    /// RTCP sender report plus an SDES CNAME item.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn sender_report(&self) -> Option<Vec<u8>> {
        if self.packet_count == 0 {
            return None;
        }

        // Extrapolate both clocks from the last sent packet so the mapping stays
        // current between media packets (gortsplib's rtpsender does the same).
        let elapsed = self.last_system.elapsed();
        let ticks = (elapsed.as_secs_f64() * f64::from(self.clock_rate)) as u32;
        let rtp = self.last_rtp.wrapping_add(ticks);
        let ntp_fraction = (elapsed.as_nanos() << 32) / 1_000_000_000;
        let ntp = self
            .last_ntp
            .wrapping_add(u64::try_from(ntp_fraction).unwrap_or(u64::MAX));

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
    out: kanal::Sender<Outbound>,
    /// Bit set of tracks this session has set up.
    subscribed: Cell<u8>,
    playing: Cell<bool>,
    /// The client paused. ffmpeg's seek path always sends PAUSE before the PLAY
    /// that carries the target range, which distinguishes a seek from the first
    /// PLAY (or a saved resume position in the initial PLAY).
    paused: Cell<bool>,
    /// NPT offset last advertised in a PLAY response. Kept unchanged for
    /// pause/resume so ffmpeg's `range_start_offset` does not jump.
    range_start: Cell<f64>,
}

enum Outbound {
    Rtsp(Vec<u8>),
    Setup { track: u8, transport: Transport },
    Rtp { track: u8, data: Vec<u8> },
    Rtcp { track: u8, data: Vec<u8> },
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
pub struct Hub {
    /// Streamer/channel name; the SDP session name and player title.
    title: String,
    sessions: RefCell<Vec<Rc<Session>>>,
    video_config: RefCell<Option<H264Config>>,
    audio_config: RefCell<Option<AudioConfig>>,
    ready: Cell<bool>,
    base_pts: Cell<Option<i64>>,
    /// Current playhead in milliseconds since the start of the broadcast.
    position_ms: Cell<u64>,
    /// Seek requests from RTSP control to the media engine (consumer channel).
    seek_tx: RefCell<Option<kanal::AsyncSender<SegmentMessage>>>,
    video: RefCell<Track>,
    audio: RefCell<Track>,
    next_session: Cell<u32>,
}

impl Hub {
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            sessions: RefCell::new(Vec::new()),
            video_config: RefCell::new(None),
            audio_config: RefCell::new(None),
            ready: Cell::new(false),
            base_pts: Cell::new(None),
            position_ms: Cell::new(0),
            seek_tx: RefCell::new(None),
            video: RefCell::new(Track::new(
                sdp::VIDEO_PAYLOAD_TYPE,
                sdp::VIDEO_CLOCK_RATE,
                0x5A5A_0001,
            )),
            audio: RefCell::new(Track::new(sdp::AUDIO_PAYLOAD_TYPE, 48_000, 0x5A5A_0002)),
            next_session: Cell::new(0),
        }
    }

    /// The first segment has been demuxed: DESCRIBE can build an SDP.
    fn mark_ready(&self) {
        self.ready.set(true);
    }

    async fn wait_ready(&self) -> bool {
        for _ in 0..100 {
            if self.ready.get() {
                return true;
            }
            compio::time::sleep(Duration::from_millis(100)).await;
        }
        self.ready.get()
    }

    fn next_session_id(&self) -> String {
        let value = self.next_session.get().wrapping_add(1);
        self.next_session.set(value);
        format!("{:08x}", value ^ 0x5A5A_0000)
    }

    fn add_session(&self, session: Rc<Session>) {
        self.sessions.borrow_mut().push(session);
    }

    fn remove_session(&self, session: &Rc<Session>) {
        self.sessions
            .borrow_mut()
            .retain(|candidate| !Rc::ptr_eq(candidate, session));
    }

    fn has_track(&self, track: u8) -> bool {
        match track {
            TRACK_VIDEO => self.video_config.borrow().is_some(),
            TRACK_AUDIO => self.audio_config.borrow().is_some(),
            _ => false,
        }
    }

    /// Fan one RTP packet out to every playing session subscribed to `track`.
    fn send(&self, track: u8, packet: &[u8]) {
        let bit = 1u8 << track;
        self.sessions.borrow_mut().retain(|session| {
            if !session.playing.get() || session.subscribed.get() & bit == 0 {
                return true;
            }
            session
                .out
                .try_send(Outbound::Rtp {
                    track,
                    data: packet.to_vec(),
                })
                .is_ok_and(|sent| sent)
        });
    }

    fn send_rtcp(&self) {
        if let Some(report) = self.video.borrow().sender_report() {
            self.send_rtcp_packet(TRACK_VIDEO, &report);
        }
        if let Some(report) = self.audio.borrow().sender_report() {
            self.send_rtcp_packet(TRACK_AUDIO, &report);
        }
    }

    fn send_rtcp_packet(&self, track: u8, packet: &[u8]) {
        let bit = 1u8 << track;
        self.sessions.borrow_mut().retain(|session| {
            if !session.playing.get() || session.subscribed.get() & bit == 0 {
                return true;
            }
            session
                .out
                .try_send(Outbound::Rtcp {
                    track,
                    data: packet.to_vec(),
                })
                .is_ok_and(|sent| sent)
        });
    }

    /// Shared 90 kHz epoch for both tracks, so the A/V offset is preserved.
    fn media_base(&self, pts: i64) -> i64 {
        self.base_pts.get().unwrap_or_else(|| {
            self.base_pts.set(Some(pts));
            pts
        })
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn rtp_timestamp(&self, pts: i64, clock_rate: u32) -> u32 {
        let base = self.media_base(pts);
        let delta = media_delta(pts, base) as u128;
        let scaled = delta * u128::from(clock_rate) / 90_000;
        (scaled as u32).wrapping_add(0)
    }

    fn set_position_ms(&self, position_ms: u64) {
        self.position_ms.set(position_ms);
    }

    /// Discard the shared RTP timestamp epoch; the next sample starts a new one.
    /// The RTCP sender reports must forget the old mapping too, or a report sent
    /// before the first post-seek packet pairs an old RTP timestamp with a new
    /// NTP time and players remap the whole stream incorrectly.
    fn reset_media_base(&self) {
        debug!("rtsp: reset RTP/RTCP timestamp mapping");
        self.base_pts.set(None);
        self.video.borrow_mut().reset_mapping();
        self.audio.borrow_mut().reset_mapping();
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

/// Bind the RTSP listener on both loopback families.
pub fn spawn_server(hub: Rc<Hub>) {
    compio::runtime::spawn(async move {
        const ADDRS: [SocketAddr; 4] = [
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), DEFAULT_RTSP_PORT),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_RTSP_PORT),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        ];

        // compio's `bind` takes the first address that works, so each loopback
        // family gets its own listener (and its own accept loop).
        for addr in ADDRS {
            let Ok(listener) = TcpListener::bind(addr).await else {
                continue;
            };
            let local = listener.local_addr().expect("local addr");
            println!("rtsp://{local}/");
            compio::runtime::spawn(accept_loop(listener, Rc::clone(&hub))).detach();
        }
    })
    .detach();
}

async fn accept_loop(listener: TcpListener, hub: Rc<Hub>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                if stream.set_nodelay(true).is_err() {
                    error!(%peer, "failed to set TCP no delay");
                    continue;
                }
                compio::runtime::spawn(control_session(stream, peer, Rc::clone(&hub))).detach();
            }
            Err(error) => warn!(%error, "rtsp accept failed"),
        }
    }
}

async fn control_session(stream: TcpStream, peer: SocketAddr, hub: Rc<Hub>) {
    let local = stream.local_addr().unwrap_or(peer);
    let (mut read, write) = stream.into_split();
    let (out_tx, out_rx) = kanal::bounded(4096);
    compio::runtime::spawn(writer(write, out_rx.to_async())).detach();

    let session = Rc::new(Session {
        out: out_tx,
        subscribed: Cell::new(0),
        playing: Cell::new(false),
        paused: Cell::new(false),
        range_start: Cell::new(0.0),
    });
    hub.add_session(Rc::clone(&session));
    let id = hub.next_session_id();
    debug!(%peer, %id, "rtsp client connected");

    let mut buffer = Vec::new();
    while let Some(request) = read_request(&mut read, &mut buffer).await {
        if !handle_request(&hub, &session, &id, local, peer, &request).await {
            break;
        }
    }

    hub.remove_session(&session);
    debug!(%peer, %id, "rtsp client disconnected");
}

async fn writer(mut stream: TcpStream, rx: AsyncReceiver<Outbound>) {
    let mut transports: [Option<Transport>; TRACK_COUNT] = [None, None];

    while let Ok(message) = rx.recv().await {
        match message {
            Outbound::Rtsp(bytes) => {
                if stream.write_all(bytes).await.0.is_err() {
                    break;
                }
            }
            Outbound::Setup { track, transport } => {
                transports[usize::from(track)] = Some(transport);
            }
            Outbound::Rtp { track, data } => match &transports[usize::from(track)] {
                Some(Transport::Tcp { rtp_channel, .. }) => {
                    if write_interleaved(&mut stream, *rtp_channel, data)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Some(Transport::Udp { rtp, rtp_peer, .. }) => {
                    let _ = rtp.send_to(data, *rtp_peer).await;
                }
                None => {}
            },
            Outbound::Rtcp { track, data } => match &transports[usize::from(track)] {
                Some(Transport::Tcp { rtcp_channel, .. }) => {
                    if write_interleaved(&mut stream, *rtcp_channel, data)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Some(Transport::Udp {
                    rtcp, rtcp_peer, ..
                }) => {
                    let _ = rtcp.send_to(data, *rtcp_peer).await;
                }
                None => {}
            },
        }
    }
}

async fn write_interleaved(
    stream: &mut TcpStream,
    channel: u8,
    data: Vec<u8>,
) -> std::io::Result<()> {
    let len = u16::try_from(data.len()).unwrap_or(0);
    let mut frame = Vec::with_capacity(data.len() + 4);
    frame.push(b'$');
    frame.push(channel);
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&data);
    stream.write_all(frame).await.0
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
    hub: &Hub,
    session: &Rc<Session>,
    id: &str,
    local: SocketAddr,
    peer: SocketAddr,
    request: &Request,
) -> bool {
    match request.method.as_str() {
        "OPTIONS" => {
            let headers = [(
                "Public",
                "OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN, GET_PARAMETER, SET_PARAMETER",
            )];
            send_response(session, response(&request.cseq, None, &headers, ""));
            true
        }
        "DESCRIBE" => {
            if !hub.wait_ready().await {
                send_response(
                    session,
                    error_response(&request.cseq, "503 Service Unavailable"),
                );
                return true;
            }

            let video = hub.video_config.borrow();
            let audio = hub.audio_config.borrow();
            // A finite end makes the stream seekable in the player; the live
            // playhead is the current end of the broadcast.
            let end = hub.position_seconds();
            let end = (end >= 0.5).then_some(end);
            let sdp = sdp::build(local.ip(), &hub.title, end, video.as_ref(), audio.as_ref());
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
        "SETUP" => setup(hub, session, id, peer, request).await,
        "PLAY" => {
            session.playing.set(true);

            // ffmpeg's RTSP seek sends PAUSE, then PLAY with the target range.
            // A first PLAY is not a seek: it starts at the live edge. A resume
            // seek (including mpv's watch-later position) is a real PAUSE+PLAY
            // and is honored.
            let seek_requested = session.paused.replace(false);
            if seek_requested
                && let Some(target) = request.range.as_deref().and_then(parse_npt_start)
            {
                let target_ms = seconds_to_ms(target);
                if target_ms.abs_diff(hub.position_ms.get()) > 1_500
                    && hub.request_seek(target_ms).await
                {
                    session.range_start.set(target);
                }
            }

            let info = rtp_info(hub, &request.uri, session.subscribed.get());
            // Keep the offset stable across pause/resume; only a seek changes it.
            let range = format!("npt={:.3}-", session.range_start.get());
            let headers = [("Range", range.as_str()), ("RTP-Info", info.as_str())];
            send_response(session, response(&request.cseq, Some(id), &headers, ""));
            true
        }
        "PAUSE" => {
            // ffmpeg's RTSP seek sends PAUSE followed by PLAY with the target
            // range; keep the session alive and stop the RTP fan-out.
            session.playing.set(false);
            session.paused.set(true);
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

#[allow(clippy::similar_names)] // rtp/rtcp are the protocol's names
async fn setup(
    hub: &Hub,
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

    if session
        .out
        .send(Outbound::Setup { track, transport })
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

fn track_ssrc(hub: &Hub, track: u8) -> u32 {
    match track {
        TRACK_VIDEO => hub.video.borrow().ssrc,
        _ => hub.audio.borrow().ssrc,
    }
}

fn send_response(session: &Session, bytes: Vec<u8>) {
    let _ = session.out.send(Outbound::Rtsp(bytes));
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

fn rtp_info(hub: &Hub, uri: &str, subscribed: u8) -> String {
    let base = uri.trim_end_matches('/');
    let mut entries = Vec::new();

    if subscribed & (1 << TRACK_VIDEO) != 0 {
        let video = hub.video.borrow();
        entries.push(format!(
            "{base}/trackID=0;seq={};rtptime={}",
            video.next_seq, video.last_rtp
        ));
    }
    if subscribed & (1 << TRACK_AUDIO) != 0 {
        let audio = hub.audio.borrow();
        entries.push(format!(
            "{base}/trackID=1;seq={};rtptime={}",
            audio.next_seq, audio.last_rtp
        ));
    }

    entries.join(",")
}

fn track_from_uri(uri: &str) -> Option<u8> {
    if uri.contains("trackID=0") {
        Some(TRACK_VIDEO)
    } else if uri.contains("trackID=1") {
        Some(TRACK_AUDIO)
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
}

impl Pacer {
    fn new() -> Self {
        Self {
            base_pts: None,
            base_wall: Instant::now(),
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

        let delta = media_delta(pts, base) as u128;
        if delta > 10 * 90_000 {
            // A jump of more than ten seconds is a discontinuity, not jitter.
            self.base_pts = Some(pts);
            self.base_wall = now;
            return;
        }

        let offset = Duration::from_micros(u64::try_from(delta * 1_000_000 / 90_000).unwrap_or(0));
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
/// How often the DVR playlist is re-fetched while seeking/playing VOD.
const VOD_REFRESH: Duration = Duration::from_secs(3);

/// Register the demuxing consumer with the scheduler's fan-out.
pub fn spawn_media(
    hub: Rc<Hub>,
    client: Rc<Client>,
    live_url: &str,
    wakes: &Rc<WakeSet>,
    add_output: &kanal::Sender<ConsumerRegistration>,
) {
    let (tx, rx) = kanal::bounded(MAX_CONSUMER_LAG);
    // The RTSP control plane sends seeks through the same ordered consumer
    // channel: kanal async receives must not be used in a select.
    hub.set_seek_sender(tx.clone().to_async());
    // A live RTSP session starts at the live edge: replaying the window would
    // put it seconds behind the pacer and the bounded queue would drop it.
    if let Err(error) = add_output.send(ConsumerRegistration { tx, backlog: false }) {
        error!(%error, "failed to register rtsp consumer");
        return;
    }

    let wake = ConsumerWake::new();
    wakes.register(&wake);
    let dvr_url = vod::dvr_url(live_url);

    compio::runtime::spawn(async move {
        Media::new(hub, client, dvr_url, rx.to_async(), wake)
            .run()
            .await;
    })
    .detach();
}

enum VodOutcome {
    Seek(u64),
    Live,
}

struct Media {
    hub: Rc<Hub>,
    client: Rc<Client>,
    dvr_url: Option<String>,
    rx: AsyncReceiver<SegmentMessage>,
    wake: Rc<ConsumerWake>,
    demuxer: TsDemuxer,
    pacer: Pacer,
    timeline: Timeline,
    samples: Vec<Sample>,
    vod: Option<VodTimeline>,
    vod_fetched: Instant,
    last_sequence: u64,
    last_rtcp: Instant,
    /// Newest live segment seen while the engine was playing VOD.
    pending_live: Option<Segment>,
    /// A discontinuity happened in the live stream while playing VOD.
    live_discontinuity: bool,
}

impl Media {
    fn new(
        hub: Rc<Hub>,
        client: Rc<Client>,
        dvr_url: Option<String>,
        rx: AsyncReceiver<SegmentMessage>,
        wake: Rc<ConsumerWake>,
    ) -> Self {
        Self {
            hub,
            client,
            dvr_url,
            rx,
            wake,
            demuxer: TsDemuxer::new(),
            pacer: Pacer::new(),
            timeline: Timeline::default(),
            samples: Vec::new(),
            vod: None,
            vod_fetched: Instant::now(),
            last_sequence: 0,
            last_rtcp: Instant::now(),
            pending_live: None,
            live_discontinuity: false,
        }
    }

    async fn run(mut self) {
        loop {
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

    async fn seek(&mut self, target_ms: u64) {
        let mut target = target_ms;
        while let VodOutcome::Seek(next) = self.play_vod(target).await {
            target = next;
        }
        // The DVR and live copies have independent timestamp continuity at the
        // handover; anchor the live pacer at its first sample instead of
        // inheriting the VOD playhead (which would burst if it is behind).
        self.pacer.reset();
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
            // Prefetches carry no playlist timestamp; the main Segment follows.
            // TS has no init segment; fMP4 is not supported by this output yet.
            SegmentMessage::Prefetch { .. } | SegmentMessage::Init { .. } => None,
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
                &mut self.last_sequence,
                &mut self.live_discontinuity,
            ) {
                return Some(seek);
            }
            self.demuxer.feed(view.as_init(), &mut self.samples);
            sync_config(&self.hub, &self.demuxer);
            for sample in self.samples.drain(..) {
                if let Some(seek) = drain_messages(
                    &self.rx,
                    &mut self.pending_live,
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
        if !self.load_vod(true).await {
            return VodOutcome::Live;
        }
        let Some(mut index) = self
            .vod
            .as_ref()
            .and_then(|timeline| timeline.find(target_ms))
        else {
            // The target is outside the DVR timeline (usually the live window).
            return VodOutcome::Live;
        };
        debug!(
            target_ms,
            dvr_end_ms = self.vod.as_ref().map_or(0, |timeline| timeline.end_ms),
            "rtsp: VOD seek"
        );

        self.demuxer = TsDemuxer::new();
        self.pacer = Pacer::new();
        self.timeline = Timeline::default();
        self.samples.clear();
        self.hub.reset_media_base();
        self.hub.set_position_ms(target_ms);
        let mut first = true;

        loop {
            if let Some(seek) = drain_messages(
                &self.rx,
                &mut self.pending_live,
                &mut self.last_sequence,
                &mut self.live_discontinuity,
            ) {
                return VodOutcome::Seek(seek);
            }

            let Some((url, start_ms)) = self.current_vod_segment(index) else {
                // End of the DVR copy: refresh and continue if it grew,
                // otherwise the live spools take over.
                if self.load_vod(true).await
                    && self.vod.as_ref().is_some_and(|v| v.segments.len() > index)
                {
                    continue;
                }
                return VodOutcome::Live;
            };

            let Some(bytes) = self.fetch_segment(&url).await else {
                warn!(url = %url, "rtsp: VOD segment fetch failed");
                if self.load_vod(true).await
                    && self.vod.as_ref().is_some_and(|v| v.segments.len() > index)
                {
                    continue;
                }
                return VodOutcome::Live;
            };

            self.samples.clear();
            self.demuxer.feed(&bytes, &mut self.samples);
            self.demuxer.finish_segment(&mut self.samples);
            sync_config(&self.hub, &self.demuxer);

            let start_index = if first {
                vod_start_index(&self.samples, start_ms, target_ms)
            } else {
                0
            };
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
            debug!(
                start_ms,
                target_ms,
                first_video_pts,
                chosen_pts,
                skipped_ms,
                samples = self.samples.len(),
                "rtsp: VOD segment loaded"
            );
            self.timeline.start(start_ms.saturating_add(skipped_ms));
            first = false;
            index += 1;

            for sample in self.samples.drain(start_index..) {
                if let Some(seek) = drain_messages(
                    &self.rx,
                    &mut self.pending_live,
                    &mut self.last_sequence,
                    &mut self.live_discontinuity,
                ) {
                    return VodOutcome::Seek(seek);
                }
                emit_sample(&self.hub, &mut self.pacer, &mut self.timeline, sample).await;
                maybe_rtcp(&self.hub, &mut self.last_rtcp);
            }
            self.hub.mark_ready();
        }
    }

    fn current_vod_segment(&self, index: usize) -> Option<(String, u64)> {
        let segment = self.vod.as_ref()?.segments.get(index)?;
        Some((segment.url.clone(), segment.start_ms))
    }

    async fn load_vod(&mut self, force: bool) -> bool {
        if !force && self.vod.is_some() && self.vod_fetched.elapsed() < VOD_REFRESH {
            return true;
        }
        let Some(url) = self.dvr_url.clone() else {
            return false;
        };
        let Some(text) = self.fetch_text(&url).await else {
            return false;
        };
        let timeline = VodTimeline::parse(&url, &text);
        if timeline.segments.is_empty() {
            return false;
        }
        self.vod = Some(timeline);
        self.vod_fetched = Instant::now();
        true
    }

    async fn fetch_text(&self, url: &str) -> Option<String> {
        let mut response = self.client.get(url).await.ok()?;
        if !response.status.is_success() {
            warn!(url = %url, status = %response.status, "rtsp: VOD playlist request failed");
            return None;
        }
        let body = response.body.read_to_end(MAX_MANIFEST).await.ok()?;
        std::str::from_utf8(&body).ok().map(str::to_owned)
    }

    async fn fetch_segment(&self, url: &str) -> Option<Vec<u8>> {
        let mut response = self.client.get(url).await.ok()?;
        if !response.status.is_success() {
            return None;
        }
        response.body.read_to_end(MAX_VOD_SEGMENT).await.ok()
    }
}

/// Drain queued live messages. Keeps the newest live segment for the handover
/// and returns the latest seek request.
fn drain_messages(
    rx: &AsyncReceiver<SegmentMessage>,
    pending_live: &mut Option<Segment>,
    last_sequence: &mut u64,
    live_discontinuity: &mut bool,
) -> Option<u64> {
    let mut seek = None;
    while let Ok(Some(message)) = rx.try_recv() {
        match message {
            SegmentMessage::Segment(segment) => {
                if pending_live
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
            SegmentMessage::Prefetch { .. } | SegmentMessage::Init { .. } => {}
        }
    }
    seek
}

fn maybe_rtcp(hub: &Hub, last_rtcp: &mut Instant) {
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

fn sync_config(hub: &Hub, demuxer: &TsDemuxer) {
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

async fn emit_sample(hub: &Hub, pacer: &mut Pacer, timeline: &mut Timeline, sample: Sample) {
    match sample {
        Sample::Video(video) => {
            if video.keyframe {
                debug!(pts = video.pts, dts = video.dts, "rtsp: video keyframe");
            }
            // Pace on DTS: that is the order the packets leave the decoder.
            pacer.wait(video.dts).await;
            hub.set_position_ms(timeline.position_ms(video.dts));

            let timestamp = hub.rtp_timestamp(video.pts, sdp::VIDEO_CLOCK_RATE);
            let ntp = ntp_now();
            let mut scratch = Vec::new();

            h264::packetize(
                &video.nals,
                RTP_MAX_PAYLOAD,
                &mut scratch,
                |payload, marker| {
                    let packet = hub
                        .video
                        .borrow_mut()
                        .packet(timestamp, marker, payload, ntp);
                    hub.send(TRACK_VIDEO, &packet);
                },
            );
        }
        Sample::Audio(audio) => {
            pacer.wait(audio.pts).await;
            hub.set_position_ms(timeline.position_ms(audio.pts));

            let clock_rate = hub.audio.borrow().clock_rate;
            let timestamp = hub.rtp_timestamp(audio.pts, clock_rate);
            let payload = aac::packetize(&audio.frame);
            let ntp = ntp_now();
            let packet = hub
                .audio
                .borrow_mut()
                .packet(timestamp, true, &payload, ntp);
            hub.send(TRACK_AUDIO, &packet);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn sender_report_is_a_valid_compound_packet() {
        let mut track = Track::new(sdp::VIDEO_PAYLOAD_TYPE, 90_000, 0x1234_5678);
        track.packet(0, true, &[1, 2, 3], ntp_now());

        let report = track.sender_report().expect("report");
        // Sender report: 28 bytes, length field 6.
        assert_eq!(report[1], 200);
        assert_eq!(u16::from_be_bytes([report[2], report[3]]), 6);
        // SDES follows at 28 bytes and must consume the rest of the compound.
        assert_eq!(report[29], 202);
        let sdes_len = u16::from_be_bytes([report[30], report[31]]);
        assert_eq!(usize::from(sdes_len) * 4 + 4, report.len() - 28);
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
