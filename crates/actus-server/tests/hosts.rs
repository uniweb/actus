//! Host aliases, end to end: a real server, raw HTTP/1.1 requests carrying
//! `Host` headers.
//!
//! * An aliased host reaches the aliased controller through that
//!   controller's own routes — typed params, verbs, JSON bodies — with the
//!   host's label as the first segment of the action, and the handler learns
//!   it was aliased from `Params::alias_prefix`.
//! * Shared mounts keep their paths there; every other mount is unreachable.
//! * A host no alias names is routed exactly as it would be without aliases.
//! * Middleware sees the aliased `path_parts`, and `match_controller` on them
//!   returns the controller the server dispatched to — one address for all.
//! * Matching ignores case, the port and a trailing dot, and an absolute-form
//!   request's authority wins over `Host`.
//! * `None` on the host side means there is no alias.

use actus::prelude::*;
use serde_json::{Value as JsonValue, json};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

/// The aliased controller: every route starts with the tenant label.
struct Tenants;

#[controller]
impl Tenants {
    routes! {
        GET  "{tenant}"             => home(params: &Params, tenant: String),
        GET  "{tenant}/orders/{id}" => order(tenant: String, id: u64),
        POST "{tenant}/orders"      => create(tenant: String, data: JsonValue),
    }

    pub async fn home(&self, params: &Params, tenant: String) -> Reply {
        reply!(json!({ "at": "tenants", "tenant": tenant, "alias_prefix": params.alias_prefix() }))
    }

    pub async fn order(&self, tenant: String, id: u64) -> Reply {
        reply!(json!({ "at": "tenants", "tenant": tenant, "order": id }))
    }

    pub async fn create(&self, tenant: String, data: JsonValue) -> Reply {
        reply!(
            status = StatusCode::CREATED,
            json!({ "at": "tenants", "tenant": tenant, "created": data })
        )
    }
}

/// Shared with the tenant hosts: reachable there at its own path.
struct Assets;

#[controller]
impl Assets {
    routes! {
        GET "{...path}" => file(params: &Params, path: String),
    }

    pub async fn file(&self, params: &Params, path: String) -> Reply {
        reply!(json!({ "at": "assets", "path": path, "alias_prefix": params.alias_prefix() }))
    }
}

/// Not shared: must be unreachable on a tenant host.
struct Api;

#[controller]
impl Api {
    routes! {
        GET "{...path}" => any(path: String),
    }

    pub async fn any(&self, path: String) -> Reply {
        reply!(json!({ "at": "api", "path": path }))
    }
}

/// The root catch-all — what an unaliased host falls through to, and what an
/// aliased host must never reach.
struct Spa;

#[controller]
impl Spa {
    routes! {
        GET "{...path}" => page(path: String),
    }

    pub async fn page(&self, path: String) -> Reply {
        reply!(json!({ "at": "spa", "path": path }))
    }
}

app_routes! {
    deps(tenant_host: Option<String>) {}
    hosts {
        // An exact host, declared first: it wins over the pattern below,
        // which would otherwise name it too.
        "admin.example.test" => "api",
        tenant_host          => "tenants/{tenant}" shares ["assets"],
    }
    routes {
        "tenants" => Tenants,
        "assets"  => Assets,
        "api"     => Api,
        "*"       => Spa,
    }
}

/// `(path_parts as a middleware saw them, the controller match_controller
/// returned for them)`, one row per request.
type Seen = Arc<Mutex<Vec<(String, Option<&'static str>)>>>;

/// Records what a middleware sees — the path, and what the framework's own
/// matcher makes of it — so a test can hold it against the dispatch.
struct SeesPath {
    router: Arc<Router>,
    seen: Seen,
}

#[async_trait]
impl Middleware for SeesPath {
    async fn before(&self, request: &mut Request) -> Result<Outcome, WebError> {
        let matched = self
            .router
            .match_controller(&request.path_parts)
            .map(|m| m.controller.__name());
        self.seen
            .lock()
            .unwrap()
            .push((request.path_parts.join("/"), matched));
        Ok(Outcome::Continue)
    }
}

/// Start a server whose tenant alias is `tenant_host`, on a listener bound
/// here and kept — never bound, dropped and re-bound (see
/// `tests/middleware.rs` for the race that shape loses).
async fn spawn(tenant_host: Option<&str>) -> (SocketAddr, Seen, oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let (tx, rx) = oneshot::channel::<()>();
    let tenant_host = tenant_host.map(String::from);
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        let server = Server::new(init(tenant_host).await.unwrap());
        let sees = SeesPath {
            router: server.router(),
            seen: log,
        };
        server
            .with_middleware(sees)
            .run_with_shutdown_listener(listener, async move {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    (addr, seen, tx)
}

/// Send one raw HTTP/1.1 request (`Connection: close`); return the status,
/// the `Allow` header if any, and the body parsed as JSON (`Null` if empty).
async fn http(addr: SocketAddr, raw: &str) -> (u16, Option<String>, JsonValue) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(raw.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a complete response head");
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
    let allow = lines
        .filter_map(|l| l.split_once(": "))
        .find(|(n, _)| n.eq_ignore_ascii_case("allow"))
        .map(|(_, v)| v.to_string());
    let body = &buf[split + 4..];
    let body = if body.is_empty() {
        JsonValue::Null
    } else {
        serde_json::from_slice(body).unwrap()
    };
    (status, allow, body)
}

async fn get(addr: SocketAddr, host: &str, path: &str) -> (u16, JsonValue) {
    let raw = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    let (status, _, body) = http(addr, &raw).await;
    (status, body)
}

const TENANT_HOST: &str = "{tenant}.example.test";

#[tokio::test]
async fn an_aliased_host_is_the_aliased_mount_through_its_own_routes() {
    let (addr, _, stop) = spawn(Some(TENANT_HOST)).await;
    let host = "acme.example.test:8080";

    // The host's root is the target itself; the handler is told it was aliased.
    assert_eq!(
        get(addr, host, "/").await,
        (
            200,
            json!({ "at": "tenants", "tenant": "acme", "alias_prefix": "tenants/acme" })
        )
    );
    // Typed path parameters.
    assert_eq!(
        get(addr, host, "/orders/7").await,
        (
            200,
            json!({ "at": "tenants", "tenant": "acme", "order": 7 })
        )
    );
    // Verbs and a JSON body, exactly as under the path mount.
    let body = r#"{"sku":"x1"}"#;
    let raw = format!(
        "POST /orders HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let (status, _, created) = http(addr, &raw).await;
    assert_eq!(status, 201);
    assert_eq!(
        created,
        json!({ "at": "tenants", "tenant": "acme", "created": { "sku": "x1" } })
    );
    // …and the controller's verb rules: DELETE is not a route, so 405 + Allow.
    let raw = format!("DELETE /orders/7 HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    let (status, allow, _) = http(addr, &raw).await;
    assert_eq!((status, allow.as_deref()), (405, Some("GET")));

    let _ = stop.send(());
}

#[tokio::test]
async fn shared_mounts_keep_their_paths_and_nothing_else_is_reachable() {
    let (addr, _, stop) = spawn(Some(TENANT_HOST)).await;
    let host = "acme.example.test";

    // Shared: its own path, and not reported as aliased.
    assert_eq!(
        get(addr, host, "/assets/app.js").await,
        (
            200,
            json!({ "at": "assets", "path": "app.js", "alias_prefix": null })
        )
    );
    // Not shared: `/api/…` goes under the tenant, whose routes 404 it — the
    // API controller is not reachable from here.
    let (status, body) = get(addr, host, "/api/things").await;
    assert_eq!(status, 404);
    assert_ne!(body["at"], "api");
    // Nor is the root catch-all: an unknown path 404s inside the tenant.
    let (status, body) = get(addr, host, "/no/such/page").await;
    assert_eq!(status, 404);
    assert_ne!(body["at"], "spa");

    let _ = stop.send(());
}

#[tokio::test]
async fn a_host_no_alias_names_is_routed_as_before() {
    let (addr, _, stop) = spawn(Some(TENANT_HOST)).await;

    for host in [
        "127.0.0.1",
        "localhost:8080",
        // One label too many, and a label a capture cannot match.
        "a.b.example.test",
        "under_score.example.test",
    ] {
        assert_eq!(
            get(addr, host, "/api/x").await,
            (200, json!({ "at": "api", "path": "x" })),
            "{host}"
        );
        assert_eq!(
            get(addr, host, "/somewhere").await,
            (200, json!({ "at": "spa", "path": "somewhere" })),
            "{host}"
        );
    }
    // The path mount the alias names still answers directly — and says it
    // was not aliased.
    assert_eq!(
        get(addr, "127.0.0.1", "/tenants/acme").await,
        (
            200,
            json!({ "at": "tenants", "tenant": "acme", "alias_prefix": null })
        )
    );

    let _ = stop.send(());
}

#[tokio::test]
async fn middleware_sees_the_aliased_path_and_the_matcher_agrees_with_the_dispatch() {
    let (addr, seen, stop) = spawn(Some(TENANT_HOST)).await;

    get(addr, "acme.example.test", "/orders/7").await;
    get(addr, "acme.example.test", "/assets/app.js").await;
    get(addr, "127.0.0.1", "/api/x").await;

    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            ("tenants/acme/orders/7".to_string(), Some("Tenants")),
            ("assets/app.js".to_string(), Some("Assets")),
            ("api/x".to_string(), Some("Api")),
        ]
    );

    let _ = stop.send(());
}

#[tokio::test]
async fn host_matching_ignores_case_port_and_trailing_dot_and_reads_the_absolute_form() {
    let (addr, _, stop) = spawn(Some(TENANT_HOST)).await;

    // The label arrives lowercased, whatever the client sent.
    assert_eq!(
        get(addr, "ACME.Example.TEST.:8080", "/orders/1").await,
        (
            200,
            json!({ "at": "tenants", "tenant": "acme", "order": 1 })
        )
    );
    // Absolute form: the request-target's authority names the host, not `Host`.
    let raw = "GET http://globex.example.test/orders/3 HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    let (status, _, body) = http(addr, raw).await;
    assert_eq!(
        (status, body),
        (
            200,
            json!({ "at": "tenants", "tenant": "globex", "order": 3 })
        )
    );

    let _ = stop.send(());
}

#[tokio::test]
async fn the_first_alias_that_names_the_host_wins() {
    let (addr, _, stop) = spawn(Some(TENANT_HOST)).await;

    // `admin.example.test` matches both entries; the exact one comes first.
    assert_eq!(
        get(addr, "admin.example.test", "/stats").await,
        (200, json!({ "at": "api", "path": "stats" }))
    );

    let _ = stop.send(());
}

#[tokio::test]
async fn an_unconfigured_alias_does_not_exist() {
    let (addr, _, stop) = spawn(None).await;

    // No tenant alias: the tenant host is just another host.
    assert_eq!(
        get(addr, "acme.example.test", "/orders/7").await,
        (200, json!({ "at": "spa", "path": "orders/7" }))
    );
    // The other, configured entry is unaffected.
    assert_eq!(
        get(addr, "admin.example.test", "/stats").await,
        (200, json!({ "at": "api", "path": "stats" }))
    );

    let _ = stop.send(());
}
