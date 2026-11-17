//! `Server::with_header_read_timeout`: a client gets that long to send a request's headers, or
//! its connection is closed, and a server with the setting serves requests like any other.
//!
//! hyper enforces the timeout with a timer it is given, and panics when the timeout is set
//! without one. The server set it without one, so every connection's task panicked before
//! reading its request, and every client got a reset. The first test pins that.

use actus::prelude::*;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

struct Ping;

#[controller]
impl Ping {
    routes! {
        GET "" => ping(),
    }

    pub async fn ping(&self) -> Reply {
        reply!()
    }
}

app_routes! {
    routes {
        "ping" => Ping,
    }
}

/// Short, so the tests that wait for it stay fast.
const TIMEOUT: Duration = Duration::from_millis(200);

/// Start a server with the header-read timeout `d`, on a listener bound here (never bind, drop
/// and re-bind: see `tests/middleware.rs`).
async fn spawn(d: Duration) -> (SocketAddr, oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        Server::new(init().await.unwrap())
            .with_header_read_timeout(d)
            .run_with_shutdown_listener(listener, async move {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    (addr, tx)
}

/// Connect, write `bytes`, and read until the server closes the connection: what it sent, and
/// how long that took from before connecting. The server's clock starts after it accepts, so it
/// cannot have run longer than this one.
async fn send_and_wait_for_close(addr: SocketAddr, bytes: &[u8]) -> (Vec<u8>, Duration) {
    let started = Instant::now();
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(bytes).await.unwrap();
    let mut buf = Vec::new();
    // A ceiling, not a wait: the longest close here comes after `TIMEOUT`.
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
        .await
        .expect("the server closes the connection")
        .expect("a close, not a reset");
    (buf, started.elapsed())
}

/// ⭐ With the timeout set, a request is served.
#[tokio::test]
async fn a_server_with_the_timeout_serves_requests() {
    let (addr, stop) = spawn(Duration::from_secs(5)).await;
    let request = b"GET /ping HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    let (reply, _) = send_and_wait_for_close(addr, request).await;
    assert!(
        reply.starts_with(b"HTTP/1.1 204 "),
        "{}",
        String::from_utf8_lossy(&reply)
    );
    let _ = stop.send(());
}

/// A client that starts a request and stalls in its headers is disconnected without a reply
/// once the timeout has passed, and not before.
#[tokio::test]
async fn a_client_that_stalls_in_its_headers_is_disconnected() {
    let (addr, stop) = spawn(TIMEOUT).await;
    let (reply, waited) =
        send_and_wait_for_close(addr, b"GET /ping HTTP/1.1\r\nHost: 127.0.0.1\r\n").await;
    assert!(reply.is_empty(), "{}", String::from_utf8_lossy(&reply));
    assert!(waited >= TIMEOUT, "closed after {waited:?}");
    let _ = stop.send(());
}

/// The clock also runs while a keep-alive connection waits for its next request, so one left
/// idle after its response is closed once the timeout has passed.
#[tokio::test]
async fn an_idle_keep_alive_connection_is_closed() {
    let (addr, stop) = spawn(TIMEOUT).await;
    let (reply, waited) =
        send_and_wait_for_close(addr, b"GET /ping HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await;
    let text = String::from_utf8_lossy(&reply);
    assert!(text.starts_with("HTTP/1.1 204 "), "{text}");
    assert_eq!(text.matches("HTTP/1.1 ").count(), 1, "{text}");
    assert!(waited >= TIMEOUT, "closed after {waited:?}");
    let _ = stop.send(());
}
