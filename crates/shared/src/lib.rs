//! What the Siphon backend and its clients agree on.
//!
//! [`build`] is the commit the binaries were compiled from. [`rpc`] is the
//! typed protocol they speak, [`settings`] the per-client settings a UI
//! persists for itself, and [`store`] the file layer both that and the daemon's
//! own config are read and written through.
//!
//! Backend state and client state are deliberately separate: a daemon has one
//! config, but several UIs may attach to it — over a pipe today, over a network
//! one day — and each of those clients keeps its own display settings.

pub mod build;
pub mod rpc;
pub mod settings;
pub mod store;
