# Changelog

All notable changes to Actus are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Versions before `1.0` may make breaking changes in a minor release; every such
change is called out explicitly so a `cargo update` is never a silent surprise.
See the [Roadmap to 1.0](README.md#roadmap-to-10) for the stability plan.

## [Unreleased]

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

[Unreleased]: https://github.com/uniweb/actus/compare/v1.1.0...HEAD
[1.1.0]: https://github.com/uniweb/actus/compare/v1.0.1...v1.1.0
[1.0.1]: https://github.com/uniweb/actus/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/uniweb/actus/releases/tag/v1.0.0
[0.4.0]: https://github.com/uniweb/actus/releases/tag/v0.4.0
