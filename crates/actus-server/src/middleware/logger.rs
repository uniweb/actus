//! A simple request logger that emits a `tracing` event for each request.

use crate::middleware::{Middleware, Outcome};
use crate::request::Request;
use actus_reply::WebError;
use async_trait::async_trait;
use tracing::info;

/// Logs every incoming request via `tracing`. Add it to the server with
/// `Server::with_middleware(RequestLogger)`.
///
/// ```ignore
/// use actus::prelude::*;
///
/// Server::new(init().await?)
///     .with_middleware(RequestLogger)
///     .run(3000).await?;
/// ```
pub struct RequestLogger;

#[async_trait]
impl Middleware for RequestLogger {
    async fn before(&self, request: &mut Request) -> Result<Outcome, WebError> {
        info!(
            method = %request.method,
            path = %format!("/{}", request.path_parts.join("/")),
            "request",
        );
        Ok(Outcome::Continue)
    }
}
