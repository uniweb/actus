//! Integration tests for the middleware lifecycle.
//!
//! * `Outcome::Respond` short-circuits still run the after-chain — that's the
//!   contract the README's `Middleware` section promises and the bug fix that
//!   made it true.
//! * `Middleware::after` receives the `Request`, so a hook can stamp response
//!   headers from request context (request-id echoing, etc.).
//! * `ReplyData::add_header` lifts non-`Rich` replies into `Rich` so an after
//!   hook can decorate any payload without manually building a `ReplySpec`.
//! * Pre-parse errors (a body over `max_body_bytes`) still carry CORS headers
//!   so a browser can read the `413` body.
//! * **The after-chain runs on every reply with a body** — handler successes,
//!   `Outcome::Respond`, *and* every error (404 from the router, 405 from a
//!   verb mismatch, 401 from a middleware `Err`, 400 from a malformed body,
//!   the pre-parse 413). The dedicated tests below pin each of those paths.

use actus::prelude::*;
use serde_json::{Value as JsonValue, json};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

/// Short-circuits in `before` with a JSON reply. The handler is never reached.
struct ShortCircuit;

#[async_trait]
impl Middleware for ShortCircuit {
    async fn before(&self, _request: &mut Request) -> Result<Outcome, WebError> {
        Ok(Outcome::Respond(reply::json(json!({ "short": "circuit" }))))
    }
}

/// Echoes the request's `X-Trace-Id` onto every response in `after`. Exercises
/// `after`-sees-request + `ReplyData::add_header` + after-runs-on-short-circuit.
struct StampTraceId;

#[async_trait]
impl Middleware for StampTraceId {
    async fn after(&self, request: &Request, response: &mut ReplyData) -> Result<(), WebError> {
        if let Some(v) = request
            .headers
            .get("x-trace-id")
            .and_then(|v| v.to_str().ok())
        {
            response.add_header("X-Trace-Id", v);
        }
        Ok(())
    }
}

/// Short-circuits in `before` with an `Err`, simulating an auth gate that
/// rejects a request that carries a magic header. Used to prove the
/// after-chain fires on a middleware-`Err` short-circuit.
struct RejectIfHeader;

#[async_trait]
impl Middleware for RejectIfHeader {
    async fn before(&self, request: &mut Request) -> Result<Outcome, WebError> {
        if request.headers.contains_key("x-reject") {
            return Err(WebError::Unauthorized);
        }
        Ok(Outcome::Continue)
    }
}

/// Rejects with `429` (and a fixed `Retry-After`) when the matched
/// controller's rate-limit class equals `class`. Proves the
/// `#[controller(rate_limit = …)]` label travels from the macro, through the
/// server, onto `request.rate_limit_class` where a `before` middleware reads
/// it. The framework supplies the label and the 429; the policy ("reject
/// everything in this class") is this middleware's.
struct LimitClass {
    class: &'static str,
}

#[async_trait]
impl Middleware for LimitClass {
    async fn before(&self, request: &mut Request) -> Result<Outcome, WebError> {
        if request.rate_limit_class == Some(self.class) {
            return Err(WebError::TooManyRequests(Some(Duration::from_secs(7))));
        }
        Ok(Outcome::Continue)
    }
}

/// A controller with a 16-byte body cap. Used by the per-controller
/// `max_body_bytes` test below. Mounted at `/tight` in `app_routes!`.
struct Tight;

/// A controller that declares a rate-limit *class* via
/// `#[controller(rate_limit = "vip")]`. Mounted at `/classified`. The class
/// label is what `LimitClass` keys on.
struct Classified;

#[controller(rate_limit = "vip")]
impl Classified {
    routes! {
        GET "" => hello(),
    }

    pub async fn hello(&self) -> Reply {
        reply!(json!({ "ok": true }))
    }
}

#[controller(max_body_bytes = 16)]
impl Tight {
    routes! {
        POST "" => post(data: JsonValue),
    }

    pub async fn post(&self, data: JsonValue) -> Reply {
        reply!(data)
    }
}

struct Dummy;

#[controller]
impl Dummy {
    routes! {
        GET ""      => hello(),
        // Used by the malformed-body test — a route that explicitly takes a
        // JSON body so `to_params` parses one and a malformed body produces
        // the 400 we want to assert the after-chain fires on.
        POST "post" => post(data: JsonValue),
        // Used by the request-timeout test — sleeps far longer than any
        // reasonable timeout, so the framework's per-request timer is the
        // thing that decides when this request ends.
        GET "slow"  => slow(),
    }

    pub async fn hello(&self) -> Reply {
        reply!(json!({ "hello": "world" }))
    }

    pub async fn post(&self, data: JsonValue) -> Reply {
        reply!(data)
    }

    pub async fn slow(&self) -> Reply {
        tokio::time::sleep(Duration::from_secs(60)).await;
        reply!(json!({ "unreachable": true }))
    }
}

app_routes! {
    routes {
        // Dummy is mounted at `/svc` (not root) so paths *outside* its
        // subtree — e.g. `/no-such-route` — produce a Router-level 404
        // rather than being caught by a root-mount and 404'd inside the
        // controller. That distinction matters for the
        // `nonexistent_path_404s_without_buffering_the_body` test, which
        // proves the new lifecycle short-circuits before body buffer.
        "svc"        => Dummy,
        "tight"      => Tight,
        "classified" => Classified,
    }
}

/// Start a server configured by `f` on an ephemeral 127.0.0.1 port; return
/// `(addr, shutdown_tx)`. Polls until the port is listening before returning.
async fn spawn<F>(f: F) -> (SocketAddr, oneshot::Sender<()>)
where
    F: FnOnce(Server) -> Server + Send + 'static,
{
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let server = f(Server::new(init().await.unwrap()));
        server
            .run_with_shutdown_on(addr, async move {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (addr, tx)
}

/// Send a raw HTTP/1.1 request (`Connection: close`, so the server closes
/// after the response and `read_to_end` terminates). Parse the response into
/// `(status, headers, body)`. Kept dep-free on purpose.
async fn http(addr: SocketAddr, raw: &str) -> (u16, http::HeaderMap, Vec<u8>) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(raw.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(buf.len());
    let header_part = std::str::from_utf8(&buf[..split]).unwrap();
    let body = if split + 4 < buf.len() {
        buf[split + 4..].to_vec()
    } else {
        Vec::new()
    };
    let mut lines = header_part.split("\r\n");
    let status: u16 = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let mut headers = http::HeaderMap::new();
    for line in lines {
        if let Some((n, v)) = line.split_once(": ")
            && let (Ok(n), Ok(v)) = (
                http::HeaderName::from_bytes(n.as_bytes()),
                http::HeaderValue::from_str(v),
            )
        {
            headers.append(n, v);
        }
    }
    (status, headers, body)
}

#[tokio::test]
async fn after_hook_runs_on_short_circuit_sees_request_and_can_stamp_headers() {
    // Order matters: StampTraceId first → its `after` runs last (outermost
    // wrap); ShortCircuit second → its `before` short-circuits the chain.
    let (addr, stop) = spawn(|s| {
        s.with_middleware(StampTraceId)
            .with_middleware(ShortCircuit)
    })
    .await;
    let req =
        "GET /svc HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nX-Trace-Id: abc123\r\n\r\n";
    let (status, headers, body) = http(addr, req).await;

    // Short-circuit reply made it through (body is from `before`, not from `hello`).
    assert_eq!(status, 200);
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body, json!({ "short": "circuit" }));

    // The after-hook ran *despite* the short-circuit, *saw* the request, and
    // stamped a header on the (non-`Rich`) reply — `add_header` lifted it.
    assert_eq!(
        headers.get("X-Trace-Id").and_then(|v| v.to_str().ok()),
        Some("abc123"),
    );

    let _ = stop.send(());
}

#[tokio::test]
async fn pre_parse_413_carries_cors_headers_and_runs_after_chain() {
    // A body-cap 413 happens *before* `to_params`, but a `Request` skeleton
    // (method / path / query / headers) exists by the time the cap is hit,
    // so the after-chain runs on the error reply and CORS still applies.
    let (addr, stop) = spawn(|s| {
        s.with_middleware(StampTraceId)
            .with_max_body_bytes(16)
            .with_cors(CorsLayer::permissive())
    })
    .await;
    let body_bytes = "x".repeat(64);
    let req = format!(
        "POST /svc HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nOrigin: https://app.example.com\r\nX-Trace-Id: trace-413\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n{}",
        body_bytes.len(),
        body_bytes,
    );
    let (status, headers, _body) = http(addr, &req).await;

    assert_eq!(status, 413);
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example.com"),
        "a pre-parse 413 should still carry CORS headers — the browser needs them to read the body",
    );
    assert_eq!(
        headers.get("X-Trace-Id").and_then(|v| v.to_str().ok()),
        Some("trace-413"),
        "the after-chain should run on a pre-parse 413 (the request skeleton exists by then)",
    );

    let _ = stop.send(());
}

#[tokio::test]
async fn after_runs_on_router_404() {
    // No route matches `/missing/path` → 404 from the router. The after-chain
    // must still fire so a request-id stamper / response logger sees errors.
    let (addr, stop) = spawn(|s| s.with_middleware(StampTraceId)).await;
    let req = "GET /missing/path HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nX-Trace-Id: trace-404\r\n\r\n";
    let (status, headers, _body) = http(addr, req).await;

    assert_eq!(status, 404);
    assert_eq!(
        headers.get("X-Trace-Id").and_then(|v| v.to_str().ok()),
        Some("trace-404"),
        "the after-chain should run on a router 404",
    );
    let _ = stop.send(());
}

#[tokio::test]
async fn after_runs_on_router_405_and_allow_header_survives() {
    // `GET "" => hello()` is GET-only — a DELETE produces a 405 with the
    // `Allow` header. The after-chain runs on the error reply and the
    // X-Trace-Id stamp doesn't clobber `Allow` (the `Rich` headers map merges
    // with the framework-emitted `Allow`).
    let (addr, stop) = spawn(|s| s.with_middleware(StampTraceId)).await;
    let req = "DELETE /svc HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nX-Trace-Id: trace-405\r\n\r\n";
    let (status, headers, _body) = http(addr, req).await;

    assert_eq!(status, 405);
    assert_eq!(
        headers.get("Allow").and_then(|v| v.to_str().ok()),
        Some("GET"),
        "the 405 should still carry the framework-emitted Allow header",
    );
    assert_eq!(
        headers.get("X-Trace-Id").and_then(|v| v.to_str().ok()),
        Some("trace-405"),
        "the after-chain should run on a 405",
    );
    let _ = stop.send(());
}

#[tokio::test]
async fn after_runs_on_middleware_err_short_circuit() {
    // `RejectIfHeader::before` returns `Err(Unauthorized)` when `X-Reject` is
    // present. The after-chain (registered *first*, so it wraps everything)
    // must fire on the 401 reply.
    let (addr, stop) = spawn(|s| {
        s.with_middleware(StampTraceId)
            .with_middleware(RejectIfHeader)
    })
    .await;
    let req = "GET /svc HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nX-Reject: yes\r\nX-Trace-Id: trace-401\r\n\r\n";
    let (status, headers, _body) = http(addr, req).await;

    assert_eq!(status, 401);
    assert_eq!(
        headers.get("X-Trace-Id").and_then(|v| v.to_str().ok()),
        Some("trace-401"),
        "the after-chain should run on a middleware-Err short-circuit",
    );
    let _ = stop.send(());
}

#[tokio::test]
async fn request_timeout_yields_504_and_skips_the_after_chain() {
    // The slow handler sleeps 60s; the configured timeout is 100ms; the
    // request should come back as a 504 well under a second.
    //
    // The after-chain is *not* run on a timeout (by design — something
    // upstream was unresponsive, and running more risks hanging again).
    // The trace-id stamper would otherwise echo X-Trace-Id; here it
    // shouldn't.
    use std::time::Instant;

    let (addr, stop) = spawn(|s| {
        s.with_middleware(StampTraceId)
            .with_request_timeout(Duration::from_millis(100))
    })
    .await;
    let req = "GET /svc/slow HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nX-Trace-Id: trace-504\r\n\r\n";
    let start = Instant::now();
    let (status, headers, _body) = http(addr, req).await;
    let elapsed = start.elapsed();

    assert_eq!(status, 504);
    assert!(
        elapsed < Duration::from_secs(1),
        "504 should come back fast (got {elapsed:?})",
    );
    assert!(
        headers.get("X-Trace-Id").is_none(),
        "after-chain should not run on a timeout-504 (got {:?})",
        headers.get("X-Trace-Id"),
    );

    let _ = stop.send(());
}

#[tokio::test]
async fn controller_max_body_attribute_overrides_server_default() {
    // `Tight` has `#[controller(max_body_bytes = 16)]`; the server's
    // `with_max_body_bytes` is 1 MiB. Sending a 64-byte body to `/tight`
    // should get 413 (controller cap wins). Sending the same body to `/`
    // (Dummy, no controller cap) should succeed under the server default.
    let (addr, stop) = spawn(|s| s.with_max_body_bytes(1024 * 1024)).await;

    let body = "x".repeat(64);
    let req = format!(
        "POST /tight HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body,
    );
    let (status, _, _) = http(addr, &req).await;
    assert_eq!(
        status, 413,
        "Tight has max_body_bytes=16; a 64-byte body must be rejected",
    );

    // Same body, mounted controller without a cap → server default
    // applies (1 MiB), well above 64 bytes. Goes through to `Dummy::post`,
    // which 400s on `{` followed by 63 'x's (invalid JSON) — that's fine;
    // we just want to confirm the request body was *accepted* (i.e. not
    // rejected at the framework cap step).
    let req = format!(
        "POST /svc/post HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body,
    );
    let (status, _, _) = http(addr, &req).await;
    assert_eq!(
        status, 400,
        "no controller cap on Dummy → server default lets the body through, \
         then to_params rejects as malformed JSON",
    );

    let _ = stop.send(());
}

#[tokio::test]
async fn controller_max_body_allows_small_bodies() {
    // Sanity: a body under the controller cap goes through normally.
    let (addr, stop) = spawn(|s| s).await;
    let body = "{}"; // 2 bytes — well under Tight's 16-byte cap
    let req = format!(
        "POST /tight HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body,
    );
    let (status, _, body_bytes) = http(addr, req.as_str()).await;
    assert_eq!(status, 200);
    let resp: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(resp, serde_json::json!({}));
    let _ = stop.send(());
}

#[tokio::test]
async fn nonexistent_path_404s_without_buffering_the_body() {
    // The lifecycle reorder lets a 404 short-circuit before body
    // buffering. We prove this by making the body big enough that
    // buffering it would 413 — and asserting we get 404, not 413.
    let (addr, stop) = spawn(|s| s.with_max_body_bytes(16)).await;
    let body = "x".repeat(1024); // way over the 16-byte cap
    let req = format!(
        "POST /no-such-route HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body,
    );
    let (status, _, _) = http(addr, &req).await;
    assert_eq!(
        status, 404,
        "no matching controller → 404 *without* attempting to buffer the body",
    );
    let _ = stop.send(());
}

#[tokio::test]
async fn inflight_body_budget_returns_503_when_exhausted() {
    // Total budget = 32 bytes. Per-request cap = 32 bytes. A single
    // request reserves the whole budget for the duration of its body
    // buffer; a concurrent request can't fit and gets 503 + Retry-After.
    //
    // We force the contention by sending a body slowly: open two
    // connections, write half the headers + Content-Length on each
    // so they're committed to a body, then race the body bytes. The
    // first one to reserve wins; the second gets 503.
    //
    // Cleaner approach: configure tiny limits so even the first request's
    // *reservation* exceeds the budget by itself. Per-request cap = 32
    // bytes, total budget = 16 bytes → every request fails immediately.
    let (addr, stop) = spawn(|s| s.with_max_body_bytes(32).with_max_inflight_body_bytes(16)).await;
    let body = b"abcdefgh"; // 8 bytes — well under the per-request cap
    let req = format!(
        "POST /svc/post HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        std::str::from_utf8(body).unwrap(),
    );
    let (status, headers, _) = http(addr, &req).await;
    assert_eq!(
        status, 503,
        "per-request cap (32 B) exceeds the inflight budget (16 B); request refused",
    );
    let retry = headers
        .get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .expect("Retry-After header present");
    assert!(retry.parse::<u64>().is_ok(), "Retry-After is delta-seconds");
    let _ = stop.send(());
}

#[tokio::test]
async fn drain_deadline_is_honored_on_shutdown() {
    // Spin up the server with a 200 ms drain deadline. Open a slow-handler
    // connection so the server has something in-flight that won't finish
    // on its own. Signal shutdown; the server task must return in roughly
    // 200 ms (the drain deadline), not in seconds (which is what we'd see
    // if the handler ran to completion or the old hardcoded 30 s applied).
    use std::time::Instant;
    use tokio::sync::oneshot;

    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let addr = SocketAddr::from(([127, 0, 0, 1], port));

    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let server_task = tokio::spawn(async move {
        Server::new(init().await.unwrap())
            .with_drain_deadline(Duration::from_millis(200))
            .run_with_shutdown_on(addr, async move {
                let _ = stop_rx.await;
            })
            .await
            .unwrap();
    });

    // Wait for the server to bind.
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Open a slow request and keep the TCP read pending. The handler will
    // sleep 60 s; we just need hyper to have routed to it before we
    // shutdown. We don't .read() — we just hold the stream open.
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /svc/slow HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    // Give hyper a moment to dispatch the request to the handler.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Signal shutdown and time how long until the server task completes.
    let start = Instant::now();
    let _ = stop_tx.send(());
    let result = tokio::time::timeout(Duration::from_secs(5), server_task).await;
    let elapsed = start.elapsed();

    assert!(result.is_ok(), "server task didn't complete inside 5 s");
    // The 200 ms deadline + scheduling slack; certainly under 2 s.
    assert!(
        elapsed < Duration::from_secs(2),
        "drain took {elapsed:?} — expected ~200 ms, well under the legacy 30 s default",
    );

    drop(stream);
}

#[tokio::test]
async fn requests_under_timeout_succeed_normally() {
    // Sanity check: a normal request inside the timeout works fine, and
    // the after-chain still runs.
    let (addr, stop) = spawn(|s| {
        s.with_middleware(StampTraceId)
            .with_request_timeout(Duration::from_secs(5))
    })
    .await;
    let req =
        "GET /svc HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nX-Trace-Id: trace-ok\r\n\r\n";
    let (status, headers, _body) = http(addr, req).await;
    assert_eq!(status, 200);
    assert_eq!(
        headers.get("X-Trace-Id").and_then(|v| v.to_str().ok()),
        Some("trace-ok"),
    );
    let _ = stop.send(());
}

#[tokio::test]
async fn after_runs_on_malformed_body_400() {
    // `Content-Type: application/json` with invalid JSON → `to_params`
    // returns `Err(BadRequest)`. The after-chain fires on the 400 reply.
    let (addr, stop) = spawn(|s| s.with_middleware(StampTraceId)).await;
    let body = "{ this is not valid json";
    let req = format!(
        "POST /svc/post HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nX-Trace-Id: trace-400\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body,
    );
    let (status, headers, _body) = http(addr, &req).await;

    assert_eq!(status, 400);
    assert_eq!(
        headers.get("X-Trace-Id").and_then(|v| v.to_str().ok()),
        Some("trace-400"),
        "the after-chain should run on a 400 from to_params",
    );
    let _ = stop.send(());
}

#[tokio::test]
async fn rate_limit_class_label_reaches_middleware() {
    // `Classified` declares `#[controller(rate_limit = "vip")]`; the server
    // stamps that class onto the matched request, so a `before` middleware
    // (which only gets `&Request`, never the matched controller) can read it
    // and apply per-class policy. `LimitClass { class: "vip" }` rejects the
    // classed route with `429` + the `Retry-After` it chose, and leaves the
    // unclassed `/svc` (Dummy) alone — proving the label gates the limit.
    let (addr, stop) = spawn(|s| s.with_middleware(LimitClass { class: "vip" })).await;

    // Classed route → 429 with the limiter's Retry-After hint (delta-seconds).
    let req = "GET /classified HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    let (status, headers, _) = http(addr, req).await;
    assert_eq!(status, 429, "the 'vip'-classed controller is rate-limited");
    assert_eq!(
        headers.get("Retry-After").and_then(|v| v.to_str().ok()),
        Some("7"),
        "TooManyRequests(Some(7s)) finalizes to `Retry-After: 7`",
    );

    // Unclassed controller → its rate_limit_class is None, so the limiter
    // passes it through. (Dummy's `GET \"\"` returns the hello payload.)
    let req = "GET /svc HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    let (status, _, _) = http(addr, req).await;
    assert_eq!(
        status, 200,
        "a controller with no rate-limit class is not limited"
    );

    let _ = stop.send(());
}
