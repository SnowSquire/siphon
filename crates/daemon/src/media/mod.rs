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
#[allow(dead_code)]
pub mod h264;
#[allow(dead_code)]
pub mod ts;
