//! Actus example exercising the framework's main features end-to-end:
//!
//! - Long-lived services (`Database`, `AuthService`) constructed once at startup
//! - Persistent controllers that hold cloned handles to the services
//! - Longest-prefix routing (any depth) over real HTTP via the built-in hyper server
//! - Wired through `app_routes! { deps { ... } routes { ... } }`
//! - HTTP-verb restrictions (`GET "..."`, `POST "..."`, `DELETE "..."`)
//! - JSON body parsing (`data: JsonValue`)
//! - A trailing `{...path}` rest parameter (`FilesController`) that captures
//!   the remainder of the URL after a fixed `{drive}` segment
//! - CORS via `Server::with_cors(CorsLayer::permissive())` — preflight
//!   `OPTIONS` answered automatically, `Access-Control-*` on every response
//! - Response compression via `Server::with_compression(CompressionLayer::new())`
//!   (gzip/brotli; the `compression` feature is enabled in this example's Cargo.toml)
//! - A WebSocket echo endpoint (`GET /ws/echo`) via `ws::upgrade(...)`
//!   (the `websocket` feature is enabled in this example's Cargo.toml)
//! - A Server-Sent Events endpoint (`GET /events/ticks`) via `reply::sse`
//!   with multi-line data, a heartbeat comment, and a `retry:` hint
//! - `RequestLogger` middleware, plus a `MaintenanceMode` middleware that
//!   short-circuits with `503` via `Outcome::Respond` when `X-Maintenance` is set
//! - A `prepare = Self::check_auth` hook that resolves a `User` from the
//!   `Authorization` header. Per-handler access decisions (admin-only,
//!   etc.) live in the handlers themselves — actus is policy-agnostic.
//! - An OpenAPI 3.1 spec generator (`openapi` feature) served at
//!   `GET /openapi.json`, filtered to `/api/...` so internal mounts
//!   (`/health`, `/files`, `/ws`) are hidden. Also dumpable via
//!   `cargo run -p actus-basic-example -- --openapi` (prints and exits).

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use actus::openapi;
use actus::prelude::*;
use futures_util::{SinkExt, StreamExt, stream};
use serde_json::{Value as JsonValue, json};

// ============================================================================
// Services (the persistent state of the app)
// ============================================================================

#[derive(Clone)]
struct Database {
    inner: Arc<DatabaseInner>,
}
struct DatabaseInner;

impl Database {
    async fn connect() -> Result<Self, std::io::Error> {
        Ok(Self {
            inner: Arc::new(DatabaseInner),
        })
    }

    async fn list_users(&self, page: u32, limit: u32) -> Vec<JsonValue> {
        let _ = &self.inner;
        (0..limit)
            .map(|i| {
                let id = (page as u64 - 1) * limit as u64 + i as u64 + 1;
                json!({ "id": id, "name": format!("User {}", id) })
            })
            .collect()
    }

    async fn get_user(&self, id: u64) -> Option<JsonValue> {
        if id == 0 {
            None
        } else {
            Some(json!({ "id": id, "name": format!("User {}", id) }))
        }
    }
}

/// Toy auth: in real code these tokens would be JWTs validated against a key.
/// Here we recognize two literal strings for demo purposes.
#[derive(Clone)]
struct AuthService;

impl AuthService {
    /// Resolve a token into a User. The prepare hook stashes this on
    /// `Params` so handlers don't have to redo the lookup.
    fn resolve(&self, token: &str) -> Option<User> {
        match token {
            "user-token" => Some(User {
                name: "alice".into(),
                is_admin: false,
            }),
            "admin-token" => Some(User {
                name: "root".into(),
                is_admin: true,
            }),
            _ => None,
        }
    }
}

/// Per-request user identity, resolved by `check_auth` and made available
/// to handlers via `params.get::<User>()`.
#[derive(Clone, Debug)]
struct User {
    name: String,
    is_admin: bool,
}

// ============================================================================
// Middleware
// ============================================================================

/// A toy "maintenance mode" middleware: when a request carries an
/// `X-Maintenance` header, it short-circuits with `503` and never reaches a
/// handler. (A real one would key off shared state, not a header — but this
/// shows `Outcome::Respond`.)
struct MaintenanceMode;

#[async_trait]
impl Middleware for MaintenanceMode {
    async fn before(&self, request: &mut Request) -> Result<Outcome, WebError> {
        if request.headers.contains_key("x-maintenance") {
            return Ok(Outcome::Respond(
                reply::build_reply()
                    .status(StatusCode::SERVICE_UNAVAILABLE)
                    .header("Retry-After", "120")
                    .body(reply::json(json!({ "error": "under maintenance" })))
                    .done(),
            ));
        }
        Ok(Outcome::Continue)
    }
}

// ============================================================================
// Controllers
// ============================================================================

struct HealthController;

#[controller]
impl HealthController {
    routes! {
        "" => index(),
    }

    pub async fn index(&self) -> Reply {
        reply!(json!({ "status": "ok" }))
    }
}

/// Demonstrates a trailing `{...path}` rest parameter.
///
/// Mounted at `/files`. `"{drive}/{...path}"` captures the first remaining
/// segment as `drive` (required) and soaks up *everything after it* — slashes
/// included — into `path` (a `String`). The rest token matches zero or more
/// segments, so `/files/c` gives `path == ""` and `/files/c/docs/2026/q2.md`
/// gives `path == "docs/2026/q2.md"`. This is the REST-shaped alternative to
/// stuffing the sub-path into a query parameter (`/files/c?path=docs/...`).
///
/// `drive` being required, `/files` (nothing after the prefix) does *not*
/// match `"{drive}/{...path}"` — it matches the explicit `"" => list_drives`
/// route instead. Without that route, `/files` would be a plain 404.
struct FilesController;

#[controller]
impl FilesController {
    routes! {
        GET ""                  => list_drives(),
        // `"list"` must be declared before `"{drive}/{...path}"` — both match
        // a single-segment action; the first one declared wins.
        GET "list"              => list(r#type: Vec<String>),
        GET "{drive}/{...path}" => read(drive: String, path: String),
        PUT "{drive}/{...path}" => write(drive: String, path: String, data: JsonValue),
    }

    pub async fn list_drives(&self) -> Reply {
        reply!(json!({ "drives": ["c", "d"] }))
    }

    /// `GET /files/list?type=md&type=txt` — `r#type` is a raw identifier, so it
    /// binds the `?type=` query key (you can't write a parameter literally
    /// named `type` — it's a keyword); `Vec<String>` collects every value.
    pub async fn list(&self, r#type: Vec<String>) -> Reply {
        reply!(json!({ "filter_types": r#type }))
    }

    pub async fn read(&self, drive: String, path: String) -> Reply {
        reply!(json!({ "drive": drive, "path": path, "is_root": path.is_empty() }))
    }

    pub async fn write(&self, drive: String, path: String, data: JsonValue) -> Reply {
        reply!(json!({ "drive": drive, "path": path, "wrote": data }))
    }
}

/// A Server-Sent Events endpoint that emits a few demo frames and ends.
/// `GET /events/ticks` returns `Content-Type: text/event-stream` with three
/// `data:`-bearing frames, a heartbeat comment, and a `retry:` hint —
/// enough to exercise `reply::sse` end-to-end.
struct EventsController;

#[controller]
impl EventsController {
    routes! {
        GET "ticks" => ticks(),
    }

    /// Stream a fixed sequence of SSE frames and close. A real-world
    /// handler would build the stream from a channel / DB cursor / pubsub
    /// subscription, and emit a heartbeat comment every ~15s.
    pub async fn ticks(&self) -> Reply {
        let events = stream::iter(vec![
            SseEvent::data("tick").id("1"),
            SseEvent::data(json!({"n": 2}).to_string())
                .event("update")
                .id("2"),
            SseEvent::comment("keep-alive"),
            SseEvent::data("multi\nline\nbody").id("3"),
            SseEvent::data("done").retry(Duration::from_secs(5)),
        ]);
        reply!(sse: events)
    }
}

/// A WebSocket echo endpoint. `GET /ws/echo` upgrades to a WebSocket and
/// bounces every text/binary frame back to the client. A real handler would
/// typically check `params.header("origin")` (and/or an auth token) *before*
/// returning `ws::upgrade(...)`.
struct WsController;

#[controller]
impl WsController {
    routes! {
        GET "echo" => echo(),
    }

    pub async fn echo(&self) -> Reply {
        Ok(ws::upgrade(|mut socket| async move {
            while let Some(Ok(msg)) = socket.next().await {
                match msg {
                    Message::Text(_) | Message::Binary(_) => {
                        if socket.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }))
    }
}

/// Serves the generated OpenAPI 3.1 spec at `/openapi.json`.
///
/// The spec is generated *after* `init()` returns the built `Router` (since
/// the generator needs the router to walk), then stored in this controller's
/// `Arc<OnceLock<JsonValue>>`. The cell is a dep on the `app_routes!` block,
/// so the controller already exists at request time — `main()` just sets the
/// value after `init()` and before `Server::run(...)`.
struct OpenApiController {
    spec: Arc<OnceLock<JsonValue>>,
}

#[controller]
impl OpenApiController {
    routes! {
        GET "" => doc(),
    }

    /// Returns the cached OpenAPI 3.1 document. (Cached because the spec is
    /// derived from static route metadata — it never changes at runtime.)
    pub async fn doc(&self) -> Reply {
        match self.spec.get() {
            Some(v) => reply!(v.clone()),
            // Should not happen: main() always sets the cell before serving.
            None => Err(WebError::Internal("openapi spec not initialized".into())),
        }
    }
}

struct UserController {
    db: Database,
    auth_svc: AuthService,
}

#[controller(prepare = Self::check_auth)]
impl UserController {
    routes! {
        GET ""        => list(page: u32 = 1, limit: u32 = 10),
        GET "{id}"    => get(id: u64),
        POST ""       => create(params: &Params, data: JsonValue),
        DELETE "{id}" => delete(params: &Params, id: u64),
    }

    /// Runs before every dispatched handler.
    ///
    /// Resolves a `User` from the `Authorization: Bearer …` header (if
    /// present) and stashes it on `Params` for handlers to read via
    /// `params.get::<User>()`. Anonymous requests are allowed through;
    /// individual handlers decide whether they require a `User` and what
    /// role they require — actus has no opinion on policy.
    ///
    /// Returns:
    /// - `Ok(None)` to continue to the handler.
    /// - `Ok(Some(reply))` to short-circuit with that reply.
    /// - `Err(WebError::*)` to short-circuit with an error response.
    async fn check_auth(
        &self,
        _route: &RouteDef,
        params: &mut Params,
    ) -> Result<Option<ReplyData>, WebError> {
        // Demonstrate the early-return capability: any request that
        // includes an `X-Demo-Greet` header short-circuits with a custom
        // reply, bypassing the handler entirely. (Header-based so strict
        // mode doesn't reject it before this hook runs.)
        if params.header("x-demo-greet").is_some() {
            return Ok(Some(json(json!({ "hello": "from prepare!" }))));
        }

        if let Some(token) = params.bearer_token() {
            let user = self.auth_svc.resolve(token).ok_or(WebError::Unauthorized)?;
            params.insert(user);
        }
        Ok(None)
    }

    pub async fn list(&self, page: u32, limit: u32) -> Reply {
        let users = self.db.list_users(page, limit).await;
        reply!(json!({ "page": page, "limit": limit, "users": users }))
    }

    pub async fn get(&self, id: u64) -> Reply {
        match self.db.get_user(id).await {
            Some(user) => reply!(user),
            None => Err(WebError::NotFound),
        }
    }

    pub async fn create(&self, params: &Params, data: JsonValue) -> Reply {
        // Authenticated-only: any User is fine.
        let user = params.get::<User>().ok_or(WebError::Unauthorized)?;
        reply!(json!({ "created": true, "by": user.name, "data": data }))
    }

    pub async fn delete(&self, params: &Params, id: u64) -> Reply {
        // Admin-only: handler enforces the role itself. When the role
        // check fails we return a structured `Problem` so the client sees
        // *which* role was required, not just `403 Forbidden`. This is
        // the pattern for any error where context aids the caller.
        let user = params.get::<User>().ok_or(WebError::Unauthorized)?;
        if !user.is_admin {
            return Err(WebError::Problem(
                ProblemDetails::new(StatusCode::FORBIDDEN, "Forbidden")
                    .detail("admin role required to delete")
                    .extra("required_role", "admin")
                    .extra("actor", user.name.clone()),
            ));
        }
        reply!(json!({ "deleted": id, "by": user.name }))
    }
}

// ============================================================================
// Wiring
// ============================================================================

// `db` is constructed in `main()` and passed in. `auth` is constructed
// inside the deps block. Either approach works; in real apps the choice
// usually depends on whether the same value is needed elsewhere
// (e.g., for non-server use such as a CLI subcommand).
// Two separate UserController instances both hold copies of `db` and
// `auth`. The first uses shorthand (`db`); the second uses the explicit
// `target: source` form (`auth_svc: auth`). The macro auto-clones both,
// so the second registration doesn't fail to compile from a moved-out
// `auth` after the first.
app_routes! {
    deps(db: Database, openapi_spec: Arc<OnceLock<JsonValue>>) {
        auth = AuthService,
    }
    routes {
        "health"        => HealthController,
        "files"         => FilesController,
        "events"        => EventsController,
        "ws"            => WsController,
        "api/users"     => UserController { db, auth_svc: auth },
        "api/v2/users"  => UserController { db, auth_svc: auth },
        "openapi.json"  => OpenApiController { spec: openapi_spec },
    }
}

// ============================================================================
// main
// ============================================================================

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let db = Database::connect().await?;
    let openapi_spec: Arc<OnceLock<JsonValue>> = Arc::new(OnceLock::new());
    let router = init(db, openapi_spec.clone()).await?;

    // Build the spec from the now-frozen route tree. Filter to `/api/...`
    // so internal mounts (`/health`, `/files`, `/ws`, `/openapi.json`
    // itself) don't appear in the public surface.
    let spec = openapi::generate(
        &router,
        &openapi::Options::new("Actus Basic Example", "0.1.0")
            .description("Demo API for the actus framework.")
            .server("http://localhost:3000", Some("local dev")),
        |mount| mount.starts_with("api/"),
    );

    // `--openapi`: dump the spec to stdout and exit. Lets the spec be piped
    // into `swagger-ui` / `redoc` / a file without running the server.
    if std::env::args().any(|a| a == "--openapi") {
        println!("{}", openapi::to_string_pretty(&spec));
        return Ok(());
    }

    openapi_spec
        .set(spec)
        .expect("openapi_spec only set once, here in main()");

    Server::new(router)
        .with_middleware(RequestLogger)
        // Runs after the logger, so even maintenance-mode requests get logged;
        // `before` returning `Outcome::Respond` short-circuits the handler.
        .with_middleware(MaintenanceMode)
        // Wide-open CORS — fine for a demo; a real app would pin the origin
        // (`CorsLayer::new().allow_origin("https://app.example.com")…`).
        .with_cors(CorsLayer::permissive())
        // gzip/brotli on responses ≥ 1 KiB (requires the `compression` feature).
        .with_compression(CompressionLayer::new())
        .run(3000)
        .await?;
    Ok(())
}
