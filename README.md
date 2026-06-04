# Actus

[![crates.io](https://img.shields.io/crates/v/actus.svg)](https://crates.io/crates/actus)
[![docs.rs](https://img.shields.io/docsrs/actus)](https://docs.rs/actus)
[![CI](https://github.com/uniweb/actus/actions/workflows/ci.yml/badge.svg)](https://github.com/uniweb/actus/actions/workflows/ci.yml)

> The pragmatic web framework for Rust. Auditable controllers, persistent services, real HTTP — out of the box.

Actus is a Rust web framework that gives you a clear two-tier structure for your application — a top-level routing blueprint and self-contained controllers — while letting you mix REST, RPC-style actions, and legacy URL migrations within the same codebase. It's built directly on [Hyper](https://hyper.rs/) and [Tokio](https://tokio.rs/); there's no separate server to run it on.

## Philosophy

Most Rust web frameworks are either unopinionated (you invent structure) or rigidly opinionated (you bend to their paradigm). Actus picks a different middle:

1. **A clear hierarchy.** Your application's URL layout is declared once, in one place, in `app_routes! { ... }`. Anyone reading the file can see the entire backend at a glance.
2. **A clear unit of code.** Each controller owns a URL prefix and declares its routes, access levels, and parameters in a single `routes! { ... }` block. The API surface for that prefix is auditable in one place.
3. **Pragmatism inside that structure.** Within a controller, you can use REST verbs (`GET`/`POST`/`PUT`/`DELETE`), RPC-style action names, path parameters (`{id}`), or migrate legacy URLs (`login.php`) — whatever the situation calls for.

The result is a framework where reviewers can answer "what endpoints exist, what they require, and who can call them" by reading two macros — without grepping for attribute decorators across many files.

## Design principles

A handful of principles shape how Actus is built.

**Two kinds of cross-cutting concerns get two shapes.** CORS, body limits, compression — concerns the *server* does, with positions in the request lifecycle dictated by HTTP — are named `Server::with_X(...)` methods. Logging, auth gates, request IDs, maintenance mode, caching — concerns the *application* applies, in an order it owns — go through `Middleware`. The two are different, and the framework treats them differently; you don't pick the ordering of CORS in a stack.

**Auditability over uniformity.** A reviewer should be able to answer "what does this server do?" and "what endpoints exist?" by reading a small, well-known set of places — the `app_routes!` block, the `routes!` blocks, and the `Server::new(...)...` chain. When a named, discoverable API and a uniform one are in tension, we pick discoverable.

**Explicit over magic.** No DI container, no extractors that reach into thin air. The `app_routes!` deps block is constructor injection; route patterns are declared, not discovered; a controller's struct names the services it needs.

**HTTP correctness out of the box.** You shouldn't need to know that compression goes on the outside, or that the body limit gates the body parse — that's framework knowledge, not application knowledge.

**Pragmatic shapes inside a clear structure.** REST verbs, RPC action names (`/charge`, `/refund`), path parameters, legacy URLs (`login.php`) all coexist in the same `routes!` block. The structure is the hierarchy and the macros; the URL shape is the application's call.

**The application owns its policy.** Actus ships no roles, no `Access` enum, no built-in RBAC. Authorization belongs in the application's policy layer, called from the `prepare` hook or the handler.

## Quick start

Add Actus to your project:

```sh
cargo add actus tokio --features tokio/full
```

```toml
# Cargo.toml
[dependencies]
actus = "1.0"
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

Optional features — all off by default: `compression` (gzip/brotli responses), `websocket` (`ws::upgrade`), `openapi` (OpenAPI 3.x generation). Enable with `cargo add actus --features compression,websocket,openapi`.

```rust
use std::sync::Arc;
use actus::prelude::*;
use serde_json::{json, Value as JsonValue};

// 1. A persistent service.
#[derive(Clone)]
struct Database { /* ... */ }

impl Database {
    async fn connect() -> Result<Self, std::io::Error> { Ok(Self { /* ... */ }) }
}

// 2. A controller that owns a URL prefix.
struct UserController { db: Database }

#[controller(prepare = Self::check_auth)]
impl UserController {
    routes! {
        GET ""        => list(page: u32 = 1, limit: u32 = 10),
        GET "{id}"    => get(id: u64),
        POST ""       => create(params: &Params, data: JsonValue),
        DELETE "{id}" => delete(params: &Params, id: u64),
    }

    async fn check_auth(&self, _route: &RouteDef, params: &mut Params)
        -> Result<Option<ReplyData>, WebError>
    {
        // Resolve a User if a token is present. Anonymous requests pass
        // through; individual handlers decide whether they require a user
        // and what role they need.
        if let Some(token) = params.bearer_token() {
            let user = self.auth.resolve(token).ok_or(WebError::Unauthorized)?;
            params.insert(user);
        }
        Ok(None)
    }

    pub async fn list(&self, page: u32, limit: u32) -> Reply { /* ... */ }
    pub async fn get(&self, id: u64) -> Reply { /* ... */ }

    pub async fn create(&self, params: &Params, _data: JsonValue) -> Reply {
        let _user = params.get::<User>().ok_or(WebError::Unauthorized)?;
        // ... write ...
        reply!()
    }

    pub async fn delete(&self, params: &Params, _id: u64) -> Reply {
        let user = params.get::<User>().ok_or(WebError::Unauthorized)?;
        if !user.is_admin { return Err(WebError::Forbidden); }
        // ... delete ...
        reply!()
    }
}

// 3. The application's URL blueprint.
app_routes! {
    deps {
        db = Database::connect().await?,
    }
    routes {
        "api/users" => UserController { db },
    }
}

// 4. Run a real HTTP server.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    Server::new(init().await?).run(3000).await?;
    Ok(())
}
```

The end-to-end runnable version of this lives at `examples/basic/`. For the application-side patterns (auth, typed bodies, domain errors, rate limiting, integration tests) in working code, see `examples/advanced/`.

## Core architecture

### Two macros, two layers

**`app_routes!`** — your application's blueprint. Declares the dependencies your controllers need (DB pools, caches, auth services) and assigns each one to whichever controllers want it. Generates an `async fn init(<inputs>) -> Result<Router, _>` that you call from `main`.

The `deps` block has two parts: an optional `(name: Type, …)` parameter list of *inputs* passed in from the caller, and a brace block of *let-bindings* constructed inside `init()`:

```rust
app_routes! {
    deps(store: Arc<Store>) {        // inputs from main()
        cache = Cache::redis(&url).await?,   // constructed inside init()
    }
    routes {
        "api/entities" => EntityController { store },
        "api/cache"    => CacheController { cache },
    }
}

// In main():
let store = Arc::new(Store::connect(&url).await?);
let router = init(store).await?;
```

The input form is what you want when the same value is needed both in `init()` and elsewhere in `main()` (e.g., for CLI subcommands that share the connection). All four shapes are valid: `deps { … }`, `deps(a: T) { … }`, `deps(a: T) {}`, or no `deps` block at all.

```rust
app_routes! {
    deps {
        db    = Database::connect(&url).await?,
        cache = Cache::redis(&url).await?,
    }
    routes {
        "api/users" => UserController { db, cache },
        "api/admin" => AdminController { db },
        "health"    => HealthController,
        "*"         => SpaController,
    }
}
```

Inside a route's controller construction, the macro auto-clones references to bound names so multiple controllers can share the same `Arc`-wrapped service without you writing `.clone()` everywhere. Three cases get auto-cloned — all gated on the right-hand side being a bare unqualified identifier:

- **Shorthand** — `Foo { db }` → `Foo { db: db.clone() }`.
- **Explicit form with a bare ident on the right** — `Foo { svc: store }` → `Foo { svc: store.clone() }`. Useful when the controller's field name differs from the dep name.
- **Rest-spread with a bare ident** — `Foo { ..base }` → `Foo { ..(base).clone() }`.

Anything else passes through unchanged. Method calls (`Foo { svc: store.identity() }`), function calls (`Foo::new(store)`), qualified paths (`Foo { svc: my_mod::STORE }`), and explicit `.clone()` calls (`Foo { ..base.clone() }`) all behave exactly as written — no double-clone, and no accidental clone on something that isn't `Clone`.

**`routes!`** — a controller's API surface. Declares every endpoint that controller serves, with optional HTTP verb, path/query parameters, and a target handler:

```rust
routes! {
    GET "search" => search(q: String, page: u32 = 1, limit: u32 = 20),
    POST "items" => create_item(data: JsonValue),
}
```

### Longest-prefix routing at any depth

When a request comes in, Actus walks the route tree segment by segment and dispatches to the **deepest** controller-bearing prefix. The remaining path becomes the controller's "action," which is matched against its `routes!` patterns.

A request to `GET /api/users/42`:

1. Router walks `api` → `users` and finds `UserController` registered there.
2. Hands `UserController` the action `"42"`.
3. Controller matches `"42"` against `"{id}"`, captures `id=42`, dispatches to `get(id: u64)`.

Patterns can be multi-segment (e.g. `"posts/{id}/comments"`); the controller sees the full remaining path.

Within one controller, routes are tried in **declaration order** — the first pattern that matches the action (and whose verb matches the request) wins. So a literal `"special"` route declared *after* a `"{id}"` route is unreachable, and `GET /…/special` would match `{id}` instead (then 400 if `id` is typed `u64`). List the more specific patterns first.

Because a mounted controller receives *all* of the unconsumed path as its action, a mount is already a catch-all for everything below it: `"foo" => FooController` handles `/foo`, `/foo/x`, `/foo/x/y`, … (with actions `""`, `"x"`, `"x/y"`) unless a deeper mount claims part of that subtree. A **trailing `*` segment is optional sugar** for this — `"foo/*"` routes identically to `"foo"`, and `"*"` identically to `""` (mount at the root). The `*` exists purely so a reader of `app_routes!` can see "this is the catch-all here" at a glance; it doesn't change behavior, and in particular `"foo/*"` still serves the bare `/foo`. (A `*` anywhere but the last segment is meaningless — that route is dropped with a warning.)

A trailing `{...name}` token is a **rest parameter** — it captures the *entire remaining path* (slashes included) as a single `String`, matching zero or more segments:

```rust
// FolderController, mounted at "api/folder"
routes! {
    GET "{folder_id}/{...path}" => read(folder_id: String, path: String),
    PUT "{folder_id}/{...path}" => write(folder_id: String, path: String, data: JsonValue),
}
// GET /api/folder/abc-uuid/notes/2026/q2.md
//   → folder_id = "abc-uuid", path = "notes/2026/q2.md"
// GET /api/folder/abc-uuid
//   → folder_id = "abc-uuid", path = ""           (zero trailing segments)
```

This is the REST-shaped way to express "a path under a resource" — the alternative is to stuff the sub-path into a query parameter (`GET /api/folder/abc-uuid?path=notes/2026/q2.md`, with `path` declared as an ordinary `String` query arg). Both work; `{...path}` keeps the hierarchy in the URL where it belongs.

Rules (enforced at macro-expansion time): `{...name}` must be the **last** `/`-segment of the pattern, may appear **at most once**, and the bound parameter must be typed `String`. It differs from a `*` segment in granularity: `*` selects a *controller* and bypasses that controller's `routes!`, verbs, and `prepare` hook; `{...name}` is an ordinary route inside a controller and goes through all of it. The required parts come first: in `"{folder_id}/{...path}"`, `folder_id` must have a segment — `GET /api/folder` (nothing after the prefix) does **not** match it, and falls through (→ 404 unless another route catches it). Only the `{...path}` token is allowed to match nothing. If you want the bare collection URL to do something, give it its own route: `GET "" => list_folders(...)`. When a request *could* match both `"{id}"` and `"{id}/{...path}"` (one segment trails the prefix), the route declared **first** wins — list the more specific one first if it matters.

### Persistent services, injected once

Services (`Database`, `Cache`, `AuthService`, etc.) are constructed once at startup, wrapped in `Arc<...>`, and shared between controllers. Controllers themselves are `Arc<dyn Controller>` — one instance per route, alive for the server's lifetime. Requests don't trigger fresh allocations of either.

This is materially different from frameworks that reconstruct context per request, and from frameworks that hide DI behind extractors. Actus's contract is explicit: your controller's struct is what services it needs.

## Parameter extraction

Parameters are extracted automatically from URL path segments, query strings, and JSON bodies, based on handler signatures.

| Type          | Source             | Example                  | Behaviour                                      |
| ------------- | ------------------ | ------------------------ | ---------------------------------------------- |
| `String`      | query              | `q: String`              | required; 400 if missing                       |
| `i64` / `u64` | query or path      | `id: u64`                | required; 400 if missing or not parseable      |
| `u32`         | query or path      | `page: u32 = 1`          | optional with default                          |
| `f64`         | query              | `score: f64`             | required floating point                        |
| `bool`        | query              | `verbose: bool = false`  | optional with default                          |
| `Vec<String>` | query              | `tags: Vec<String>`      | all values of a repeated key (`?tags=a&tags=b` → `["a", "b"]`; `[]` if absent) |
| `JsonValue`   | request body       | `data: JsonValue`        | parsed `serde_json::Value`                     |

Path parameters use `{name}` syntax in the route pattern and are always required. A trailing `{...name}` is a *rest* path parameter — typed `String`, capturing the joined remainder of the path (zero or more segments; `""` when nothing trails). Query parameters declared with a default are optional.

Query parameters are a **multimap**: repeated keys (`?tags=a&tags=b`) accumulate in request order. A scalar parameter (`String`, `u64`, `bool`, …) reads the *first* value; a `Vec<String>` parameter reads *all* of them, so a one-element list (`?tags=a`) and a many-element one flow through the same path. `application/x-www-form-urlencoded` body fields are folded into this same map (appended, not overwritten — a form field shares a name space with the query string). Comma-separated values in a single key (`?tags=a,b`) are *not* split — that's one value, `"a,b"`.

A path `{id}` and a query `?id=…` can't both bind to a handler (one Rust parameter, one source): the path capture wins, and a stray same-named query param counts as undeclared — so strict mode 400s it and lax mode ignores it.

For *open-ended* query parameters — a search endpoint with arbitrary filters, a request proxy — declare `params: &Params`, mark the controller `#[controller(lax)]`, and read `params.query()` (the whole `HashMap<String, Vec<String>>`). Handlers that know their parameter names up front should declare them as typed args (raw-identifier-named if need be); this is the escape hatch, not the default.

When declaring a parameter whose name is a Rust keyword, you must uses a **raw identifier** only in the code. For example, `r#type: Vec<String>` binds the `type` query key, `r#move: String` the `move` key, etc. The `r#` is just how you write a keyword as an identifier; it isn't part of the wire name. So a `?type=` filter doesn't need any special treatment in its declaration.

## Authentication and authorization

**Actus is policy-agnostic.** It provides a `prepare` hook that runs before every handler — what that hook does is up to you. There is no built-in `Access` enum or per-route access tag; authorization belongs in your application's policy layer (e.g., a `services::policy` module that knows about your domain's roles, ownership, and grants).

The typical pattern is a two-step hook:

```rust
#[controller(prepare = Self::check_auth)]
impl UserController {
    async fn check_auth(&self, _route: &RouteDef, params: &mut Params)
        -> Result<Option<ReplyData>, WebError>
    {
        // Resolve a User if a token is present. Anonymous requests pass
        // through; individual handlers decide whether they require a user
        // and what role / permission they need.
        if let Some(token) = params.bearer_token() {
            let user = self.auth.resolve(token).ok_or(WebError::Unauthorized)?;
            params.insert(user);
        }
        Ok(None)
    }

    routes! { /* ... */ }
}
```

The hook receives the matched route and a mutable reference to `Params` (so it can both *read* headers / body / undeclared query params and *attach* per-request state via `params.insert(...)`). Three return shapes:

- `Ok(None)` continues to the handler.
- `Ok(Some(reply))` short-circuits with that reply (any status the hook chooses) — useful for redirects, custom 401 bodies, or feature-flag gating.
- `Err(WebError::*)` short-circuits with the corresponding error response (`401 Unauthorized`, `403 Forbidden`, etc.).

Per-handler authorization decisions live in the handlers themselves — they know what they're doing and what permissions it requires:

```rust
pub async fn delete(&self, params: &Params, id: u64) -> Reply {
    let user = params.get::<User>().ok_or(WebError::Unauthorized)?;
    if !user.is_admin { return Err(WebError::Forbidden); }
    // ... delete ...
    reply!()
}
```

This pushes resource-aware decisions ("can *this* user delete *this* entity") to the layer that has the resource in hand. For framework-level "did the user authenticate at all" decisions, the prepare hook is the right place.

`Params` exposes `header(name)` for case-insensitive header lookup (returns the *first* value if the header appears more than once — see `header_all(name)` for every value, useful for `Forwarded` / `Via` / chained `X-Forwarded-*` from a proxy chain), `bearer_token()` for the common `Authorization: Bearer ...` pattern, and a typed extensions slot via `insert<T>()` / `get::<T>()`.

**Carrying state from `prepare` to the handler:** declare `params: &Params` as a handler parameter and the macro passes the request `Params` through. Handlers use `params.get::<T>()` to read what `prepare` stashed:

```rust
routes! {
    POST "" => create(params: &Params, data: JsonValue),
}

pub async fn create(&self, params: &Params, data: JsonValue) -> Reply {
    let user = params.get::<User>().expect("auth runs first");
    reply!(json!({ "created_by": user.name, "data": data }))
}
```

## CORS

Hand a `CorsLayer` to `Server::with_cors(...)` and Actus handles CORS itself — no reverse proxy required. The server answers preflight `OPTIONS` requests (a `204` with the negotiated `Access-Control-*` headers, before middleware or routing — a preflight isn't an application request, so neither `before` nor `after` middleware runs on it), and adds the CORS headers to every cross-origin response — success *and* error, so the browser can read 4xx/5xx bodies.

```rust
use std::time::Duration;

// Development: anything goes.
Server::new(router).with_cors(CorsLayer::permissive());

// Production: pin it down.
Server::new(router).with_cors(
    CorsLayer::new()
        .allow_origin("https://app.example.com")
        .allow_methods([Verb::GET, Verb::POST, Verb::DELETE])
        .allow_headers([http::header::CONTENT_TYPE, http::header::AUTHORIZATION])
        .allow_credentials(true)
        .max_age(Duration::from_secs(3600)),
);
```

The response always echoes the concrete `Origin` (never the literal `*`), so the policy stays valid alongside `allow_credentials(true)`; `Vary: Origin` is appended (an existing `Vary` isn't clobbered). A request with no `Origin`, or an `Origin` that isn't on the allow-list, gets no CORS headers — the browser then blocks it.

## Compression

`Server::with_compression(...)` gzip/brotli-encodes responses — no reverse proxy needed. For each response Actus picks an encoding from the request's `Accept-Encoding` (parsed per RFC 7231 §5.3.4: q-values, `*` wildcard, `q=0` explicit disallow — the highest non-zero `q` wins, ties go to `prefer_brotli`), and if the body is a buffered, compressible type above a size threshold it compresses it, setting `Content-Encoding` and appending `Vary: Accept-Encoding`.

```rust
Server::new(router).with_compression(CompressionLayer::new());                 // gzip/brotli, ≥ 1 KiB, brotli q=4
Server::new(router).with_compression(CompressionLayer::new().min_size(256).prefer_gzip());
Server::new(router).with_compression(CompressionLayer::new().brotli_quality(1)); // faster brotli, looser ratio
```

Requires the `compression` feature — `actus = { version = "…", features = ["compression"] }` — which pulls in `flate2` + `brotli`; without it, `with_compression` / `CompressionLayer` don't exist. Scope today: buffered bodies of compressible content types — `text/*`, `application/json`, `*+json`, `*+xml`, `image/svg+xml`, `application/wasm`, … — including the `application/problem+json` error bodies; already-compressed types (images, video, zip) are skipped, as are streamed responses. No double-encoding (a handler that already set `Content-Encoding` is left alone).

**Honors `Cache-Control: no-transform`** per RFC 7234 §5.2.1.6 / RFC 9111 §5.2.2.6 — a handler that stamps `Cache-Control: no-transform` on its reply opts out of compression. The directive name is matched case-insensitively and parsed token-by-token, so `no-cache, no-transform` and `private, no-transform, max-age=0` both work. Use this for signed payloads, content-addressed responses, or anything where byte-exact transit matters.

```rust
reply::build_reply()
    .header("Cache-Control", "no-transform")
    .body(reply::bytes("application/octet-stream", signed_payload))
    .done()
```

**Brotli quality** is configurable via `CompressionLayer::quality(u32)` — `0` for fastest / loosest, `11` for slowest / tightest. The default is `4`, which is the speed/ratio sweet spot for per-request dynamic content. Quality `11` is 10-100× slower for ~5% additional savings — appropriate for pre-compressed static assets, *not* for per-request work. Values above 11 are clamped. Has no effect on the gzip path (gzip uses `flate2`'s default level).

## WebSocket

A route handler that wants to serve a WebSocket validates the request as usual (origin, auth, subprotocol — whatever it needs) and then returns `ws::upgrade(...)` instead of an ordinary reply. The server completes the handshake (`101 Switching Protocols`), upgrades the connection, and runs the closure you supplied on the resulting `WebSocket` — a `Stream` of incoming `Message`s and a `Sink` for outgoing ones (re-exported from `tungstenite`).

```rust
use actus::prelude::*;            // brings in `ws`, `Message`, `WebSocket`
use futures_util::{SinkExt, StreamExt};

#[controller]
impl Live {
    routes! { GET "echo" => echo() }

    pub async fn echo(&self) -> Reply {
        // (check `_params.header("origin")` / an auth token here if you need to)
        Ok(ws::upgrade(|mut socket| async move {
            while let Some(Ok(msg)) = socket.next().await {
                if msg.is_text() || msg.is_binary() {
                    if socket.send(msg).await.is_err() { break; }
                }
            }
        }))
    }
}
// app_routes!:  "live" => Live,   → GET /live/echo upgrades to a WebSocket
```

Mount it like any other route. If the request reaching such a handler isn't actually a WebSocket handshake, the server replies `426 Upgrade Required`. Requires the `websocket` feature — `actus = { version = "…", features = ["websocket"] }` — which pulls in `tokio-tungstenite`; without it, `actus::ws` doesn't exist.

## OpenAPI

`actus::openapi::generate(&router, &options, filter)` walks the built `Router` and emits an OpenAPI 3.1 document as a `serde_json::Value` — no separate route inventory, no hand-maintained YAML. The spec reflects what the `#[controller]` and `app_routes!` macros recorded.

```rust
use actus::openapi;

let router = init().await?;
let spec = openapi::generate(
    &router,
    &openapi::Options::new("My API", "1.0.0")
        .description("…")
        .server("https://api.example.com", Some("prod")),
    // Document only `/api/...`; hide internal mounts like `/health` and `/openapi.json`.
    |mount| mount.starts_with("api/"),
);
println!("{}", openapi::to_string_pretty(&spec));
```

Requires the `openapi` feature — `actus = { version = "…", features = ["openapi"] }`; without it, `actus::openapi` doesn't exist (no extra crates are pulled in, so the cost is purely the module's compilation).

**Route selection.** `filter: Fn(&str) -> bool` runs against each controller's mount path (no leading or trailing slash) — `"api/users"`, `"health"`, `""` for a root mount. A controller is included iff its mount passes. The common case is a prefix check (`|m| m.starts_with("api/")`), but anything more elaborate (regex, allow-list, deny-list) works.

**Mapping.**

| What | Becomes |
| --- | --- |
| Route pattern | OpenAPI path with the mount prefix; `{name}` passes through; `{...name}` is stripped to `{name}` plus `x-actus-rest-param: true` on the parameter |
| `verb == DEFAULT_VERBS` (`[GET, POST]`) | Two operations on the path, one per verb |
| Single-verb route | One operation |
| Path param | `parameters[in: path, required: true]` |
| Query param | `parameters[in: query, required: <no default>]`; `Vec<String>` is always optional |
| `JsonValue` body | `requestBody` with `application/json` / `{}` schema |
| `Bytes` body | `requestBody` with `application/octet-stream` / `string`+`binary` |
| `///` doc on the handler | First non-empty line → `summary`; full doc → `description` |
| `ParamType` → schema | `String`→`string`; `Int`/`U64`→`integer`/`int64` (`U64` adds `minimum: 0`); `U32`→`integer`/`int32`+`minimum:0`; `F64`→`number`; `Bool`→`boolean`; `Vec<String>`→`array`/`string`; `Json`→`{}` (any); `Bytes`→`string`/`binary` |

**OperationId**: `{sanitized_path}_{handler}_{method}` — guaranteed unique by construction.

**Limitations.** No response-body schema is inferred — `Reply` is untyped at the type level. Each operation gets a generic `default` response; if you need richer responses, post-process the returned `Value`. Trailing `{...rest}` parameters have no native OpenAPI form (path templating is segment-sized); the `x-actus-rest-param` extension marks them so tooling that wants to can recognise them.

**When to call it.** After `init()` returns the `Router`, before `Server::new(router).run(...)`. For a served `/openapi.json`, an `Arc<OnceLock<Value>>` dep — set after the spec is generated, read by a tiny controller — is the simplest shape; `examples/basic` does exactly that and also accepts `--openapi` to dump the spec to stdout for piping into Swagger UI or Redoc.

## Middleware

`Server::with_middleware(...)` registers a `Middleware` — for *application* cross-cutting concerns (logging, auth gates, request IDs, maintenance mode, caching, …). HTTP-protocol concerns (CORS, body limits, compression) are named server features and live outside the chain — see [Principles](#principles), point 1. Implement either hook (both have default no-op impls):

```rust
use actus::prelude::*;

struct StampTraceId;

#[async_trait]
impl Middleware for StampTraceId {
    async fn before(&self, _request: &mut Request) -> Result<Outcome, WebError> {
        Ok(Outcome::Continue)
        //   ^ `Outcome::Respond(reply)` to short-circuit with a normal response,
        //     or `Err(WebError::*)` to short-circuit with an error response.
    }

    async fn after(&self, request: &Request, response: &mut ReplyData) -> Result<(), WebError> {
        // `after` sees the request, so it can echo / decide from request context.
        if let Some(id) = request.headers.get("x-trace-id").and_then(|v| v.to_str().ok()) {
            response.add_header("X-Trace-Id", id);   // lifts to `Rich` if needed
        }
        Ok(())
    }
}
```

`before` runs in registration order; `after` runs in reverse, so a middleware wraps the ones added after it (`[A, B]`: `A.before`, `B.before`, handler, `B.after`, `A.after`). `before` returns `Outcome::Continue` (proceed), `Outcome::Respond(reply)` (short-circuit with a normal response — the handler and any remaining `before` hooks are skipped), or `Err(WebError)` (short-circuit with an error response).

**The `after` chain runs on every reply with a body and a request** — handler successes, `Outcome::Respond` short-circuits, *and* every error: a `before` hook's `Err`, a 400 from a malformed body, a 404 / 405 from the router, a handler-returned `Err(WebError)`, even the 413 from the body-size cap (the request skeleton is preserved even when body collection fails). A request-id stamper, a response logger, an audit hook — anything in `after` — sees them all. Compression and CORS also apply uniformly.

The exceptions:
- **WebSocket upgrade success (`101 Switching Protocols`)** — no HTTP body to decorate, and the upgrade machinery consumes the connection. (The 426 fallback when a handler returns `ws::upgrade(...)` but the request isn't a real handshake *does* run through the after-chain — it's a normal HTTP error.)
- **CORS preflight (`204`)** — HTTP-protocol traffic, not an application request (see the CORS section).
- **Pre-parse failures** — a request hyper itself can't parse never reaches the `Request` skeleton, so there's nothing to hand the hook.

`after` takes `&Request` so a hook can decide based on the request (echo a header, log with method/path, etc.). To shape the response from `after`, use `response.add_header(name, value)` and `response.set_status(code)` — both lift the `ReplyData` into `Rich` if needed, so the variant the handler returned doesn't matter.

## Body caps

The framework's first-line defense against oversized request bodies. Three levels of granularity, finer wins:

```rust
#[controller(max_body_bytes = 4 * KIB)]    // controller-wide
impl MessagesController {
    routes! {
        POST "" => send(data: JsonValue),
        // ... all routes on this controller share the 4 KiB cap ...
    }
}
```

```rust
Server::new(router)
    .with_max_body_bytes(64 * KIB)   // server-wide fallback
```

```rust
pub const DEFAULT_MAX_BODY_BYTES: usize = 2 * MIB;  // built-in default
```

`KIB` / `MIB` / `GIB` are byte-unit consts in the prelude, so the same `N * UNIT` expression reads the same in an attribute, a builder call, or anywhere else.

Resolution: per-controller cap if set → server-wide cap if set → 2 MiB default.

The framework matches the controller for a request *before* buffering its body, then reads the cap off the matched controller. A 50 MB request to a controller with `max_body_bytes = 4 * KIB` is rejected with `413 Payload Too Large` after ~zero allocation — the bytes aren't read off the wire. (Compare with putting the check inside the handler, which can only fire *after* the framework has buffered the body using whatever the server-wide cap is.)

### Mixed body sizes within one controller

Some endpoints want a smaller cap than the controller default. Some want larger. The current shape is one cap per controller; **split the routes into a dedicated controller** for the odd one out:

```rust
#[controller(max_body_bytes = 4 * KIB)]
impl MessagesController {
    routes! {
        POST ""        => send(data: JsonValue),
        GET  "{id}"    => get(id: u64),
        DELETE "{id}"  => delete(id: u64),
    }
}

// Sibling controller for the wide-body endpoint, mounted at a sibling path.
#[controller(max_body_bytes = 25 * MIB)]
impl MessageAttachmentsController {
    routes! {
        POST "{id}" => attach(id: u64, body: Bytes),
    }
}

app_routes! {
    routes {
        "api/messages"             => MessagesController { ... },
        "api/message-attachments"  => MessageAttachmentsController { ... },
    }
}
```

The URL shape changes — `POST /api/message-attachments/42` rather than the more REST-shaped `POST /api/messages/42/attach`, because Actus's app-level routing is literal-segment-only (a mount with a `{param}` in it isn't supported). If the URL shape matters for your API, a per-route cap (planned, see `docs/proposals/per-route-body-caps.md`) will let you keep the route nested while overriding just that endpoint's cap.

### What it does and doesn't solve

- **Correctness.** An endpoint that documents "accepts up to 4 KiB" actually rejects bigger bodies. The handler's deserializer doesn't have to redo the check.
- **Attack-surface narrowing.** An attacker probing your API can only fill memory up to the cap of the legitimately-large endpoints (the upload ones); the rest stay tight.
- **Not full DoS protection.** That comes from `with_max_inflight_body_bytes` (semaphore over total buffered bytes), `with_max_connections` (cap on concurrent connection tasks), and `with_header_read_timeout` (slowloris guard). Together with body caps these give a complete picture; alone, body caps just narrow the surface.

## Strict vs lax mode

By default, controllers are **strict**: requests with query parameters that no route declared are rejected with `400 Bad Request`. This catches typos and casual API misuse early. (`application/x-www-form-urlencoded` body fields count as query parameters here — they're folded into the same map — so a strict no-param handler will reject a form `POST` carrying extra fields.) Use `#[controller(lax)]` for handlers that read open-ended parameters via `params.query()`.

```rust
#[controller]              // strict by default
#[controller(strict)]      // explicit
#[controller(lax)]         // accept and ignore unknown query params
```

## HTTP verb constraints

Verbs are *constraints*, not identities. A route declared without a verb prefix accepts both `GET` and `POST` — the two methods HTML forms emit natively, and the natural baseline for "this endpoint doesn't need extra protocol restrictions." Prefixing a route with a verb tightens the constraint to that single verb.

```rust
routes! {
    GET "posts"    => list_posts(),         // GET only
    POST "posts"   => create_post(),        // POST only
    DELETE "{id}"  => delete_post(id: u64), // DELETE only
    "search"       => search(q: String),    // GET or POST
    "login.php"    => login(),              // GET or POST (legacy form handler)
}
```

`PUT`, `DELETE`, `PATCH` (semantic REST verbs) and `HEAD`, `OPTIONS` (protocol verbs) are deliberately not in the default set — they must be opted into explicitly. This prevents accidentally exposing destructive verbs on a route the author didn't think about.

When a request's path matches a route pattern but the verb doesn't, Actus returns `405 Method Not Allowed` (not 404), with an `Allow` header listing the verbs that path *does* accept (and the same list as an `allowed_methods` member in the `application/problem+json` body).

## JSON body

Declare a parameter of type `JsonValue` and Actus parses the request body for you:

```rust
routes! {
    POST "items" => create_item(data: JsonValue),
}

pub async fn create_item(&self, data: JsonValue) -> Reply {
    let name = data.get("name").and_then(|v| v.as_str()).unwrap_or("");
    // ...
    reply!(json!({ "created": true }))
}
```

## Replies

`reply!` is the macro for constructing a `Reply` (i.e., `Result<ReplyData, WebError>`). It accepts any [`Serialize`] value:

```rust
reply!(my_struct)                          // serialize as JSON
reply!(json!({"status": "ok"}))            // inline JSON literal
reply!()                                   // 204 No Content
reply!(stream: byte_stream)                // streaming body
reply!(status = StatusCode::CREATED, value)
reply!(
    status = StatusCode::CREATED,
    headers = { "Location": "/users/123" },
    new_user
)
```

Serialization is fallible: if the response type's `Serialize` impl errors (rare with derived impls; possible with custom ones), `reply!` returns `Err(WebError::Internal(…))` → 500 — *not* a panic that would drop the connection. If you need the loud-failure behavior, call `actus::prelude::json(value)` directly.

**Streaming bodies.** `reply!(stream: s)` (or `reply::stream(s)` / `ReplyData::Stream`) sends a chunked response from any `Stream<Item = Result<Bytes, io::Error>>` — the body is written out as the stream yields, not buffered. To set a content type (or other headers) on a streamed response, build it explicitly:

```rust
use actus::prelude::*;
let body = reply::build_reply()
    .header("Content-Type", "application/x-ndjson")
    .body(reply::stream(jsonl_stream))
    .done();
Ok(body)
```

**Server-Sent Events.** `reply!(sse: events)` (or `reply::sse(events)`) sends a streaming SSE response from any `Stream<Item = SseEvent>`. `Content-Type: text/event-stream` and `Cache-Control: no-cache` are set for you, and `SseEvent`'s encoder handles the wire-format details — multi-line `data` becomes one `data:` line per source line, the blank-line frame separator, embedded newlines in `event:` / `id:` stripped:

```rust
use actus::prelude::*;
use futures_util::stream;
use std::time::Duration;

pub async fn updates(&self) -> Reply {
    let events = stream::iter(vec![
        SseEvent::data("tick").id("1"),
        SseEvent::data(serde_json::to_string(&payload)?).event("update"),
        SseEvent::data("multi\nline\ndata"),     // becomes three `data:` lines
        SseEvent::comment("keep-alive"),         // heartbeat through proxies
        SseEvent::data("done").retry(Duration::from_secs(5)),
    ]);
    reply!(sse: events)
}
```

When the stream ends, the connection closes. For an open-ended stream, send an `SseEvent::comment(...)` heartbeat every 15–30 seconds — without one, idle stretches can be killed by NAT timeouts, proxy timeouts, or load-balancer idle policies. (Real-world SSE deployments are usually behind a proxy that buffers; if you see your events arrive in batches, look for `proxy_buffering off` (nginx) or equivalent.)

Errors are returned as `Err(WebError::*)`; the framework's `Finalizer` converts them into RFC 7807-style `application/problem+json` responses with the correct status code:

| `WebError`              | HTTP status |
| ----------------------- | ----------- |
| `NotFound`              | 404         |
| `MethodNotAllowed(methods)` | 405 (sets `Allow`) |
| `BadRequest(msg)`       | 400         |
| `PayloadTooLarge`       | 413         |
| `TooManyRequests(retry)` | 429 (sets `Retry-After` if `retry.is_some()`) |
| `Timeout`             | 504         |
| `Busy(retry)`         | 503 (sets `Retry-After` if `retry.is_some()`) |
| `Unauthorized`        | 401         |
| `Forbidden`           | 403         |
| `Internal(msg)`       | 500         |
| `Problem(p)`          | `p.status`  |

For structured error responses with extension members (e.g. naming the failing field, the violated rule, the required role), use `WebError::Problem(ProblemDetails)`:

```rust
return Err(WebError::Problem(
    ProblemDetails::new(StatusCode::FORBIDDEN, "Forbidden")
        .detail("admin role required to delete")
        .extra("required_role", "admin")
        .extra("actor", user.name.clone()),
));
```

Wire shape:

```json
{ "status": 403, "title": "Forbidden",
  "detail": "admin role required to delete",
  "required_role": "admin", "actor": "alice" }
```

Apps with a rich domain error type (e.g. `services::Error` carrying field/rule context) typically write one `impl From<MyError> for WebError` that produces `Problem(...)` per variant, so handlers can `?`-propagate domain errors and the framework does the rest.

## Legacy URL migration

Because `app_routes!` does literal-prefix matching, you can map legacy URLs naturally during a gradual migration:

```rust
app_routes! {
    routes {
        "api/users"           => UserController { db },
        "login.php"           => LegacyAuthController { db },
        "admin/dashboard.php" => LegacyDashboardController { db },
        "*"                   => SpaController,
    }
}
```

No regex, no rewrite rules — the legacy paths sit alongside modern routes.

## Patterns

These aren't framework features — they're shapes that came up while wiring actus into a real backend and turned out to be worth recording. Each is a few lines of glue that you write once in your own crate; subsequent controllers stay short.

### Reusable `prepare`-hook bodies

The `#[controller(prepare = …)]` macro wants `Self::method`, so each controller has its own method. But the *body* of that method is usually the same across controllers — "resolve the bearer token if present, stash a User, pass anonymous through." Factor the body into a free function and let each controller delegate from a 3-line stub:

```rust
// Once, in your binary's wiring layer:
pub async fn lax_auth(store: &MyStore, params: &mut Params)
    -> Result<Option<ReplyData>, WebError>
{
    if let Some(token) = params.bearer_token()
        && let Some(user) = my_app::auth::resolve(store, token).await?
    {
        params.insert(user);
    }
    Ok(None)
}

// Per controller:
#[controller(prepare = Self::auth)]
impl FooController {
    async fn auth(&self, _route: &RouteDef, params: &mut Params)
        -> Result<Option<ReplyData>, WebError>
    {
        crate::lax_auth(&self.store, params).await
    }
    routes! { /* … */ }
    /* handlers */
}
```

When a controller really needs a different hook (require auth on every route, or a different identity backend), it writes one — but the boilerplate is scoped to the controllers that deviate.

### Typed `Params` extensions

Handlers reading state stashed by `prepare` end up repeating the same shape:

```rust
let user = params.get::<User>().ok_or(WebError::Unauthorized)?;
```

Bundle the pattern in a small extension trait:

```rust
pub trait AuthParamsExt {
    fn require_user(&self) -> Result<&User, WebError>;
}

impl AuthParamsExt for Params {
    fn require_user(&self) -> Result<&User, WebError> {
        self.get::<User>().ok_or(WebError::Unauthorized)
    }
}

// Handler:
let user = params.require_user()?;
```

One trait per kind of stashed value (auth, request id, tenant id, …); each handler imports only the ones it cares about.

### Error mapping at the binary

`impl From<MyDomainError> for WebError` can't live in your domain crate (it doesn't depend on actus) and can't live in `actus` (it doesn't know your domain). The orphan rule pushes it into the binary that wires both. That's also the right *architectural* place: "how this domain error becomes an HTTP status" is a wiring decision, not a property of either layer.

The ergonomic shape is a `Result` extension trait so `?` works with no per-call-site `.map_err()`:

```rust
fn map_err(e: MyDomainError) -> WebError { /* match on variants → WebError::Problem(...) */ }

pub trait MyResultExt<T> {
    fn web(self) -> Result<T, WebError>;
}

impl<T> MyResultExt<T> for Result<T, MyDomainError> {
    fn web(self) -> Result<T, WebError> { self.map_err(map_err) }
}

// Handler:
let result = my_op(...).await.web()?;
```

For rich error responses, return `WebError::Problem(ProblemDetails)` with extension members (`field`, `rule`, `op`, `target`, …) — clients receive structured `application/problem+json` they can program against.

### JSON body deserialization with informative 400s

A `data: JsonValue` parameter in a route signature gives the handler the raw body. Deserialize it into a typed struct yourself so a malformed body becomes a structured `400` instead of a confusing parser error landing in `Internal`:

```rust
#[derive(Deserialize)]
struct CreateBookRequest {
    title: String,
    author_id: u64,
}

routes! {
    POST "" => create(data: JsonValue),
}

pub async fn create(&self, data: JsonValue) -> Reply {
    let req: CreateBookRequest = serde_json::from_value(data).map_err(|e| {
        WebError::BadRequest(format!("invalid create-book body: {e}"))
    })?;
    // … use req …
}
```

The same shape works for tagged enums (`#[serde(tag = "kind")]`) — let serde dispatch the discriminator. The handler's structured 400 is much more useful to clients than a generic 500.

### HTTP integration tests via a `Daemon` guard

For end-to-end tests through the real HTTP stack, spawn your binary as a subprocess on an ephemeral port. A small RAII guard cleans up so a panicking test doesn't leak a server:

```rust
pub struct Daemon { child: Child, port: u16, client: reqwest::Client }

impl Daemon {
    pub async fn spawn() -> Self {
        // bind 127.0.0.1:0 → take the OS-assigned port → drop the listener
        let port = pick_ephemeral_port();
        let child = Command::new(env!("CARGO_BIN_EXE_yourbin"))
            .args(["serve", "--port", &port.to_string()])
            .spawn().expect("spawn");
        wait_until_live(port).await;     // poll /health/live with a deadline
        Self { child, port, client: reqwest::Client::new() }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
```

Tests boot a fresh `Daemon`, set up fixture data via direct library calls (faster than the HTTP API and scoped to one transaction), make real requests with `reqwest`, and let `Drop` reap the child. The whole pipeline — routing, auth, services, error mapping, the `Finalizer` — is exercised in the same shape it runs in production.

The ephemeral-port-via-bind-and-drop trick has a tiny TOCTOU window where another process could grab the port between the drop and the daemon's bind; in practice it never fires on a dev box. If it ever does, the daemon errors loudly on bind — not a silent flake.

### Rate-limiting

Actus doesn't ship a built-in limiter — *what* gets limited (by IP / user / API key / per-route / per-tenant), *which algorithm* (token bucket / sliding window / fixed window), and *which store* (in-memory single instance / Redis for an autoscaling group) are all policy decisions the framework can't pick correctly for someone else. But the *response* shape is HTTP-correctness, and that's `WebError::TooManyRequests(Option<Duration>)` — sets status `429` and, when the hint is present, the `Retry-After` header (RFC 7231 §7.1.3) plus a `retry_after_seconds` extra member in the problem body.

The other thing the framework owns is **auditability of scope**. A controller declares which rate-limit *class* it belongs to with `#[controller(rate_limit = "name")]`; the framework stamps that label onto the matched request (`request.rate_limit_class`) before middleware runs. So a reviewer reads each endpoint's class straight off its `#[controller(...)]` line, and your limiter reads the label to choose the policy. The class is a *label, not a policy* — you still bring the key, the algorithm, and the store. (See [Scoping by controller](#scoping-by-controller-the-rate_limit-class) below.)

The application provides a `Middleware` that calls into the limiter it picked. Most production deployments use a shared store (Redis, etc.) so multiple instances see one count; the in-memory case below is a single-instance starting point. Either shape, the `Middleware` impl is the same — only the field type changes.

```rust
use actus::prelude::*;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Token-bucket state for one client key (IP / user / API key — your call).
struct Bucket { tokens: f64, last_refill: Instant }

pub struct RateLimit {
    state: Mutex<HashMap<String, Bucket>>,
    capacity: f64,
    refill_per_sec: f64,
}

impl RateLimit {
    pub fn new(capacity: u32, refill_per_sec: f64) -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
            capacity: capacity as f64,
            refill_per_sec,
        }
    }

    /// Pick the key for this request. Replace with your own: a user id from
    /// a `Params` extension, an API key from a header, a tenant id, …
    fn key_for(&self, request: &Request) -> String {
        request
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(',').next().unwrap_or(s).trim().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// `Ok(())` to allow; `Err(retry)` to reject with that retry hint.
    fn check(&self, key: &str) -> Result<(), Duration> {
        let mut state = self.state.lock().unwrap();
        let bucket = state.entry(key.to_string()).or_insert_with(|| Bucket {
            tokens: self.capacity,
            last_refill: Instant::now(),
        });
        let now = Instant::now();
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let deficit = 1.0 - bucket.tokens;
            let secs = (deficit / self.refill_per_sec).ceil() as u64;
            Err(Duration::from_secs(secs.max(1)))
        }
    }
}

#[async_trait]
impl Middleware for RateLimit {
    async fn before(&self, request: &mut Request) -> Result<Outcome, WebError> {
        let key = self.key_for(request);
        match self.check(&key) {
            Ok(()) => Ok(Outcome::Continue),
            Err(retry) => Err(WebError::TooManyRequests(Some(retry))),
        }
    }
}

// Wire it in: `Server::new(router).with_middleware(RateLimit::new(60, 1.0))`
// → bucket capacity 60, refilling at 1 token / second → roughly 60 req/min
// per IP, with bursts up to 60.
```

Swap `Mutex<HashMap<...>>` for an `Arc<RedisClient>` (and `check` becomes an `async fn` doing `INCR` + `EXPIRE` against a key like `rl:{key}:{window}`) to get the same shape backed by a shared store. The `Middleware` impl is unchanged; only the storage layer differs.

#### Scoping by controller: the `rate_limit` class

The limiter above runs on every request. To apply *different* limits to different parts of the API — and keep that visible at the route declaration — tag each controller with a class and let one limiter middleware key its policy off the matched request:

```rust
#[controller(rate_limit = "auth")]     // tight: login / token endpoints
impl AuthController { /* routes! { … } */ }

#[controller(rate_limit = "search")]   // looser: read-heavy search
impl SearchController { /* routes! { … } */ }

// A controller with no `rate_limit` declares no class → never limited
// (exactly what you want for a `/health` liveness probe).
```

The framework matches the controller, then stamps its class onto `request.rate_limit_class` (an `Option<&'static str>`) *before* the `before` chain runs — the one piece of routing context a `before` middleware can't otherwise see, since it receives `&Request`, not the matched controller. The middleware reads the label and looks up a per-class policy; an unclassed controller (or a class with no registered policy) passes through:

```rust
async fn before(&self, request: &mut Request) -> Result<Outcome, WebError> {
    let Some(class) = request.rate_limit_class else { return Ok(Outcome::Continue) };
    let Some(&policy) = self.policies.get(class) else { return Ok(Outcome::Continue) };
    let client = Self::client_key(request);
    match self.check(class, policy, &client) {
        Ok(())     => Ok(Outcome::Continue),
        Err(retry) => Err(WebError::TooManyRequests(Some(retry))),
    }
}

// Server::new(router).with_middleware(
//     RateLimit::new().class("auth", 10, 0.2).class("search", 600, 5.0))
```

This is the rate-limit analogue of `#[controller(max_body_bytes = …)]`: one declarative knob that reads off the `#[controller(...)]` line. The difference is what the knob carries — a body cap is a *number the framework enforces*, while a rate-limit class is a *label your policy interprets*, so Actus stays policy-agnostic about the limiter itself. `examples/advanced` has the full class-based middleware.

**Fail fast on a typo'd class.** Because the class is a string label an unrelated middleware interprets, a misspelling (`"ath"` for `"auth"`) would mean *unlimited*, not an error — a silent fail-open. Guard against it at startup: `Router::rate_limit_classes()` returns every declared class as a `RateLimitClass { mount, class }`, so `main()` can assert each declared class has a registered policy *before the server binds*, and abort boot (naming the offending controller) if one doesn't. One router walk, no per-request cost; the runtime stays lenient (an unmatched class passes through) precisely because the boot check is the backstop. `examples/advanced` runs this on every startup and also exposes it as `--check` for CI.

Swap `Mutex<HashMap<...>>` for Redis exactly as above; the class lookup is unchanged. For *per-route* granularity within one controller (one endpoint stricter than its siblings), route the limiter through the `prepare` hook — it receives the matched `&RouteDef`, so it can branch on `route.handler` / `route.pattern`. A declarative per-route `[rate_limit = …]` is a possible additive future, sharing the options bracket the per-route body-cap proposal sketches.

A few common variants the same pattern covers:

- **Per-user limits** (key = `params.get::<User>()` after auth resolution) — register the limiter *after* an auth middleware that stashes the user, and read it through `Params`.
- **Per-controller / per-route limits** — tag controllers with `#[controller(rate_limit = "class")]` and key the policy off `request.rate_limit_class` (above). For finer-than-controller granularity, route the limiter through a `prepare` hook, which sees the matched `&RouteDef`.
- **Different limit per tier** — key by `(user_id, tier)` and let `capacity`/`refill_per_sec` come from the user's tier (closure or trait).

The framework's contribution is the `rate_limit` class label + the 429 + `Retry-After` plumbing; the limiter and its store are yours.

## Workspace layout

```
actus/
├── crates/
│   ├── actus/                    # facade crate; re-exports the prelude
│   ├── actus-reply/              # Reply, ReplyData, WebError, the reply! macro,
│   │                             #   and the Finalizer (ReplyData → hyper Response)
│   ├── actus-controller/         # Controller trait, Params, Verb, RouteDef
│   │   └── macros/               # #[controller], routes!, app_routes! proc-macros
│   └── actus-server/             # hyper-based Server, longest-prefix Router, Request, Middleware,
│                                 #   CorsLayer; CompressionLayer + ws behind features
└── examples/
    ├── basic/                    # services + app_routes! + JSON body + header auth + verb
    │                             #   restrictions + `{...path}` rest param + CORS + compression + WS
    │                             #   + OpenAPI (served at /openapi.json, dumpable via --openapi)
    └── advanced/                 # the README's application-side patterns in working code:
                                  #   reusable prepare-hook + AuthParamsExt + typed JSON bodies +
                                  #   MyError/MyResultExt + a real rate-limit middleware; plus the
                                  #   daemon-guard integration tests (`cargo test -p actus-advanced-example`)
```

End users typically depend on just `actus` and import `actus::prelude::*`.

## Status

Actus is **1.0** — production-shaped knobs (per-request timeout, configurable drain deadline, three DoS guards, per-controller body cap, header-read timeout), correctness refinements landed across the lifecycle (the after-chain on *every* reply with a body and a request, q-value-aware `Accept-Encoding`, `Cache-Control: no-transform` respect, request-skeleton-on-error so error responses still flow through middleware and CORS), and the lifecycle reorder that puts route matching before body buffering. 117 tests pin the behavior across the workspace; `scripts/stress/` provides reproducible HTTP load, drain, and WebSocket-fanout runbooks for sanity-checking under sustained load.

The API is **stable**: the public names and shapes are committed to, and breaking changes now go through a `2.0`. It's exercised heavily by a substantial production backend, and every public item is documented. See [Stability](#stability).

What's there today:

- **Hyper-based HTTP server** with TCP accept loop, request parsing, response building, and middleware. `Server::run(port)` binds `127.0.0.1`; `Server::run_on(addr)` binds anywhere (e.g. `0.0.0.0:port` in a container).
- **Request body cap** — bodies are buffered up to 2 MiB by default. Three resolution levels, finer wins: per-controller via `#[controller(max_body_bytes = N)]`, server-wide via `Server::with_max_body_bytes(n)`, falling back to the 2 MiB default. A larger body is rejected with `413 Payload Too Large` *before* allocating — the framework matches the controller, reads its cap, *then* buffers. (Per-route caps that override the controller default are a planned additive change; see `docs/proposals/per-route-body-caps.md`.) The cap bounds buffered bytes, so it also covers chunked bodies that lie about `Content-Length`.
- **Graceful shutdown** on `SIGTERM` / `SIGINT` (Unix) or Ctrl-C (Windows): stops accepting, signals in-flight connections to finish, drains up to 30 s by default (`Server::with_drain_deadline(d)` to override). `Server::run_with_shutdown(port, future)` (or `run_with_shutdown_on(addr, future)`) for custom triggers.
- **Per-request timeout** — `Server::with_request_timeout(d)` caps the total time any request may take (body parse + middleware + handler + after-chain + finalize); an over-budget request is aborted (the handler's future is dropped) and the client gets `504 Gateway Timeout`. Off by default. The post-101 WebSocket conversation runs in its own task and isn't bound by this timer.
- **DoS knobs** — three knobs that together put a hard ceiling on what the framework will absorb under adversarial load:
  - `Server::with_max_connections(n)` — accept-loop semaphore. At capacity the loop pauses; new SYNs queue in the kernel backlog and (once `SOMAXCONN` fills) get dropped at the OS level. No userland reject cost.
  - `Server::with_max_inflight_body_bytes(n)` — semaphore over body-buffer memory. Each `from_hyper` call reserves its per-request cap from this budget; over-budget requests get `503 Service Unavailable` (`WebError::Busy`) with `Retry-After`. Caps total framework-side buffering at `n` bytes regardless of connection count.
  - `Server::with_header_read_timeout(d)` — forwards to hyper's `http1::Builder::header_read_timeout`. Catches slowloris and clients that TCP-connect-and-send-nothing.
- **`app_routes!`** with `deps` and per-route service injection (auto-clone of struct-literal shorthand, bare-ident `target: source` form, and `..base`).
- **`#[controller]` + `routes!`** with HTTP verbs, path patterns, type-safe query/body extraction, defaults, strict/lax modes, the `prepare = ...` hook (returns `Ok(None)`, a custom early-return reply, or an error), and per-controller knobs `#[controller(max_body_bytes = …)]` / `#[controller(rate_limit = "class")]`. Actus is **policy-agnostic** — authorization lives in your application's policy layer, called from the prepare hook and/or handlers.
- **Per-request state carry**: `prepare` hooks stash typed values via `params.insert::<T>(value)`; handlers read them by declaring `params: &Params` and calling `params.get::<T>()`.
- **Longest-prefix routing** at arbitrary depth, with multi-segment patterns inside controllers and a trailing `{...rest}` catch-all path parameter.
- **Query as a multimap** — repeated keys accumulate; `Vec<String>` params get all values; `params.query()` for "catch the rest". Form-urlencoded bodies fold into the same map.
- **CORS** — `Server::with_cors(CorsLayer::…)`: preflight `OPTIONS` answered automatically, `Access-Control-*` on every cross-origin response.
- **Response compression** — `Server::with_compression(CompressionLayer::…)`: gzip/brotli, content-type- and size-gated (behind the `compression` feature).
- **Streaming responses** — `reply!(stream: …)` writes a chunked body from any byte stream.
- **Server-Sent Events** — `reply!(sse: events)` / `reply::sse(...)` with an `SseEvent` builder (multi-line `data`, `event`/`id`/`retry`, comment-line heartbeats); sets `Content-Type` and `Cache-Control` for you.
- **WebSocket** — `ws::upgrade(...)` from a route handler: the server does the handshake and runs your closure on the connection (behind the `websocket` feature).
- **Header-based auth** via `Params::bearer_token()`.
- **Distinct 404 / 405** based on whether path-only or path+verb didn't match; the `405` carries an `Allow` header.
- **Structured error responses** via `WebError::Problem(ProblemDetails)` — RFC 7807 `application/problem+json` with arbitrary extension members.
- **`429 Too Many Requests` plumbing** — `WebError::TooManyRequests(Option<Duration>)` sets the status and, when the hint is present, the `Retry-After` header + a `retry_after_seconds` extra in the problem body. The framework also surfaces a per-controller rate-limit **class** (`#[controller(rate_limit = "name")]` → `request.rate_limit_class`) so a limiter middleware can scope by controller. (The limiter itself is application policy; see the rate-limiting pattern.)
- **Middleware** — `before` / `after` hooks via `Server::with_middleware(...)`; `before` can `Outcome::Continue`, `Outcome::Respond(reply)` (short-circuit), or `Err`. The `after` chain runs on *every* reply with a body and a request — successes, short-circuits, and every error path (404 / 405 / 400 / 401 / 413 / handler-returned `Err`). Ships `RequestLogger`.
- **OpenAPI 3.1 generation** — `actus::openapi::generate(&router, options, filter)`: walks the route tree and emits a spec as `serde_json::Value`, with a mount-path predicate to filter which routes are documented (behind the `openapi` feature).

### Not built in

- **Streaming-body compression** — compression covers buffered bodies (including `application/problem+json` errors); a streamed response (including SSE) goes out uncompressed for now.
- **A rate-limit middleware** — the *policy* (key by what; algorithm; store) is application-specific and the framework can't pick correctly for someone else; see the [rate-limiting pattern](#rate-limiting) for a token-bucket recipe that's three field-types away from a Redis-backed version. What *is* shipped: the 429 + `Retry-After` plumbing (`WebError::TooManyRequests(Option<Duration>)`), the per-controller `#[controller(rate_limit = "class")]` label (`request.rate_limit_class`) that lets one limiter scope by controller without the scope living invisibly in `main()`, and `Router::rate_limit_classes()` for a startup coverage check that turns a typo'd class into a boot failure instead of a silently-unlimited controller.
- **Service helpers** (DB pools, Redis, JWT) — by design. Services are just types you write; connect to whatever you want.

### Stability

Actus is **1.0** — an API-stability commitment. Breaking changes now go through a `2.0`, and any breaking change is called out in the [changelog](CHANGELOG.md) so a `cargo update` within `1.x` is never a silent surprise. Minor releases add features; patch releases are bug-fix-only.

1.0 was earned, not declared:

- **A real consumer shipped against it.** A substantial production backend built on Actus — 27 controllers across ~13 k lines — exercises the core continuously: `app_routes!` / `routes!`, `#[controller(prepare = …)]` auth hooks, `Params`, `WebError`, `reply!`, `ReplyData`, `Outcome`, and a custom `Middleware`. The stress runbooks in `scripts/stress/` add load-shape confidence (124 k req/s on `/health`, 5 k concurrent WebSockets, no FD leak).
- **The public API was deliberately reviewed.** Every public type, method, trait, and macro option was auditioned for naming, shape, and docs. The surface the real consumer leans on is committed to; every public item carries a `///` (enforced by `#![warn(missing_docs)]`); and the late-0.4 surface the consumer doesn't yet exercise got its own once-over before the freeze (see [`docs/1.0-freeze-audit.md`](docs/1.0-freeze-audit.md)).

What's deliberately deferred is additive, not breaking: per-route body caps and timeouts (today's are per-controller and server-wide; see [`docs/proposals/per-route-body-caps.md`](docs/proposals/per-route-body-caps.md)) and streaming-body compression both slot in without a `2.0`. See [Not built in](#not-built-in) for the by-design omissions.

## Comparison

| Feature                | Actus                              | Axum / Actix-web         | Rocket            |
| ---------------------- | ---------------------------------- | ------------------------ | ----------------- |
| Route organization     | Hierarchical & centralized         | Distributed              | Attribute-based   |
| Primary strength       | Auditability + ergonomics          | Ecosystem                | Ergonomics        |
| Routing style          | REST + RPC + legacy, hybrid-first  | Primarily REST           | Primarily REST    |
| Service / DI model     | Per-route struct-literal injection | Extensions / extractors  | State guards      |
| Legacy URL migration   | First-class                        | Manual                   | Manual            |

Actus aims for codebases that need to stay maintainable and auditable as they grow — particularly ones migrating from PHP or other older stacks, or where multiple developers need to reason about access control without reading framework source.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
