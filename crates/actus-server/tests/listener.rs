//! Integration tests for the pre-bound-listener entry points
//! (`Server::run_listener` / `Server::run_with_shutdown_listener`).
//!
//! * A server handed a listener serves on it — race-free: the test binds
//!   `127.0.0.1:0`, keeps the listener, and never re-binds, and the request
//!   may connect before the accept loop starts (the kernel queues it).
//! * **Graceful drain**: a request in flight when shutdown fires still
//!   completes, while the socket stops accepting new connections.
//! * **The drain deadline is enforced**: a handler that outlives it is
//!   aborted and the server returns instead of hanging on the connection.

use actus::prelude::*;
use serde_json::json;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, oneshot};

/// Handler-side coordination for the drain test: the handler announces it
/// has started, then holds the response until the test releases it — so the
/// test can fire shutdown while the request is *deterministically* in
/// flight. `Notify::notify_one` stores a permit when nobody is waiting yet,
/// so neither side can miss the other.
static DRAIN_ENTERED: Notify = Notify::const_new();
static DRAIN_RELEASE: Notify = Notify::const_new();
/// Same announce for the deadline test's never-finishing handler.
static HANG_ENTERED: Notify = Notify::const_new();

struct Api;

#[controller]
impl Api {
    routes! {
        GET "hello" => hello(),
        GET "drain" => drain(),
        GET "hang"  => hang(),
    }

    pub async fn hello(&self) -> Reply {
        reply!(json!({ "hello": "listener" }))
    }

    /// In-flight during shutdown: announces entry, waits for the release.
    pub async fn drain(&self) -> Reply {
        DRAIN_ENTERED.notify_one();
        DRAIN_RELEASE.notified().await;
        reply!(json!({ "drained": true }))
    }

    /// Outlives any reasonable drain deadline; only the deadline abort ends it.
    pub async fn hang(&self) -> Reply {
        HANG_ENTERED.notify_one();
        tokio::time::sleep(Duration::from_secs(60)).await;
        reply!(json!({ "unreachable": true }))
    }
}

app_routes! {
    routes {
        "api" => Api,
    }
}

/// Bind an ephemeral listener and start a server on it (configured by `f`).
/// Returns the bound address, the shutdown trigger, and the server task's
/// handle — the listener tests assert on the run method's *return*, which
/// the port-based harness in `middleware.rs` never could.
async fn spawn_on_listener<F>(
    f: F,
) -> (
    SocketAddr,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<Result<(), actus_server::ServerError>>,
)
where
    F: FnOnce(Server) -> Server + Send + 'static,
{
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = std_listener.local_addr().unwrap();
    // The inherited-fd adoption path the doc example prescribes: nonblocking
    // first, then from_std — exercised here so the prescription stays true.
    std_listener.set_nonblocking(true).unwrap();
    let listener = TcpListener::from_std(std_listener).unwrap();

    let (tx, rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let server = f(Server::new(init().await.unwrap()));
        server
            .run_with_shutdown_listener(listener, async move {
                let _ = rx.await;
            })
            .await
    });
    (addr, tx, handle)
}

/// Raw HTTP/1.1 request with `Connection: close`; parse `(status, body)`.
/// Same dep-free shape as `middleware.rs`'s helper.
async fn http(addr: SocketAddr, raw: &str) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(raw.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    parse_response(&buf)
}

fn parse_response(buf: &[u8]) -> (u16, Vec<u8>) {
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(buf.len());
    let status: u16 = std::str::from_utf8(&buf[..split])
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let body = if split + 4 < buf.len() {
        buf[split + 4..].to_vec()
    } else {
        Vec::new()
    };
    (status, body)
}

#[tokio::test]
async fn serves_on_a_prebound_listener() {
    let (addr, stop, handle) = spawn_on_listener(|s| s).await;

    // No listening-poll: the socket has been listening since `bind`, before
    // the server task even started — the race-free property under test.
    let (status, body) = http(
        addr,
        "GET /api/hello HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, 200);
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body, json!({ "hello": "listener" }));

    let _ = stop.send(());
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn drain_completes_the_inflight_request_and_stops_accepting() {
    let (addr, stop, handle) = spawn_on_listener(|s| s).await;

    // Put a request deterministically in flight: connect, send, and wait for
    // the handler to announce it entered.
    let mut held = TcpStream::connect(addr).await.unwrap();
    held.write_all(b"GET /api/drain HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    DRAIN_ENTERED.notified().await;

    // Shutdown fires while the request is in flight. The accept loop breaks
    // and drops the listener; poll until the socket refuses new connections.
    let _ = stop.send(());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect(addr).await.is_err() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "socket still accepting 5s after shutdown"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The in-flight request is *not* cut: release the handler and read the
    // full response on the held connection.
    DRAIN_RELEASE.notify_one();
    let mut buf = Vec::new();
    held.read_to_end(&mut buf).await.unwrap();
    let (status, body) = parse_response(&buf);
    assert_eq!(status, 200);
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body, json!({ "drained": true }));

    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn drain_deadline_aborts_a_connection_that_outlives_it() {
    // A short deadline so the test is fast; the handler sleeps 60 s, so only
    // the deadline abort can end the connection.
    let (addr, stop, handle) =
        spawn_on_listener(|s| s.with_drain_deadline(Duration::from_millis(200))).await;

    let mut held = TcpStream::connect(addr).await.unwrap();
    held.write_all(b"GET /api/hang HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    HANG_ENTERED.notified().await;

    let _ = stop.send(());

    // The server must return once the deadline aborts the hung connection —
    // well inside this generous ceiling (the handler alone would take 60 s).
    tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("server hung past the drain deadline")
        .unwrap()
        .unwrap();

    // The held connection was aborted, not answered: the peer closed without
    // a complete response.
    let mut buf = Vec::new();
    let _ = held.read_to_end(&mut buf).await;
    assert!(
        !buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.is_empty(),
        "hung request unexpectedly received a complete response: {:?}",
        String::from_utf8_lossy(&buf)
    );
}
