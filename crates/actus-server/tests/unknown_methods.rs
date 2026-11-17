//! A request whose method no route can declare (none of the seven `Verb`s: `TRACE`, `CONNECT`,
//! an extension method like `PURGE`) is answered `501 Not Implemented` (RFC 9110 §9.1). It used
//! to be dispatched as a `GET`, so a `PURGE` from a cache got the page and a `200`. Routing
//! still comes first, so an unknown path is a `404`; the `before` chain still runs, so a
//! middleware can answer such a method itself; and the `501` goes through the `after` chain
//! like any other error.

use actus::prelude::*;
use serde_json::json;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

struct Page;

#[controller]
impl Page {
    routes! {
        GET "" => page(),
    }

    pub async fn page(&self) -> Reply {
        reply!(json!({ "page": "the GET body" }))
    }
}

app_routes! {
    routes {
        "page" => Page,
    }
}

/// Answers `PURGE` itself, as a cache-invalidation middleware would.
struct AnswersPurge;

#[async_trait]
impl Middleware for AnswersPurge {
    async fn before(&self, request: &mut Request) -> Result<Outcome, WebError> {
        if request.method.as_str() == "PURGE" {
            return Ok(Outcome::Respond(reply::json(json!({ "purged": true }))));
        }
        Ok(Outcome::Continue)
    }
}

/// Stamps every reply it sees, so a test can tell the `after` chain ran.
struct Stamp;

#[async_trait]
impl Middleware for Stamp {
    async fn after(&self, _request: &Request, response: &mut ReplyData) -> Result<(), WebError> {
        response.add_header("X-After", "ran");
        Ok(())
    }
}

/// Start a server, configured by `f`, on a listener bound here (never bind, drop and re-bind:
/// see `tests/middleware.rs`).
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

/// `method path` on a connection of its own: the status, and the response as text with its
/// header names lowercased.
async fn send(addr: SocketAddr, method: &str, path: &str) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let raw = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(raw.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
        .await
        .expect("the server answers and closes the connection")
        .unwrap();
    let text = String::from_utf8_lossy(&buf).to_ascii_lowercase();
    let status = text.split(' ').nth(1).unwrap().parse().unwrap();
    (status, text)
}

#[tokio::test]
async fn a_method_no_route_can_declare_is_501_not_the_get() {
    let (addr, stop) = spawn(|s| s.with_middleware(Stamp)).await;
    for method in ["TRACE", "PURGE", "PROPFIND", "FOO"] {
        let (status, text) = send(addr, method, "/page").await;
        assert_eq!(status, 501, "{method}: {text}");
        assert!(!text.contains("the get body"), "{method}: {text}");
        assert!(text.contains("\r\nx-after: ran\r\n"), "{method}: {text}");
    }
    // The control: the `GET` itself is served.
    let (status, text) = send(addr, "GET", "/page").await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("the get body"), "{text}");
    let _ = stop.send(());
}

#[tokio::test]
async fn routing_comes_first_so_an_unknown_path_is_still_404() {
    let (addr, stop) = spawn(|s| s).await;
    assert_eq!(send(addr, "PURGE", "/nowhere").await.0, 404);
    let _ = stop.send(());
}

#[tokio::test]
async fn a_middleware_can_still_answer_such_a_method() {
    let (addr, stop) = spawn(|s| s.with_middleware(AnswersPurge)).await;
    let (status, text) = send(addr, "PURGE", "/page").await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("\"purged\":true"), "{text}");
    // A method it does not answer is still a 501.
    assert_eq!(send(addr, "TRACE", "/page").await.0, 501);
    let _ = stop.send(());
}
