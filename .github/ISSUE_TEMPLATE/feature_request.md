---
name: Feature request
about: Suggest an addition or change
title: ""
labels: enhancement
assignees: ""
---

## Problem

What are you trying to do that Actus makes hard or impossible today?

## Proposed solution

What you'd like to see. If it adds API surface, sketch how it would read at the
call site (a `Server::with_X(...)` method? a `routes!` option? a middleware?).

## Fit with the framework's principles

Actus is deliberately shaped differently from other Rust web frameworks — see
the [Principles](../../README.md#principles). Which principle does this serve,
and does the proposed shape honor the others? In particular:

- Is this an **HTTP-protocol** concern (→ a named `Server::with_X` feature) or
  an **application** concern (→ `Middleware`)?
- Can a reviewer still answer "what endpoints exist?" from the two macros?

## Alternatives considered

Other approaches, and why this one is better.
