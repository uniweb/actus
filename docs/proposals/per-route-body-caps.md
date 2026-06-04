# Proposal: per-route body caps

**Status:** Phase 1 (controller-only) shipped (2026-05-13); Phase 2 (per-route override) deferred pending real-usage feedback.
**Scope:** pre-1.0 API addition

> **2026-05-13 update.** The discussion landed on shipping Phase 1 only — a per-controller cap via `#[controller(max_body = N)]`. Phase 2's per-route override is a purely additive change on top (the lifecycle reorder is already done, `RouteDef.max_body` would just add a new field, etc.) — but we don't ship it speculatively. The plan: see whether a real consumer hits the "wide-body endpoint nested under a `{param}` parent" pattern that the controller-only design handles awkwardly. If yes, we ship per-route. If not, we never do. The rest of this doc is the original draft, kept as the design record.

---

A way to declare a maximum buffered-body size *per route*, with a per-controller default and the existing server-level cap as the final fallback.

This document is for calm review before implementation. Nothing in it has shipped.

---

## What problem this solves

Today the body cap is server-wide (`Server::with_max_body_bytes(n)`, default 2 MiB). Every endpoint accepts up to `n`. In real APIs that mix small JSON routes with one or two large-body routes, this means the server has to be sized to the *largest* endpoint, and every other endpoint then over-accepts.

Concrete cases this comes up:

1. **Mixed shapes inside one controller.** A `MessagesController` where `POST /messages/send` is small JSON (<1 KB) but `POST /messages/{id}/attach` accepts an image up to 25 MB. Today, both routes share whatever the server-wide cap is set to.

2. **Admin-vs-public surfaces.** `/api/admin/import-csv` accepts 10 MB; the rest of the public API is JSON CRUD. Setting the global to 10 MB means the public routes also accept 10 MB JSON blobs.

3. **Contract clarity.** An API doc that says "create-task accepts up to 4 KB JSON" should be enforced by the framework, not by the handler's deserializer (which will reject a 50 MB JSON object *after* parsing it, having already spent the CPU and memory).

## What it does NOT solve

**Per-route caps are not the DoS story.** That conversation lives elsewhere — `Server::with_max_connections`, `Server::with_max_inflight_body_bytes`, `Server::with_header_read_timeout` (shipped 2026-05-13) are the actual DoS knobs. Per-route caps narrow the *attack surface* (an attacker can only buffer the large limit via the legitimate large-body endpoint, not against every endpoint), but they don't shrink the per-request maximum and they don't bound concurrent buffering. That's important framing: this proposal is about correctness and contract clarity, with attack-surface narrowing as a bonus.

## Constraints from the design

Two constraints from prior conversation that the design has to honor:

1. **`routes!` should stay readable.** Adding `max_body = …` to every route line is ugly. The common case (no per-route cap) should look identical to today.
2. **Auditability.** A reviewer reading `routes!` and the `#[controller(...)]` line should see what each route accepts. Caps that live in `main()` and are matched by path prefix would be invisible at the call site — that's the wrong direction.

## Three shapes considered

### Shape A — controller attribute + per-route override (recommended)

```rust
#[controller(max_body = 4 * 1024)]               // default for the controller
impl MessagesController {
    routes! {
        POST ""             => send(data: JsonValue),
        GET  "{id}"         => get(id: u64),
        // The one wide-body endpoint pays the visual cost; the rest don't.
        POST "{id}/attach" => attach(id: u64, body: Bytes)
            [max_body = 25 * 1024 * 1024],
    }
}
```

A codebase with no caps looks identical to today (no `#[controller(max_body=...)]`, no `[max_body=...]`). A codebase that wants tight defaults sets one attribute on the controller. A codebase with one weird endpoint adds one bracketed clause to one routes! line.

The bracket form is forward-compatible: if future route-level options come along (per-route timeouts, per-route compression policy, etc.), they extend the same bracket list:

```rust
POST "{id}/attach" => attach(id: u64, body: Bytes)
    [max_body = 25 * 1024 * 1024, timeout = 60s],
```

### Shape B — controller-only, no per-route override

```rust
#[controller(max_body = 4 * 1024)]
impl MessagesController { ... }
```

A controller cap applies to all its routes. To get a wide-body endpoint, split it into its own controller:

```rust
#[controller(max_body = 4 * 1024)]
impl MessagesController { /* most routes */ }

#[controller(max_body = 25 * 1024 * 1024)]
impl AttachmentsController { /* the upload route */ }
```

**Why not:** real APIs hit the "one weird endpoint in an otherwise small-body controller" shape often enough that this becomes friction. Splitting a controller just to set a body cap is a procedural cost with no resource-grouping benefit — controllers are about *services* (the database handle, the auth service), not about *body sizes*.

### Shape C — path-prefix rules at the server

```rust
Server::new(router)
    .with_max_body_bytes(4 * 1024)
    .with_route_cap("api/uploads/", 100 * 1024 * 1024)
```

**Why not:** the cap is invisible from the route declaration. A reviewer looking at `app_routes!` and `routes!` would see neither — they'd have to cross-check `main()` to know what each route actually accepts. That trades auditability for one less line in the macro, which is the wrong trade against the framework's principles.

### Shape D — a `bulk { ... }` block inside `routes!`

```rust
routes! {
    bulk { max_body = 4 * 1024, }
    POST ""             => send(data: JsonValue),
    POST "{id}/attach" max_body = 25 * MiB => attach(id: u64, body: Bytes),
}
```

**Why not:** adds a new syntactic element to a macro that's already non-trivial, for the same effect as the controller attribute (which already exists as a parsing site for `lax` / `strict` / `prepare = …`). Reuse over invention.

## Recommended shape

Go with Shape A: `#[controller(max_body = …)]` for the default, `[max_body = …]` after the handler signature for per-route override.

### Resolution order

For each incoming request:

1. The route's `max_body` if declared.
2. The controller's `max_body` if declared.
3. `Server::with_max_body_bytes(...)` if set.
4. `DEFAULT_MAX_BODY_BYTES` (2 MiB).

## Implementation sketch

The interesting part: per-route caps are only DoS-resistant if the cap is known *before* body buffering. That means **routing has to happen before body buffering** — different from today, where `from_hyper` buffers the body up-front.

### Current request lifecycle (today)

```
1. capture WS upgrade
2. from_hyper            ← buffers body with the server-level cap
3. CORS preflight short-circuit
4. middleware.before
5. to_params
6. router.route → handler  (routing + dispatch in one call)
7. middleware.after
8. finalize
```

### Proposed lifecycle (with per-route caps)

```
1. capture WS upgrade
2. from_hyper_parts       ← skeleton only; body stream not yet consumed
3. CORS preflight short-circuit
4. router.match           ← matches the path, returns RouteMatch
                            (controller + RouteDef + action + captures);
                            no dispatch yet
5. collect_body            ← buffer body with route_match.effective_cap()
6. middleware.before
7. to_params
8. route_match.dispatch    ← runs the prepare hook + handler
9. middleware.after
10. finalize
```

Side effects:
- **Middleware `before` still runs after buffering** — same as today. No contract change for any middleware in the current codebase (none of them read the body in `before`).
- **The error skeleton contract holds** — `from_hyper_parts` always returns the skeleton; body errors come from `collect_body` and flow through the after-chain like every other error (the existing pattern from commit `3cd0ae2`).
- **Routing happens once.** `RouteMatch` carries the matched controller + route, so `dispatch` is "call this controller's handler with this action," not "re-route the path."

### Code changes

The work is bounded but spans four crates.

#### `actus-controller`

- `RouteDef` grows `pub max_body: Option<usize>`. Same shape as the existing `Option`-valued fields (`doc`).
- `Controller` trait grows `fn actus_max_body(&self) -> Option<usize> { None }` — the per-controller default; macro overrides it.
- `routing::resolve` doesn't change shape — it already returns `(&RouteDef, ExtractedParams)`. Callers read `.max_body` off the RouteDef.
- Test fixtures (the `static ROUTES: &[RouteDef]` hand-built ones in `resolve_tests` and `router::tests`) gain `max_body: None` — mechanical, same pattern as the `handler` field addition in commit `cdb1eb7`.

#### `actus-controller-macros`

- `#[controller(max_body = <expr>)]` — parse as one more `ControllerAttr` clause alongside `strict` / `lax` / `prepare = ...`. Emit as `actus_max_body() -> Some(<expr>)` impl.
- `routes!` parser grows an optional `[max_body = <expr>]` clause after the handler signature. Parse as `Punctuated<RouteOpt, Token![,]>` inside `[...]`; the only `RouteOpt` for v1 is `max_body = <expr>`. Forward-compatible.
- `RouteDef` literals emitted by the macro fill in `max_body: <expr>` or `max_body: None`.

#### `actus-server`

- `Router` grows `pub fn match_route(&self, path_parts: &[String]) -> Option<RouteMatch>`. `RouteMatch { controller: Arc<dyn Controller>, action: String }`. Walking logic is the same as `Router::route`; just stop before `actus_dispatch`.
- `RouteMatch::dispatch(self, params: Params) -> Reply` calls `controller.actus_dispatch(&action, params)`.
- `Request::from_hyper` splits into `from_hyper_parts(req) -> (Self, hyper::body::Incoming)` (cheap; no body buffer) plus `collect_body(self, body, max_body_bytes, inflight_budget) -> Result<Self, WebError>` (the body-buffer half, used to be `from_hyper`'s second phase). The skeleton-on-error contract is preserved by `collect_body` returning the skeleton (with `body = Bytes::new()`) inside its Err.
- `Server::handle_request_inner` is reordered to the new lifecycle. About 30 lines of moves.

#### `effective_cap` resolution

A small helper on `RouteMatch`:

```rust
impl RouteMatch {
    fn effective_max_body(&self, server_default: usize) -> usize {
        self.route.max_body
            .or_else(|| self.controller.actus_max_body())
            .unwrap_or(server_default)
    }
}
```

### Tests

- **Unit (actus-controller):** `resolve` test that asserts a RouteDef's `max_body` field round-trips correctly. (Mechanical; just exercises the new field on `RouteDef`.)
- **Unit (actus-server):** `Router::match_route` returns the right RouteMatch for a path that hits a deep mount; returns `None` for an unmatched path.
- **Integration (actus-server::tests::middleware):** add a `LimitedController` with `#[controller(max_body = 16)]` and `routes! { POST "tiny" => tiny(data: JsonValue), POST "large" max_body = 1024 => large(data: JsonValue) }`. Send 32-byte body to `/tiny` → 413. Send 32-byte body to `/large` → 200. Send 2 KiB body to `/large` → 413. Send 8-byte body to a route that doesn't exist → 404. Verifies the resolution order and the deferred-buffering path.
- **Integration:** body-cap-with-CORS still stamps CORS on the 413 (the skeleton-on-error contract holds for the new path too).

### Docs

- README `## Per-route body caps` section near the existing body-cap mention.
- Update `WebError::PayloadTooLarge` row to mention per-route resolution.
- `examples/advanced`'s TasksController grows a `[max_body = …]` on one route to demo.

## What this is *not* trying to do

- **Per-route timeouts.** Same shape would work (`[timeout = 60s]`) but they're a separate concern and we don't ship them yet.
- **Per-route middleware.** Different problem; needs its own design pass.
- **Per-route content-type whitelist** ("this route only accepts `application/json`"). Already covered by handler-level deserialization; not framework business.
- **Cap on streaming response bodies.** Compression / streaming caps are orthogonal.

## Open questions for the review

1. **Bracket syntax — `[max_body = …]`.** Is that the right shape? Alternatives we considered: `with max_body = …` (more English-y, harder to extend); `#[max_body = …]` (Rust-attribute-y, conflicts visually with `#[controller(...)]`); a prefix form (`POST max_body = …, "path" => …` — ugly). The bracket form is unambiguous in the current `routes!` grammar (brackets aren't used anywhere else) and reads as a "route-level options list."

2. **Controller attribute key — `max_body` vs `max_body_bytes`.** The server method is `with_max_body_bytes(n)`; consistency would say `max_body_bytes`. The macro syntax already has `prepare = ...` (terse), `lax` (terse), `strict` (terse) — `max_body` matches that tone. Open call.

3. **What if `max_body = 0`?** Same as the server-level method: "this route rejects every non-empty body." Useful on a strictly-GET route that *should* never see a body. Document this explicitly.

4. **Should `RouteMatch` cache the captures?** Currently `routing::resolve` does pattern matching twice — once in `match_route` (would have to add it), once in `actus_dispatch` (inside the controller). Could be optimized by passing captures through `RouteMatch`. Probably worth doing, but not required for correctness.

5. **Forward compatibility of the bracket list.** When (if) we add `[timeout = …]`, will the macro grammar still parse cleanly? Yes — `Punctuated<RouteOpt, Token![,]>` admits arbitrary `RouteOpt` variants. The first added variant just is `max_body`; future variants slot in. Open question: do we want to ship `[max_body]` *now* and add others later, or wait until we know what else goes there? I'd say ship `[max_body]` now; it's load-bearing and the bracket grammar is forward-compatible.

6. **Streaming bodies past the cap.** The cap is on buffered bytes. A handler that does `body: Bytes` (declared, capped). A handler that does streaming-in (we don't currently support this; bodies are always buffered) is a future concern.

## Estimated effort

About 250-400 lines of code change across the four crates, dominated by:
- Macro work for the new attribute + bracket clause (~100 lines).
- The `from_hyper` split + `handle_request_inner` reordering (~60 lines).
- `Router::match_route` extraction (~40 lines).
- Tests (~80 lines).
- Hand-built `RouteDef` fixtures in tests (mechanical, ~20 lines total).

Plus README updates.

## Why now, why not later

Per-route caps are exactly the kind of feature that's hard to retrofit after 1.0. The lifecycle change (route-before-buffer) is invasive enough that doing it after API freeze means either a 2.0 or a feature-flagged second path that confuses everyone. Better to land it pre-1.0 alongside the other lifecycle-shaping work (the after-chain consistency commit `3cd0ae2`, the DoS knobs commit `f596cb5`).

The cost of *not* doing it: real users hit it in their first or second deployment, file an issue, we add it under pressure with less time to think. Better to design it calmly now.
