//! The media pipeline: the transport stream demuxer, the codec configuration and
//! the DVR timeline.
//!
//! There is no server here. RTSP and its RTP packetizers are gone; the
//! elementary stream leaves through the Riffle transport (`notes/riffle.md`),
//! and this module is only where the access units and their timings come from.
//!
//! [`ts`] and [`fmp4`] turn a segment into access units, [`wire`] packs an
//! access unit into the objects the lane ships, and [`lane`] is the lane: a
//! demuxer per lane, a datagram sender per connection.

#[allow(dead_code)]
pub mod aac;
pub mod dvr;
// The fMP4 demuxer's fixtures are the only thing exercising part of it, so the
// allowance stops at the test build and a stale accessor does not hide behind
// it.
#[cfg_attr(not(test), allow(dead_code))]
pub mod fmp4;
#[allow(dead_code)]
pub mod h264;
// The lane that carries access units to a viewer: one producer per lane, one
// datagram sender per connection.
pub mod lane;
#[allow(dead_code)]
pub mod ts;
// The segment source and the packer: the media lane is their consumer.
pub mod wire;
