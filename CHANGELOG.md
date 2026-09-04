# Changelog

All notable changes to Actus are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Versions before `1.0` may make breaking changes in a minor release; every such
change is called out explicitly so a `cargo update` is never a silent surprise.
See the [Roadmap to 1.0](README.md#roadmap-to-10) for the stability plan.

## [Unreleased]

## [1.5.0]

### Fixed

- **A declared `bool` route-parameter default was unreachable.** Every typed
  parameter reaches its default through the generated
  `get_x(name).unwrap_or(default)`, which works because `get_x` returns `Err`
  on a missing parameter. `bool` is the one type whose *absence* is itself a
  usable value, so `get_bool` answered `Ok(false)` rather than erroring —
  `unwrap_or` unwrapped that `false` and **the default was dead code**. Every
  `param: bool = true` silently behaved as `false`. Found in a consumer: a
  cancellation route declaring `at_period_end: bool = true` cancelled
  *immediately* when the client omitted the parameter — the destructive
  direction, and the opposite of what the route documented. It stayed invisible
  because the route behaved correctly whenever the parameter *was* supplied.

  A **bare** `param: bool` is unchanged — it is **required**, and a request
  omitting it gets a `400`, exactly as a bare `String` or `u64` does. (An
  earlier draft of this entry said a bare `bool` reads `false` when absent.
  That was wrong and unverified: `routing::resolve` rejects it before
  extraction. An optional flag is spelled `confirm: bool = false`.)

- **`ExtractedParams::get_bool` now returns `400` when the parameter is absent**,
  as `get_string`, `get_i64` and every other scalar getter already did. It was
  the one getter that invented a value — `Ok(false)` — instead of erroring, and
  that invented value is what made a declared `bool` default unreachable.

  **Not a breaking change**, despite being a behaviour change to a public
  method: `ExtractedParams` has private fields and no public constructor, so
  `routing::resolve` is the only way to obtain one, and it rejects an absent
  bare `bool` before this method can be reached. The branch was unreachable from
  outside the crate. Use `get_bool_optional` to tell absence from an explicit
  `false`.

### Added

- `ExtractedParams::get_bool_optional` — distinguishes an **absent** bool
  parameter (`None`) from one supplied as `false`, which is the reader a
  declared default needs. Mirrors `Params::get_bool_optional` on the
  pre-resolution type.

- `routing::param_is_required(&ParamDef)`, re-exported at
  `actus::routing`: whether an absent value for a parameter makes the request a
  `400` — **the rule `routing::resolve` enforces**, exported so a tool reporting
  requiredness cannot disagree with the router. The OpenAPI generator's
  `required` flag now calls it instead of re-deriving the same expression in a
  second crate, where the two agreed only by diligence and would have diverged
  the first time a new inherently-optional type was added.

  The rule it fixes in one place: a query parameter is **required unless it
  declared a default**, uniformly across every scalar type, so requiredness is
  readable off a `routes!` block without knowing the type. `ParamType::StringArray`
  is the sole exemption, and a forced one — urlencoding cannot express "present
  but empty", so no distinction is available to lose. `bool` is deliberately not
  exempt: exempting it would leave no way to declare a required bool, and would
  discard a distinction the wire does carry.

## [1.4.0]

### Added

- `routing::covering_family(mount, prefixes)` (and `routing::family_segments`),
  re-exported at `actus::routing`: which family covers a mount, by the exact
  rule the `families` block of `app_routes!` applies at compile time —
  segment-aligned, longest prefix wins, `*` sugar, root covers all. A boot-time
  coverage check written against it cannot disagree with the compile-time one.
  Consumers had been re-deriving the rule as `mount.split('/').next()`, which
  agrees only while every family is a single segment and diverges silently the
  moment one nests — found in production the first time a family was nested.
  The README snippet and `examples/advanced` now use it.

## [1.3.0]

### Added

- **Route families, Phase 2 — the compile-time half.** `app_routes!` accepts a
  `families { "api" => ["credential", "anonymous"], "hooks" => ["signature"],
  "admin" }` block: every controller mounted under a listed prefix must carry
  `#[controller(expects = "…")]` or the crate does not compile (an `E0277`
  whose message names the controller and says what to add), and an entry with
  an accepted list also checks the declared floor in a `const` evaluated when
  `init` is compiled (`E0080`). Prefix matching is segment-wise and treats
  `"api/*"` as `"api"`; a family covering no mount is a compile error at its
  literal. New public items in `actus-controller`: the `DeclaresExpectation`
  marker trait (emitted by `#[controller(expects = …)]`, with the
  `on_unimplemented` diagnostic), the `Family` trait, the `const fn`s
  `str_eq` / `floor_accepted`, and the pass-throughs `declares_expectation` /
  `declares_expectation_in`. `expects` must now be a `const` expression (a
  string literal or a `const` path). Doctests on the `actus` crate root pin
  all three failure modes as `compile_fail`.

## [1.2.0]

### Added

- **Route families, Phase 1** — a coverage mechanism for client-segregated
  route surfaces; coverage, not authorization (README § "Route families";
  design record `docs/proposals/route-family-contracts.md`):
  - `#[controller(expects = "…")]` declares the controller's caller **floor**
    — the least-privileged caller it is written to accept. An opaque
    `&'static str` the framework never interprets; surfaced as
    `Controller::actus_expects()` (defaulted, so every existing controller
    keeps compiling).
  - `Controller::actus_prepare()` — the `prepare` hook's presence (and its
    written path, e.g. `"Self::auth"`), so a coverage rule can require *"a
    `"credential"` floor has a hook to resolve one"*.
  - `Router::mounts()` — the per-mount inventory: one `Mount` row (mount
    path, controller name, `expects`, `prepare`, rate-limit class, body cap)
    for **every** mounted controller, absences included — the omission is a
    row, not a skip, which is what makes a coverage check able to catch it.
    `Mount` is `#[non_exhaustive]` and can grow fields in minor releases.
    (`Router::rate_limit_classes()` keeps its declaring-only shape; its
    rustdoc now documents the asymmetry and points at `mounts()`.)
  - `Server::router()` — shares the served route tree (`Arc<Router>`), so
    application middleware can use the framework's own longest-prefix
    matcher — e.g. a declaration-keyed gate reading
    `match_controller(...).controller.actus_expects()` — instead of
    re-deriving the routing.
  - `examples/advanced` grows the worked example: `family_coverage` (boot
    check, wired into `--check`), a `FloorGate` middleware, and unit +
    integration tests for both.

## [1.1.0]

### Added

- `Server::run_listener` / `Server::run_with_shutdown_listener` — serve on a
  listener the caller already bound or inherited. This is the **socket
  activation** entry point (systemd `LISTEN_FDS`, launchd): the supervisor
  owns the socket, so connections arriving during a process restart queue in
  the kernel's accept backlog instead of being refused, and the next process
  serves them. Also the race-free shape for tests and embedding (bind
  `127.0.0.1:0`, keep the listener, pass it in). `run_with_shutdown_on` is
  now a thin bind-then-delegate over the listener form; behavior of every
  existing `run*` method is unchanged.

## [1.0.1]

Documentation-only release; the public API is identical to 1.0.0.

- Enriched the crate-level rustdoc landing page that docs.rs renders — added
  Philosophy, Design principles, and a feature overview so it mirrors the
  README, plus a repository link. The README's badge row also dropped the
  flaky crates.io license badge.

## [1.0.0]

First stable release. `1.0` is an API-stability commitment: from here, breaking
changes go through a `2.0`. The public surface was validated by a substantial
production backend, every public item is documented, and the
late-0.4 surface was reviewed (see `docs/1.0-freeze-audit.md`) before the freeze.

### Added

- `KIB` / `MIB` / `GIB` byte-unit consts (prelude-exported) so a body cap reads
  the same in an attribute, a builder call, or any expression: `4 * KIB`,
  `2 * MIB`.

### Changed (breaking)

The final pre-1.0 polish — renames for consistency and explicitness:

- `#[controller(max_body = N)]` → `#[controller(max_body_bytes = N)]`, and the
  `Controller::actus_max_body` trait method → `actus_max_body_bytes`, matching
  `Server::with_max_body_bytes` / `DEFAULT_MAX_BODY_BYTES`.
- `Router::rate_limit_classes()` now returns `Vec<RateLimitClass>` (fields
  `mount: String`, `class: &'static str`) instead of `Vec<(String, &'static str)>`.
- `CompressionLayer::quality(u32)` → `CompressionLayer::brotli_quality(u32)`
  (it only ever set brotli quality; gzip is unaffected).
- Lowered the default `DEFAULT_MAX_BODY_BYTES` from **16 MiB → 2 MiB** (matches
  axum; a default should lean safe). Endpoints that accept larger bodies opt in
  via `Server::with_max_body_bytes` or `#[controller(max_body_bytes = …)]`.

## [0.4.0]

### Added

- DoS-mitigation knobs: `Server::with_max_connections`,
  `Server::with_max_inflight_body_bytes`, `Server::with_header_read_timeout`.
- `WebError::Timeout`, `WebError::Busy`, and `WebError::TooManyRequests`
  (the last emits `429` plus `Retry-After` when given a hint).
- `Server::with_drain_deadline` for bounded graceful shutdown.
- Per-controller rate-limit scoping: `#[controller(rate_limit = "class")]`
  stamps `request.rate_limit_class` before the middleware chain runs.
- `Router::rate_limit_classes()` for a startup coverage check that turns a
  typo'd rate-limit class into a boot failure instead of a silently-unlimited
  controller.
- `CompressionLayer::quality(u32)` to tune brotli quality.
- OpenAPI 3.x document generation behind the `openapi` feature
  (`actus::openapi::generate`).

### Changed

- Request lifecycle reordered so route matching happens before body buffering;
  nonexistent paths now `404` without buffering the request body.
- Compression honors `Cache-Control: no-transform`.
- The after-chain now runs on every reply that has a body.

[Unreleased]: https://github.com/uniweb/actus/compare/v1.5.0...HEAD
[1.5.0]: https://github.com/uniweb/actus/compare/v1.4.0...v1.5.0
[1.4.0]: https://github.com/uniweb/actus/compare/v1.3.0...v1.4.0
[1.3.0]: https://github.com/uniweb/actus/compare/v1.2.0...v1.3.0
[1.2.0]: https://github.com/uniweb/actus/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/uniweb/actus/compare/v1.0.1...v1.1.0
[1.0.1]: https://github.com/uniweb/actus/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/uniweb/actus/releases/tag/v1.0.0
[0.4.0]: https://github.com/uniweb/actus/releases/tag/v0.4.0
