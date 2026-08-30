# Actus — for contributors and agents

Actus is a standalone Rust web framework: its own Cargo workspace, dual-licensed under MIT OR Apache-2.0, designed to be reused outside Uniweb. This file is the root context for agents and contributors working in this repo. Read `README.md` for the user-facing tour; this file is the part the README doesn't say.

## Don't guess about this codebase

Before claiming how anything in Actus works — in a reply, a doc, a plan, a commit message — ground it on one of:

1. **Code you've read in this session** and directly observed the behavior of.
2. **A committed test** that asserts the behavior, which you've read.
3. **A statement in this file, `README.md`, or a `///` doc on the type/function in question.**

If none of those apply, you don't know — read the code. The names of types are *hints*, not facts; an empty `grep` is an experiment, not a proof.

This rule matters more in Actus than in most codebases. The framework's shape is *deliberately not* the same as the prevailing Rust web frameworks — that's the whole point of the [Principles](#principles). Pattern-matching from axum / tower / Rocket / Express will reliably steer you wrong here.

## Crate layout

Five crates plus two examples:

- `crates/actus-reply/` — `Reply`, `ReplyData`, `WebError`, the `reply!` macro, and the `Finalizer` (`ReplyData` → `hyper` `Response`).
- `crates/actus-controller/` — `Controller` trait, `Params`, `Verb`, `RouteDef`, the `routing::{match_pattern, resolve}` functions, and the route-family compile-time half (`DeclaresExpectation`, `Family`, `declares_expectation[_in]`). The runtime side of controllers.
- `crates/actus-controller/macros/` — `#[controller]`, `routes!`, `app_routes!` proc-macros.
- `crates/actus-server/` — hyper-based `Server` (with `router()`), longest-prefix `Router` (`mounts()` — the per-mount inventory, absences included — and `RateLimitClass`), `Request`, `Middleware`, `CorsLayer`; `CompressionLayer` (behind `compression`); `websocket::{upgrade, WebSocket, Message}` (behind `websocket`); `openapi::{generate, Options}` (behind `openapi`).
- `crates/actus/` — facade crate; re-exports the prelude for end users.
- `examples/basic/` — wires services + `app_routes!` + JSON body + header auth + verb restrictions + `{...path}` rest param + CORS + compression + WebSocket echo + SSE + OpenAPI + a maintenance-mode middleware, all served over real HTTP. Always keep this compiling.
- `examples/advanced/` — the application-side patterns in working code: a domain-error → `WebError` mapping, `ProblemDetails` with `field`/`rule`, a class-based rate-limit `Middleware` with a startup coverage check (`--check`), per-controller `max_body_bytes`, **route families** (`#[controller(expects = …)]` floors, the `families { … }` block on `app_routes!`, `family_coverage` at boot, a declaration-keyed `FloorGate` middleware via `server.router()`), and the daemon-guard integration tests (`tests/integration.rs`). Also keep compiling.

## Feature flags

Default-off; opt in on the `actus` facade crate, which forwards to `actus-server`:

- `compression` — pulls in `flate2` + `brotli`; enables `Server::with_compression(CompressionLayer::…)`.
- `websocket` — pulls in `tokio-tungstenite`; enables `actus::ws::upgrade(...)`, `WebSocket`, `Message`, and the `with_upgrades()` branch in the accept loop.
- `openapi` — enables `actus::openapi::{generate, Options}` (OpenAPI 3.x doc generation). Pulls in no extra crates — it just compiles the module against the already-present `serde_json`.

When touching code that's feature-gated, verify both configurations (default and all-features):

```sh
cargo test
cargo test --features actus/compression,actus/websocket,actus/openapi
cargo clippy --all-targets
cargo clippy --all-targets --features actus/compression,actus/websocket,actus/openapi
```

## Testing patterns

- **Unit tests** live alongside the code (`#[cfg(test)] mod tests` at the bottom of the file). Match the style of the existing tests in the same file.
- **Integration tests** live in `crates/actus-server/tests/`. The pattern: bind `127.0.0.1:0` with `std::net::TcpListener`, take the port, drop the listener, then `Server::run_with_shutdown_on(addr, shutdown_future)` on the freed port; poll `tokio::net::TcpStream::connect` until it succeeds before sending the test request. See `tests/websocket.rs` and `tests/middleware.rs` for the shape.
- For HTTP requests in tests, prefer **raw `tokio::net::TcpStream` + HTTP/1.1 with `Connection: close`** and a small response parser — keeps test dependencies at zero. WebSocket tests use `tokio-tungstenite` (which is already there via the `websocket` feature).
- Tests must be deterministic. Don't add tests that race or rely on wall-clock timeouts beyond a small drain ceiling.

## Workflow

Commits go directly to `main`. No PR flow; treat the commit message as the review record:

- Subject: tight, imperative, scoped (`fix(server): …`, `feat: …`, `docs: …`, …).
- Body: what changed *and why*. Call out breaking changes explicitly.
- Split into separate commits when the pieces are separately reviewable.

**Never add an attribution or provenance trailer. This is strict and has no
exceptions.** No `Claude-Session:` line, no session or conversation URL, no
`Co-Authored-By:` naming a tool or model, no "generated with"/"authored by" footer,
and no agent, model, or vendor name anywhere in the subject, body, or trailers. The
same applies to tag messages, release notes, `CHANGELOG.md` entries, PR and issue
text, and code comments.

The commit message is the review record for a *change*: what it does, why, and what
it breaks. What typed it is not part of that record, it is noise in `git log`, and in
a public repository it is permanent. **If a tool, harness, or template appends one by
default, remove it before committing** — a default is not an exemption, and "the
tooling added it" is not a reason it may stay.

Before pushing, run the full check (build/test/clippy/fmt) with **both** feature configs. If you touched anything in `crates/actus-server/src/server.rs`, `crates/actus-server/src/middleware/`, or the routing in `crates/actus-controller/src/lib.rs`, also smoke-test `examples/basic` end-to-end with `cargo run -p actus-basic-example` + `curl`.

### Releasing

`./scripts/release.sh <major|minor|patch>` — computes the version from the manifest, bumps it, cuts the CHANGELOG, gates, commits, tags, pushes. (An explicit `X.Y.Z` is accepted for a prerelease or a deliberate skip.) **Pushing the tag is what publishes**: `.github/workflows/release.yml` fires on `v*` and uploads all five crates via crates.io Trusted Publishing (GitHub OIDC → a 30-minute token; no stored registry token, and none on any laptop). Don't publish by hand — `cargo publish` from a workstation bypasses the gate and the version/tag agreement check.

Pick the bump from the `[Unreleased]` entries: new public API is **minor**, fixes and docs are **patch**. The script refuses an empty `[Unreleased]`, a dirty tree, a non-default branch, a version that doesn't move forward, and an existing tag. Full description: `CONTRIBUTING.md` → Releasing.

## Principles

These shape how Actus is designed *and* how it should be extended. They are the failure-mode hedge: the patterns Actus uses are not the same as other Rust web frameworks, and the path to wrong design changes is usually "I reflexively imported a pattern from somewhere else."

1. **HTTP-protocol concerns are named server features; application concerns are middleware.** CORS, body limits, compression, content negotiation, the `Allow` / `Vary` stamping — concerns the server does, with positions in the request lifecycle dictated by HTTP semantics. They get named `Server::with_X(...)` methods. Logging, auth gates, request IDs, maintenance mode, caching, rate-limit — concerns the application chooses to apply, with ordering it owns. They are `Middleware`. Don't blur the categories: a CORS "middleware" the user has to position correctly has moved framework knowledge into application code.

2. **Auditability over uniformity.** A reviewer should be able to answer "what does this server do?" and "what endpoints exist?" from a small, well-known set of places — `Server::new(...).with_X(...)` and the two macros. Don't add abstractions that require walking a chain or grepping for attributes to answer those questions. When a uniform API and a discoverable one are in tension, prefer discoverable.

3. **Explicit > magic.** No DI container. No request extractors that reach into thin air. The `app_routes!` deps block is constructor injection; route patterns are declared, not discovered; the `Controller` struct names its services. If two things have different semantics, give them different names — don't make the reader infer from context.

4. **Real HTTP, out of the box.** The server should do HTTP correctly without making the user think about it. They should never need to know that "compression must be the outermost outgoing transform" or "the body limit gates the body parse" — that's framework knowledge, not application knowledge. New HTTP-protocol features become named server methods with their position built in, not layers to order.

5. **Two macros, one audit surface.** `app_routes!` declares the application's URL blueprint; `routes!` declares a controller's API surface. That is where endpoints, parameters, and access points live. Anything that *adds endpoints* belongs in a `routes!` block. Middleware shapes *how* requests flow, not *which* endpoints exist, so it doesn't violate this — but a feature that secretly attaches routes to your app from somewhere else would.

6. **Pragmatism inside the structure.** REST verbs, RPC action names (`/charge`, `/refund`), path params, legacy URLs (`login.php`) all coexist in the same `routes!` block. Don't force one style; the structure is the hierarchy and the macros, not the URL shape.

7. **Policy-agnostic.** Actus has no `Access` enum, no built-in RBAC, no notion of "roles." Authorization belongs in the application's policy layer, called from the `prepare` hook or the handler. The framework's job ends at "the handler ran"; what the handler *allowed* is the application's job.

8. **Services are persistent.** Constructed once at startup, wrapped in `Arc`, held by controllers for the server's lifetime. No per-request construction; no per-request DI lookup. Controllers state their dependencies in their struct — which is itself an audit surface.

### When extending Actus

The question to ask isn't *"how does axum / Rocket / Express do this?"* — it's *"which of these principles does the change serve, and does the proposed shape honor the others?"* Reaching for a familiar pattern from another framework is fine as a starting point; staying with it without checking it against these principles is the failure mode. When you find yourself wanting to add a single uniform `Layer` trait so everything composes the same way, re-read principles 1 and 2: that uniformity is exactly the trade Actus chose not to make.

One mechanical rule falls out of the 1.0 freeze (`docs/2.0-docket.md` §1): **every new public type ships `#[non_exhaustive]` unless consumers must construct it.** An all-`pub` struct or a plain public enum is welded shut the moment it is released — the next field or variant is then a major change — and the marking costs a framework-populated type nothing. The docket was born from one proposal making this mistake, and the next revision of that proposal repeated it in a brand-new struct *while citing the docket* — so treat this as a checklist item on any change that adds public API, not as something you'll remember.
