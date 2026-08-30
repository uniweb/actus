//! Integration tests for the advanced example, using the daemon-guard
//! pattern from the README §Patterns/"HTTP integration tests via a `Daemon`
//! guard". Each test spawns the binary as a subprocess on an ephemeral port,
//! makes real HTTP requests, and lets `Drop` reap the child — so a panicking
//! test doesn't leak a server.
//!
//! The pattern's docs in the README mention `reqwest::Client`; we keep this
//! test dep-free by talking HTTP/1.1 over a raw `TcpStream`, the same shape
//! as the workspace's existing integration tests. The Daemon guard itself is
//! the load-bearing piece; the HTTP client is an implementation detail.

use std::net::SocketAddr;
use std::process::{Child, Command};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Holds the subprocess and the port it bound to. `Drop` kills the child
/// and reaps it — a panicking test that drops a `Daemon` mid-flight won't
/// leak a server process. (Kill is `SIGKILL` on Unix; the child has no
/// shutdown work to flush, so the abrupt stop is fine for tests.)
struct Daemon {
    child: Option<Child>,
    addr: SocketAddr,
}

impl Daemon {
    async fn spawn() -> Self {
        // bind 127.0.0.1:0 → take the OS-assigned port → drop the listener
        // so the daemon can rebind. Tiny TOCTOU window where another process
        // could grab the port; in practice never fires on a dev box, and
        // the daemon errors loudly on bind if it does.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind ephemeral")
            .local_addr()
            .expect("local_addr")
            .port();
        let addr = SocketAddr::from(([127, 0, 0, 1], port));

        let mut child = Command::new(env!("CARGO_BIN_EXE_actus-advanced-example"))
            .args(["--port", &port.to_string()])
            // Silence the daemon's own tracing output so the test runner's
            // log noise stays manageable. (Each daemon emits ~3 lines per
            // request; the rate-limit test makes 40+ requests.)
            .env("RUST_LOG", "warn")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn daemon");

        // Poll the port until it accepts a connection (or give up). The
        // daemon prints a tracing line when it binds; we don't have to
        // scrape that — the TCP connect is the cheap, reliable signal.
        for _ in 0..200 {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                return Self {
                    child: Some(child),
                    addr,
                };
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        // Daemon never came up. Reap it before panicking — otherwise it'd
        // leak as a zombie, since `std::process::Child::drop` is a no-op
        // by design.
        let _ = child.kill();
        let _ = child.wait();
        panic!("daemon never started listening on {addr}");
    }

    /// Send an HTTP/1.1 request and parse the response into
    /// `(status, headers, body_bytes)`.
    ///
    /// Uses keep-alive (no `Connection: close`) and reads exactly the
    /// response's `Content-Length` bytes — the server never has to close, and
    /// we never depend on EOF. On top of that, the request is retried on a
    /// fresh connection if the transport faults: a freshly-spawned subprocess
    /// daemon over loopback, under the CPU contention of the parallel test
    /// runner, occasionally resets a connection before a complete response
    /// arrives (~3%). That's a transport hiccup, not behavior under test — a
    /// genuinely wrong status or body is returned and asserted on, and would
    /// fail on every attempt, so the retry cannot mask a real bug.
    async fn http(
        &self,
        method: &str,
        path: &str,
        extra_headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> (u16, http::HeaderMap, Vec<u8>) {
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
        for (k, v) in extra_headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some(b) = body {
            req.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
        req.push_str("\r\n");

        let mut last_err = None;
        for attempt in 0..8 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            match self.try_http(&req, body).await {
                Ok(parsed) => return parsed,
                Err(e) => last_err = Some(e),
            }
        }
        panic!(
            "request {method} {path} to {} failed after retries: {last_err:?}",
            self.addr
        );
    }

    /// One round-trip on a fresh connection. Returns `Err` on a transport
    /// fault so [`Self::http`] can retry; a returned response is authoritative.
    async fn try_http(
        &self,
        req: &str,
        body: Option<&[u8]>,
    ) -> std::io::Result<(u16, http::HeaderMap, Vec<u8>)> {
        let mut stream = tokio::net::TcpStream::connect(self.addr).await?;
        stream.write_all(req.as_bytes()).await?;
        if let Some(b) = body {
            stream.write_all(b).await?;
        }
        let mut buf = Vec::new();
        read_http_response(&mut stream, &mut buf).await?;
        Ok(parse_http_response(&buf))
    }

    async fn get(&self, path: &str, headers: &[(&str, &str)]) -> (u16, http::HeaderMap, Vec<u8>) {
        self.http("GET", path, headers, None).await
    }

    async fn post_json(
        &self,
        path: &str,
        headers: &[(&str, &str)],
        body: serde_json::Value,
    ) -> (u16, http::HeaderMap, Vec<u8>) {
        let bytes = serde_json::to_vec(&body).expect("serialize body");
        let mut all = vec![("Content-Type", "application/json")];
        all.extend_from_slice(headers);
        self.http("POST", path, &all, Some(&bytes)).await
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Read a complete HTTP/1.1 response into `buf` — the status line and headers,
/// then exactly `Content-Length` body bytes — without relying on EOF. Returns
/// `Err` on any transport error (so the caller can retry on a fresh
/// connection) and on a connection closed before the message is complete.
async fn read_http_response(
    stream: &mut tokio::net::TcpStream,
    buf: &mut Vec<u8>,
) -> std::io::Result<()> {
    let mut chunk = [0u8; 8192];
    while !response_complete(buf) {
        match stream.read(&mut chunk).await {
            Ok(0) => {
                return if response_complete(buf) {
                    Ok(())
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "connection closed before a complete response arrived",
                    ))
                };
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Parse a fully-buffered HTTP/1.1 response into `(status, headers, body)`.
fn parse_http_response(buf: &[u8]) -> (u16, http::HeaderMap, Vec<u8>) {
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(buf.len());
    let header_part = std::str::from_utf8(&buf[..split]).expect("utf-8 headers");
    let body_bytes = if split + 4 < buf.len() {
        buf[split + 4..].to_vec()
    } else {
        Vec::new()
    };
    let mut lines = header_part.split("\r\n");
    let status: u16 = lines
        .next()
        .expect("status line")
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("status u16");
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
    (status, headers, body_bytes)
}

/// True once `buf` holds the full header block plus all declared
/// `Content-Length` body bytes. A response with no `Content-Length`
/// (e.g. `204 No Content`) is complete as soon as the `\r\n\r\n` header
/// terminator is present.
fn response_complete(buf: &[u8]) -> bool {
    let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let headers_end = pos + 4;
    let Ok(headers) = std::str::from_utf8(&buf[..pos]) else {
        return false;
    };
    let content_len = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    buf.len() >= headers_end + content_len
}

fn parse_json(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).expect("response is JSON")
}

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
async fn health_check() {
    let d = Daemon::spawn().await;
    let (status, _, _) = d.get("/health", &[]).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn anonymous_can_read_but_not_write() {
    // Pattern under test: `lax_auth` lets anonymous through; handlers gate
    // writes via `require_role`. So anonymous → 200 on GET, 401 on POST.
    let d = Daemon::spawn().await;

    let (status, _, _) = d.get("/api/tasks", &[]).await;
    assert_eq!(status, 200, "anonymous list is OK");

    let (status, _, body) = d
        .post_json("/api/tasks", &[], serde_json::json!({"title": "x"}))
        .await;
    assert_eq!(status, 401, "anonymous create is rejected");
    let problem = parse_json(&body);
    assert_eq!(problem["status"], 401);
}

#[tokio::test]
async fn member_can_create_admin_can_delete() {
    // Pattern under test: `require_role(Member)` / `require_role(Admin)`.
    // alice is a Member, root is an Admin. alice can create; alice cannot
    // delete (403 with `required_role: admin` in the body); root can.
    let d = Daemon::spawn().await;

    let (status, _, body) = d
        .post_json(
            "/api/tasks",
            &[("Authorization", "Bearer alice-token")],
            serde_json::json!({"title": "Read the manual", "tags": ["docs"]}),
        )
        .await;
    assert_eq!(status, 201, "alice (Member) can create");
    let task = parse_json(&body);
    let id = task["id"].as_u64().expect("id");
    assert_eq!(task["title"], "Read the manual");

    let (status, _, body) = d
        .http(
            "DELETE",
            &format!("/api/tasks/{id}"),
            &[("Authorization", "Bearer alice-token")],
            None,
        )
        .await;
    assert_eq!(status, 403, "alice (Member) cannot delete");
    let problem = parse_json(&body);
    assert_eq!(problem["required_role"], "admin");
    assert_eq!(problem["actor_role"], "member");

    let (status, _, _) = d
        .http(
            "DELETE",
            &format!("/api/tasks/{id}"),
            &[("Authorization", "Bearer admin-token")],
            None,
        )
        .await;
    assert_eq!(status, 204, "root (Admin) can delete");

    // After delete the task is gone — GET → 404 mapped from `MyError::NotFound`.
    let (status, _, _) = d.get(&format!("/api/tasks/{id}"), &[]).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn malformed_json_body_yields_informative_400() {
    // Pattern under test: `typed_body` maps `serde_json::Error` into a
    // `BadRequest` whose `detail` names the failure. The client gets a
    // structured 400, not an opaque 500.
    let d = Daemon::spawn().await;
    let (status, _, body) = d
        .http(
            "POST",
            "/api/tasks",
            &[
                ("Authorization", "Bearer alice-token"),
                ("Content-Type", "application/json"),
            ],
            Some(b"{ not valid json"),
        )
        .await;
    assert_eq!(status, 400);
    let problem = parse_json(&body);
    assert_eq!(problem["title"], "Bad Request");
    let detail = problem["detail"].as_str().unwrap_or("");
    assert!(
        detail.contains("valid JSON"),
        "detail names the failure ({detail})"
    );
}

#[tokio::test]
async fn validation_error_carries_field_and_rule() {
    // Pattern under test: a domain `MyError::Validation` flows through
    // `From<MyError> for WebError` into a `Problem` with `field` + `rule`
    // extension members the client can program against.
    let d = Daemon::spawn().await;
    let (status, _, body) = d
        .post_json(
            "/api/tasks",
            &[("Authorization", "Bearer alice-token")],
            serde_json::json!({"title": ""}),
        )
        .await;
    assert_eq!(status, 400);
    let problem = parse_json(&body);
    assert_eq!(problem["title"], "Validation");
    assert_eq!(problem["field"], "title");
    assert_eq!(problem["rule"], "non-empty");
}

#[tokio::test]
async fn me_endpoint_requires_auth_and_echoes_the_user() {
    // Pattern under test: `params.require_user()` short-circuits with 401
    // for anonymous, and returns the User stashed by `lax_auth` for an
    // authenticated request.
    let d = Daemon::spawn().await;

    let (status, _, _) = d.get("/api/me", &[]).await;
    assert_eq!(status, 401, "anonymous → 401");

    let (status, _, body) = d
        .get("/api/me", &[("Authorization", "Bearer bob-token")])
        .await;
    assert_eq!(status, 200);
    let user = parse_json(&body);
    assert_eq!(user["name"], "bob");
    assert_eq!(user["role"], "editor");
}

#[tokio::test]
async fn rate_limit_returns_429_with_retry_after() {
    // Pattern under test: per-controller rate-limit *class*. `TasksController`
    // declares `#[controller(rate_limit = "tasks")]`; the server stamps that
    // class onto the matched request; the `RateLimit` middleware reads it and
    // applies the `"tasks"` policy. When the bucket empties it emits
    // `WebError::TooManyRequests(Some(d))`, which the framework finalizes as
    // `429` + `Retry-After: <seconds>` (delta-seconds per RFC 7231 §7.1.3) +
    // a `retry_after_seconds` extra in the problem body.
    //
    // The "tasks" class has 30 burst tokens in `main()`; we burn through them
    // (and over) on a fresh daemon, anonymously (GET list is anonymous-OK).
    let d = Daemon::spawn().await;
    let mut got_429 = false;
    let mut retry_after_header: Option<String> = None;
    for _ in 0..40 {
        let (status, headers, _body) = d.get("/api/tasks", &[]).await;
        if status == 429 {
            got_429 = true;
            retry_after_header = headers
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            break;
        }
    }
    assert!(got_429, "rate limit didn't kick in within 40 requests");
    let retry = retry_after_header.expect("Retry-After header present");
    assert!(
        retry.parse::<u64>().is_ok(),
        "Retry-After is delta-seconds: {retry}"
    );
}

#[tokio::test]
async fn floor_gate_refuses_anonymous_before_anything_else() {
    // Pattern under test: the declaration-keyed floor gate. `MeController`
    // declares `expects = "credential"`; `FloorGate` reads that off the
    // matched controller (via `server.router()` + `match_controller`) and
    // refuses a request that presents nothing at all — with a
    // self-describing title, which is how we know THIS refusal came from
    // the gate and not from the handler's `require_user()`.
    let d = Daemon::spawn().await;

    let (status, _, body) = d.get("/api/me", &[]).await;
    assert_eq!(status, 401, "bare-anonymous → refused at the floor");
    let problem = parse_json(&body);
    assert_eq!(problem["title"], "Credential Required");

    // A garbage credential *passes the presence floor* and is then refused
    // by the hook (`lax_auth` fails to resolve it) — same status, different
    // title. The gate is defense in depth, not the auth system.
    let (status, _, body) = d
        .get("/api/me", &[("Authorization", "Bearer not-a-real-token")])
        .await;
    assert_eq!(status, 401, "unresolvable credential → refused by the hook");
    let problem = parse_json(&body);
    assert_ne!(
        problem["title"], "Credential Required",
        "past the gate; this 401 is the hook's"
    );

    // A real credential sails through both.
    let (status, _, _) = d
        .get("/api/me", &[("Authorization", "Bearer alice-token")])
        .await;
    assert_eq!(status, 200);

    // And the gate does not touch an `"anonymous"`-floor controller:
    // anonymous reads on /api/tasks keep working (its writes are gated
    // per-handler, asserted elsewhere).
    let (status, _, _) = d.get("/api/tasks", &[]).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn unclassed_route_is_not_rate_limited() {
    // The flip side of the per-class label: `HealthController` declares no
    // `rate_limit`, so its `rate_limit_class` is `None` and the limiter
    // passes it through. 50 rapid requests (well past any class's burst)
    // all succeed — exactly the behavior you want for a liveness probe.
    let d = Daemon::spawn().await;
    for i in 0..50 {
        let (status, _, _) = d.get("/health", &[]).await;
        assert_eq!(
            status, 200,
            "/health request {i} should not be rate-limited"
        );
    }
}
