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
}

pub type Result<T> = std::result::Result<T, Error>;
