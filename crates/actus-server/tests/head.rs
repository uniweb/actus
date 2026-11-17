//! `HEAD` comes with `GET` (RFC 9110 §9.1, §9.3.2). A `HEAD` to a route that accepts `GET` is
//! handled as that `GET` (the same route, `prepare` hook, middleware and handler) and answered
//! with the `GET`'s status and headers, without the body. A route that declares `HEAD` takes it
//! itself, and a `405`'s `Allow` names `HEAD` wherever it names `GET`.
//!
//! Each test compares a `HEAD` with the `GET` it stands for, over raw HTTP/1.1, so what is
//! checked is what reaches the wire: above all, that no body byte follows a `HEAD`'s headers.

use actus::prelude::*;
use serde_json::json;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

struct Pages;

#[controller]
impl Pages {
    routes! {
        GET "page"      => page(params: &Params),
        // The `GET` first, on purpose: a declared `HEAD` still takes `HEAD`.
        GET "declared"  => declared_get(),
        HEAD "declared" => declared_head(),
        // No verb prefix: `GET` or `POST`, so `HEAD` too.
        "form"          => form(params: &Params),
        GET "tagged"    => tagged(params: &Params),
        GET "endless"   => endless(),
        GET "empty"     => empty(),
        POST "submit"   => submit(),
    }

    /// A `200` with no body.
    pub async fn empty(&self) -> Reply {
        Ok(reply::build_reply()
            .status(StatusCode::OK)
            .header("X-Empty", "1")
            .done())
    }

    /// Names the verb it was dispatched with, so a test sees what the handler saw. The body is
    /// long enough for gzip to shrink it: the compression layer keeps a reply as it is when
    /// encoding would not make it smaller.
    pub async fn page(&self, params: &Params) -> Reply {
        reply!(
            status = StatusCode::OK,
            headers = { "X-Verb": params.verb() },
            json!({ "page": "a page with a body. ".repeat(32) })
        )
    }

    pub async fn declared_get(&self) -> Reply {
        reply!(
            status = StatusCode::OK,
            headers = { "X-Handler": "get" },
            json!({ "from": "get" })
        )
    }

    /// A route that answers `HEAD` itself, more cheaply than its `GET` would.
    pub async fn declared_head(&self) -> Reply {
        Ok(reply::build_reply()
            .status(StatusCode::OK)
            .header("X-Handler", "head")
            .done())
    }

    /// A legacy form handler: shows the form on `GET`, processes it on any other verb.
    pub async fn form(&self, params: &Params) -> Reply {
        if params.verb() == Verb::GET {
            reply!(json!({ "form": "shown" }))
        } else {
            reply!(status = StatusCode::CREATED, json!({ "form": "processed" }))
        }
    }

    /// A conditional `GET`: `304` when the client already holds `"v1"`.
    pub async fn tagged(&self, params: &Params) -> Reply {
        let spec = reply::build_reply().header("ETag", "\"v1\"");
        if params.header("if-none-match") == Some("\"v1\"") {
            return Ok(spec.status(StatusCode::NOT_MODIFIED).done());
        }
        Ok(spec.body(reply::json(json!({ "version": 1 }))).done())
    }

    /// An event stream that never yields: a `GET` of it stays open until the client leaves.
    pub async fn endless(&self) -> Reply {
        reply!(sse: futures_util::stream::pending::<SseEvent>())
    }

    pub async fn submit(&self) -> Reply {
        reply!()
    }
}

struct Private;

#[controller(prepare = Self::auth)]
impl Private {
    routes! {
        GET "" => secret(),
    }

    /// Refuses a caller without the token.
    async fn auth(
        &self,
        _route: &RouteDef,
        params: &mut Params,
    ) -> Result<Option<ReplyData>, WebError> {
        match params.bearer_token() {
            Some("let-me-in") => Ok(None),
            _ => Err(WebError::Unauthorized),
        }
    }

    pub async fn secret(&self) -> Reply {
        reply!(json!({ "secret": 42 }))
    }
}

/// A `HEAD` declared for one pattern beside a `GET` for another that it also matches.
struct Items;

#[controller]
impl Items {
    routes! {
        GET "export"  => export(),
        HEAD "{id}"   => item_head(id: u64),
    }

    pub async fn export(&self) -> Reply {
        reply!(json!({ "export": "everything" }))
    }

    pub async fn item_head(&self, _id: u64) -> Reply {
        Ok(reply::build_reply()
            .status(StatusCode::OK)
            .header("X-Handler", "item_head")
            .done())
    }
}

app_routes! {
    routes {
        "pages"   => Pages,
        "private" => Private,
        "items"   => Items,
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

/// Write `raw` on a new connection and read until the server closes it.
async fn exchange(addr: SocketAddr, raw: &str) -> Vec<u8> {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(raw.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    // A ceiling, not a wait: every exchange here ends at once, and one that hangs is the bug.
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
        .await
        .expect("the server answers and closes the connection")
        .unwrap();
    buf
}

/// One response, as it came off the wire.
struct Response {
    status: u16,
    headers: http::HeaderMap,
    /// Every byte after the header block.
    body: Vec<u8>,
}

impl Response {
    fn parse(buf: &[u8]) -> Self {
        let split = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("a complete header block");
        let head = std::str::from_utf8(&buf[..split]).unwrap();
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let mut headers = http::HeaderMap::new();
        for line in lines {
            let (name, value) = line.split_once(": ").expect("a header line");
            headers.append(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        Self {
            status,
            headers,
            body: buf[split + 4..].to_vec(),
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|v| v.to_str().unwrap())
    }
}

/// `method path` with the `extra` header lines, on a connection of its own.
async fn request(addr: SocketAddr, method: &str, path: &str, extra: &str) -> Response {
    let raw =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Connection: close\r\n\r\n");
    Response::parse(&exchange(addr, &raw).await)
}

/// The `GET` of `path`, and its `HEAD`, sent alike.
async fn get_and_head(addr: SocketAddr, path: &str, extra: &str) -> (Response, Response) {
    (
        request(addr, "GET", path, extra).await,
        request(addr, "HEAD", path, extra).await,
    )
}

/// The fields a `HEAD`'s reply must carry exactly as its `GET`'s does (`Date` can tick between
/// the two).
const SAME_AS_GET: &[&str] = &[
    "content-type",
    "content-length",
    "content-encoding",
    "vary",
    "allow",
    "etag",
    "cache-control",
    "www-authenticate",
    "x-verb",
    "x-handler",
];

/// ⭐ The property under test: a `HEAD` gets its `GET`'s status and headers, and no body.
fn assert_head_is_get_without_body(get: &Response, head: &Response) {
    assert_eq!(head.status, get.status);
    for name in SAME_AS_GET {
        assert_eq!(head.header(name), get.header(name), "{name}");
    }
    assert!(
        head.body.is_empty(),
        "a HEAD reply carried a body: {:?}",
        String::from_utf8_lossy(&head.body)
    );
}

#[tokio::test]
async fn head_is_the_get_without_its_body() {
    let (addr, stop) = spawn(|s| s).await;
    let (get, head) = get_and_head(addr, "/pages/page", "").await;
    assert_eq!(get.status, 200);
    assert!(!get.body.is_empty());
    // The `HEAD` states the length the `GET` sent, not a false `0`.
    let length = get.body.len().to_string();
    assert_eq!(get.header("content-length"), Some(length.as_str()));
    assert_head_is_get_without_body(&get, &head);
    // The handler ran as the `GET`: it saw the verb its route declares.
    assert_eq!(head.header("x-verb"), Some("GET"));
    let _ = stop.send(());
}

#[tokio::test]
async fn an_unmarked_route_answers_head_as_its_get() {
    // `form` processes on every verb but `GET`. Shown a `HEAD`, it would process; it is shown
    // the `GET`.
    let (addr, stop) = spawn(|s| s).await;
    let (get, head) = get_and_head(addr, "/pages/form", "").await;
    assert_eq!(get.status, 200);
    assert_head_is_get_without_body(&get, &head);
    let _ = stop.send(());
}

#[tokio::test]
async fn a_declared_head_route_takes_head() {
    let (addr, stop) = spawn(|s| s).await;
    let (get, head) = get_and_head(addr, "/pages/declared", "").await;
    assert_eq!(get.header("x-handler"), Some("get"));
    assert_eq!(head.status, 200);
    assert_eq!(head.header("x-handler"), Some("head"));
    assert!(head.body.is_empty());
    let _ = stop.send(());
}

#[tokio::test]
async fn head_meets_every_refusal_its_get_meets() {
    let (addr, stop) = spawn(|s| s).await;
    // The `prepare` hook refuses both alike…
    let (get, head) = get_and_head(addr, "/private", "").await;
    assert_eq!(get.status, 401);
    assert_head_is_get_without_body(&get, &head);
    // …and admits both alike.
    let token = "Authorization: Bearer let-me-in\r\n";
    let (get, head) = get_and_head(addr, "/private", token).await;
    assert_eq!(get.status, 200);
    assert_head_is_get_without_body(&get, &head);
    // A path the router does not know.
    let (get, head) = get_and_head(addr, "/nowhere", "").await;
    assert_eq!(get.status, 404);
    assert_head_is_get_without_body(&get, &head);
    let _ = stop.send(());
}

#[tokio::test]
async fn allow_names_head_wherever_it_names_get() {
    let (addr, stop) = spawn(|s| s).await;
    let refused = request(addr, "DELETE", "/pages/page", "").await;
    assert_eq!(refused.status, 405);
    assert_eq!(refused.header("allow"), Some("GET, HEAD"));
    let problem: serde_json::Value = serde_json::from_slice(&refused.body).unwrap();
    assert_eq!(problem["allowed_methods"], json!(["GET", "HEAD"]));

    // Where no route accepts `GET`, a `HEAD` is refused like any other verb, with an `Allow`
    // naming what is accepted.
    let refused = request(addr, "HEAD", "/pages/submit", "").await;
    assert_eq!(refused.status, 405);
    assert_eq!(refused.header("allow"), Some("POST"));
    assert!(refused.body.is_empty());
    let _ = stop.send(());
}

#[tokio::test]
async fn head_gets_the_304_its_get_gets() {
    let (addr, stop) = spawn(|s| s).await;
    let (get, head) = get_and_head(addr, "/pages/tagged", "If-None-Match: \"v1\"\r\n").await;
    assert_eq!(get.status, 304);
    assert_head_is_get_without_body(&get, &head);
    let (get, head) = get_and_head(addr, "/pages/tagged", "").await;
    assert_eq!(get.status, 200);
    assert_eq!(get.header("etag"), Some("\"v1\""));
    assert_head_is_get_without_body(&get, &head);
    let _ = stop.send(());
}

#[tokio::test]
async fn a_head_declared_for_another_pattern_leaves_an_earlier_get_its_head() {
    // `HEAD "{id}"` also matches `export`, but `GET "export"` is declared first, so
    // `HEAD /items/export` is that `GET`'s. (It reached `item_head` and got a `400`: `export`
    // is no `u64`.)
    let (addr, stop) = spawn(|s| s).await;
    let (get, head) = get_and_head(addr, "/items/export", "").await;
    assert_eq!(get.status, 200);
    assert_head_is_get_without_body(&get, &head);
    let head = request(addr, "HEAD", "/items/7", "").await;
    assert_eq!(head.status, 200);
    assert_eq!(head.header("x-handler"), Some("item_head"));
    let _ = stop.send(());
}

#[tokio::test]
async fn an_empty_reply_keeps_its_status_but_not_its_implicit_zero_length() {
    // hyper states `content-length: 0` for an empty `GET` reply and leaves it off the `HEAD`'s,
    // which RFC 9110 §9.3.2 allows: the one field that differs.
    let (addr, stop) = spawn(|s| s).await;
    let (get, head) = get_and_head(addr, "/pages/empty", "").await;
    assert_eq!((get.status, head.status), (200, 200));
    assert_eq!(get.header("content-length"), Some("0"));
    assert_eq!(head.header("content-length"), None);
    assert_eq!(head.header("x-empty"), Some("1"));
    assert!(head.body.is_empty());
    let _ = stop.send(());
}

#[tokio::test]
async fn head_does_not_hold_a_stream_open() {
    // A `GET` of `endless` never finishes. Its `HEAD` sends the headers and is done: the stream
    // is dropped without being read. (`exchange` fails the test if the server holds on.)
    let (addr, stop) = spawn(|s| s).await;
    let head = request(addr, "HEAD", "/pages/endless", "").await;
    assert_eq!(head.status, 200);
    assert_eq!(head.header("content-type"), Some("text/event-stream"));
    assert_eq!(head.header("cache-control"), Some("no-cache"));
    // A stream has no length to state, and a `HEAD` reply no body to frame.
    assert_eq!(head.header("content-length"), None);
    assert_eq!(head.header("transfer-encoding"), None);
    assert!(head.body.is_empty());
    let _ = stop.send(());
}

#[tokio::test]
async fn the_connection_carries_on_after_a_head() {
    // A `HEAD` and then a `GET`, pipelined on one connection. Had a body byte followed the
    // `HEAD`'s headers, the `GET`'s response would not start right after them. The stream route
    // checks the same for a `HEAD` whose body was dropped unread.
    let (addr, stop) = spawn(|s| s).await;
    for first in ["/pages/page", "/pages/endless"] {
        let raw = format!(
            "HEAD {first} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n\
             GET /pages/page HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        );
        let buf = exchange(addr, &raw).await;
        let end_of_head = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        assert_eq!(Response::parse(&buf[..end_of_head]).status, 200, "{first}");
        let rest = &buf[end_of_head..];
        assert!(
            rest.starts_with(b"HTTP/1.1 200 "),
            "after HEAD {first}: {:?}",
            String::from_utf8_lossy(rest)
        );
        let get = Response::parse(rest);
        assert_eq!(get.header("x-verb"), Some("GET"));
        let length = get.body.len().to_string();
        assert_eq!(get.header("content-length"), Some(length.as_str()));
    }
    let _ = stop.send(());
}

/// A compressed reply's `HEAD` states the encoding and the length its `GET` sends.
#[cfg(feature = "compression")]
#[tokio::test]
async fn head_carries_the_encoding_its_get_carries() {
    let (addr, stop) =
        spawn(|s| s.with_compression(actus_server::CompressionLayer::new().min_size(1))).await;
    let (get, head) = get_and_head(addr, "/pages/page", "Accept-Encoding: gzip\r\n").await;
    assert_eq!(get.header("content-encoding"), Some("gzip"));
    let length = get.body.len().to_string();
    assert_eq!(get.header("content-length"), Some(length.as_str()));
    assert_head_is_get_without_body(&get, &head);
    let _ = stop.send(());
}
