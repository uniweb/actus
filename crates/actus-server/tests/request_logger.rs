//! `RequestLogger` and `Server::with_request_span`: one event per request, written when its
//! reply is ready, with the method, the routed path, the status and the time taken — inside the
//! request's span, so a field the application records there is printed with it.
//!
//! Each test reads the events through a subscriber of its own, set as the thread's default. The
//! tests run on tokio's single-threaded runtime, so the server's tasks report to it too.

use actus::prelude::*;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

struct Hello;

#[controller]
impl Hello {
    routes! {
        GET "" => hello(),
    }

    /// Records who the request is for, as an application's `prepare` hook would once it knows.
    pub async fn hello(&self) -> Reply {
        tracing::Span::current().record("who", tracing::field::display("alice"));
        reply!(json!({ "hello": "world" }))
    }
}

app_routes! {
    routes {
        "svc" => Hello,
    }
}

fn request_span() -> tracing::Span {
    tracing::info_span!("request", who = tracing::field::Empty)
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

/// Start a server configured by `f` on an ephemeral port, handing it the bound listener (never
/// bind, drop and re-bind: see `tests/middleware.rs`).
async fn spawn<F>(f: F) -> (SocketAddr, oneshot::Sender<()>)
where
    F: FnOnce(Server) -> Server + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        f(Server::new(init().await.unwrap()))
            .run_with_shutdown_listener(listener, async move {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    (addr, tx)
}

/// `GET path` over HTTP/1.1 with `Connection: close`; the response's status.
async fn get(addr: SocketAddr, path: &str) -> u16 {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let raw = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(raw.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let head = String::from_utf8_lossy(&buf);
    head.split(' ').nth(1).unwrap().parse().unwrap()
}

fn the_line_for(lines: &[String], path: &str) -> String {
    lines
        .iter()
        .find(|l| l.contains(&format!("path={path} ")))
        .cloned()
        .unwrap_or_else(|| panic!("no line for {path}: {lines:#?}"))
}

/// ⭐ One line per request, once its reply is ready, with what the span recorded. ⭐ The
/// control: a `404` from the router is logged as well, before any handler or `before` hook
/// could have run, and without the field nothing recorded.
#[tokio::test]
async fn a_request_is_logged_when_its_reply_is_ready_with_what_its_span_recorded() {
    let (captured, _guard) = capture();
    let (addr, stop) = spawn(|s| {
        s.with_request_span(request_span)
            .with_middleware(RequestLogger)
    })
    .await;
    assert_eq!(get(addr, "/svc").await, 200);
    assert_eq!(get(addr, "/nowhere").await, 404);
    let _ = stop.send(());

    let lines = captured.lines();
    let ok = the_line_for(&lines, "/svc");
    assert!(ok.contains("request{who=alice}"), "{ok}");
    assert!(ok.contains("method=GET"), "{ok}");
    assert!(ok.contains("status=200"), "{ok}");
    assert!(ok.contains("elapsed_ms="), "{ok}");

    let missing = the_line_for(&lines, "/nowhere");
    assert!(missing.contains("status=404"), "{missing}");
    assert!(!missing.contains("who="), "{missing}");

    let logged = lines
        .iter()
        .filter(|l| l.contains(" request method="))
        .count();
    assert_eq!(logged, 2, "one line per request: {lines:#?}");
}

/// With no span named, a request still runs in one called `request`, and is logged in it.
#[tokio::test]
async fn without_a_named_span_a_request_is_logged_in_the_default_one() {
    let (captured, _guard) = capture();
    let (addr, stop) = spawn(|s| s.with_middleware(RequestLogger)).await;
    assert_eq!(get(addr, "/svc").await, 200);
    let _ = stop.send(());

    let ok = the_line_for(&captured.lines(), "/svc");
    assert!(ok.contains(" request: "), "{ok}");
    assert!(ok.contains("status=200"), "{ok}");
}
