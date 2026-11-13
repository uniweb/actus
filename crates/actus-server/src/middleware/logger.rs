//! A request logger: one `tracing` event for each request, when its reply is ready.

use crate::middleware::Middleware;
use crate::request::Request;
use actus_reply::{ReplyData, WebError};
use async_trait::async_trait;
use tracing::info;

/// Logs every request via `tracing` once its reply is ready: the method, the routed path, the
/// reply's status and the milliseconds since the request arrived, as
///
/// ```text
/// request method=GET path=/users/7 status=200 elapsed_ms=1.284
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
/// - **When:** from its `after` hook, which runs on every reply but a CORS preflight and a
///   completed WebSocket handshake — a `404` from the router, a refused body and a
///   short-circuiting middleware included. Add it **first**: `after` hooks run in reverse, so
///   it then reports the reply as every other middleware left it.
/// - **Who:** the event is emitted inside the request's span, so a field the application
///   records there ([`Server::with_request_span`](crate::Server::with_request_span)), such as
///   the caller it resolved, is printed with it.
pub struct RequestLogger;

#[async_trait]
impl Middleware for RequestLogger {
    async fn after(&self, request: &Request, response: &mut ReplyData) -> Result<(), WebError> {
        let path = format!("/{}", request.path_parts.join("/"));
        let status = response.status_code().as_u16();
        match crate::server::request_elapsed() {
            Some(elapsed) => info!(
                method = %request.method,
                path = %path,
                status,
                // Milliseconds to the microsecond: most replies take under one.
                elapsed_ms = (elapsed.as_secs_f64() * 1_000_000.0).round() / 1000.0,
                "request",
            ),
            None => info!(method = %request.method, path = %path, status, "request"),
        }
        Ok(())
    }
}
