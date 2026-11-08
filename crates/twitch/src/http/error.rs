use std::io;

/// Errors from the HTTP client.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] io::Error),

    #[error("invalid url: {0}")]
    Url(String),

    #[error("unsupported scheme: {0}")]
    Scheme(String),

    #[error("malformed response: {0}")]
    Protocol(&'static str),

    #[error("timed out: {0}")]
    Timeout(&'static str),

    #[error("too many redirects")]
    Redirects,

    #[error("connection closed before the body ended")]
    Closed,

    #[error("tls: {0}")]
    Tls(#[from] rustls::Error),
}
