//! A request logger that emits a `tracing` event for each request as it is routed.

use crate::middleware::{Middleware, Outcome};
use crate::request::Request;
use actus_reply::WebError;
use async_trait::async_trait;
use tracing::info;

/// Logs each request via `tracing` from its `before` hook: the method and the routed path, as
/// the request reaches the middleware chain, before its handler runs. A request that never
/// finishes still leaves its line.
///
/// ```text
/// request method=GET path=/users/7
/// ```
///
/// Add it to the server with `Server::with_middleware(RequestLogger)`:
///
/// ```ignore
/// use actus::prelude::*;
///
/// Server::new(init().await?)
///     .with_middleware(RequestLogger)
///     .run(3000).await?;
/// ```
///
/// It sees no reply, so it logs no status and no time. A request answered before the `before`
/// chain runs (a `404` from the router, a refused body, a CORS preflight), or by a `before`
/// hook registered ahead of it, leaves no line. For one line per response, with its status
/// and how long it took, written when it ends for every response the server sends, use the
/// server's access log, [`Server::with_access_log`](crate::Server::with_access_log).
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
