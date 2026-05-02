//! Errors specific to the native client.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum NativeError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("connection lost: {0}")]
    Lost(String),

    #[error("not connected")]
    NotConnected,

    #[error("protocol violation: {0}")]
    Protocol(String),

    #[error("server version {0} below minimum supported {1}")]
    ServerVersionTooLow(i32, i32),

    #[error("handshake timed out")]
    HandshakeTimeout,

    #[error("malformed message: {0}")]
    Malformed(String),
}
