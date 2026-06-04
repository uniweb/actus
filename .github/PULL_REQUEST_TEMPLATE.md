## What & why

What does this change, and why? Link any related issue (`Closes #123`).

## Checklist

- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --all-targets` and `--features actus/compression,actus/websocket,actus/openapi` are clean
- [ ] `cargo test` passes in both feature configs
- [ ] If I touched `server.rs`, `middleware/`, or the routing in `actus-controller`, I smoke-tested the examples (`cargo run -p actus-basic-example`)
- [ ] Public API changes are documented (`///`) and have a `CHANGELOG.md` entry under `[Unreleased]`
- [ ] Breaking changes are called out explicitly below

## Breaking changes

None / describe them here.

## Notes for reviewers

Anything that helps the review — design trade-offs, alternatives rejected, the principle this serves (see the [README Principles](../README.md#principles)).
