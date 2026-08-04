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

## Releasing

One command, from a clean default branch — say the *kind* of bump and the
version is computed from the manifest:

```sh
./scripts/release.sh minor     # 1.0.1 → 1.1.0
./scripts/release.sh patch     # 1.0.1 → 1.0.2
./scripts/release.sh major     # 1.0.1 → 2.0.0
```

Pick the kind from what the `[Unreleased]` entries say: new public API is a
**minor** bump, fixes and docs are a **patch**. An explicit `X.Y.Z` is also
accepted, for what arithmetic can't express (`2.0.0-rc.1`, or skipping a
version deliberately). Write the CHANGELOG entry as part of the change itself —
the release script refuses an empty `[Unreleased]`.

It bumps the workspace version and the five internal dependency pins, cuts
`[Unreleased]` in `CHANGELOG.md` to the new version, refreshes `Cargo.lock`,
runs the full gate (fmt · clippy · tests, both feature configs · MSRV ·
`cargo publish --workspace --dry-run`), then commits, tags, and — after one
confirmation — pushes.

**Pushing the tag is what publishes.** `.github/workflows/release.yml` fires on
`v*`, uploads all five crates, and opens a GitHub Release from the CHANGELOG
section. Nobody needs crates.io credentials on their machine: CI authenticates
with [Trusted Publishing](https://crates.io/docs/trusted-publishing), which
trades the workflow's GitHub OIDC identity for a 30-minute token. There is no
long-lived registry token in the repo secrets.

That workflow deliberately does **not** re-run the test matrix: the tagged
commit is pushed to the default branch first, so `ci.yml` is already running on
those exact bytes, and the release script ran the same checks locally before the
tag existed. It checks the two things nothing else does — that the tag agrees
with the manifest version, and that `cargo publish` can build every crate from
its packaged tarball.

`--no-push` does everything locally and stops, so you can inspect the commit
before it becomes permanent; `--yes` skips the confirmation for unattended use.
Nothing before the push leaves your machine, and the script prints the exact
`git tag -d` / `git reset` to undo it.

## License of contributions

Unless you state otherwise, any contribution you submit is dual licensed under
**MIT OR Apache-2.0**, matching the project license, with no additional terms.
