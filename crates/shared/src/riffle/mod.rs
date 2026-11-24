//! Typed Riffle between the Siphon backend (the daemon) and its UI.
//!
//! The wire is a big-endian `u32` length followed by a `postcard` payload;
//! [`protocol`] owns the messages and state types, [`codec`] the framing, and
//! [`client`] the UI side of a connection. The server side lives in the
//! daemon because it needs the application state.
//!
//! [`object`] is the media form of the same framing: one header shared by every
//! carrier, with a class, policy bits and fragment offsets. The RPC is class
//! `Control`; nothing about media uses a second protocol.

// compio is thread-per-core: Riffle futures are deliberately !Send.
#![allow(clippy::future_not_send)]

pub mod client;
pub mod codec;
pub mod endpoint;
pub mod object;
pub mod protocol;
pub mod quic;

pub use client::Client;
pub use client::ClientEvent;
pub use client::ConnectError;
pub use client::Pairing;
pub use codec::CodecError;
pub use codec::Frame;
pub use codec::FrameClass;
pub use codec::FrameDecoder;
pub use codec::FrameReader;
pub use endpoint::admin_endpoint;
pub use endpoint::data_dir;
pub use endpoint::log_path;
pub use endpoint::probe;
pub use endpoint::state_dir;
pub use object::Class;
pub use object::Flags;
pub use object::Object;
pub use object::ObjectError;
pub use object::fragment_count;
pub use protocol::Account;
pub use protocol::ArenaSummary;
pub use protocol::AudioConfig;
pub use protocol::Channel;
pub use protocol::ChannelView;
pub use protocol::ChatColor;
pub use protocol::ClientMessage;
pub use protocol::Config;
pub use protocol::ConnHealth;
pub use protocol::ConnStats;
pub use protocol::DiskView;
pub use protocol::DownloaderSummary;
pub use protocol::FetchOutcome;
pub use protocol::LaneState;
pub use protocol::LaneView;
pub use protocol::LogEntry;
pub use protocol::LogLevel;
pub use protocol::MAGIC;
pub use protocol::MAX_AVATAR;
pub use protocol::MAX_FRAME;
pub use protocol::Network;
pub use protocol::PROTOCOL_VERSION;
pub use protocol::PipelineState;
pub use protocol::PipelineView;
pub use protocol::PlaybackPosition;
pub use protocol::Request;
pub use protocol::Response;
pub use protocol::RpcError;
pub use protocol::SegmentRecord;
pub use protocol::ServerMessage;
pub use protocol::Snapshot;
pub use protocol::StreamConfig;
pub use protocol::StreamDetail;
pub use protocol::StreamEvent;
pub use protocol::StreamEventKind;
pub use protocol::StreamKey;
pub use protocol::StreamPlayback;
pub use protocol::StreamSeek;
pub use protocol::StreamStarted;
pub use protocol::Update;
pub use protocol::UpdateView;
pub use protocol::VideoCodec;
pub use protocol::VideoConfig;
