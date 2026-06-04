//! [`ServerError`] — the error type returned by the server's lifecycle
//! methods (`run`, `run_with_shutdown_on`).

use actus_reply::WebError;
use thiserror::Error;

/// An error from the server itself — binding, accepting, or serving
/// connections — as opposed to a per-request [`WebError`]. Returned by
/// [`Server::run`](crate::server::Server::run) and friends.
#[derive(Error, Debug)]
pub enum ServerError {
    /// The underlying hyper server failed (connection or protocol error).
    #[error("Hyper server error: {0}")]
    Hyper(#[from] hyper::Error),

    /// An I/O error, typically from binding or accepting on the listener.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// A per-request error surfaced at the server boundary.
    #[error("Web error: {0}")]
    Web(#[from] WebError),

    /// The provided bind address could not be parsed.
    #[error("Failed to parse address: {0}")]
    AddrParse(#[from] std::net::AddrParseError),
}
