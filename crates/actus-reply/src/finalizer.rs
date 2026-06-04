//! Contains the Finalizer, which converts a ReplyData into a real HTTP response.
use crate::reply::{ProblemDetails, ReplyData, ReplySpec, WebError};
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use http::{HeaderName, HeaderValue, Response, StatusCode, header};
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::Frame;
use std::str::FromStr;
use tracing::warn;

type BoxBody = http_body_util::combinators::BoxBody<Bytes, WebError>;

/// Converts a [`ReplyData`] into a concrete `hyper` HTTP response — setting
/// status, headers, and body, and driving buffered, streaming, SSE, and
/// connection-upgrade replies.
pub struct Finalizer;

impl Default for Finalizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Finalizer {
    /// Create a finalizer.
    pub fn new() -> Self {
        Finalizer
    }

    /// Build the `hyper` response for `data` — its status line, headers, and
    /// body (buffered, streaming, SSE, or a connection upgrade).
    pub fn build_response<'a>(&'a self, data: ReplyData) -> BoxFuture<'a, Response<BoxBody>> {
        Box::pin(async move {
            match data {
                ReplyData::Empty => Response::builder()
                    .status(StatusCode::NO_CONTENT)
                    .body(
                        Full::new(Bytes::new())
                            .map_err(|never| match never {})
                            .boxed(),
                    )
                    .unwrap(),

                ReplyData::Bytes { content_type, data } => Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, content_type.as_ref())
                    .body(
                        Full::new(Bytes::from(data))
                            .map_err(|never| match never {})
                            .boxed(),
                    )
                    .unwrap(),

                ReplyData::Json(val) => {
                    let bytes = serde_json::to_vec(&val).expect("json");
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(
                            Full::new(Bytes::from(bytes))
                                .map_err(|never| match never {})
                                .boxed(),
                        )
                        .unwrap()
                }

                ReplyData::Stream(body_stream) => {
                    let stream_of_frames = body_stream.map(|chunk| {
                        chunk
                            .map(Frame::data)
                            .map_err(|e| WebError::Internal(e.to_string()))
                    });
                    let body = StreamBody::new(stream_of_frames);
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(BodyExt::boxed(body))
                        .unwrap()
                }
                ReplyData::Rich(spec) => self.build_rich_response(*spec).await,

                // The server intercepts `Upgrade` replies (to complete the
                // handshake) before they reach the finalizer; reaching here
                // means it wasn't intercepted — surface that as a 500 rather
                // than panic.
                ReplyData::Upgrade(_) => Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
                    .body(
                        Full::new(Bytes::from_static(
                            b"upgrade reply was not handled by the server",
                        ))
                        .map_err(|never| match never {})
                        .boxed(),
                    )
                    .unwrap(),
            }
        })
    }

    async fn build_rich_response(&self, spec: ReplySpec) -> Response<BoxBody> {
        let mut res = self.build_response(spec.payload).await;

        if let Some(status) = spec.status {
            *res.status_mut() = status;
        }

        // Insert headers defensively: a hostile or sloppy caller of
        // `ReplyData::add_header("\n", …)` shouldn't panic the request.
        // Drop invalid name/value pairs with a `warn!` and carry on —
        // the rest of the response is still useful.
        for (k, v) in spec.headers {
            let key = match HeaderName::from_str(&k) {
                Ok(name) => name,
                Err(e) => {
                    warn!(name = %k, error = %e, "dropping invalid response header name");
                    continue;
                }
            };
            let value = match HeaderValue::from_str(&v) {
                Ok(val) => val,
                Err(e) => {
                    warn!(name = %k, value = %v, error = %e, "dropping invalid response header value");
                    continue;
                }
            };
            res.headers_mut().insert(key, value);
        }
        res
    }

    /// Convert a [`WebError`] into a [`ReplyData`] carrying the canonical
    /// `application/problem+json` body (per RFC 7807), the appropriate
    /// status, and any error-specific headers (e.g. `Allow` for 405).
    ///
    /// Use this when you want an error to flow through the same response
    /// pipeline as a handler success — after-chain middleware, compression,
    /// CORS. The simple variants (`NotFound`, `BadRequest`, …) map to obvious
    /// status/title pairs; `Problem(p)` preserves extension members; the
    /// returned reply is a [`ReplyData::Rich`] so an `after` hook can stamp
    /// headers or replace the status without manual juggling.
    pub fn error_to_reply(&self, error: WebError) -> ReplyData {
        let mut allow_header: Option<String> = None;
        let mut retry_after_seconds: Option<u64> = None;
        let problem = match error {
            WebError::NotFound => ProblemDetails::new(StatusCode::NOT_FOUND, "Not Found"),
            WebError::MethodNotAllowed(methods) => {
                allow_header = Some(methods.join(", "));
                ProblemDetails::new(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed")
                    .extra("allowed_methods", serde_json::Value::from(methods))
            }
            WebError::BadRequest(msg) => {
                ProblemDetails::new(StatusCode::BAD_REQUEST, "Bad Request").detail(msg)
            }
            WebError::PayloadTooLarge => {
                ProblemDetails::new(StatusCode::PAYLOAD_TOO_LARGE, "Payload Too Large")
            }
            WebError::TooManyRequests(retry_after) => {
                let mut p = ProblemDetails::new(StatusCode::TOO_MANY_REQUESTS, "Too Many Requests");
                if let Some(d) = retry_after {
                    // RFC 7231 §7.1.3: `Retry-After` is either an HTTP-date
                    // or delta-seconds (an integer). We use seconds; sub-
                    // second precision rounds up so we don't tell the
                    // client "retry in 0s" for a 500ms hint.
                    let secs = d.as_secs().max(if d.subsec_nanos() > 0 { 1 } else { 0 });
                    retry_after_seconds = Some(secs);
                    // Mirror in the problem body too, so a JSON-only client
                    // (no header inspection) can see the hint.
                    p = p.extra("retry_after_seconds", serde_json::Value::from(secs));
                }
                p
            }
            WebError::Timeout => {
                ProblemDetails::new(StatusCode::GATEWAY_TIMEOUT, "Gateway Timeout")
                    .detail("the request did not complete within the configured timeout")
            }
            WebError::Busy(retry_after) => {
                let mut p =
                    ProblemDetails::new(StatusCode::SERVICE_UNAVAILABLE, "Service Unavailable")
                        .detail("server is overloaded");
                if let Some(d) = retry_after {
                    let secs = d.as_secs().max(if d.subsec_nanos() > 0 { 1 } else { 0 });
                    retry_after_seconds = Some(secs);
                    p = p.extra("retry_after_seconds", serde_json::Value::from(secs));
                }
                p
            }
            WebError::Unauthorized => ProblemDetails::new(StatusCode::UNAUTHORIZED, "Unauthorized"),
            WebError::Forbidden => ProblemDetails::new(StatusCode::FORBIDDEN, "Forbidden"),
            WebError::Internal(msg) => {
                ProblemDetails::new(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
                    .detail(msg)
            }
            WebError::Problem(p) => p,
        };

        let mut body = serde_json::Map::new();
        body.insert("status".into(), problem.status.as_u16().into());
        body.insert("title".into(), problem.title.into());
        if let Some(d) = problem.detail {
            body.insert("detail".into(), d.into());
        }
        // Extension members: don't let them shadow the standard fields.
        for (k, v) in *problem.extra {
            if !matches!(k.as_str(), "status" | "title" | "detail") {
                body.insert(k, v);
            }
        }
        let bytes = serde_json::to_vec(&serde_json::Value::Object(body)).expect("json");
        let status = problem.status;

        let mut headers = std::collections::HashMap::new();
        if let Some(allow) = allow_header {
            headers.insert("Allow".to_string(), allow);
        }
        if let Some(secs) = retry_after_seconds {
            headers.insert("Retry-After".to_string(), secs.to_string());
        }

        ReplyData::Rich(Box::new(ReplySpec {
            payload: ReplyData::Bytes {
                content_type: std::borrow::Cow::Borrowed("application/problem+json"),
                data: bytes,
            },
            status: Some(status),
            headers,
        }))
    }

    /// Build a complete error `Response` directly — the one-shot
    /// `error → response` path used for fallback paths (after-chain failures
    /// while finalizing an error reply, etc.) where running the error
    /// through the after-chain would risk recursion. For the normal error
    /// path, prefer [`Finalizer::error_to_reply`] + [`Finalizer::build_response`]
    /// so middleware / compression / CORS apply uniformly.
    pub async fn build_error(&self, error: WebError) -> Response<BoxBody> {
        self.build_response(self.error_to_reply(error)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body_json(res: Response<BoxBody>) -> serde_json::Value {
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn method_not_allowed_emits_allow_header_and_lists_methods() {
        let res = Finalizer::new()
            .build_error(WebError::MethodNotAllowed(vec!["GET", "POST"]))
            .await;
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(res.headers().get(header::ALLOW).unwrap(), "GET, POST");
        let body = body_json(res).await;
        assert_eq!(body["status"], 405);
        assert_eq!(body["title"], "Method Not Allowed");
        assert_eq!(body["allowed_methods"], serde_json::json!(["GET", "POST"]));
    }

    #[tokio::test]
    async fn other_errors_have_no_allow_header() {
        let res = Finalizer::new().build_error(WebError::NotFound).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert!(res.headers().get(header::ALLOW).is_none());
    }

    #[tokio::test]
    async fn too_many_requests_emits_retry_after_header_and_extra_member() {
        use std::time::Duration;

        // With a retry hint: 429 + `Retry-After: <seconds>` + an extra
        // member in the body so a JSON-only client can read it without
        // header inspection.
        let res = Finalizer::new()
            .build_error(WebError::TooManyRequests(Some(Duration::from_secs(42))))
            .await;
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(res.headers().get("Retry-After").unwrap(), "42");
        let body = body_json(res).await;
        assert_eq!(body["status"], 429);
        assert_eq!(body["title"], "Too Many Requests");
        assert_eq!(body["retry_after_seconds"], 42);

        // Sub-second hints round up — we never tell a client "retry in 0s"
        // for a 500ms hint (which they'd typically interpret as "now").
        let res = Finalizer::new()
            .build_error(WebError::TooManyRequests(Some(Duration::from_millis(500))))
            .await;
        assert_eq!(res.headers().get("Retry-After").unwrap(), "1");

        // Without a retry hint: 429 + no `Retry-After` header.
        let res = Finalizer::new()
            .build_error(WebError::TooManyRequests(None))
            .await;
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(res.headers().get("Retry-After").is_none());
    }

    #[tokio::test]
    async fn busy_emits_503_with_retry_after() {
        use std::time::Duration;

        // With a hint: 503 + `Retry-After: <seconds>` + extra in the body.
        let res = Finalizer::new()
            .build_error(WebError::Busy(Some(Duration::from_secs(2))))
            .await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(res.headers().get("Retry-After").unwrap(), "2");
        let body = body_json(res).await;
        assert_eq!(body["status"], 503);
        assert_eq!(body["title"], "Service Unavailable");
        assert_eq!(body["retry_after_seconds"], 2);

        // No hint: 503 + no Retry-After.
        let res = Finalizer::new().build_error(WebError::Busy(None)).await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(res.headers().get("Retry-After").is_none());
    }

    #[tokio::test]
    async fn build_rich_response_drops_invalid_headers_without_panicking() {
        use crate::reply::ReplySpec;
        use std::collections::HashMap;

        let mut headers = HashMap::new();
        // A newline in a header name is invalid per RFC 7230 §3.2 — must
        // be dropped, not panicked on.
        headers.insert("X-Bad\nName".to_string(), "value".to_string());
        // A control byte in the value is also invalid.
        headers.insert("X-Bad-Value".to_string(), "with\nnewline".to_string());
        // And a valid one alongside, to prove the rest of the spec survives.
        headers.insert("X-OK".to_string(), "fine".to_string());

        let spec = ReplySpec {
            payload: ReplyData::Empty,
            status: Some(StatusCode::CREATED),
            headers,
        };
        let res = Finalizer::new()
            .build_response(ReplyData::Rich(Box::new(spec)))
            .await;
        assert_eq!(res.status(), StatusCode::CREATED);
        assert_eq!(res.headers().get("X-OK").unwrap(), "fine");
        // The two invalid pairs got dropped.
        assert!(res.headers().get("X-Bad-Value").is_none());
    }
}
