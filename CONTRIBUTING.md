# Contributing to Actus

Thanks for your interest in Actus. This document covers how to build, test, and
submit changes.

## Before you start: read the Principles

Actus is **deliberately not shaped like other Rust web frameworks** — that is
the point of the project, not an accident. Before proposing a design change,
read the [Principles section of the README](README.md#principles). The question
that gates a change is never "how does axum / Rocket / Express do this?" — it is
"which of these principles does the change serve, and does its shape honor the
others?"

In short:

- **HTTP-protocol concerns** (CORS, body limits, compression, content
  negotiation) are named `Server::with_X(...)` features with their lifecycle
  position built in. **Application concerns** (logging, auth gates, request IDs,
  rate-limit policy) are `Middleware`. Don't blur the two.
- **Auditability over uniformity.** A reviewer should be able to answer "what
  does this server do?" and "what endpoints exist?" from `Server::new(...)` and
  the two macros (`app_routes!`, `routes!`) — without grepping for attributes.
- **Explicit over magic.** No DI container, no request extractors reaching into
  thin air. Dependencies are named in the controller struct and injected in the
  `app_routes!` deps block.

## Development setup

Actus is a standard Cargo workspace. You need a recent stable Rust toolchain —
**Rust 1.88+** (edition 2024 needs 1.85, but the crate uses `let` chains, which
stabilized in 1.88).

```sh
git clone https://github.com/uniweb/actus
cd actus
cargo build
```

## The full check

Two features are off by default (`compression`, `websocket`) plus `openapi`.
**Verify every configuration** before opening a pull request:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features actus/compression,actus/websocket,actus/openapi -- -D warnings
cargo test
cargo test --features actus/compression,actus/websocket,actus/openapi
```

If you touched `crates/actus-server/src/server.rs`, anything under
`crates/actus-server/src/middleware/`, or the routing in
`crates/actus-controller/src/lib.rs`, also smoke-test the examples end-to-end:

```sh
cargo run -p actus-basic-example      # then curl the endpoints it prints
cargo run -p actus-advanced-example
```

The `examples/basic` and `examples/advanced` crates must always compile and run.

## Testing conventions

- **Unit tests** live alongside the code (`#[cfg(test)] mod tests` at the bottom
  of the file). Match the style of the existing tests in that file.
- **Integration tests** live in `crates/actus-server/tests/`. Bind
  `127.0.0.1:0`, take the freed port, then `run_with_shutdown_on(addr, ...)`;
  poll `TcpStream::connect` until it succeeds before sending the request. See
  `tests/websocket.rs` and `tests/middleware.rs` for the shape.
- For HTTP requests in tests, prefer raw `tokio::net::TcpStream` + HTTP/1.1 with
  `Connection: close` and a small response parser — this keeps test
  dependencies at zero.
- **Tests must be deterministic.** Don't add tests that race or rely on
  wall-clock timeouts beyond a small drain ceiling.

## Commit and PR conventions

- Subject line: tight, imperative, scoped — `fix(server): …`, `feat: …`,
  `docs: …`.
- Body: explain *what* changed **and why**. Call out breaking changes
  explicitly, and add a `### Changed` / `### Added` entry to `CHANGELOG.md`
  under `[Unreleased]`.
- Split separately-reviewable pieces into separate commits.

## License of contributions

Unless you state otherwise, any contribution you submit is dual licensed under
**MIT OR Apache-2.0**, matching the project license, with no additional terms.
