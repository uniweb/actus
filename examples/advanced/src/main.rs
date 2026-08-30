//! Advanced Actus example — demonstrates the patterns from the README in
//! working code. A small "tasks" API with auth, typed bodies, domain errors,
//! and a real rate-limit middleware. Compare with `examples/basic`, which
//! shows the *framework features* (CORS, compression, WS, SSE, OpenAPI) on
//! a single controller; this example shows the *application-side patterns*
//! you reach for when you start building a real service.
//!
//! Patterns covered:
//!
//! - **Reusable `prepare`-hook bodies** (§Patterns/"Reusable prepare-hook
//!   bodies") — a free function `lax_auth` does the work; each controller
//!   has a 3-line stub method that delegates. The boilerplate is scoped to
//!   the controllers that don't need it.
//! - **Typed `Params` extensions** (§Patterns/"Typed Params extensions") —
//!   `AuthParamsExt` adds `require_user()` and `require_role(...)` so
//!   handlers don't repeat `params.get::<User>().ok_or(Unauthorized)?`.
//! - **Error mapping at the binary** (§Patterns/"Error mapping at the
//!   binary") — `MyError` lives in the example crate; a single
//!   `impl From<MyError> for WebError` produces `Problem(...)` per variant;
//!   `MyResultExt::web()` lets handlers `?`-propagate domain errors.
//! - **JSON body deserialization with informative 400s** (§Patterns/"JSON
//!   body deserialization with informative 400s") — `CreateTaskRequest` /
//!   `UpdateTaskRequest` deserialize into typed structs; malformed bodies
//!   become structured `400`s naming the failure, not opaque `Internal`s.
//! - **Per-controller rate-limit class** (§Patterns/"Rate-limiting") — a
//!   token-bucket middleware that reads each request's `rate_limit_class`
//!   (declared on the controller via `#[controller(rate_limit = "…")]`) and
//!   applies the matching per-class policy, keyed on the first
//!   `X-Forwarded-For` IP. `MeController` is class `"auth"` (tight),
//!   `TasksController` is class `"tasks"` (looser), and `HealthController`
//!   declares no class — so `/health` is never limited. The framework owns
//!   the label + the `429` / `Retry-After`; the buckets and limits are here.
//! - **Per-controller body cap** — `TasksController` uses
//!   `#[controller(max_body_bytes = 4 * KIB)]` to refuse oversized JSON
//!   bodies before allocating. Compare with the global cap on
//!   `Server::with_max_body_bytes`.
//! - **Route families: floor, coverage, gate** (README §"Route families") —
//!   each controller under `api/` declares the least-privileged caller it
//!   accepts (`#[controller(expects = "…")]`): `MeController` is
//!   `"credential"`, `TasksController` is `"anonymous"` (reads are open;
//!   writes gate per-handler). `family_coverage` walks `Router::mounts()` at
//!   boot and fails on a controller that declared nothing, declared a floor
//!   the family doesn't accept, or declared `"credential"` with no `prepare`
//!   hook. `FloorGate` then enforces the floor per request: it reads the
//!   declaration off the matched controller via `server.router()` +
//!   `match_controller` and refuses un-credentialed callers on
//!   `"credential"` mounts before the handler runs. `HealthController`
//!   declares nothing and is mounted outside every family — unconstrained
//!   by construction.
//!
//! The integration tests in `tests/integration.rs` exercise the daemon-guard
//! pattern: spawn this binary as a subprocess, run real HTTP requests, let
//! `Drop` reap the child.
//!
//! Run: `cargo run -p actus-advanced-example` (port 3001 by default).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use actus::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use thiserror::Error;

// ============================================================================
// Domain
// ============================================================================

#[derive(Clone, Debug, Serialize)]
struct Task {
    id: u64,
    title: String,
    tags: Vec<String>,
    done: bool,
}

#[derive(Clone, Debug, Serialize)]
struct User {
    name: String,
    role: Role,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    Member,
    Editor,
    Admin,
}

// ============================================================================
// Storage — in-memory `Arc<Mutex<Vec<Task>>>` with auto-increment id
// ============================================================================

#[derive(Clone)]
struct Storage {
    tasks: Arc<Mutex<Vec<Task>>>,
    next_id: Arc<AtomicU64>,
}

impl Storage {
    fn new() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(Vec::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    fn list(&self, tags: &[String], page: u32, limit: u32) -> Vec<Task> {
        let tasks = self.tasks.lock().unwrap();
        let filtered: Vec<Task> = tasks
            .iter()
            .filter(|t| tags.is_empty() || tags.iter().any(|wanted| t.tags.contains(wanted)))
            .cloned()
            .collect();
        let start = ((page.saturating_sub(1)) as usize).saturating_mul(limit as usize);
        filtered
            .into_iter()
            .skip(start)
            .take(limit as usize)
            .collect()
    }

    fn get(&self, id: u64) -> Result<Task, MyError> {
        self.tasks
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.id == id)
            .cloned()
            .ok_or(MyError::NotFound)
    }

    fn create(&self, req: CreateTaskRequest) -> Result<Task, MyError> {
        if req.title.trim().is_empty() {
            return Err(MyError::Validation {
                field: "title".into(),
                rule: "non-empty".into(),
            });
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let task = Task {
            id,
            title: req.title,
            tags: req.tags,
            done: false,
        };
        self.tasks.lock().unwrap().push(task.clone());
        Ok(task)
    }

    fn update(&self, id: u64, req: UpdateTaskRequest) -> Result<Task, MyError> {
        let mut tasks = self.tasks.lock().unwrap();
        let task = tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or(MyError::NotFound)?;
        if let Some(title) = req.title {
            if title.trim().is_empty() {
                return Err(MyError::Validation {
                    field: "title".into(),
                    rule: "non-empty".into(),
                });
            }
            task.title = title;
        }
        if let Some(tags) = req.tags {
            task.tags = tags;
        }
        if let Some(done) = req.done {
            task.done = done;
        }
        Ok(task.clone())
    }

    fn delete(&self, id: u64) -> Result<(), MyError> {
        let mut tasks = self.tasks.lock().unwrap();
        let before = tasks.len();
        tasks.retain(|t| t.id != id);
        if tasks.len() == before {
            Err(MyError::NotFound)
        } else {
            Ok(())
        }
    }
}

// ============================================================================
// Pattern: error mapping at the binary
// ============================================================================
//
// `MyError` is a domain error — it knows nothing about HTTP. The `From` impl
// turns each variant into a `WebError::Problem(...)` so the framework can
// produce a structured `application/problem+json` response. Handlers use
// `?` against `Result<_, WebError>`; the `web()` extension trait converts a
// `Result<_, MyError>` into the right shape for `?`-propagation.

#[derive(Debug, Error)]
enum MyError {
    #[error("not found")]
    NotFound,
    #[error("validation: {field} must be {rule}")]
    Validation { field: String, rule: String },
}

impl From<MyError> for WebError {
    fn from(e: MyError) -> WebError {
        match e {
            MyError::NotFound => WebError::NotFound,
            MyError::Validation { field, rule } => WebError::Problem(
                ProblemDetails::new(StatusCode::BAD_REQUEST, "Validation")
                    .detail(format!("{field} must be {rule}"))
                    .extra("field", field)
                    .extra("rule", rule),
            ),
        }
    }
}

/// Extension trait so `result.web()?` reads naturally in handlers.
trait MyResultExt<T> {
    fn web(self) -> Result<T, WebError>;
}

impl<T> MyResultExt<T> for Result<T, MyError> {
    fn web(self) -> Result<T, WebError> {
        self.map_err(WebError::from)
    }
}

// ============================================================================
// Pattern: reusable `prepare`-hook body + typed Params extensions
// ============================================================================
//
// `lax_auth` is the same shape in every controller that has it — resolve a
// bearer token if present, stash the user, pass anonymous through. Each
// controller has a 3-line stub method calling this; only controllers that
// need a *different* hook write one.
//
// `AuthParamsExt` makes the "stashed user" lookup ergonomic — `params
// .require_user()` instead of `params.get::<User>().ok_or(Unauthorized)`.

fn resolve_token(token: &str) -> Option<User> {
    // Toy auth: in a real app these would be JWTs validated against a key,
    // or session-tokens looked up in a store.
    match token {
        "alice-token" => Some(User {
            name: "alice".into(),
            role: Role::Member,
        }),
        "bob-token" => Some(User {
            name: "bob".into(),
            role: Role::Editor,
        }),
        "admin-token" => Some(User {
            name: "root".into(),
            role: Role::Admin,
        }),
        _ => None,
    }
}

/// Anonymous requests pass through; a token that doesn't resolve returns
/// `Unauthorized`. Handlers that need a user call `params.require_user()`.
async fn lax_auth(params: &mut Params) -> Result<Option<ReplyData>, WebError> {
    if let Some(token) = params.bearer_token() {
        let user = resolve_token(token).ok_or(WebError::Unauthorized)?;
        params.insert(user);
    }
    Ok(None)
}

trait AuthParamsExt {
    /// Return the user stashed by `lax_auth`, or `Unauthorized` if there
    /// isn't one (anonymous request).
    fn require_user(&self) -> Result<&User, WebError>;
    /// Like `require_user` but also enforces a minimum role. Returns
    /// `Forbidden` (with a `required_role` extension in the problem body)
    /// if the user's role is lower than `min`.
    fn require_role(&self, min: Role) -> Result<&User, WebError>;
}

impl AuthParamsExt for Params {
    fn require_user(&self) -> Result<&User, WebError> {
        self.get::<User>().ok_or(WebError::Unauthorized)
    }
    fn require_role(&self, min: Role) -> Result<&User, WebError> {
        let user = self.require_user()?;
        if user.role < min {
            return Err(WebError::Problem(
                ProblemDetails::new(StatusCode::FORBIDDEN, "Forbidden")
                    .detail(format!("requires role at least {min:?}"))
                    .extra("required_role", format!("{min:?}").to_lowercase())
                    .extra("actor_role", format!("{:?}", user.role).to_lowercase()),
            ));
        }
        Ok(user)
    }
}

// ============================================================================
// Pattern: per-controller rate-limit class (from README §Patterns/"Rate-limiting")
// ============================================================================
//
// The controller declares *which rate-limit class it belongs to* via
// `#[controller(rate_limit = "…")]`; the framework stamps that label onto the
// matched `Request` (`request.rate_limit_class`). This middleware owns the
// *policy*: a token bucket per (class, client), with per-class capacity /
// refill. A request whose controller declared no class — or a class with no
// registered policy — passes through unlimited.
//
// In-memory buckets keyed on the first `X-Forwarded-For` segment (or "peer"
// when there's no such header — single-instance demo). The `Middleware` impl
// is identical for a Redis-backed version; only the storage layer changes.

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// Token-bucket parameters for one rate-limit class.
#[derive(Clone, Copy)]
struct Policy {
    capacity: f64,
    refill_per_sec: f64,
}

struct RateLimit {
    /// class label (the `#[controller(rate_limit = "…")]` value) → policy.
    policies: HashMap<&'static str, Policy>,
    /// (class, client-key) → bucket. One namespace per class, so the same
    /// client gets independent buckets for "auth" vs "tasks".
    state: Mutex<HashMap<(&'static str, String), Bucket>>,
}

impl RateLimit {
    fn new() -> Self {
        Self {
            policies: HashMap::new(),
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Register a policy for a rate-limit class. Requests to controllers in
    /// `class` get `capacity` burst, refilling at `refill_per_sec` tokens/sec.
    /// Classes with no policy (and controllers with no class) aren't limited.
    fn class(mut self, class: &'static str, capacity: u32, refill_per_sec: f64) -> Self {
        self.policies.insert(
            class,
            Policy {
                capacity: capacity as f64,
                refill_per_sec,
            },
        );
        self
    }

    /// The classes this limiter has a policy for. The startup coverage check
    /// (`rate_limit_coverage`) diffs these against the classes controllers
    /// actually declare.
    fn classes(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.policies.keys().copied()
    }

    fn client_key(request: &Request) -> String {
        request
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(',').next().unwrap_or(s).trim().to_string())
            .unwrap_or_else(|| "peer".to_string())
    }

    fn check(&self, class: &'static str, policy: Policy, client: &str) -> Result<(), Duration> {
        let mut state = self.state.lock().unwrap();
        let bucket = state
            .entry((class, client.to_string()))
            .or_insert_with(|| Bucket {
                tokens: policy.capacity,
                last_refill: Instant::now(),
            });
        let now = Instant::now();
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * policy.refill_per_sec).min(policy.capacity);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let deficit = 1.0 - bucket.tokens;
            let secs = (deficit / policy.refill_per_sec).ceil() as u64;
            Err(Duration::from_secs(secs.max(1)))
        }
    }
}

#[async_trait]
impl Middleware for RateLimit {
    async fn before(&self, request: &mut Request) -> Result<Outcome, WebError> {
        // No class on the matched controller → not subject to limiting.
        let Some(class) = request.rate_limit_class else {
            return Ok(Outcome::Continue);
        };
        // A class with no registered policy is likewise unlimited (lets you
        // label a controller before deciding its limits).
        let Some(&policy) = self.policies.get(class) else {
            return Ok(Outcome::Continue);
        };
        let client = Self::client_key(request);
        match self.check(class, policy, &client) {
            Ok(()) => Ok(Outcome::Continue),
            Err(retry) => Err(WebError::TooManyRequests(Some(retry))),
        }
    }
}

/// Startup coverage check: every rate-limit *class* a controller declares
/// (`#[controller(rate_limit = "…")]`) must have a registered policy. The
/// class is a string label an unrelated middleware interprets, so a typo
/// (`"ath"` for `"auth"`) would otherwise mean *unlimited* rather than an
/// error. Run this once at boot — `Router::rate_limit_classes()` is the
/// declared half, `limiter.classes()` is the registered half — so a mismatch
/// fails fast instead of shipping a silently-open controller. One router walk;
/// no per-request cost.
///
/// Returns `Err(message)` if any declared class lacks a policy (fatal). A
/// policy registered for a class no controller declares is only warned about —
/// it's harmless on its own, but it's often the *other* footprint of a typo
/// (you wrote `"auth"`, the controller says `"ath"`), so naming it helps.
fn rate_limit_coverage(router: &Router, limiter: &RateLimit) -> Result<(), String> {
    let declared = router.rate_limit_classes(); // Vec<RateLimitClass>
    let registered: HashSet<&'static str> = limiter.classes().collect();
    let declared_set: HashSet<&'static str> = declared.iter().map(|rlc| rlc.class).collect();

    // Nice-to-have (non-fatal): a policy nobody uses.
    for &class in &registered {
        if !declared_set.contains(class) {
            tracing::warn!(%class, "rate-limit policy registered for a class no controller declares");
        }
    }

    // Crucial (fatal): a declared class with no policy → silently unlimited.
    let mut missing: Vec<String> = declared
        .iter()
        .filter(|rlc| !registered.contains(rlc.class))
        .map(|rlc| {
            format!(
                "  - controller at `{}` declares rate_limit = {:?}",
                rlc.mount, rlc.class
            )
        })
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        missing.sort();
        Err(format!(
            "rate-limit class(es) declared on a controller but never registered with a policy — \
             a typo here would silently disable limiting:\n{}\nRegister each via \
             `RateLimit::class(...)`, or drop the `rate_limit` attribute.",
            missing.join("\n"),
        ))
    }
}

// ============================================================================
// Pattern: route families — floor coverage at boot + a declaration-keyed gate
// ============================================================================
//
// The controllers declare a *floor* (`#[controller(expects = "…")]` — the
// least-privileged caller they accept); this application owns what the floors
// mean and which floors each mount-prefix family accepts. Two pieces:
//
// - `family_coverage` runs once at boot over `Router::mounts()` — the
//   absence-inclusive inventory — and fails fast on the three ways a
//   controller can be wrong about its family: declaring nothing, declaring a
//   floor the family doesn't accept, or declaring `"credential"` without a
//   `prepare` hook to resolve one.
// - `FloorGate` enforces the `"credential"` floor per request, keyed to the
//   *declaration* rather than the path: it asks the framework's own matcher
//   (`server.router()` + `match_controller`) which controller the request
//   will reach and refuses it up front if the controller expects a
//   credential and none was presented. No path allow-list anywhere — the
//   carve-outs are the `"anonymous"` declarations on the controllers
//   themselves, so a controller mounted somewhere new is covered the moment
//   it is mounted.

/// Which floors each route-family (top-level mount prefix) accepts. A mount
/// whose first segment is in no family — `health`, a root catch-all — is
/// unconstrained.
const FAMILIES: &[(&str, &[&str])] = &[
    // `"anonymous"` is the deliberate carve-out lane: reads-open controllers
    // like `TasksController` declare it and gate writes per-handler.
    ("api", &["credential", "anonymous"]),
];

/// Startup coverage check: every controller mounted under a family prefix
/// must declare a floor the family accepts — and a `"credential"` floor must
/// come with a `prepare` hook, because the hook is what resolves the
/// credential the floor promises to require. Run once at boot (and via
/// `--check`); one router walk, no per-request cost.
///
/// This is the check `rate_limit_coverage` cannot be the template for:
/// there, an undeclared controller is legitimately unlimited and is skipped;
/// here, the *omission is the failure being hunted*, which is why this walks
/// `Router::mounts()` (a row per mounted controller, absences included).
fn family_coverage(router: &Router) -> Result<(), String> {
    let mut bad = Vec::new();
    for m in router.mounts() {
        let family = m.mount.split('/').next().unwrap_or("");
        // Not under a declared family → unconstrained (`health`, `*`, …).
        let Some((_, floors)) = FAMILIES.iter().find(|(f, _)| *f == family) else {
            continue;
        };
        match m.expects {
            None => bad.push(format!(
                "  - {} at `{}` declares no caller expectation (add `expects = …` \
                 to its #[controller] attribute)",
                m.controller, m.mount
            )),
            Some(e) if !floors.contains(&e) => bad.push(format!(
                "  - {} at `{}` declares {:?}, which family `{}` does not accept \
                 (accepted: {:?})",
                m.controller, m.mount, e, family, floors
            )),
            Some("credential") if m.prepare.is_none() => bad.push(format!(
                "  - {} at `{}` expects a credential but has no `prepare` hook \
                 to resolve one",
                m.controller, m.mount
            )),
            _ => {}
        }
    }
    if bad.is_empty() {
        Ok(())
    } else {
        bad.sort();
        Err(format!(
            "route-family coverage failed — a controller must state what it \
             expects of its callers before it can join a family:\n{}",
            bad.join("\n"),
        ))
    }
}

/// Declaration-keyed floor gate: refuses a request to a `"credential"`-floor
/// controller when no credential was presented at all, before the handler
/// (or its `prepare` hook) runs. Presence-only by design — *validating* a
/// credential needs the store and stays in the hook; a bad token passes this
/// gate and is refused there. Defense in depth, not the auth system.
struct FloorGate {
    /// The route tree the server serves ([`Server::router`]) — so the gate
    /// uses the framework's own longest-prefix matcher instead of
    /// re-deriving it.
    router: Arc<Router>,
}

#[async_trait]
impl Middleware for FloorGate {
    async fn before(&self, request: &mut Request) -> Result<Outcome, WebError> {
        let Some(rm) = self.router.match_controller(&request.path_parts) else {
            return Ok(Outcome::Continue); // no route → the 404 path handles it
        };
        if rm.controller.actus_expects() == Some("credential")
            && !request.headers.contains_key("authorization")
        {
            // Self-describing 401 so a client (and the integration test) can
            // tell the gate's refusal from a handler's: the floor was
            // "credential" and nothing at all was presented.
            return Err(WebError::Problem(
                ProblemDetails::new(StatusCode::UNAUTHORIZED, "Credential Required").detail(
                    "this endpoint's caller floor is \"credential\"; send an \
                     Authorization header",
                ),
            ));
        }
        Ok(Outcome::Continue)
    }
}

// ============================================================================
// Pattern: typed JSON body with informative 400
// ============================================================================
//
// Each request body has its own `#[derive(Deserialize)]` struct. The helper
// turns the framework-extracted `JsonValue` into the typed struct, mapping a
// deserialize error into a `BadRequest` whose detail tells the client what
// went wrong. The handler is then back to plain `?`-propagation.

#[derive(Deserialize)]
struct CreateTaskRequest {
    title: String,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Deserialize)]
struct UpdateTaskRequest {
    title: Option<String>,
    tags: Option<Vec<String>>,
    done: Option<bool>,
}

fn typed_body<T: serde::de::DeserializeOwned>(
    data: serde_json::Value,
    name: &str,
) -> Result<T, WebError> {
    serde_json::from_value(data)
        .map_err(|e| WebError::BadRequest(format!("invalid {name} body: {e}")))
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

struct MeController;

// `expects = "credential"` is this controller's *floor*: its one route
// requires a resolved user, so refusing bare-anonymous callers up front (see
// `FloorGate`) loses nothing and catches misconfiguration early. The hook
// stays `lax_auth` — the floor is a declaration, the hook is the resolver.
#[controller(prepare = Self::auth, rate_limit = "auth", expects = "credential")]
impl MeController {
    routes! {
        GET "" => me(params: &Params),
    }

    async fn auth(
        &self,
        _route: &RouteDef,
        params: &mut Params,
    ) -> Result<Option<ReplyData>, WebError> {
        lax_auth(params).await
    }

    /// Returns the resolved user. 401 for anonymous (the `require_user`
    /// extension makes that one line).
    pub async fn me(&self, params: &Params) -> Reply {
        let user = params.require_user()?;
        reply!(user)
    }
}

struct TasksController {
    store: Storage,
}

// `max_body_bytes = 4 * KIB` caps the buffered body at 4 KiB for every route on
// this controller. Task create/update bodies are JSON of ~100-500 bytes;
// anything bigger is almost certainly misuse, and capping at 4 KiB makes
// the framework reject it with a 413 before allocating. The framework
// resolves: per-controller cap → `Server::with_max_body_bytes` → default
// (2 MiB). When this controller wants a route that takes bigger bodies
// (e.g. an `attach` endpoint), the cleanest pattern is to split it into
// a sibling controller with a higher cap, mounted at a sibling path —
// see the README §"Body caps" for the pattern.
// `expects = "anonymous"` — the floor is the *weakest* route's requirement:
// GET list/get are open to anonymous callers, so `"anonymous"` is the honest
// label even though create/update/delete demand roles in their handlers. A
// mixed controller declares its floor; the stricter routes keep their
// per-handler checks (README §"Route families").
#[controller(prepare = Self::auth, max_body_bytes = 4 * KIB, rate_limit = "tasks", expects = "anonymous")]
impl TasksController {
    routes! {
        GET ""        => list(page: u32 = 1, limit: u32 = 20, tags: Vec<String>),
        GET "{id}"    => get(id: u64),
        POST ""       => create(params: &Params, data: JsonValue),
        PUT "{id}"    => update(params: &Params, id: u64, data: JsonValue),
        DELETE "{id}" => delete(params: &Params, id: u64),
    }

    async fn auth(
        &self,
        _route: &RouteDef,
        params: &mut Params,
    ) -> Result<Option<ReplyData>, WebError> {
        lax_auth(params).await
    }

    pub async fn list(&self, page: u32, limit: u32, tags: Vec<String>) -> Reply {
        let tasks = self.store.list(&tags, page, limit);
        reply!(json!({ "page": page, "limit": limit, "tasks": tasks }))
    }

    pub async fn get(&self, id: u64) -> Reply {
        let task = self.store.get(id).web()?; // domain Result → `?`-friendly Result<_, WebError>
        reply!(task)
    }

    /// Member or above. Demonstrates: `require_role` for the auth check,
    /// `typed_body` for the deserialize-with-informative-400, `web()?` for
    /// the domain-error → HTTP-error mapping.
    pub async fn create(&self, params: &Params, data: JsonValue) -> Reply {
        let _user = params.require_role(Role::Member)?;
        let req: CreateTaskRequest = typed_body(data, "create-task")?;
        let task = self.store.create(req).web()?;
        reply!(status = StatusCode::CREATED, task)
    }

    /// Editor or above.
    pub async fn update(&self, params: &Params, id: u64, data: JsonValue) -> Reply {
        let _user = params.require_role(Role::Editor)?;
        let req: UpdateTaskRequest = typed_body(data, "update-task")?;
        let task = self.store.update(id, req).web()?;
        reply!(task)
    }

    /// Admin only — the `require_role` rejects with a structured Forbidden
    /// that includes which role was required.
    pub async fn delete(&self, params: &Params, id: u64) -> Reply {
        let _user = params.require_role(Role::Admin)?;
        self.store.delete(id).web()?;
        reply!() // 204 No Content
    }
}

// ============================================================================
// Wiring
// ============================================================================

app_routes! {
    deps(store: Storage) {}
    routes {
        "health"    => HealthController,
        "api/me"    => MeController,
        "api/tasks" => TasksController { store },
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

    // `--port N` lets the integration tests spawn this binary on an
    // ephemeral port via the daemon-guard pattern (see `tests/`). `--check`
    // runs the rate-limit coverage check and exits (a CI gate — validate the
    // config without binding a port).
    let mut port: u16 = 3001;
    let mut check_only = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                port = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--port needs a value"))?
                    .parse()
                    .map_err(|e| anyhow::anyhow!("invalid --port: {e}"))?;
            }
            "--check" => check_only = true,
            other => anyhow::bail!("unknown arg: {other}"),
        }
    }

    let store = Storage::new();
    let router = init(store).await?;

    // Per-class limits. `"auth"` (MeController) is tight: 10 burst, 1 token
    // every 5 s. `"tasks"` (TasksController) is looser: 30 burst, 1 every 2 s.
    // HealthController declares no class, so `/health` is never limited —
    // exactly what you want for liveness probes. Generous for a demo; tighten
    // for prod, and back the buckets with Redis for a multi-instance fleet.
    let limiter = RateLimit::new()
        .class("auth", 10, 0.2)
        .class("tasks", 30, 0.5);

    // Fail fast if a controller declares a class with no policy — a typo must
    // not silently disable limiting. Runs at every startup (and standalone via
    // `--check`); checked *before* `limiter` is moved into the server.
    if let Err(msg) = rate_limit_coverage(&router, &limiter) {
        anyhow::bail!("{msg}");
    }
    // Same fail-fast shape for the route-family floors: every controller
    // under `api/` must declare one the family accepts.
    if let Err(msg) = family_coverage(&router) {
        anyhow::bail!("{msg}");
    }
    if check_only {
        println!("rate-limit class coverage OK");
        println!("route-family coverage OK");
        return Ok(());
    }

    let server = Server::new(router);
    // The gate reads declarations off the route tree the server serves —
    // `router()` shares it, so there's exactly one matcher and one tree.
    let floor_gate = FloorGate {
        router: server.router(),
    };
    server
        .with_middleware(RequestLogger)
        .with_middleware(limiter)
        .with_middleware(floor_gate)
        .with_cors(CorsLayer::permissive())
        .with_request_timeout(Duration::from_secs(10))
        .run(port)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn coverage_flags_a_declared_class_with_no_policy() {
        // MeController declares "auth", TasksController declares "tasks".
        // Register only "tasks" → "auth" is uncovered → the check must fail
        // and name the offending class.
        let router = init(Storage::new()).await.expect("init");
        let limiter = RateLimit::new().class("tasks", 30, 0.5);
        let err = rate_limit_coverage(&router, &limiter)
            .expect_err("an unregistered declared class must fail the check");
        assert!(
            err.contains("auth"),
            "error names the offending class: {err}"
        );
    }

    #[tokio::test]
    async fn coverage_passes_when_every_declared_class_has_a_policy() {
        // The real configuration from `main()` must pass — otherwise every
        // daemon-guard integration test would fail to boot.
        let router = init(Storage::new()).await.expect("init");
        let limiter = RateLimit::new()
            .class("auth", 10, 0.2)
            .class("tasks", 30, 0.5);
        assert!(rate_limit_coverage(&router, &limiter).is_ok());
    }

    #[tokio::test]
    async fn family_coverage_passes_for_the_real_routes() {
        // MeController declares "credential" (with a hook), TasksController
        // declares "anonymous", HealthController is outside every family.
        let router = init(Storage::new()).await.expect("init");
        assert!(family_coverage(&router).is_ok());
    }

    // The three ways a controller can be wrong about its family, as
    // macro-generated controllers (not hand impls) so the attribute →
    // `Router::mounts()` path is what's under test.

    struct NoFloor;
    #[controller]
    impl NoFloor {
        routes! { "" => idx() }
        async fn idx(&self) -> Reply {
            reply!()
        }
    }

    struct WrongFloor;
    #[controller(expects = "signature")]
    impl WrongFloor {
        routes! { "" => idx() }
        async fn idx(&self) -> Reply {
            reply!()
        }
    }

    struct HooklessCredential;
    #[controller(expects = "credential")]
    impl HooklessCredential {
        routes! { "" => idx() }
        async fn idx(&self) -> Reply {
            reply!()
        }
    }

    #[test]
    fn family_coverage_flags_omission_wrong_floor_and_hookless_credential() {
        let router = RouterBuilder::new()
            .add_route("api/nofloor", Arc::new(NoFloor))
            .add_route("api/wrong", Arc::new(WrongFloor))
            .add_route("api/hookless", Arc::new(HooklessCredential))
            // Outside the family → unconstrained, must NOT be flagged even
            // though it declares nothing.
            .add_route("health", Arc::new(NoFloor))
            .build();
        let err = family_coverage(&router).expect_err("three violations must fail the check");
        assert!(
            err.contains("NoFloor") && err.contains("api/nofloor"),
            "names the silent controller: {err}"
        );
        assert!(
            err.contains("\"signature\"") && err.contains("api/wrong"),
            "names the unaccepted floor: {err}"
        );
        assert!(
            err.contains("no `prepare` hook") && err.contains("api/hookless"),
            "names the hookless credential floor: {err}"
        );
        assert!(
            !err.contains("`health`"),
            "a mount outside every family is unconstrained: {err}"
        );
    }

    #[test]
    fn mounts_inventory_reflects_the_attributes() {
        // The macro → trait → inventory path end-to-end: `#[controller(...)]`
        // declarations come back out of `Router::mounts()`, and the
        // undeclared controller is a row (with `None`s), not a skip.
        let router = RouterBuilder::new()
            .add_route("api/hookless", Arc::new(HooklessCredential))
            .add_route("api/nofloor", Arc::new(NoFloor))
            .build();
        let rows = router.mounts();
        assert_eq!(rows.len(), 2, "one row per mounted controller");
        assert_eq!(rows[0].controller, "HooklessCredential");
        assert_eq!(rows[0].expects, Some("credential"));
        assert_eq!(rows[0].prepare, None);
        assert_eq!(rows[1].controller, "NoFloor");
        assert_eq!(rows[1].expects, None);
    }

    #[tokio::test]
    async fn me_controller_declares_its_hook_and_floor() {
        // `prepare = Self::auth` surfaces as `actus_prepare()` — presence is
        // the payload; the string is the written path, for route dumps.
        let router = init(Storage::new()).await.expect("init");
        let me = router
            .mounts()
            .into_iter()
            .find(|m| m.mount == "api/me")
            .expect("api/me is mounted");
        assert_eq!(me.expects, Some("credential"));
        assert_eq!(me.prepare, Some("Self::auth"));
        assert_eq!(me.rate_limit_class, Some("auth"));
    }
}
