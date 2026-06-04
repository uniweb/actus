---
name: Bug report
about: Report incorrect or unexpected behavior
title: ""
labels: bug
assignees: ""
---

## What happened

A clear description of the bug.

## Expected behavior

What you expected to happen instead.

## Minimal reproduction

The smallest `routes!` / `app_routes!` + handler that shows the problem, plus
the request that triggers it (a `curl` line is ideal):

```rust
// ...
```

```sh
curl ...
```

## Environment

- Actus version:
- Features enabled (`compression` / `websocket` / `openapi`):
- Rust version (`rustc --version`):
- OS:

## Additional context

Logs, backtraces (`RUST_BACKTRACE=1`), or anything else relevant.
