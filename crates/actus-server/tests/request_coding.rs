//! A request body that would reach its handler still encoded is refused,
//! before it is read: Actus decodes no request coding, so a handler must never
//! receive encoded bytes as though they were the content.
//!
//! * A `Content-Encoding` other than `identity` is a `415` carrying
//!   `Accept-Encoding: identity` (RFC 9110 §12.5.3); the handler never runs,
//!   and the `415` still flows through the after-chain.
//! * No coding, or `identity`, reaches the handler; so does a request with no
//!   body, whatever its headers say.
//! * Routing still comes first (an unknown path is a `404`), and the refusal
//!   precedes the body cap: an encoded body over it is never read.
//! * A transfer coding besides a lone `chunked` is a `501` (RFC 9112 §6.1).

use actus::prelude::*;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

/// Stores whatever body it is sent — the kind of route an encoded body would
/// silently corrupt. Counts the requests that reach it.
struct Store {
    hits: Arc<AtomicUsize>,
}

#[controller(max_body_bytes = 64)]
impl Store {
    routes! {
        PUT "" => put(params: &Params),
        GET "" => get(),
    }

    pub async fn put(&self, params: &Params) -> Reply {
        self.hits.fetch_add(1, Ordering::Relaxed);
        reply!(json!({ "stored": params.body_bytes().len() }))
    }

    pub async fn get(&self) -> Reply {
        self.hits.fetch_add(1, Ordering::Relaxed);
        reply!(json!({ "ok": true }))
    }
}

app_routes! {
    deps(hits: Arc<AtomicUsize>) {}
    routes {
        "store" => Store { hits },
    }
}

/// Stamps every reply in `after`, to show the refusal flows through it.
struct StampAfter;

#[async_trait]
impl Middleware for StampAfter {
    async fn after(&self, _request: &Request, response: &mut ReplyData) -> Result<(), WebError> {
        response.add_header("X-After", "ran");
        Ok(())
    }
}

/// Start a server on an ephemeral port it keeps (never bind, drop and re-bind
/// — see `tests/middleware.rs`); return `(addr, hits, shutdown_tx)`.
async fn spawn() -> (SocketAddr, Arc<AtomicUsize>, oneshot::Sender<()>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let router = init(hits.clone()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        Server::new(router)
            .with_middleware(StampAfter)
            .run_with_shutdown_listener(listener, async move {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    (addr, hits, tx)
}

/// `head` (a request line and headers, each ending in `\r\n`) plus `Host`,
/// `Connection: close`, and a `Content-Length` when `body` is non-empty.
fn request(head: &str, body: &[u8]) -> Vec<u8> {
    let mut raw = format!("{head}Host: x\r\nConnection: close\r\n").into_bytes();
    if !body.is_empty() {
        raw.extend(format!("Content-Length: {}\r\n", body.len()).into_bytes());
    }
    raw.extend(b"\r\n");
    raw.extend(body);
    raw
}

/// Send `raw` and read the whole response; returns `(status, lowercased
/// response)`.
async fn send(addr: SocketAddr, raw: &[u8]) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(raw).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let response = String::from_utf8_lossy(&buf).to_ascii_lowercase();
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("a status code");
    (status, response)
}

/// The start of a gzip stream. The bytes do not matter, only the label.
const GZIP_BYTES: &[u8] = b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\x03";

const PUT_BYTES: &str = "PUT /store HTTP/1.1\r\nContent-Type: application/octet-stream\r\n";

#[tokio::test]
async fn a_gzip_body_is_refused_before_the_handler_runs() {
    let (addr, hits, _stop) = spawn().await;

    let head = format!("{PUT_BYTES}Content-Encoding: gzip\r\n");
    let (status, response) = send(addr, &request(&head, GZIP_BYTES)).await;
    assert_eq!(status, 415, "{response}");
    assert!(response.contains("accept-encoding: identity"), "{response}");
    assert!(response.contains("application/problem+json"), "{response}");
    assert!(
        response.contains("x-after: ran"),
        "the 415 flows through the after-chain:\n{response}"
    );
    assert_eq!(hits.load(Ordering::Relaxed), 0, "the handler never ran");
}

#[tokio::test]
async fn every_content_coding_but_identity_is_refused() {
    let (addr, hits, _stop) = spawn().await;

    for coding in ["br", "deflate", "zstd", "x-gzip", "GZIP", "identity, gzip"] {
        let head = format!("{PUT_BYTES}Content-Encoding: {coding}\r\n");
        let (status, _) = send(addr, &request(&head, b"abc")).await;
        assert_eq!(status, 415, "Content-Encoding: {coding}");
    }
    // The field may span several lines; one coding on any of them counts.
    let head = format!("{PUT_BYTES}Content-Encoding: identity\r\nContent-Encoding: br\r\n");
    let (status, _) = send(addr, &request(&head, b"abc")).await;
    assert_eq!(status, 415);
    assert_eq!(hits.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn an_unencoded_or_identity_body_reaches_the_handler() {
    let (addr, hits, _stop) = spawn().await;

    for coding in [
        "",
        "Content-Encoding: identity\r\n",
        "Content-Encoding: Identity\r\n",
    ] {
        let head = format!("{PUT_BYTES}{coding}");
        let (status, response) = send(addr, &request(&head, b"abc")).await;
        assert_eq!(status, 200, "{response}");
        assert!(response.contains(r#""stored":3"#), "{response}");
    }
    assert_eq!(hits.load(Ordering::Relaxed), 3);
}

#[tokio::test]
async fn a_request_without_a_body_is_not_refused_over_the_header() {
    let (addr, hits, _stop) = spawn().await;

    let head = "GET /store HTTP/1.1\r\nContent-Encoding: gzip\r\n";
    let (status, _) = send(addr, &request(head, b"")).await;
    assert_eq!(status, 200);
    let head = "PUT /store HTTP/1.1\r\nContent-Encoding: gzip\r\nContent-Length: 0\r\n";
    let (status, _) = send(addr, &request(head, b"")).await;
    assert_eq!(status, 200);
    assert_eq!(hits.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn routing_comes_first_and_an_encoded_body_is_never_read() {
    let (addr, _hits, _stop) = spawn().await;

    // An unknown path is a 404, whatever its body.
    let head = "PUT /elsewhere HTTP/1.1\r\nContent-Type: application/octet-stream\r\n\
                Content-Encoding: gzip\r\n";
    let (status, _) = send(addr, &request(head, GZIP_BYTES)).await;
    assert_eq!(status, 404);

    // `Store` caps bodies at 64 bytes. Unencoded, 1 KiB is a 413; encoded, the
    // refusal comes first, so the body is never buffered at all.
    let big = vec![0u8; 1024];
    let (status, _) = send(addr, &request(PUT_BYTES, &big)).await;
    assert_eq!(status, 413);
    let head = format!("{PUT_BYTES}Content-Encoding: gzip\r\n");
    let (status, _) = send(addr, &request(&head, &big)).await;
    assert_eq!(status, 415);
}

#[tokio::test]
async fn a_transfer_coding_besides_chunked_is_not_implemented() {
    let (addr, hits, _stop) = spawn().await;

    // hyper removes the chunked framing and nothing else: without the 501,
    // the handler would get `abc` still gzip-coded.
    let raw = format!(
        "{PUT_BYTES}Transfer-Encoding: gzip, chunked\r\nHost: x\r\nConnection: close\r\n\r\n\
         3\r\nabc\r\n0\r\n\r\n"
    );
    let (status, response) = send(addr, raw.as_bytes()).await;
    assert_eq!(status, 501, "{response}");
    assert!(
        !response.contains("accept-encoding"),
        "Accept-Encoding belongs only on a content-coding 415:\n{response}"
    );

    // A lone `chunked` is decoded, and reaches the handler.
    let raw = format!(
        "{PUT_BYTES}Transfer-Encoding: chunked\r\nHost: x\r\nConnection: close\r\n\r\n\
         3\r\nabc\r\n0\r\n\r\n"
    );
    let (status, response) = send(addr, raw.as_bytes()).await;
    assert_eq!(status, 200, "{response}");
    assert!(response.contains(r#""stored":3"#), "{response}");
    assert_eq!(hits.load(Ordering::Relaxed), 1);
}
