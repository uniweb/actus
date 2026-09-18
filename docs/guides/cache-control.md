# Cache-Control in Actus

**Owner:** Diego Macrini

**Status:** CURRENT — 2026-09-18. How to give a controller, or a family of
controllers, a `Cache-Control` default that a single route can override, and
what leaving the header off lets caches do. Actus ships no Cache-Control
setting: which responses are private is application knowledge. This is the
pattern its parts compose into.

## What a missing header means

A missing `Cache-Control` does not mean "don't cache". It means no
instructions, and HTTP then lets each cache decide:

- **A cache may store the response** when its status is *heuristically
  cacheable*: 200, 203, 204, 206, 300, 301, 308, 404, 405, 410, 414 or 501
  (RFC 9110 §15.1; the storing rules are RFC 9111 §3). 400, 401, 403 and 500
  are not on the list. In practice this concerns GET and HEAD: a POST response
  is cacheable only with explicit freshness and a matching `Content-Location`
  (RFC 9110 §9.3.3).
- **The cache picks the lifetime** (RFC 9111 §4.2.2). The suggested heuristic
  is a fraction, typically 10%, of the time since `Last-Modified`. Actus's
  finalizer ([`Finalizer::build_response`](../../crates/actus-reply/src/finalizer.rs))
  sets no `Last-Modified`, so unless a handler sends one, that heuristic has
  nothing to work from.
- **Chromium** — read 2026-09-18, `HttpResponseHeaders::GetFreshnessLifetimes`
  in `net/http/http_response_headers.cc`; re-read that function to re-check.
  With no `max-age`, `Expires`, `no-cache` or `no-store`, a 200/203/206
  carrying `Last-Modified` is fresh for a tenth of its age, **a 300, 301, 308
  or 410 is fresh indefinitely**, and anything else gets 0 seconds, so the
  browser asks the server again. This licenses the *permanent statuses*
  caveat below.
- **Stale is not deleted.** A stored response can stay in the browser's disk
  cache, and history features such as the Back button may redisplay an
  expired copy (RFC 9111 §6). Only `no-store` keeps it off disk.
- **Shared caches** — reverse proxies, CDNs — must not reuse a response to a
  request that carried `Authorization` unless the response explicitly allows
  it (RFC 9111 §3.5). **A request authenticated by cookie gets no such
  protection**: whether its response is stored is up to the proxy's
  configuration.

| directive | meaning |
|---|---|
| `no-cache` | may be stored, but must be revalidated before every reuse (RFC 9111 §5.2.2.4). Not "don't cache". `reply::sse` sets it. |
| `private` | shared caches must not store it; the browser's cache may (§5.2.2.7) |
| `no-store` | no cache stores it (§5.2.2.5). Adding `private` changes nothing for a compliant cache. |

## The pattern

Two parts, each where a reviewer already looks:

1. **The default, per controller.** An `after` middleware finds the controller
   the request reached — `server.router()` plus `match_controller`, the
   technique `FloorGate` uses in [Route families](../../README.md#route-families)
   — reads its declaration, and sets `Cache-Control` **only when the reply has
   none**.
2. **The exception, per route.** A handler that needs different caching sets
   the header on its own reply, and the default leaves it alone.

```rust
use actus::prelude::*;
use std::sync::Arc;

/// Floor → Cache-Control: the application's table, read top to bottom.
const CACHE_BY_FLOOR: &[(&str, &str)] = &[
    ("credential", "private, no-store"),
];

struct CachePolicy {
    router: Arc<Router>,
}

#[async_trait]
impl Middleware for CachePolicy {
    async fn after(&self, request: &Request, response: &mut ReplyData) -> Result<(), WebError> {
        if response.header("cache-control").is_some() {
            return Ok(()); // the route decided
        }
        let floor = self
            .router
            .match_controller(&request.path_parts)
            .and_then(|rm| rm.controller.actus_expects());
        if let Some((_, value)) = floor.and_then(|f| CACHE_BY_FLOOR.iter().find(|(k, _)| *k == f)) {
            response.add_header("Cache-Control", *value);
        }
        Ok(())
    }
}

// Wiring — the router is shared, so there is one tree and one matcher:
let server = Server::new(router);
let policy = CachePolicy { router: server.router() };
let server = server.with_middleware(policy);
```

A route's exception:

```rust
/// Per-user, but fine to keep in the browser for an hour.
pub async fn avatar(&self) -> Reply {
    reply!(
        status = StatusCode::OK,
        headers = { "Cache-Control": "private, max-age=3600" },
        json!({ "url": "/a.png" })
    )
}
```

`reply!` has no form that takes headers without a status. For bytes or a
stream, use `reply::build_reply().header(…).body(…).done()`.

### Why this shape

- **`after` runs on errors too**, so a 401, a 403 or an in-controller 404 from
  a private controller gets the default. It does not run on a WebSocket `101`
  or a CORS preflight (README [Middleware](../../README.md#middleware)), nor
  on the `504` from `Server::with_request_timeout` (its rustdoc).
- **Forgetting an exception costs a cache miss, not a leak**, because the
  default is the conservative value.
- **Not `prepare`.** It runs before the handler and can only short-circuit: it
  cannot touch the handler's reply, and what it stores in `Params` never
  reaches `after`.
- **Keyed on the declaration, not the path.** A controller is covered the
  moment it is mounted, and a `families` block in `app_routes!` makes every
  controller under a prefix declare a floor. If you do need a prefix, use
  `actus::routing::covering_family`, never `split('/').next()`.
- **Check, then set.** `add_header` *replaces*, so an unconditional stamp
  would overwrite every route's exception.

### Caveats

- **A floor is not a ceiling.** A controller declared `expects = "anonymous"`
  can still have routes that need a login, and a floor-keyed table skips them.
  If you have such controllers, also set the default when the request carried
  a credential — an `Authorization` header, or your session cookie.
- **SSE decides for itself.** `reply::sse` sets `cache-control: no-cache`,
  which counts as the route's choice. If a private stream must be `no-store`,
  set that in its handler.
- **Permanent statuses outlive you.** A 300, 301, 308 or 410 sent without
  explicit freshness is kept indefinitely by Chromium (above). If such a route
  may change its answer, send `no-cache` or a `max-age` with it; `private`
  alone does not help, since it says nothing about freshness.

## Header names are case-insensitive

So are the reply's header methods (RFC 9110 §5.1). `ReplyData::add_header`
and `ReplySpec::header` replace a header of the same name in any letter case,
and `ReplyData::header` finds one the same way. Writing to `ReplySpec::headers`
directly bypasses this: two spellings of one name then both reach the
finalizer, and only one of them is sent.
