//! The error every protocol crate returns, sorted by what the user can do about it.

use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The machine did not answer, or the connection broke.
    #[error("network: {0}")]
    Io(#[from] std::io::Error),
    #[error("no answer within {0:?}")]
    Timeout(Duration),
    /// The server refused the user name, password or key.
    #[error("login refused: {0}")]
    Auth(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("permission denied: {0}")]
    Denied(String),
    /// The protocol or this server cannot do it (the caps said so, or the server refused the command).
    #[error("not supported: {0}")]
    Unsupported(String),
    /// The server answered something this crate could not make sense of.
    #[error("unexpected reply: {0}")]
    Protocol(String),
    #[error("not connected")]
    NotConnected,
    /// The server closed the link or the link broke (EOF, reset, FTP 421, SSH disconnect).
    #[error("connection lost: {0}")]
    Lost(String),
}

/// Whether an I/O error means the link itself is gone rather than one request failing.
pub fn io_lost(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    if matches!(
        e.kind(),
        UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe | NotConnected
    ) {
        return true;
    }
    e.get_ref().is_some_and(|inner| is_link_lost(inner))
}

/// Whether anything in `e`'s chain says the link is gone; a timeout does not, on its own.
pub fn is_link_lost(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut next = Some(e);
    while let Some(e) = next {
        if let Some(lc) = e.downcast_ref::<Error>() {
            match lc {
                Error::Lost(_) | Error::NotConnected => return true,
                Error::Io(io) if io_lost(io) => return true,
                _ => {}
            }
        }
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if io_lost(io) {
                return true;
            }
        }
        next = e.source();
    }
    false
}

/// Whether anything in `e`'s chain is a timeout: the link may be gone, or only slow.
pub fn is_timeout(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut next = Some(e);
    while let Some(e) = next {
        if let Some(Error::Timeout(_)) = e.downcast_ref::<Error>() {
            return true;
        }
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) {
                return true;
            }
            if io.get_ref().is_some_and(|inner| is_timeout(inner)) {
                return true;
            }
        }
        next = e.source();
    }
    false
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, ErrorKind};

    #[test]
    fn lost_links_are_told_from_failed_requests() {
        assert!(is_link_lost(&Error::Lost("421".into())));
        assert!(is_link_lost(&Error::Io(ErrorKind::ConnectionReset.into())));
        assert!(!is_link_lost(&Error::NotFound("x".into())));
        assert!(!is_link_lost(&Error::Timeout(
            std::time::Duration::from_secs(1)
        )));
        // A reader wraps the crate's error in an io::Error; the kind is still found.
        let wrapped = io::Error::other(Error::Io(ErrorKind::UnexpectedEof.into()));
        assert!(is_link_lost(&wrapped));
        let slow = io::Error::other(Error::Timeout(std::time::Duration::from_secs(1)));
        assert!(is_timeout(&slow) && !is_link_lost(&slow));
    }
}
