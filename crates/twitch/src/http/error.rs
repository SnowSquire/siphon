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

impl Error {
    /// Whether the same request could succeed on a fresh connection.
    ///
    /// Only failures that are deterministic for a given URL are terminal: a
    /// retry would spend the whole backoff and fail the same way.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        match self {
            // The transfer or the connection died; another attempt may work.
            Self::Io(_) | Self::Timeout(_) | Self::Protocol(_) | Self::Tls(_) | Self::Closed => {
                true
            }
            // Deterministic for this URL, or a state we cannot make progress on.
            Self::Url(_) | Self::Scheme(_) | Self::Redirects => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_deterministic_failures_are_terminal() {
        assert!(Error::Closed.retryable());
        assert!(Error::Timeout("body").retryable());
        assert!(Error::Protocol("bad chunk").retryable());
        assert!(Error::Io(io::Error::from(io::ErrorKind::ConnectionReset)).retryable());

        assert!(!Error::Url("no scheme".into()).retryable());
        assert!(!Error::Scheme("ftp".into()).retryable());
        assert!(!Error::Redirects.retryable());
    }
}
