//! # actus-server
//!
//! The hyper-based HTTP server and longest-prefix router for Actus.
//!
//! Routes are matched by longest-prefix at arbitrary depth: the framework picks
//! the deepest controller-bearing prefix in the route tree, then hands the
//! remaining path segment (the "action") to that controller's dispatcher.
#![warn(missing_docs)]

#[cfg(feature = "compression")]
pub mod compression;
pub mod cors;
pub mod error;
pub mod middleware;
#[cfg(feature = "openapi")]
pub mod openapi;
pub mod request;
pub mod router;
pub mod server;
#[cfg(feature = "websocket")]
pub mod websocket;

#[cfg(feature = "compression")]
pub use compression::CompressionLayer;
pub use cors::CorsLayer;
pub use error::ServerError;
pub use middleware::{Middleware, MiddlewareChain, Outcome, RequestLogger};
pub use request::Request;
pub use router::{RateLimitClass, Router, RouterBuilder};
pub use server::{DEFAULT_MAX_BODY_BYTES, GIB, KIB, MIB, Server};
#[cfg(feature = "websocket")]
pub use websocket::{Message, WebSocket};
