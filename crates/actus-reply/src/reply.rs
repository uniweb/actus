//! Contains the user-facing API for creating replies.
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde::Serialize;
use serde_json::Value;
use std::any::Any;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::pin::Pin;
use std::time::Duration;

// ===================== Core Types =====================

/// A boxed, `Send + Sync` stream of body chunks, used by [`ReplyData::Stream`]
/// and SSE responses.
pub type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send + Sync>>;

/// The concrete payload of a successful reply. The [`Finalizer`](crate::Finalizer)
/// turns it into an HTTP response. Build one with the [`reply!`](crate::reply!)
/// macro or the `reply::*` constructor functions rather than by hand.
#[derive()]
pub enum ReplyData {
    /// A JSON body (`Content-Type: application/json`).
    Json(Value),
    /// Raw bytes plus the `Content-Type` to send. There is no default —
    /// callers pick the type at the call site (`application/zip`,
    /// `image/png`, `application/octet-stream`, …). Construct via
    /// [`bytes()`](crate::bytes). The finalizer emits the header verbatim.
    Bytes {
        /// The `Content-Type` header value to send with the bytes.
        content_type: Cow<'static, str>,
        /// The raw response body.
        data: Vec<u8>,
    },
    /// An empty body — `204 No Content` by default.
    Empty,
    /// A reply carrying explicit status / headers (a [`ReplySpec`]) on top of
    /// one of the other payload kinds.
    Rich(Box<ReplySpec>),
    /// A streaming body, written out as the stream yields.
    Stream(BodyStream),
    /// An HTTP connection-upgrade reply (e.g. WebSocket). Produced by
    /// `actus::ws::upgrade(...)` (with the `websocket` feature) and consumed
    /// by the server, which completes the handshake (`101 Switching Protocols`)
    /// and then hands the upgraded connection to the handler. The boxed value
    /// is opaque server plumbing — don't construct or inspect it directly.
    Upgrade(Box<dyn Any + Send>),
}

// Manual `Debug` implementation to handle the non-Debug variants.
impl fmt::Debug for ReplyData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReplyData::Json(j) => f.debug_tuple("Json").field(j).finish(),
            ReplyData::Bytes { content_type, data } => f
                .debug_struct("Bytes")
                .field("content_type", content_type)
                .field("data", data)
                .finish(),
            ReplyData::Empty => write!(f, "Empty"),
            ReplyData::Rich(r) => f.debug_tuple("Rich").field(r).finish(),
            ReplyData::Stream(_) => f.debug_tuple("Stream").field(&"...").finish(),
            ReplyData::Upgrade(_) => f.debug_tuple("Upgrade").field(&"...").finish(),
        }
    }
}

// Manual `PartialEq` implementation. Streams are never equal.
impl PartialEq for ReplyData {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ReplyData::Json(l), ReplyData::Json(r)) => l == r,
            (
                ReplyData::Bytes {
                    content_type: lc,
                    data: ld,
                },
                ReplyData::Bytes {
                    content_type: rc,
                    data: rd,
                },
            ) => lc == rc && ld == rd,
            (ReplyData::Empty, ReplyData::Empty) => true,
            (ReplyData::Rich(l), ReplyData::Rich(r)) => l == r,
            _ => false, // Streams and different variants are not equal
        }
    }
}

/// A handler's return type: `Ok(`[`ReplyData`]`)` for a success payload, or
/// `Err(`[`WebError`]`)` for an error the framework renders as an HTTP error
/// response. The [`reply!`](crate::reply!) macro produces the `Ok` side.
pub type Reply = Result<ReplyData, WebError>;

impl ReplyData {
    /// Add a response header. Lifts `self` into [`ReplyData::Rich`] if the
    /// current variant doesn't already carry headers (Json/Bytes/Empty/Stream),
    /// so middleware `after` hooks (and any code that has a `ReplyData` it
    /// wants to decorate) can stamp headers without manually wrangling
    /// [`ReplySpec`]. The original payload is preserved.
    ///
    /// No-op on [`ReplyData::Upgrade`] — that variant is intercepted by the
    /// server before the response body is finalized, and lifting it into
    /// `Rich` would hide it from that interception.
    pub fn add_header(&mut self, name: impl Into<String>, value: impl Into<String>) {
        if matches!(self, ReplyData::Upgrade(_)) {
            return;
        }
        let name = name.into();
        let value = value.into();
        if let ReplyData::Rich(spec) = self {
            spec.headers.insert(name, value);
            return;
        }
        let payload = std::mem::replace(self, ReplyData::Empty);
        let mut headers = HashMap::new();
        headers.insert(name, value);
        *self = ReplyData::Rich(Box::new(ReplySpec {
            payload,
            status: None,
            headers,
        }));
    }

    /// Set the response status, lifting into [`ReplyData::Rich`] if needed
    /// (see [`add_header`](Self::add_header)).
    pub fn set_status(&mut self, status: http::StatusCode) {
        if matches!(self, ReplyData::Upgrade(_)) {
            return;
        }
        if let ReplyData::Rich(spec) = self {
            spec.status = Some(status);
            return;
        }
        let payload = std::mem::replace(self, ReplyData::Empty);
        *self = ReplyData::Rich(Box::new(ReplySpec {
            payload,
            status: Some(status),
            headers: HashMap::new(),
        }));
    }
}

// ===================== Simple Constructors =====================
// All constructor functions must be `pub`.

/// Build a JSON `ReplyData` from any [`Serialize`] value.
///
/// Panics on serialization failure. Most domain types (those with derived
/// `Serialize` over stringy fields) never fail, but custom impls can.
/// The `reply!` macro uses [`try_json`] internally to convert that case
/// into a `WebError::Internal` (→ 500) rather than a handler-level panic
/// that would drop the whole connection.
///
/// Prefer `reply!(my_value)` over calling this directly.
pub fn json<T: Serialize>(val: T) -> ReplyData {
    ReplyData::Json(serde_json::to_value(val).expect("serialize"))
}

/// Fallible counterpart of [`json`]. Returns the raw `serde_json::Error`
/// on failure so callers (notably the `reply!` macro) can map it to a
/// structured response instead of panicking.
pub fn try_json<T: Serialize>(val: T) -> Result<ReplyData, serde_json::Error> {
    Ok(ReplyData::Json(serde_json::to_value(val)?))
}

/// Build a `ReplyData::Bytes` with an explicit `Content-Type`. The
/// content-type is required: there is no honest default for arbitrary
/// bytes, and `application/octet-stream` masquerading as one tends to
/// cause more bugs than it fixes (browsers guess; downstream HTTP
/// caches mis-cache). Pass the right media type at the call site.
///
/// ```ignore
/// reply::bytes("application/zip", zip_payload)
/// reply::bytes("image/png", png_payload)
/// ```
pub fn bytes(content_type: impl Into<Cow<'static, str>>, data: impl Into<Vec<u8>>) -> ReplyData {
    ReplyData::Bytes {
        content_type: content_type.into(),
        data: data.into(),
    }
}

/// An empty-body reply (`204 No Content` unless a status is set via
/// [`build_reply`]).
pub fn empty() -> ReplyData {
    ReplyData::Empty
}

/// Start a [`ReplySpec`] builder for a reply that needs an explicit status
/// and/or headers on top of its body.
pub fn build_reply() -> ReplySpec {
    ReplySpec::new()
}

/// Build a streaming-body reply from a stream of byte chunks. The body is
/// written out as the stream yields, not buffered.
pub fn stream<S>(s: S) -> ReplyData
where
    S: Stream<Item = Result<Bytes, io::Error>> + Send + Sync + 'static,
{
    ReplyData::Stream(Box::pin(s))
}

// ===================== Server-Sent Events =====================

/// One frame in a [Server-Sent Events][sse] stream. A frame may carry data
/// (the common case), a comment (heartbeats and debug pings), and the
/// optional `event` / `id` / `retry` fields. The wire encoding is handled
/// by [`SseEvent::to_bytes`] — embedded newlines in `data` become multiple
/// `data:` lines, the way the spec requires.
///
/// ```ignore
/// SseEvent::data("hello").event("greeting").id("42")
/// SseEvent::data("line1\nline2")  // becomes two `data: ...` lines
/// SseEvent::comment("keep-alive")
/// ```
///
/// [sse]: https://html.spec.whatwg.org/multipage/server-sent-events.html
#[derive(Default, Debug, Clone)]
pub struct SseEvent {
    event: Option<String>,
    id: Option<String>,
    retry: Option<Duration>,
    data: Option<String>,
    comment: Option<String>,
}

impl SseEvent {
    /// New event with a `data:` field. The most common entry point.
    pub fn data(data: impl Into<String>) -> Self {
        Self {
            data: Some(data.into()),
            ..Self::default()
        }
    }

    /// New event with just a comment line (`: ...\n\n`). Comments are
    /// ignored by `EventSource` clients but keep the connection alive
    /// through intermediaries — useful as a heartbeat when the stream is
    /// idle for long stretches.
    pub fn comment(text: impl Into<String>) -> Self {
        Self {
            comment: Some(text.into()),
            ..Self::default()
        }
    }

    /// Set the event-type name (the `event:` field). Clients can route on
    /// this with `EventSource.addEventListener(name, …)`. Embedded newlines
    /// are replaced with spaces (the spec doesn't allow newlines in event
    /// names).
    pub fn event(mut self, name: impl Into<String>) -> Self {
        self.event = Some(name.into());
        self
    }

    /// Set the `id:` field. The browser will replay it on reconnect via
    /// the `Last-Event-ID` request header — use it to support resumable
    /// streams.
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Set the `retry:` field — tells the client how long (in
    /// milliseconds) to wait before reconnecting after a drop.
    pub fn retry(mut self, d: Duration) -> Self {
        self.retry = Some(d);
        self
    }

    /// Encode this event in the SSE wire format, ending with the blank-line
    /// frame separator (`\n`). Used by [`sse`]; you rarely need to call it
    /// directly unless you're building a custom streaming pipeline.
    pub fn to_bytes(&self) -> Bytes {
        // Per HTML spec §9.2.6: lines starting with `:` are comments; each
        // field is `field-name: value\n` (the leading space after the colon
        // is consumed by the parser, so it's pretty but not significant);
        // a blank line ends the event. `data` spanning multiple lines is
        // expressed as one `data:` line per source line, then reassembled
        // by the client with `\n` joins.
        let mut out = String::new();
        if let Some(event) = &self.event {
            out.push_str("event: ");
            out.push_str(&single_line(event));
            out.push('\n');
        }
        if let Some(id) = &self.id {
            out.push_str("id: ");
            out.push_str(&single_line(id));
            out.push('\n');
        }
        if let Some(retry) = self.retry {
            // u128 → u64 saturate. >584M years of retry delay would
            // overflow; nobody cares.
            let ms = u64::try_from(retry.as_millis()).unwrap_or(u64::MAX);
            out.push_str("retry: ");
            out.push_str(&ms.to_string());
            out.push('\n');
        }
        if let Some(comment) = &self.comment {
            for line in comment.split('\n') {
                out.push(':');
                if !line.is_empty() {
                    out.push(' ');
                    out.push_str(line);
                }
                out.push('\n');
            }
        }
        if let Some(data) = &self.data {
            for line in data.split('\n') {
                out.push_str("data: ");
                out.push_str(line);
                out.push('\n');
            }
        }
        out.push('\n'); // blank line ends the event
        Bytes::from(out)
    }
}

/// Build a streaming Server-Sent Events response from a stream of
/// [`SseEvent`]s. Sets `Content-Type: text/event-stream` and
/// `Cache-Control: no-cache` (without which an intermediate cache might
/// hold the live stream, defeating the point).
///
/// ```ignore
/// use actus::prelude::*;
/// use futures_util::stream;
/// use std::time::Duration;
///
/// pub async fn updates(&self) -> Reply {
///     let events = stream::iter(vec![
///         SseEvent::data("tick").id("1"),
///         SseEvent::data("tick").id("2").retry(Duration::from_secs(5)),
///         SseEvent::comment("keep-alive"),
///     ]);
///     Ok(reply::sse(events))
/// }
/// ```
///
/// The stream is consumed lazily by the connection — frames are written
/// out as the stream yields, not buffered. When the stream ends, the
/// connection closes.
pub fn sse<S>(events: S) -> ReplyData
where
    S: Stream<Item = SseEvent> + Send + Sync + 'static,
{
    let byte_stream = events.map(|ev| Ok::<Bytes, io::Error>(ev.to_bytes()));
    let mut headers = HashMap::new();
    headers.insert("content-type".to_string(), "text/event-stream".to_string());
    headers.insert("cache-control".to_string(), "no-cache".to_string());
    ReplyData::Rich(Box::new(ReplySpec {
        payload: ReplyData::Stream(Box::pin(byte_stream)),
        status: None,
        headers,
    }))
}

/// Replace `\n` and `\r` with `\x20` (space). For the `event` and `id`
/// fields, where the SSE spec doesn't permit embedded newlines.
fn single_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

// ===================== Builder Pattern =====================

/// A builder for a reply with an explicit status and/or headers wrapped
/// around a payload. Start one with [`build_reply`], chain the setters, and
/// finish with [`ReplySpec::done`] (which yields a [`ReplyData::Rich`]).
#[derive(Debug, PartialEq)]
pub struct ReplySpec {
    /// The body payload this spec decorates.
    pub payload: ReplyData,
    /// The status code to send, or `None` to use the payload's default.
    pub status: Option<http::StatusCode>,
    /// Extra response headers, as name → value.
    pub headers: HashMap<String, String>,
}

impl Default for ReplySpec {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplySpec {
    /// Start an empty spec (no body, no status, no headers).
    pub fn new() -> Self {
        Self {
            payload: ReplyData::Empty,
            status: None,
            headers: HashMap::new(),
        }
    }

    /// Set the body payload.
    pub fn body(mut self, data: ReplyData) -> Self {
        self.payload = data;
        self
    }

    /// Set the response status code.
    pub fn status(mut self, s: http::StatusCode) -> Self {
        self.status = Some(s);
        self
    }

    /// Add a response header.
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(key.into(), value.into());
        self
    }

    /// Finish the builder, producing a [`ReplyData::Rich`].
    pub fn done(self) -> ReplyData {
        ReplyData::Rich(Box::new(self))
    }
}

// ===================== Ergonomic Macro =====================
/// Creates a `Reply` (a `Result<ReplyData, WebError>`) for a success case.
///
/// Accepts any [`Serialize`] value; failures during JSON conversion become
/// `Err(WebError::Internal(...))` (→ 500) rather than panicking. The
/// underlying [`json`] function still panics for callers that prefer to
/// surface bugs loudly; if you need that, call it directly.
///
/// ```ignore
/// reply!(my_struct)                          // serialize as JSON
/// reply!(json!({"status": "ok"}))            // inline JSON literal
/// reply!()                                   // 204 No Content
/// reply!(stream: byte_stream)                // streaming body
/// reply!(sse: event_stream)                  // Server-Sent Events
/// reply!(status = StatusCode::CREATED, value)
/// reply!(
///     status = StatusCode::CREATED,
///     headers = { "Location": "/users/123" },
///     value
/// )
/// ```
#[macro_export]
macro_rules! reply {
    // With status and headers
    (status = $status:expr, headers = { $($k:literal : $v:expr),* $(,)? }, $body:expr) => {{
        match $crate::reply::try_json($body) {
            Ok(__rd) => {
                let mut spec = $crate::reply::build_reply()
                    .status($status)
                    .body(__rd);
                $(
                    spec = spec.header($k, &$v.to_string());
                )*
                Ok(spec.done())
            }
            Err(e) => Err($crate::reply::WebError::Internal(
                ::std::format!("serialize response: {}", e),
            )),
        }
    }};
    // With just status
    (status = $status:expr, $body:expr) => {
        match $crate::reply::try_json($body) {
            Ok(__rd) => Ok($crate::reply::build_reply()
                .status($status)
                .body(__rd)
                .done()),
            Err(e) => Err($crate::reply::WebError::Internal(
                ::std::format!("serialize response: {}", e),
            )),
        }
    };
    // Stream
    (stream: $s:expr) => {
        Ok($crate::reply::stream($s))
    };
    // Server-Sent Events
    (sse: $s:expr) => {
        Ok($crate::reply::sse($s))
    };
    // Empty
    () => {
        Ok($crate::reply::empty())
    };
    // JSON from struct or literal
    ($expr:expr) => {
        match $crate::reply::try_json($expr) {
            Ok(__rd) => Ok(__rd),
            Err(e) => Err($crate::reply::WebError::Internal(
                ::std::format!("serialize response: {}", e),
            )),
        }
    };
}

// ===================== Error Type =====================

/// Structured error response shaped like an [RFC 7807 Problem Details]
/// document, plus arbitrary extension members that get serialized into
/// the JSON body.
///
/// Use this when one of the simple `WebError::*` variants doesn't carry
/// enough information (e.g., a validation failure that wants to name
/// the failing field and rule).
///
/// ```ignore
/// return Err(WebError::Problem(
///     ProblemDetails::new(StatusCode::CONFLICT, "Validation")
///         .detail("title is required")
///         .extra("field", "data.title")
///         .extra("rule", "required"),
/// ));
/// ```
///
/// Wire shape (`application/problem+json`):
/// ```json
/// { "status": 409, "title": "Validation", "detail": "title is required",
///   "field": "data.title", "rule": "required" }
/// ```
///
/// [RFC 7807 Problem Details]: https://datatracker.ietf.org/doc/html/rfc7807
#[derive(Debug, Clone)]
pub struct ProblemDetails {
    /// The HTTP status code, serialized as the `status` member.
    pub status: http::StatusCode,
    /// A short, human-readable summary, serialized as the `title` member.
    pub title: String,
    /// An optional longer explanation, serialized as the `detail` member.
    pub detail: Option<String>,
    /// Extension members. Serialized alongside `status` / `title` / `detail`.
    /// Keys that collide with the standard members are ignored.
    ///
    /// Boxed to keep `WebError` small: this map dominates the struct's size,
    /// yet extensions are optional and most errors carry none — so the common
    /// case pays one pointer, not a full inline map, on every `Result` return.
    pub extra: Box<serde_json::Map<String, serde_json::Value>>,
}

impl ProblemDetails {
    /// Construct a new problem with the given status code and title.
    pub fn new(status: http::StatusCode, title: impl Into<String>) -> Self {
        Self {
            status,
            title: title.into(),
            detail: None,
            extra: Box::new(serde_json::Map::new()),
        }
    }

    /// Set the human-readable detail message.
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Add an extension member. Use any JSON-serializable value via `.into()`,
    /// `serde_json::json!(…)`, or `serde_json::to_value(…)`.
    pub fn extra(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        self.extra.insert(key.into(), value.into());
        self
    }
}

impl fmt::Display for ProblemDetails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.detail {
            Some(d) => write!(f, "{}: {}", self.title, d),
            None => write!(f, "{}", self.title),
        }
    }
}

/// An error returned from a handler, a `prepare` hook, or middleware. Each
/// variant maps to an HTTP status the framework renders as the response
/// ([`WebError::Problem`] as an `application/problem+json` body).
#[derive(thiserror::Error, Debug)]
pub enum WebError {
    /// `404 Not Found`.
    #[error("Not found")]
    NotFound,
    /// `405`. Carries the methods the matched resource *does* accept, so the
    /// response can include the `Allow` header RFC 7231 §6.5.5 requires.
    /// Tokens are the canonical uppercase verb names (`"GET"`, `"POST"`, …);
    /// `actus-server`'s router builds this list, so handlers rarely construct
    /// it directly.
    #[error("Method not allowed; allowed methods: {0:?}")]
    MethodNotAllowed(Vec<&'static str>),
    /// `400 Bad Request`. The string is a human-readable reason (e.g. a
    /// malformed-body or missing-parameter explanation).
    #[error("Bad request: {0}")]
    BadRequest(String),
    /// `413`. The request body exceeded the configured limit (see
    /// `Server::with_max_body_bytes`).
    #[error("Payload too large")]
    PayloadTooLarge,
    /// `429`. Rate limit exceeded. The optional [`Duration`] is the
    /// retry-after hint — when present, the framework sets a
    /// `Retry-After: <seconds>` header on the response, per RFC 7231
    /// §7.1.3. Used by an application-supplied rate-limit middleware /
    /// prepare hook; the framework itself doesn't ship a limiter (policy
    /// belongs in the application; see the rate-limiting pattern in the
    /// README).
    #[error("Too many requests")]
    TooManyRequests(Option<Duration>),
    /// `504`. The request didn't complete within the configured timeout
    /// (see `Server::with_request_timeout`). Mapped to `504 Gateway
    /// Timeout`: the framework is acting as a gateway between the HTTP
    /// connection and the handler/middleware stack, and the upstream
    /// (handler) didn't respond in time. Produced by the framework when
    /// the per-request timer elapses; handlers can also `Err(Timeout)`
    /// from a downstream call that timed out themselves.
    #[error("Request timeout")]
    Timeout,
    /// `503`. The server is overloaded — typically the framework-level
    /// concurrency / buffer budget is exhausted (see
    /// `Server::with_max_inflight_body_bytes`). The optional [`Duration`]
    /// is a retry-after hint; when present, the framework sets a
    /// `Retry-After: <seconds>` header on the response. Distinct from
    /// `TooManyRequests` (429): `Busy` is "the server is overwhelmed
    /// globally," not "this client has hit its specific limit."
    #[error("Server busy")]
    Busy(Option<Duration>),
    /// `401 Unauthorized` — authentication is missing or invalid.
    #[error("Unauthorized")]
    Unauthorized,
    /// `403 Forbidden` — authenticated but not permitted.
    #[error("Forbidden")]
    Forbidden,
    /// `500 Internal Server Error`. The string is logged/returned as the
    /// reason; use it for unexpected failures, not for client mistakes.
    #[error("Internal error: {0}")]
    Internal(String),
    /// Structured error with an explicit status code, title, optional detail,
    /// and arbitrary extension members. Renders as `application/problem+json`.
    #[error("{0}")]
    Problem(ProblemDetails),
}

// ===================== Tests =====================
#[cfg(test)]
mod tests {
    // Note: bring everything from the parent module into scope for tests
    use super::*;
    use futures_util::{StreamExt, stream};
    use http::StatusCode;
    use serde_json::json;

    // Test that the macro now correctly returns a Result
    #[test]
    fn test_macro_returns_result() {
        let success_reply: Reply = reply!(json!({"status": "ok"}));
        assert!(success_reply.is_ok());

        let empty_reply: Reply = reply!();
        assert!(empty_reply.is_ok());
    }

    #[test]
    fn test_constructors() {
        matches!(json(json!({"ok": true})), ReplyData::Json(_));
        matches!(
            bytes("application/octet-stream", vec![0, 1]),
            ReplyData::Bytes { .. }
        );
        matches!(empty(), ReplyData::Empty);
    }

    #[tokio::test]
    async fn test_stream_constructor() {
        let test_stream = stream::once(async { Ok::<_, io::Error>(Bytes::from("test")) });
        let reply_data = stream(test_stream);
        if let ReplyData::Stream(mut s) = reply_data {
            assert_eq!(s.next().await.unwrap().unwrap(), "test");
        } else {
            panic!("Expected a stream reply");
        }
    }

    #[test]
    fn test_reply_macro_variants() {
        // JSON literal
        let data_res: Reply = reply!(json!({"a":1}));
        matches!(data_res.unwrap(), ReplyData::Json(_));

        // Struct
        #[derive(Serialize)]
        struct TestStruct {
            val: i32,
        }
        let data_res: Reply = reply!(TestStruct { val: 10 });
        matches!(data_res.unwrap(), ReplyData::Json(_));

        // Empty
        let data_res: Reply = reply!();
        matches!(data_res.unwrap(), ReplyData::Empty);

        // With status
        let data_res: Reply = reply!(status = StatusCode::CREATED, json!({"id": 1}));
        match data_res.unwrap() {
            ReplyData::Rich(spec) => {
                assert_eq!(spec.status, Some(StatusCode::CREATED));
            }
            _ => panic!("Expected Rich reply"),
        }

        // With status and headers
        let data_res: Reply =
            reply!(status = StatusCode::CREATED, headers = {"X-Test": "value"}, json!({"id": 1}));
        match data_res.unwrap() {
            ReplyData::Rich(spec) => {
                assert_eq!(spec.headers.get("X-Test"), Some(&"value".to_string()));
            }
            _ => panic!("Expected Rich reply"),
        }
    }

    #[test]
    fn add_header_lifts_non_rich_into_rich_preserving_payload() {
        // Json → Rich(Json), header set.
        let mut r = ReplyData::Json(json!({"x": 1}));
        r.add_header("X-Trace-Id", "abc");
        match &r {
            ReplyData::Rich(spec) => {
                assert!(matches!(spec.payload, ReplyData::Json(_)));
                assert_eq!(spec.headers.get("X-Trace-Id"), Some(&"abc".to_string()));
                assert_eq!(spec.status, None);
            }
            _ => panic!("expected Rich, got {r:?}"),
        }

        // Empty → Rich(Empty), header set.
        let mut r = ReplyData::Empty;
        r.add_header("X-K", "v");
        assert!(matches!(&r, ReplyData::Rich(s) if matches!(s.payload, ReplyData::Empty)));

        // Already Rich → mutate in place, payload untouched.
        let mut r = ReplyData::Rich(Box::new(ReplySpec {
            payload: ReplyData::Json(json!({"y": 2})),
            status: Some(StatusCode::CREATED),
            headers: HashMap::from([("Existing".into(), "1".into())]),
        }));
        r.add_header("New", "2");
        match &r {
            ReplyData::Rich(spec) => {
                assert_eq!(spec.status, Some(StatusCode::CREATED));
                assert_eq!(spec.headers.get("Existing"), Some(&"1".to_string()));
                assert_eq!(spec.headers.get("New"), Some(&"2".to_string()));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn set_status_lifts_or_mutates() {
        let mut r = ReplyData::Json(json!({"x": 1}));
        r.set_status(StatusCode::ACCEPTED);
        match &r {
            ReplyData::Rich(spec) => {
                assert_eq!(spec.status, Some(StatusCode::ACCEPTED));
                assert!(matches!(spec.payload, ReplyData::Json(_)));
            }
            _ => panic!("expected Rich"),
        }

        // Already Rich: mutate.
        let mut r = ReplyData::Rich(Box::new(ReplySpec {
            payload: ReplyData::Empty,
            status: None,
            headers: HashMap::new(),
        }));
        r.set_status(StatusCode::NOT_FOUND);
        if let ReplyData::Rich(spec) = &r {
            assert_eq!(spec.status, Some(StatusCode::NOT_FOUND));
        }
    }

    #[test]
    fn add_header_and_set_status_are_a_noop_on_upgrade() {
        let task: Box<dyn std::any::Any + Send> = Box::new(42i32); // any `Any+Send` is fine for this test
        let mut r = ReplyData::Upgrade(task);
        r.add_header("X-K", "v");
        r.set_status(StatusCode::CREATED);
        assert!(matches!(r, ReplyData::Upgrade(_))); // unchanged
    }

    // ===== SSE encoding =====
    //
    // Each test pins one bit of the W3C wire spec. Decode-side reassembly
    // would be the obvious sanity check, but a `tokio-tungstenite`-style
    // SSE-parser dep isn't worth pulling in for what amounts to "did we
    // emit the right bytes" — we read the encoded string directly.

    fn s(b: &Bytes) -> &str {
        std::str::from_utf8(b).expect("SSE encoding is always valid UTF-8")
    }

    #[test]
    fn sse_data_only_event_emits_one_data_line_and_blank_separator() {
        let bytes = SseEvent::data("hello").to_bytes();
        assert_eq!(s(&bytes), "data: hello\n\n");
    }

    #[test]
    fn sse_multi_line_data_emits_one_data_line_per_source_line() {
        // The browser reassembles by `\n`-joining the values, so a source
        // string "a\nb\nc" must round-trip as the same string.
        let bytes = SseEvent::data("a\nb\nc").to_bytes();
        assert_eq!(s(&bytes), "data: a\ndata: b\ndata: c\n\n");
    }

    #[test]
    fn sse_event_id_retry_data_all_in_one_frame() {
        let bytes = SseEvent::data("payload")
            .event("update")
            .id("17")
            .retry(Duration::from_millis(2500))
            .to_bytes();
        assert_eq!(
            s(&bytes),
            "event: update\nid: 17\nretry: 2500\ndata: payload\n\n",
        );
    }

    #[test]
    fn sse_event_and_id_strip_embedded_newlines() {
        // The spec doesn't allow `\n` in single-line fields. We replace
        // with a space rather than panic / reject — the value still
        // travels, the field stays well-formed.
        let bytes = SseEvent::data("ok").event("a\nb").id("x\ry").to_bytes();
        assert_eq!(s(&bytes), "event: a b\nid: x y\ndata: ok\n\n");
    }

    #[test]
    fn sse_comment_only_event_is_a_heartbeat() {
        // Multi-line comments: each source line becomes its own `:` line.
        // Empty comment lines emit a bare `:\n` (still a valid comment).
        let single = SseEvent::comment("keep-alive").to_bytes();
        assert_eq!(s(&single), ": keep-alive\n\n");
        let multi = SseEvent::comment("line1\n\nline3").to_bytes();
        assert_eq!(s(&multi), ": line1\n:\n: line3\n\n");
    }

    #[test]
    fn sse_empty_data_string_still_emits_a_data_field() {
        // `data("")` is explicit "send an empty data field"; absent data
        // (a comment-only event) skips the field entirely. The previous
        // test covers absent.
        let bytes = SseEvent::data("").to_bytes();
        assert_eq!(s(&bytes), "data: \n\n");
    }

    #[tokio::test]
    async fn sse_stream_wraps_in_rich_with_correct_headers_and_streams_each_event() {
        use futures_util::stream;
        let events = stream::iter(vec![
            SseEvent::data("first").id("1"),
            SseEvent::data("second").event("update"),
        ]);
        let reply = sse(events);
        let ReplyData::Rich(spec) = reply else {
            panic!("sse() produces a Rich reply for the headers");
        };
        assert_eq!(
            spec.headers.get("content-type"),
            Some(&"text/event-stream".to_string())
        );
        assert_eq!(
            spec.headers.get("cache-control"),
            Some(&"no-cache".to_string())
        );

        let ReplyData::Stream(byte_stream) = spec.payload else {
            panic!("sse() payload is a Stream of bytes");
        };
        // Drain the stream and concat to verify the wire framing of the
        // whole conversation.
        let chunks: Vec<Bytes> = byte_stream
            .map(|r| r.expect("infallible map"))
            .collect()
            .await;
        let joined: String = chunks
            .iter()
            .map(|b| std::str::from_utf8(b).unwrap())
            .collect();
        assert_eq!(
            joined,
            "id: 1\ndata: first\n\nevent: update\ndata: second\n\n",
        );
    }

    #[test]
    fn test_builder_pattern() {
        let data = build_reply()
            .status(StatusCode::ACCEPTED)
            .header("X-Custom", "value123")
            .body(bytes("application/zip", vec![1, 2, 3]))
            .done();

        match data {
            ReplyData::Rich(spec) => {
                assert_eq!(spec.status, Some(StatusCode::ACCEPTED));
                assert_eq!(spec.headers.get("X-Custom"), Some(&"value123".to_string()));
                assert_eq!(
                    spec.payload,
                    ReplyData::Bytes {
                        content_type: Cow::Borrowed("application/zip"),
                        data: vec![1, 2, 3],
                    }
                );
            }
            _ => panic!("Expected Rich reply"),
        }
    }
}
