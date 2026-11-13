use std::io;

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
    /// Whether this failure can be blamed on a pooled connection that the server
    /// closed while it was idle.
    ///
    /// A keep-alive connection is validated only by using it, so a write or the
    /// first read may discover a socket the peer already ended. That is not a
    /// property of the request; a fresh connection can succeed. `Protocol` is
    /// excluded on purpose: a malformed response will parse the same way twice.
    #[must_use]
    pub const fn stale_connection(&self) -> bool {
        matches!(self, Self::Io(_) | Self::Timeout(_) | Self::Closed | Self::Tls(_))
    }

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
    fn only_transport_failures_look_stale() {
        assert!(Error::Closed.stale_connection());
        assert!(Error::Timeout("io").stale_connection());
        assert!(Error::Io(io::Error::from(io::ErrorKind::ConnectionReset)).stale_connection());

        assert!(!Error::Protocol("invalid response head").stale_connection());
        assert!(!Error::Url("no scheme".into()).stale_connection());
        assert!(!Error::Scheme("ftp".into()).stale_connection());
    }

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
