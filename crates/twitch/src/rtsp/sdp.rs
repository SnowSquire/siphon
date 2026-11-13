//! SDP generation for the two elementary tracks.

use std::net::IpAddr;

use crate::rtsp::aac;
use crate::rtsp::aac::AudioConfig;
use crate::rtsp::h264::H264Config;

pub const VIDEO_PAYLOAD_TYPE: u8 = 96;
pub const AUDIO_PAYLOAD_TYPE: u8 = 97;
/// The timeline beacon: a second audio track whose packets carry only the
/// current live-edge NPT as silent AAC frames. It produces no audible output
/// (the real audio track is the default) but its packet timestamps are the only
/// in-band way to tell a player where the live edge is after DESCRIBE.
pub const TIMELINE_PAYLOAD_TYPE: u8 = 98;
pub const VIDEO_CLOCK_RATE: u32 = 90_000;

/// Build the session description from the codec configuration discovered in
/// the elementary stream. Tracks that have not been discovered yet are omitted.
///
/// `title` becomes the SDP session name (`s=`), which players such as mpv show
/// as the media title.
///
/// The session range starts at NPT 0 deliberately. `npt_end` is the current
/// end of the broadcast when known: a finite range makes the stream seekable
/// (clients enable their seek bar), while an omitted end marks it live.
///
/// `timeline` appends the live-edge beacon track, which carries no pictures
/// and exists only so the player can grow its duration after DESCRIBE.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn build(
    origin: IpAddr,
    title: &str,
    npt_end: Option<f64>,
    video: Option<&H264Config>,
    audio: Option<&AudioConfig>,
    timeline: bool,
) -> String {
    use std::fmt::Write as _;

    // Never let a channel name inject an SDP line.
    let title: String = title
        .chars()
        .filter(|&character| !character.is_control())
        .collect();
    let title = match title.trim() {
        "" => "twitch-hls-client2",
        title => title,
    };

    let mut sdp = String::with_capacity(512);
    sdp.push_str("v=0\r\n");
    let _ = write!(sdp, "o=- 0 0 IN IP4 {origin}\r\n");
    let _ = write!(sdp, "s={title}\r\n");
    sdp.push_str("c=IN IP4 0.0.0.0\r\n");
    sdp.push_str("t=0 0\r\n");
    sdp.push_str("a=control:*\r\n");
    match npt_end {
        Some(end) => {
            let _ = write!(sdp, "a=range:npt=0-{end:.3}\r\n");
        }
        // No end: an open-ended NPT range is how a live broadcast is signalled.
        None => sdp.push_str("a=range:npt=0-\r\n"),
    }

    if let Some(video) = video {
        sdp.push_str("m=video 0 RTP/AVP 96\r\n");
        sdp.push_str("a=rtpmap:96 H264/90000\r\n");
        let mut fmtp = String::from("packetization-mode=1");
        if let Some([profile, constraints, level]) = video.profile_level_id() {
            let _ = write!(
                fmtp,
                ";profile-level-id={profile:02X}{constraints:02X}{level:02X}"
            );
        }
        if let Some(sps) = video.sps.last() {
            fmtp.push_str(";sprop-parameter-sets=");
            fmtp.push_str(&base64(sps));
            if let Some(pps) = video.pps.last() {
                fmtp.push(',');
                fmtp.push_str(&base64(pps));
            }
        }
        let _ = write!(sdp, "a=fmtp:96 {fmtp}\r\n");
        sdp.push_str("a=control:trackID=0\r\n");
    }

    if let Some(audio) = audio {
        sdp.push_str("m=audio 0 RTP/AVP 97\r\n");
        let _ = write!(
            sdp,
            "a=rtpmap:97 mpeg4-generic/{}/{}\r\n",
            audio.sample_rate, audio.channels
        );
        let _ = write!(
            sdp,
            "a=fmtp:97 streamtype=5;profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config={}\r\n",
            aac::config_hex(&audio.asc)
        );
        sdp.push_str("a=control:trackID=1\r\n");
    }

    if timeline {
        // A second AAC track: silent frames carrying the live-edge timestamps.
        // The real audio is first, so a player's default audio pick is unchanged.
        let _ = write!(sdp, "m=audio 0 RTP/AVP {TIMELINE_PAYLOAD_TYPE}\r\n");
        let _ = write!(
            sdp,
            "a=rtpmap:{TIMELINE_PAYLOAD_TYPE} mpeg4-generic/48000/2\r\n"
        );
        let _ = write!(
            sdp,
            "a=fmtp:{TIMELINE_PAYLOAD_TYPE} streamtype=5;profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config=1190\r\n"
        );
        sdp.push_str("a=control:trackID=2\r\n");
    }

    sdp
}

#[must_use]
pub fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).map_or(0, |&byte| u32::from(byte));
        let b2 = chunk.get(2).map_or(0, |&byte| u32::from(byte));
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(char::from(ALPHABET[((triple >> 18) & 0x3F) as usize]));
        out.push(char::from(ALPHABET[((triple >> 12) & 0x3F) as usize]));
        out.push(if chunk.len() > 1 {
            char::from(ALPHABET[((triple >> 6) & 0x3F) as usize])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(ALPHABET[(triple & 0x3F) as usize])
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn describes_both_tracks() {
        let mut video = H264Config::default();
        video.push(&[0x67, 0x64, 0x00, 0x2A, 0xAC]);
        video.push(&[0x68, 0xEE, 0x3C]);
        let audio = AudioConfig {
            sample_rate: 48_000,
            channels: 2,
            asc: vec![0x11, 0x90],
        };

        let sdp = build(
            "127.0.0.1".parse().expect("ip"),
            "sunnys",
            Some(123.456),
            Some(&video),
            Some(&audio),
            true,
        );
        assert!(sdp.contains("s=sunnys\r\n"));
        assert!(sdp.contains("a=range:npt=0-123.456\r\n"));
        assert!(sdp.contains("m=video 0 RTP/AVP 96\r\n"));
        assert!(sdp.contains("profile-level-id=64002A"));
        assert!(sdp.contains("packetization-mode=1"));
        assert!(sdp.contains("sprop-parameter-sets="));
        assert!(sdp.contains("a=control:trackID=0\r\n"));
        assert!(sdp.contains("m=audio 0 RTP/AVP 97\r\n"));
        assert!(sdp.contains("mpeg4-generic/48000/2"));
        assert!(sdp.contains("config=1190"));
        assert!(sdp.contains("a=control:trackID=1\r\n"));
        assert!(sdp.contains("m=audio 0 RTP/AVP 98\r\n"));
        assert!(sdp.contains("a=rtpmap:98 mpeg4-generic/48000/2\r\n"));
        assert!(sdp.contains("a=control:trackID=2\r\n"));
        assert!(sdp.contains("a=fmtp:98"));
    }
}
