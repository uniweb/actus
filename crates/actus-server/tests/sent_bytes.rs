//! `Server::with_sent_bytes` — every response's body bytes are counted as they are handed to
//! the connection, after compression.
//!
//! * The count equals the body bytes a client receives, for a handler's reply and for an
//!   error reply alike.
//! * With compression on, it counts the compressed bytes — what left, not what the handler built.

use actus::prelude::*;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

/// 4 KiB of compressible text — above the compression layer's default 1 KiB threshold.
const BODY_LEN: usize = 4096;

struct Text;

#[controller]
impl Text {
    routes! {
        GET "" => text(),
    }

    pub async fn text(&self) -> Reply {
        Ok(reply::bytes(
            "text/plain; charset=utf-8",
            vec![b'a'; BODY_LEN],
        ))
    }
}

app_routes! {
    routes {
        "text" => Text,
    }
}

/// Start a server configured by `f` on an ephemeral port it keeps (never bind, drop and
/// re-bind — see `tests/middleware.rs`); return `(addr, shutdown_tx)`.
async fn spawn<F>(f: F) -> (SocketAddr, oneshot::Sender<()>)
where
    F: FnOnce(Server) -> Server + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let server = f(Server::new(init().await.unwrap()));
        server
            .run_with_shutdown_listener(listener, async move {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    (addr, tx)
}

/// A raw HTTP/1.1 request with `Connection: close`; returns `(status, headers, body)`.
async fn http(addr: SocketAddr, raw: &str) -> (u16, String, Vec<u8>) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(raw.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a header terminator");
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("a status code");
    (status, head, buf[split + 4..].to_vec())
}

#[tokio::test]
async fn every_response_body_is_counted() {
    let sent = Arc::new(AtomicU64::new(0));
    let counter = sent.clone();
    let (addr, _stop) = spawn(move |s| s.with_sent_bytes(counter)).await;

    let (status, head, body) = http(
        addr,
        "GET /text HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        !head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
    );
    assert_eq!(body.len(), BODY_LEN);
    assert_eq!(sent.load(Ordering::Relaxed), BODY_LEN as u64);

    // An error reply's body is counted too.
    let (status, _, error) = http(
        addr,
        "GET /no-such-route HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, 404);
    assert!(!error.is_empty(), "the 404 carries a body");
    assert_eq!(
        sent.load(Ordering::Relaxed),
        (BODY_LEN + error.len()) as u64
    );
}

#[cfg(feature = "compression")]
#[tokio::test]
async fn the_compressed_bytes_are_what_is_counted() {
    let sent = Arc::new(AtomicU64::new(0));
    let counter = sent.clone();
    let (addr, _stop) = spawn(move |s| {
        s.with_compression(actus_server::CompressionLayer::new())
            .with_sent_bytes(counter)
    })
    .await;

    let (status, head, body) = http(
        addr,
        "GET /text HTTP/1.1\r\nHost: x\r\nAccept-Encoding: gzip\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        head.to_ascii_lowercase().contains("content-encoding: gzip"),
        "the reply was compressed:\n{head}"
    );
    assert!(body.len() < BODY_LEN, "compression shrank the body");
    assert_eq!(sent.load(Ordering::Relaxed), body.len() as u64);
}
