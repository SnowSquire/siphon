//! Typed RPC between the Siphon backend (the daemon) and its UI.
//!
//! The wire is a big-endian `u32` length followed by a `postcard` payload;
//! [`protocol`] owns the messages and state types, [`codec`] the framing, and
//! [`client`] the UI side of a connection. The server side lives in the
//! daemon because it needs the application state.

// compio is thread-per-core: RPC futures are deliberately !Send.
#![allow(clippy::future_not_send)]

pub mod client;
pub mod codec;
pub mod protocol;

pub use client::Client;
pub use client::ClientEvent;
pub use client::ConnectError;
pub use codec::CodecError;
pub use codec::FrameDecoder;
pub use codec::FrameReader;
pub use protocol::ArenaSummary;
pub use protocol::Channel;
pub use protocol::ChannelView;
pub use protocol::ClientMessage;
pub use protocol::Config;
pub use protocol::ConnHealth;
pub use protocol::DownloaderSummary;
pub use protocol::FetchOutcome;
pub use protocol::MAGIC;
pub use protocol::MAX_FRAME;
pub use protocol::NotificationView;
pub use protocol::PROTOCOL_VERSION;
pub use protocol::PipelineState;
pub use protocol::PipelineView;
pub use protocol::Request;
pub use protocol::Response;
pub use protocol::RpcError;
pub use protocol::SegmentRecord;
pub use protocol::ServerMessage;
pub use protocol::Snapshot;
pub use protocol::StreamDetail;
pub use protocol::StreamKey;
pub use protocol::Update;
pub use protocol::UpdateView;
