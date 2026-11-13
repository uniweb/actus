//! `Server::with_access_log` and `Server::with_request_span`: one event per response, written by
//! the server when the response ends, with the status sent, inside the request's span. Plus
//! `RequestLogger`, the middleware that logs a request as it is routed.
//!
//! Each test reads the events through a subscriber of its own, set as the thread's default. The
//! tests run on tokio's single-threaded runtime, so the server's tasks report to it too. Every
//! test stops its server and waits for the drain before reading, so each response's event has
//! been written whenever hyper let go of its body. The keep-alive test reads before that, on
//! purpose.

use actus::prelude::*;
use bytes::Bytes;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

struct Svc;

#[controller(prepare = Self::who)]
impl Svc {
    routes! {
        GET "" => hello(),
        GET "slow" => slow(),
        GET "stream" => stream(),
    }

    /// Records who the request is for, as an application's `prepare` hook does once it knows.
    async fn who(
        &self,
        _route: &RouteDef,
        _params: &mut Params,
    ) -> Result<Option<ReplyData>, WebError> {
        tracing::Span::current().record("who", tracing::field::display("alice"));
        Ok(None)
    }

    pub async fn hello(&self) -> Reply {
        reply!(json!({ "hello": "world" }))
    }

    /// Outlives any request timeout a test sets.
    pub async fn slow(&self) -> Reply {
        tokio::time::sleep(Duration::from_secs(5)).await;
        reply!()
    }

    /// Two chunks 150 ms apart: the response lives at least that long.
    pub async fn stream(&self) -> Reply {
        let chunks = futures_util::stream::unfold(0u8, |n| async move {
            match n {
                0 => Some((Ok(Bytes::from_static(b"first")), 1)),
                1 => {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    Some((Ok(Bytes::from_static(b"second")), 2))
                }
                _ => None,
            }
        });
        reply!(stream: chunks)
    }
}

app_routes! {
    routes {
        "svc" => Svc,
    }
}

/// The span the tests name: a correlation id from the request, and `who` for `prepare`.
fn span_from(req: &Request) -> tracing::Span {
    let id = req
        .headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok());
    tracing::info_span!("request", id = %id.unwrap_or("-"), who = tracing::field::Empty)
}

/// An `after` hook that fails, swapping every reply for a `500`.
struct FailsAfter;

#[async_trait]
impl Middleware for FailsAfter {
    async fn after(&self, _request: &Request, _response: &mut ReplyData) -> Result<(), WebError> {
        Err(WebError::Internal("after hook failed".into()))
    }
}

/// What a subscriber wrote, line by line.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Captured {
    fn lines(&self) -> Vec<String> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The access-log lines.
    fn access(&self) -> Vec<String> {
        self.lines()
            .into_iter()
            .filter(|l| l.contains(" actus::access: "))
            .collect()
    }

    /// The one access-log line for `path`.
    fn access_for(&self, path: &str) -> String {
        let lines: Vec<String> = self
            .access()
            .into_iter()
            .filter(|l| l.contains(&format!(" path={path} ")))
            .collect();
        assert_eq!(
            lines.len(),
            1,
            "one access line for {path}: {:#?}",
            self.lines()
        );
        lines.into_iter().next().unwrap()
    }
}

fn capture() -> (Captured, tracing::subscriber::DefaultGuard) {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    (captured, tracing::subscriber::set_default(subscriber))
}

/// A server under test.
struct Running {
    addr: SocketAddr,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Running {
    /// Stop the server and wait for it to drain, so every response's event has been written.
    async fn stop(self) {
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("the server drains")
            .unwrap();
    }
}

/// Start a server for `router`, configured by `f`, on an ephemeral port, handing it the bound
/// listener (never bind, drop and re-bind: see `tests/middleware.rs`).
async fn spawn<F>(router: Router, f: F) -> Running
where
    F: FnOnce(Server) -> Server + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        f(Server::new(router))
            .run_with_shutdown_listener(listener, async move {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    Running { addr, stop, task }
}

/// Send `raw` (which must ask for `Connection: close`); the response's status and its raw text.
async fn send(addr: SocketAddr, raw: &str) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(raw.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text.split(' ').nth(1).unwrap().parse().unwrap();
    (status, text)
}

/// `GET path` with the given `X-Request-Id`; the response's status.
async fn get(addr: SocketAddr, path: &str, id: &str) -> u16 {
    let raw = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Request-Id: {id}\r\nConnection: close\r\n\r\n"
    );
    send(addr, &raw).await.0
}

fn elapsed_ms(line: &str) -> f64 {
    let (_, rest) = line.split_once("elapsed_ms=").expect("an elapsed_ms field");
    rest.split_whitespace().next().unwrap().parse().unwrap()
}

/// ⭐ One line per response, with the status sent, inside the span: the field `make` set from
/// the request, and the one `prepare` recorded. ⭐ The control: a `404` from the router is
/// logged too — no handler, `prepare` or middleware ran for it — with the field `make` set,
/// which proves the span is made before routing, and without the one `prepare` would have.
#[tokio::test]
async fn every_response_is_logged_once_inside_its_span() {
    let (captured, _guard) = capture();
    let server = spawn(init().await.unwrap(), |s| {
        s.with_request_span(span_from).with_access_log()
    })
    .await;
    assert_eq!(get(server.addr, "/svc", "r1").await, 200);
    assert_eq!(get(server.addr, "/nowhere", "r2").await, 404);
    server.stop().await;

    let ok = captured.access_for("/svc");
    assert!(
        ok.contains(" INFO request{id=r1 who=alice}: actus::access: "),
        "{ok}"
    );
    assert!(ok.contains("method=GET"), "{ok}");
    assert!(ok.contains("status=200"), "{ok}");
    assert!(elapsed_ms(&ok) >= 0.0, "{ok}");

    let missing = captured.access_for("/nowhere");
    assert!(
        missing.contains("request{id=r2}: actus::access: "),
        "{missing}"
    );
    assert!(missing.contains("status=404"), "{missing}");

    assert_eq!(captured.access().len(), 2, "{:#?}", captured.lines());
}

/// A request timeout's `504` skips the `after` chain; the access log has it anyway, inside the
/// request's span — and so is the server's warning about it.
#[tokio::test]
async fn a_timed_out_request_is_logged_with_its_504() {
    let (captured, _guard) = capture();
    let server = spawn(init().await.unwrap(), |s| {
        s.with_request_span(span_from)
            .with_request_timeout(Duration::from_millis(50))
            .with_access_log()
    })
    .await;
    assert_eq!(get(server.addr, "/svc/slow", "r3").await, 504);
    server.stop().await;

    let line = captured.access_for("/svc/slow");
    assert!(
        line.contains("request{id=r3 who=alice}: actus::access: "),
        "{line}"
    );
    assert!(line.contains("status=504"), "{line}");
    assert!(elapsed_ms(&line) >= 50.0, "{line}");

    let lines = captured.lines();
    let warned = lines
        .iter()
        .find(|l| l.contains("request exceeded configured timeout"))
        .unwrap_or_else(|| panic!("no timeout warning: {lines:#?}"));
    assert!(warned.contains("request{id=r3 who=alice}"), "{warned}");
}

/// An `after` hook's error replaces the reply with a `500` and stops the `after` chain; the
/// access log reports the `500` the client got.
#[tokio::test]
async fn an_after_hooks_error_is_logged_as_the_500_it_became() {
    let (captured, _guard) = capture();
    let server = spawn(init().await.unwrap(), |s| {
        s.with_middleware(FailsAfter).with_access_log()
    })
    .await;
    assert_eq!(get(server.addr, "/svc", "r4").await, 500);
    server.stop().await;

    let line = captured.access_for("/svc");
    assert!(line.contains("status=500"), "{line}");
}

/// A streamed response is logged when its stream ends, so its time is its whole life: at least
/// the 150 ms between its two chunks, where a log written as the reply was built would say ~0.
#[tokio::test]
async fn a_streamed_response_is_logged_when_its_stream_ends() {
    let (captured, _guard) = capture();
    let server = spawn(init().await.unwrap(), |s| s.with_access_log()).await;
    assert_eq!(get(server.addr, "/svc/stream", "r5").await, 200);
    server.stop().await;

    let line = captured.access_for("/svc/stream");
    assert!(line.contains("status=200"), "{line}");
    assert!(elapsed_ms(&line) >= 150.0, "{line}");
}

/// A CORS preflight is answered by the server before any middleware or routing; it is still a
/// response the server sent, so it is logged.
#[tokio::test]
async fn a_cors_preflight_is_logged() {
    let (captured, _guard) = capture();
    let server = spawn(init().await.unwrap(), |s| {
        s.with_cors(CorsLayer::permissive()).with_access_log()
    })
    .await;
    let raw = "OPTIONS /svc HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: https://app.example.com\r\n\
               Access-Control-Request-Method: GET\r\nConnection: close\r\n\r\n";
    assert_eq!(send(server.addr, raw).await.0, 204);
    server.stop().await;

    let line = captured.access_for("/svc");
    assert!(line.contains("method=OPTIONS"), "{line}");
    assert!(line.contains("status=204"), "{line}");
}

/// The wrapper leaves hyper's framing alone — a buffered reply keeps its `Content-Length` — and
/// a response's line is written as the response ends, not when its connection closes: on a
/// kept-alive connection, the first response's line is there before the second request is sent.
#[tokio::test]
async fn a_line_is_written_per_response_and_framing_is_untouched() {
    let (captured, _guard) = capture();
    let server = spawn(init().await.unwrap(), |s| s.with_access_log()).await;

    let mut stream = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    stream
        .write_all(b"GET /svc HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    let head_end = loop {
        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0, "the connection closed early");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
    let length: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length: "))
        .unwrap_or_else(|| panic!("no content-length: {head}"))
        .trim()
        .parse()
        .unwrap();
    assert!(!head.contains("transfer-encoding"), "{head}");
    while buf.len() < head_end + length {
        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0, "the connection closed early");
        buf.extend_from_slice(&chunk[..n]);
    }
    assert_eq!(captured.access().len(), 1, "{:#?}", captured.lines());

    stream
        .write_all(b"GET /svc HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).await.unwrap();
    server.stop().await;
    assert_eq!(captured.access().len(), 2, "{:#?}", captured.lines());
}

/// The access log is opt-in: a server without it writes nothing under `actus::access`.
#[tokio::test]
async fn without_the_access_log_nothing_is_written_under_its_target() {
    let (captured, _guard) = capture();
    let server = spawn(init().await.unwrap(), |s| s.with_request_span(span_from)).await;
    assert_eq!(get(server.addr, "/svc", "r6").await, 200);
    server.stop().await;

    assert!(captured.access().is_empty(), "{:#?}", captured.lines());
}

/// With no span named, a request still runs in one called `request`, and is logged in it.
#[tokio::test]
async fn without_a_named_span_a_request_is_logged_in_the_default_one() {
    let (captured, _guard) = capture();
    let server = spawn(init().await.unwrap(), |s| s.with_access_log()).await;
    assert_eq!(get(server.addr, "/svc", "r7").await, 200);
    server.stop().await;

    let line = captured.access_for("/svc");
    assert!(line.contains(" INFO request: actus::access: "), "{line}");
}

/// `RequestLogger` writes its line from its `before` hook, as the request is routed: method and
/// path, no status. A `404` from the router never reaches the `before` chain, so it leaves no
/// `RequestLogger` line — the access log is what covers it.
#[tokio::test]
async fn request_logger_writes_its_line_as_the_request_is_routed() {
    let (captured, _guard) = capture();
    let server = spawn(init().await.unwrap(), |s| {
        s.with_middleware(RequestLogger).with_access_log()
    })
    .await;
    assert_eq!(get(server.addr, "/svc", "r8").await, 200);
    assert_eq!(get(server.addr, "/nowhere", "r9").await, 404);
    server.stop().await;

    let lines = captured.lines();
    let routed: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains(" request method="))
        .collect();
    assert_eq!(routed.len(), 1, "{lines:#?}");
    assert!(routed[0].contains("path=/svc"), "{}", routed[0]);
    assert!(!routed[0].contains("status="), "{}", routed[0]);
    assert_eq!(captured.access().len(), 2, "{lines:#?}");
}

/// A WebSocket handshake's `101` skips the `after` chain; it is a response the server sent, so
/// the access log has it.
#[cfg(feature = "websocket")]
mod websocket {
    use super::*;

    struct Live;

    #[controller]
    impl Live {
        routes! {
            GET "" => open(),
        }

        pub async fn open(&self) -> Reply {
            Ok(ws::upgrade(|_socket| async {}))
        }
    }

    app_routes! {
        routes {
            "live" => Live,
        }
    }

    #[tokio::test]
    async fn a_websocket_handshake_is_logged_with_its_101() {
        let (captured, _guard) = capture();
        let server = spawn(init().await.unwrap(), |s| s.with_access_log()).await;
        let raw = "GET /live HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: Upgrade\r\n\
                   Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
                   Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n";
        let (status, _) = send(server.addr, raw).await;
        assert_eq!(status, 101);
        server.stop().await;

        let line = captured.access_for("/live");
        assert!(line.contains("status=101"), "{line}");
    }
}
