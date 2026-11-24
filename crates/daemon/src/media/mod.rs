//! The media pipeline: the transport stream demuxer, the codec configuration and
//! the DVR timeline.
//!
//! There is no server here. RTSP and its RTP packetizers are gone; the
//! elementary stream leaves through the Riffle transport (`notes/riffle.md`),
//! and this module is only where the access units and their timings come from.
//!
//! The demuxer half is the input side of the Riffle media lane. Nothing consumes
//! it until that lane exists, so its items are allowed to look unused here
//! rather than being deleted and re-ported.

#[allow(dead_code)]
pub mod aac;
pub mod dvr;
// The fMP4 demuxer has no consumer yet either, but its fixtures are the only
// thing exercising it, so the allowance stops at the test build and a stale
// accessor does not hide behind it.
#[cfg_attr(not(test), allow(dead_code))]
pub mod fmp4;
#[allow(dead_code)]
pub mod h264;
#[allow(dead_code)]
pub mod ts;
