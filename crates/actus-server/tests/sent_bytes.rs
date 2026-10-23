//! `Server::with_sent_bytes` — every byte the server writes to a connection is
//! counted, so the count equals what a client receives: status line, headers
//! and body alike.
//!
//! * A buffered reply, a streamed (chunked) reply, and an error reply.
//! * With compression on: the compressed bytes — what left, not what the
//!   handler built.
//! * An upgraded connection: the `101`, and every WebSocket frame after it.

use actus::prelude::*;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

/// 4 KiB of compressible text — above the compression layer's default 1 KiB
/// threshold.
const BODY_LEN: usize = 4096;

struct Text;

#[controller]
impl Text {
    routes! {
        GET "" => text(),
        GET "stream" => streamed(),
    }

    pub async fn text(&self) -> Reply {
        Ok(reply::bytes(
            "text/plain; charset=utf-8",
            vec![b'a'; BODY_LEN],
        ))
    }

    pub async fn streamed(&self) -> Reply {
        let chunks = ["one", "two", "three"]
            .map(|s| Ok::<_, std::io::Error>(Bytes::from_static(s.as_bytes())));
        Ok(reply::stream(futures_util::stream::iter(chunks)))
    }
}

app_routes! {
    routes {
        "text" => Text,
    }
}

/// Serve `server` on an ephemeral port it keeps (never bind, drop and re-bind
/// — see `tests/middleware.rs`); return `(addr, shutdown_tx)`.
async fn spawn(server: Server) -> (SocketAddr, oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        server
            .run_with_shutdown_listener(listener, async move {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    (addr, tx)
}

/// `GET path` as raw HTTP/1.1 with `Connection: close`, plus `headers` (each
/// ending in `\r\n`); returns `(lowercased head, every byte received)`.
async fn get(addr: SocketAddr, path: &str, headers: &str) -> (String, Vec<u8>) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let raw = format!("GET {path} HTTP/1.1\r\nHost: x\r\n{headers}Connection: close\r\n\r\n");
    stream.write_all(raw.as_bytes()).await.unwrap();
    let mut received = Vec::new();
    stream.read_to_end(&mut received).await.unwrap();
    let split = received
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a header terminator");
    let head = String::from_utf8_lossy(&received[..split]).to_ascii_lowercase();
    (head, received)
}

#[tokio::test]
async fn every_byte_a_client_receives_is_counted() {
    let sent = Arc::new(AtomicU64::new(0));
    let (addr, _stop) =
        spawn(Server::new(init().await.unwrap()).with_sent_bytes(sent.clone())).await;
    let mut received = 0;

    // A buffered reply: its status line and headers count, not only its body.
    let (head, all) = get(addr, "/text", "").await;
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert!(all.len() > BODY_LEN);
    received += all.len();
    assert_eq!(sent.load(Ordering::Relaxed), received as u64);

    // A streamed reply: the chunked framing counts too.
    let (head, all) = get(addr, "/text/stream", "").await;
    assert!(head.contains("transfer-encoding: chunked"), "{head}");
    received += all.len();
    assert_eq!(sent.load(Ordering::Relaxed), received as u64);

    // An error reply.
    let (head, all) = get(addr, "/no-such-route", "").await;
    assert!(head.starts_with("http/1.1 404"), "{head}");
    received += all.len();
    assert_eq!(sent.load(Ordering::Relaxed), received as u64);
}

#[cfg(feature = "compression")]
#[tokio::test]
async fn a_compressed_reply_counts_the_compressed_bytes() {
    let sent = Arc::new(AtomicU64::new(0));
    let (addr, _stop) = spawn(
        Server::new(init().await.unwrap())
            .with_compression(actus_server::CompressionLayer::new())
            .with_sent_bytes(sent.clone()),
    )
    .await;

    let (head, all) = get(addr, "/text", "Accept-Encoding: gzip\r\n").await;
    assert!(head.contains("content-encoding: gzip"), "{head}");
    assert!(all.len() < BODY_LEN, "compression shrank what was sent");
    assert_eq!(sent.load(Ordering::Relaxed), all.len() as u64);
}

#[cfg(feature = "websocket")]
mod upgraded {
    use super::*;
    use futures_util::{SinkExt, StreamExt};

    struct Echo;

    #[controller]
    impl Echo {
        routes! {
            GET "" => echo(),
        }

        pub async fn echo(&self) -> Reply {
            Ok(ws::upgrade(|mut socket| async move {
                while let Some(Ok(msg)) = socket.next().await {
                    if msg.is_text() && socket.send(msg).await.is_err() {
                        break;
                    }
                }
            }))
        }
    }

    app_routes! {
        routes {
            "ws" => Echo,
        }
    }

    /// The handshake is spoken by hand over a raw socket, so the test sees
    /// every byte the server sends: the `101` head, then one echoed frame.
    #[tokio::test]
    async fn the_101_and_every_frame_after_it_are_counted() {
        let sent = Arc::new(AtomicU64::new(0));
        let (addr, _stop) =
            spawn(Server::new(init().await.unwrap()).with_sent_bytes(sent.clone())).await;

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(
                b"GET /ws HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
                  Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
            )
            .await
            .unwrap();
        // The server sends nothing after the `101` until a frame arrives, so
        // reading up to the blank line reads exactly the head.
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(stream.read_u8().await.unwrap());
        }
        assert!(head.starts_with(b"HTTP/1.1 101"));
        assert_eq!(sent.load(Ordering::Relaxed), head.len() as u64);

        // A masked client text frame carrying "hi"; the echo comes back
        // unmasked: two header bytes, then the payload.
        let mask = [0x11, 0x22, 0x33, 0x44];
        let mut frame = vec![0x81, 0x80 | 2];
        frame.extend(mask);
        frame.extend(b"hi".iter().zip(mask).map(|(b, m)| b ^ m));
        stream.write_all(&frame).await.unwrap();
        let mut echo = [0u8; 4];
        stream.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, b"\x81\x02hi");
        assert_eq!(
            sent.load(Ordering::Relaxed),
            (head.len() + echo.len()) as u64
        );
    }
}
