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
//! * With `Server::with_gzip_request_bodies`, a gzip body is decoded instead,
//!   the body cap holding its decoded size, and every other coding is still a
//!   `415` — now naming `gzip` as accepted.

use actus::prelude::*;
use serde_json::{Value as JsonValue, json};
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
        POST "echo" => echo(params: &Params),
        POST "json" => json_echo(data: JsonValue),
    }

    pub async fn put(&self, params: &Params) -> Reply {
        self.hits.fetch_add(1, Ordering::Relaxed);
        reply!(json!({ "stored": params.body_bytes().len() }))
    }

    pub async fn get(&self) -> Reply {
        self.hits.fetch_add(1, Ordering::Relaxed);
        reply!(json!({ "ok": true }))
    }

    /// The body and the coding headers as the handler sees them.
    pub async fn echo(&self, params: &Params) -> Reply {
        reply!(json!({
            "body": String::from_utf8_lossy(params.body_bytes()),
            "content_encoding": params.header("content-encoding"),
            "content_length": params.header("content-length"),
        }))
    }

    pub async fn json_echo(&self, data: JsonValue) -> Reply {
        reply!(data)
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
    spawn_with(|server| server).await
}

/// [`spawn`], with `configure` applied to the server.
async fn spawn_with(
    configure: impl FnOnce(Server) -> Server,
) -> (SocketAddr, Arc<AtomicUsize>, oneshot::Sender<()>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let server =
        configure(Server::new(init(hits.clone()).await.unwrap()).with_middleware(StampAfter));
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

/// `Server::with_gzip_request_bodies`: a gzip body is decoded, not refused.
#[cfg(feature = "compression")]
mod gzip_bodies {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    async fn spawn() -> (SocketAddr, Arc<AtomicUsize>, oneshot::Sender<()>) {
        spawn_with(Server::with_gzip_request_bodies).await
    }

    #[tokio::test]
    async fn a_gzip_body_reaches_the_handler_decoded() {
        let (addr, _hits, _stop) = spawn().await;

        // A JSON route parses the decoded body.
        let head = "POST /store/json HTTP/1.1\r\nContent-Type: application/json\r\n\
                    Content-Encoding: gzip\r\n";
        let (status, response) = send(addr, &request(head, &gzip(br#"{"name":"site"}"#))).await;
        assert_eq!(status, 200, "{response}");
        assert!(response.contains(r#"{"name":"site"}"#), "{response}");

        // A route reading bytes gets the content, and neither header that
        // described the coded bytes. `x-gzip` is gzip's alias.
        for coding in ["gzip", "x-gzip"] {
            let head = format!(
                "POST /store/echo HTTP/1.1\r\nContent-Type: application/octet-stream\r\n\
                 Content-Encoding: {coding}\r\n"
            );
            let (status, response) = send(addr, &request(&head, &gzip(b"hello, world"))).await;
            assert_eq!(status, 200, "{coding}: {response}");
            assert!(response.contains(r#""body":"hello, world""#), "{response}");
            assert!(
                response.contains(r#""content_encoding":null"#),
                "{response}"
            );
            assert!(response.contains(r#""content_length":null"#), "{response}");
        }
    }

    #[tokio::test]
    async fn the_body_cap_holds_the_decoded_size() {
        let (addr, hits, _stop) = spawn().await;

        // 1 KiB of zeros codes to a few dozen bytes: under `Store`'s 64-byte
        // cap as sent, sixteen times over it decoded.
        let coded = gzip(&[0u8; 1024]);
        assert!(coded.len() < 64, "coded to {} bytes", coded.len());
        let head = format!("{PUT_BYTES}Content-Encoding: gzip\r\n");
        let (status, response) = send(addr, &request(&head, &coded)).await;
        assert_eq!(status, 413, "{response}");
        assert_eq!(hits.load(Ordering::Relaxed), 0, "the handler never ran");
    }

    #[tokio::test]
    async fn a_body_that_is_not_valid_gzip_is_400() {
        let (addr, hits, _stop) = spawn().await;

        let head = format!("{PUT_BYTES}Content-Encoding: gzip\r\n");
        let (status, response) = send(addr, &request(&head, b"plainly not gzip")).await;
        assert_eq!(status, 400, "{response}");
        assert!(response.contains("not valid gzip"), "{response}");
        assert_eq!(hits.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn other_codings_are_still_refused_now_naming_gzip() {
        let (addr, hits, _stop) = spawn().await;

        for coding in ["br", "gzip, gzip"] {
            let head = format!("{PUT_BYTES}Content-Encoding: {coding}\r\n");
            let (status, response) = send(addr, &request(&head, b"abc")).await;
            assert_eq!(status, 415, "Content-Encoding: {coding}");
            assert!(
                response.contains("accept-encoding: gzip, identity"),
                "{response}"
            );
        }
        assert_eq!(hits.load(Ordering::Relaxed), 0);
    }
}
