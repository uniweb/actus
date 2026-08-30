# Proposal: route-family contracts

**Owner:** Diego Macrini

**Status:** SHIPPED — 2026-08-30, both phases. **Phase 1** (Actus 1.2.0): the `expects`
floor label, `Controller::actus_expects()` / `actus_prepare()`, the `#[non_exhaustive]`
`Mount` inventory via `Router::mounts()`, `Server::router()`. **Phase 2** (Actus 1.3.0): the
`families` block in `app_routes!`, with the presence check as an `E0277` trait bound and
the accepted-floor check as a `const` assertion — the refinement § Decisions #2 queued,
probed on 1.88 first. The "wait a release cycle" advice in § Why now was overridden the
same day by the owner (*"I don't like leaving work to-do behind"*); the cost that advice
guarded against — designing the grammar twice — was paid by designing it once with the
value check included. Worked example in `examples/advanced`; user docs in README § "Route
families"; the production consumer adopted both phases. *(Doc history: v2, revised
2026-08-30 after an adversarial review that re-derived every checkable claim and verified
the consumer premises; the eight questions v1 left open are **decided** below, and the
running errors list is in
[For a reviewer](#for-a-reviewer--what-to-attack-and-what-you-can-check).)*
**Scope:** post-1.0 API addition. Everything in both phases is **purely additive** — a
defaulted trait method or two, one `#[non_exhaustive]` struct, a `Router` method, a `Server`
accessor, a `#[controller]` attribute key, and an optional `app_routes!` block. Neither
phase needs a `2.0`. The per-request stamp v1 queued for `2.0` turned out to be an
**optimisation, not a capability** — the capability is reachable additively
(§ [Enforcement](#enforcement-key-the-gate-to-the-declaration-not-the-path)), which
also shrinks the corresponding [`2.0` docket](../2.0-docket.md) entry.

---

A way for an application to declare that a **family of mounts** — every controller
under `api/`, under `public/`, under `admin/` — must state what it expects of its
callers, and to have the framework refuse a controller that states nothing.

The framework never learns what the statement *means*. It enforces that one was
made.

---

## What problem this solves

An application segregates its route surface by **client class**, naming each class
with a top-level prefix:

| prefix | who calls it | the expectation |
|---|---|---|
| `public/` | anyone, unauthenticated | read-only; no session ever required |
| `api/` | the first-party web app, over a session | authenticated, except the handful of routes that *establish* the session |
| `admin/` | the same app, elevated | authenticated **and** privileged |
| `hooks/` | an external service | signature-verified, never session-authed |

This is a good design and a common one. The prefix tells a reviewer who is allowed
to be calling before they read a single handler, which turns per-client rules into
checkable invariants instead of per-endpoint hopes.

**Except nothing checks them.** A new controller is mounted under `api/` and simply
does not do the thing every other controller under `api/` does. It compiles, it
serves, and it is wrong. This has happened in production — and the incident was
re-examined for v2: the failure was **precisely an omission** (a controller on a
credentialed family that declared nothing, and served anonymous callers its full
payload), not a wrongly-written policy. That distinction matters, because it is the
omission this design catches.

Three properties make the omission invisible, and all three are structural rather
than anyone's carelessness:

1. **The two halves of the fact live in different files.** The mount
   (`"api/things" => ThingController { db }`) is in `app_routes!`. The posture
   (`#[controller(prepare = …)]`, or its absence) is on the `impl` block, in the
   controller's own module. Neither half looks wrong on its own.
2. **The default is silence.** A controller that declares nothing is well-formed.
   Absence produces no token, no row, no artifact anywhere — there is nothing for a
   reviewer, a grep, or a test to find.
3. **The invariant is prose.** It lives in a design document with a column headed
   *audit invariant*. Prose cannot fail a build.

Actus is the only component that holds both halves. `app_routes!` is the one place
a mount and a controller appear in the same token stream; `Router` is the one place
a mount and a live controller object appear in the same data structure. An
application cannot write this check for itself without re-deriving the route tree
the framework already built — which is why, in practice, nobody does.

### The measurement that prompted this

In the production consumer (measured 2026-08-30): roughly fifty mounted
controllers; about three-quarters share one permissive `prepare` hook; around a
dozen deliberately declare nothing — the anonymous content lanes, the SPA shell,
the signature-authed webhook receiver, a few dev-only surfaces with their own
guard. Establishing that took grepping every controller module and hand-joining
the result against a mount table two hundred lines long in a different file.
**That join exists nowhere in the codebase, in any form, for any reader.**

⚠️ **Do not trust the counts; trust the story of the counts.** The number was
corrected **twice on the day it was first taken** — once because the grep required
a parenthesis and missed bare `#[controller]`, once because closing the motivating
incident changed it. A figure nobody can keep accurate for a single day is the
measurement: the inventory this proposal adds is the only way the join stays true.

## The label is a floor

One definition carries the whole design, so it comes before the mechanism:

> **The label names the least-privileged caller the controller is written to
> accept.** It is a *floor*, not a ceiling and not a policy. A controller labelled
> `"credential"` refuses anonymous callers somewhere; a controller labelled
> `"anonymous"` accepts them — and says nothing about whether some of its routes
> demand more, which stays the handlers' business exactly as today.

Two consequences, both load-bearing later:

- A **green coverage check reads correctly**: "every controller stated its floor,
  and every floor is one its family accepts." Nobody can read a floor as "auth is
  enforced," because a floor visibly is not a ceiling. This is the honest version
  of the guarantee; see [What it does NOT solve](#what-it-does-not-solve).
- The **genuinely mixed controller fits without contortion** — it declares its
  weakest route's floor. See
  [The mixed controller](#the-mixed-controller-the-label-is-the-floor).

## What it does NOT solve

**It cannot tell you a route is protected.** It tells you a route did not join a
family *silently*. That is a strictly smaller promise, and it is deliberately the
largest promise a policy-agnostic framework can honestly make (Principle 7).

The distance is real and worth naming. In the production consumer the shared
`prepare` hook resolves a credential *if one is presented* and passes anonymous
callers through; the actual "you must be signed in" decision lives inside
individual handlers, because it is resource-aware and belongs there. So even
"this controller has a `prepare` hook" would not prove "this controller requires
authentication" — which is exactly why the mechanism below deals in **declarations**
and never in **semantics**.

What closes the remaining distance is enforcement wired to the declaration
(§ [Enforcement](#enforcement-key-the-gate-to-the-declaration-not-the-path)) plus a
test the declaration makes possible
(§ [Second-order win](#second-order-win-the-claim-becomes-falsifiable)).

## Naming: the axis, then the word

v1 called the label `audience` and left the word as open question #1. The review
resolved it, and the resolution is about the **axis**, not the spelling:

- **A label must be true at every mount of the type.** The production consumer
  mounts one controller type at two prefixes belonging to **different client
  classes** — same code, different base path, different transport. No client-class
  value (*"the CLI"*, *"the app"*) is true at both mounts. What *is* true at both
  is what the caller must **present**: a credential. So the value vocabulary that
  works is a **credential posture** — `"credential"`, `"anonymous"`,
  `"signature"`, … — not a client identity.
- **`audience` already means the client class** in the consumer's own design
  vocabulary — it is the *family's* property, the thing the top-level prefix
  names. Borrowing it for the controller's property would put the two meanings one
  attribute apart.
- Two outside readings point the wrong way too: in JWT, `aud` names the token's
  **recipient**; in common doc conventions, an *audience* line names a document's
  **readers**. Both make "audience" the receiving end, and this label describes the
  calling end.

**Decision: the attribute is `expects`.** `#[controller(expects = "credential")]`
— *this controller expects a credential of its callers*; `expects = "anonymous"`,
`expects = "signature"` read the same way. The trait method is `actus_expects()`,
matching the attribute the way `rate_limit` → `actus_rate_limit` does. The value
stays an opaque `&'static str` the framework compares for presence and passes
through, never interprets. (`caller = …` was the runner-up; `access` remains
rejected — it is the removed enum's word and would read as a revived authorization
feature; `audience` is retired for the reasons above.)

The word **family** keeps naming the prefix and its client class — that is what
`families { "api", … }` declares in Phase 2.

## Constraints from the design

Anything shipped here has to survive the principles in `CLAUDE.md`:

- **Principle 7 (policy-agnostic).** Actus ships no roles, no `Access` enum, no
  notion of authentication. It removed one already — the macro still carries the
  tombstone (`crates/actus-controller/macros/src/lib.rs`, the `[Access::*]`
  rejection). **Nothing here may reintroduce it by another name.** The label this
  proposal adds is an opaque string the framework compares for presence and passes
  through; it must never be interpreted.
- **Principle 2 (auditability over uniformity).** The answer to "what does this
  server do?" must stay readable from `Server::new(...).with_X(...)` and the two
  macros. A family contract declared in `app_routes!` *strengthens* that; a
  contract discovered by walking a chain would not.
- **Principle 5 (two macros, one audit surface).** The family declaration belongs
  in `app_routes!`, next to the mounts it constrains — not in a builder call, not
  in a config file, not in an attribute on some third item.
- **Principle 3 (explicit over magic).** A controller's expectation is written on
  the controller. It is never inferred from the mount, from the presence of a
  `prepare` hook, or from anything else the framework could cleverly deduce.

### The precedent, and its blind spot

Actus has already solved a structurally identical problem once, and solved it
well: `#[controller(rate_limit = "…")]` is a **label, not a policy**, and
`Router::rate_limit_classes()` exposes the declared half so an application can diff
it at startup against the classes its limiter has a policy for. A typo'd class
becomes a boot failure instead of a silently-unlimited controller.

That is the shape to follow. But it has a blind spot that matters here:
`Router::rate_limit_classes()` **skips controllers that declared nothing**
(`walk_classes` in `crates/actus-server/src/router.rs` only emits a row when
`actus_rate_limit()` is `Some`). It therefore catches a **misspelling** and is
blind to an **omission**.

For rate limiting that is a tolerable asymmetry. For route families, **omission is
the entire failure mode**. The inventory added here emits a row for every mounted
controller, including the ones that declared nothing — and
`rate_limit_classes()` keeps its shape (changing it is breaking) with its
asymmetry **documented**, pointing at the new inventory for the
absence-inclusive view. *(That resolves v1's open question #4.)*

## Shapes considered

### Shape A — surface the `prepare` hook

The `#[controller(prepare = Self::auth)]` attribute is parsed by the macro and
compiled directly into `actus_dispatch`. It is never surfaced: the `Controller`
trait exposes `actus_describe_routes`, `actus_max_body_bytes`, `actus_rate_limit`
and `__name`, and nothing about `prepare`. Add
`actus_prepare() -> Option<&'static str>` (the hook's path, stringified; **presence
is the payload**, the string is a courtesy for route dumps).

v1 called this "worth doing for its own sake" but "not the answer," since a hook's
presence says nothing about what it enforces. The review **promoted it to a Phase 1
requirement**: it is one half of the boot rule that closes the enforcement loop —
*"a controller whose floor is `credential` must have a hook to refuse anonymous
callers with."* See
[Enforcement](#enforcement-key-the-gate-to-the-declaration-not-the-path).

*Verdict: **in Phase 1, load-bearing.***

### Shape B — an expectation label plus an absence-inclusive inventory

`#[controller(expects = "…")]` records an opaque label; the router exposes one row
**per mounted controller**, carrying `Option<&'static str>`. The application writes
the family rule itself, in `main()`, and fails boot on a violation.

Follows the rate-limit precedent exactly, fixes its blind spot, adds no new
grammar, and leaves every judgement with the application.

*Verdict: **recommended, as Phase 1** — with the inventory shaped as the one
record it should always have been; see [Recommended shape](#recommended-shape).*

### Shape C — a compile-time family contract in `app_routes!`

A `families` block names the prefixes that require a declaration; the macro emits a
trait-bound assertion per mount under one. A controller with no `expects` label
mounted under a listed prefix **fails to compile**.

Catches the failure at the moment the developer adds the mount, in their editor,
rather than at the next boot. Zero runtime cost. Costs new macro grammar and one
public marker trait.

*Verdict: **recommended, as Phase 2** — deferred until Phase 1 has a release cycle
of real use behind it; see [Why now](#why-now-why-not-later).*

### Shape D — the framework owns the family policy

`Server::with_route_families([("api", Auth::Required), ("public", Auth::None)])`,
and Actus enforces it per request.

**Rejected.** This is the `Access` enum with a new spelling. It requires Actus to
have a concept of authentication, to define what "required" means, to decide what a
failure returns, and to be wrong about all three for somebody. It is the exact trade
Principle 7 exists to refuse.

### Shape E — a prefix middleware, no framework change at all

The obvious objection, and the strongest one: an application can write a
`Middleware` today that inspects the request path, and rejects anything under
`api/` without a valid session. That is *real enforcement*, not merely a
declaration, and it needs nothing from Actus.

**It is complementary, and most applications should have one.** It is not a
substitute, for three reasons:

1. **The carve-outs move the bug rather than fixing it.** Every family has them —
   the login routes that must be anonymous inside an authenticated family; the
   unauthenticated discovery route on a client lane. Encoding those in a middleware
   means a path allow-list: a second out-of-band list, maintained by hand, drifting
   from the routes exactly the way the prose invariant does today. The failure mode
   is preserved and relocated. *(Not hypothetical: the production consumer already
   carries one path-keyed middleware exemption, and when the route under it was
   renamed, only a deliberately-planted comment kept the carve-out from silently
   ceasing to carve — a path comparison over segments contains no literal a
   search-and-replace can find.)*
2. **It enforces a floor, not the policy.** The real decisions are resource-aware
   ("may *this* caller modify *this* record") and stay in handlers. A middleware
   that tried to subsume them would duplicate the policy layer.
3. **It is silent about new routes, which is the reported failure.** A new mount
   under a covered prefix is simply swept up. That is fine when the new route wanted
   the family default and invisible when it did not — and *"invisible when it did
   not"* is the case that shipped wrong.

⭐ **The sharper way to say it: the middleware is the right mechanism with the wrong
key.** A gate keyed on the request path protects a *location*; a gate keyed on the
controller's declaration protects a *claim*, needs no allow-list, and covers a
controller mounted anywhere — including somewhere nobody anticipated. Keep the
middleware; change what it reads. See
[Enforcement](#enforcement-key-the-gate-to-the-declaration-not-the-path).

## Recommended shape

**Phase 1 — the label, the inventory, and the accessor.**

```rust
// The controller states the least-privileged caller it accepts. An opaque
// label; Actus compares it for presence and hands it back untouched.
#[controller(expects = "credential", prepare = Self::auth)]
impl ThingController {
    routes! { GET "" => list() }
}
```

```rust
// actus-controller — two new trait methods, both defaulted, so every
// existing controller keeps compiling.
fn actus_expects(&self) -> Option<&'static str> { None }
fn actus_prepare(&self) -> Option<&'static str> { None }   // Shape A
```

```rust
// actus-server — one row per MOUNTED CONTROLLER, not per declaration.
// Framework-populated: consumers read it, they never construct it — so it
// is #[non_exhaustive] from day one, and fields can be added in minors.
#[non_exhaustive]
pub struct Mount {
    pub mount: String,                          // "api/things"; "" for a root mount
    pub controller: &'static str,               // Controller::__name()
    pub expects: Option<&'static str>,          // None = declared nothing
    pub prepare: Option<&'static str>,          // None = no prepare hook
    pub rate_limit_class: Option<&'static str>,
    pub max_body_bytes: Option<usize>,
}

impl Router {
    /// One row per mounted controller — the inventory. Deterministic DFS,
    /// like `routes()`. Absence is a row, never a skip.
    pub fn mounts(&self) -> Vec<Mount>;
}

impl Server {
    /// The router this server serves. Lets application middleware use the
    /// framework's own matcher instead of re-deriving the route tree.
    pub fn router(&self) -> Arc<Router>;
}
```

⭐ **`None` is a row, not a skip.** This is the one place the design departs from
`rate_limit_classes()`, and it is the entire point: the omission has to be
representable before it can be caught.

⭐ **`Mount` is the inventory `Router` should always have had.** v1 sketched a
three-field `audiences()` record and asked (open question #3) whether a future
`inventory()` should consolidate the walks. The review's answer: ship the
consolidated record *now*, `#[non_exhaustive]` so it can grow, and keep
`routes()` / `rate_limit_classes()` as documented projections. One walk, one
record, no third method with a third emptiness semantics — and no repeat of the
freeze the [`2.0` docket](../2.0-docket.md) documents. *(A v1 draft of this very
struct was all-`pub` with no `#[non_exhaustive]` — the exact mistake the docket
records, made while citing it. The general rule now lives in `CLAUDE.md`: every
new public type ships `#[non_exhaustive]` unless consumers must construct it.)*

The application owns the rule, and writes it once. Family membership is tested
**first**, so mounts outside every family — a health probe, a root catch-all —
are unconstrained, as the text has always promised:

```rust
fn family_coverage(router: &Router) -> Result<(), String> {
    // family prefix → the floors it accepts
    let accepted: &[(&str, &[&str])] = &[
        ("public", &["anonymous"]),
        ("api",    &["credential", "anonymous"]), // "anonymous": the login carve-outs, deliberate
        ("admin",  &["credential"]),
        ("hooks",  &["signature"]),
    ];
    let mut bad = Vec::new();
    for m in router.mounts() {
        let family = m.mount.split('/').next().unwrap_or("");
        // Not under a declared family → unconstrained ("health", the "*" shell, …).
        let Some((_, floors)) = accepted.iter().find(|(f, _)| *f == family) else { continue };
        match m.expects {
            None => bad.push(format!(
                "  - {} at `{}` declares no caller expectation", m.controller, m.mount)),
            Some(e) if !floors.contains(&e) => bad.push(format!(
                "  - {} at `{}` declares {:?}, which family `{}` does not accept",
                m.controller, m.mount, e, family)),
            // The boot half of the enforcement loop: a credential floor needs
            // a hook to refuse anonymous callers with (see § Enforcement).
            Some("credential") if m.prepare.is_none() => bad.push(format!(
                "  - {} at `{}` expects a credential but has no `prepare` hook",
                m.controller, m.mount)),
            _ => {}
        }
    }
    if bad.is_empty() { Ok(()) } else { Err(bad.join("\n")) }
}
```

Every carve-out is now **a line of code somebody had to write**, in one file, in a
table a reviewer reads top to bottom. That is the artifact that does not exist
today in any form.

**Phase 2 — the presence half of the check, moved to compile time.**

```rust
app_routes! {
    families { "public", "api", "admin", "hooks" }   // these prefixes require a declaration
    deps { db = Database::connect(&url).await?, }
    routes {
        "api/things" => ThingController { db },
        "health"     => HealthController,          // not in a family; unconstrained
        "*"          => SpaController,             // likewise
    }
}
```

For each route whose mount is under a listed prefix, the macro wraps the
construction in a pass-through assertion:

```rust
.add_route("api/things", Arc::new(::actus::__internal::declares_expectation(ThingController { db })))
```

```rust
// actus-controller
pub fn declares_expectation<T: Controller + DeclaresExpectation>(c: T) -> T { c }

#[diagnostic::on_unimplemented(
    message = "`{Self}` is mounted under a route family that requires a declared caller expectation",
    label = "this controller declares no `expects` label",
    note = "add `expects = \"…\"` to its `#[controller(...)]` attribute, or drop the \
            prefix from the `families` block in `app_routes!`"
)]
pub trait DeclaresExpectation {}
```

`#[controller(expects = "…")]` emits `impl DeclaresExpectation for ThingController {}`.
A controller with no label, mounted under a declared family, is a compile error.

**Verified, not assumed** — compile-probed against the 1.88 MSRV toolchain on
2026-08-30 (`rustc +1.88 --edition 2024`). This paste is the *complete* output the
mechanism produces (a v1 paste omitted the two extra `help:` lines the compiler
adds):

```text
error[E0277]: `SpaController` is mounted under a route family that requires a declared caller expectation
   |
19 |     let _ = declares_expectation(SpaController);
   |             -------------------- ^^^^^^^^^^^^^ this controller declares no `expects` label
   |             |
   |             required by a bound introduced by this call
   |
   = help: the trait `DeclaresExpectation` is not implemented for `SpaController`
   = note: add `expects = "…"` to its `#[controller(...)]` attribute, or drop the prefix from the `families` block in `app_routes!`
   = help: the trait `DeclaresExpectation` is implemented for `ThingController`
note: required by a bound in `declares_expectation`
```

Both halves hold at MSRV: `#[diagnostic::on_unimplemented]` is accepted (stabilized
in Rust 1.78, comfortably inside 1.88), and the custom `message` / `label` / `note`
all render — so the error a developer meets is the one written above, not a bare
unsatisfied-trait-bound.

Wrapping the **expression** rather than naming the type is what makes this work for
any construction form — `Foo { db }`, `Foo::new(db)`, `make_foo()` — with no type
extraction in the macro and no runtime cost. And the bound always applies:
`add_route` takes `Arc<dyn Controller>` and the macro emits `Arc::new(expr)`, so
every construction is a concrete `T: Controller`.

### Which phase does what

| | Phase 1 | Phase 2 |
|---|---|---|
| catches a **missing** declaration | at boot | **at compile time** |
| catches a **wrong** declaration | at boot | at boot (unchanged) |
| catches a credential floor with **no hook** | at boot | at boot (unchanged) |
| new public API | 2 trait methods, 1 struct, 1 `Router` method, 1 `Server` method, 1 attribute key | + 1 marker trait, + 1 `app_routes!` block |
| the check can be skipped by | not calling it | nothing (presence); not calling it (value) |
| enables a **per-request** gate | yes — via `Server::router()` or the hook (see below) | unchanged |

Phase 2 does not replace Phase 1's startup check — it only guarantees *presence*.
Whether the declared value is the right one for the family stays a string
comparison, and that stays at boot.

## Enforcement: key the gate to the declaration, not the path

A declaration is worth more than an audit if something can *act* on it per request.

**The question that raised this.** A family reserved for authenticated callers must
still expose the routes that *establish* authentication — login, registration, an
OAuth callback — so the family has deliberate holes, and the tempting fix is to
**hoist** those routes into their own top-level family. Hoisting is the wrong
lever: it re-parents endpoints without merging them, it creates a top-level lane
with *no* invariant at all (an auth surface is irreducibly mixed — see
[The mixed controller](#the-mixed-controller-the-label-is-the-floor)), and it
spends the top-level path segment — the one thing every prefix-scoped intermediary
keys on — to buy a property a label provides for free. **The general rule: the
top-level segment is already spent. Anything else you want to segregate belongs at
controller granularity, where a declaration is cheap and the framework can check
it.** *(The long-form version of this argument lives with the production consumer's
design records, where the concrete case was decided; the rule above is the part
that generalises.)*

### The gate, additively — v1 was wrong here

v1 concluded that a per-request gate needed the framework to stamp the label onto
`Request`, that the stamp was a `2.0` change (true — see below), and that until
then an application had to re-implement longest-prefix matching over a boot-built
map, where "a subtly different match is a subtly different security boundary."
**The review overturned the conclusion.** Three facts, all checkable:

1. **`Router::match_controller` is public** (`crates/actus-server/src/router.rs`).
   The exact matcher the server uses is callable by anyone holding the router.
   Nothing needs re-implementing, so the "subtly different boundary" hazard does
   not exist.
2. **`RouteMatch.controller: Arc<dyn Controller>` exposes every trait method** —
   the label, the rate-limit class, the name. Everything a stamp would carry is one
   tree walk away.
3. The only missing piece is that **nothing can hold the router**: `Router` is not
   `Clone`, `Server::new` moves it into a private `Arc`, and there is no accessor.
   **`Server::router()` — one additive method — closes the gap.**

```rust
let server = Server::new(router);
let router = server.router();                        // Arc<Router>
let server = server.with_middleware(Gate { router });
```

```rust
async fn before(&self, req: &mut Request) -> Result<Outcome, WebError> {
    let Some(rm) = self.router.match_controller(&req.path_parts) else {
        return Ok(Outcome::Continue);               // no route → the 404 path handles it
    };
    if rm.controller.actus_expects() == Some("credential") && !carries_credential(req) {
        return Err(WebError::Unauthorized);
    }
    Ok(Outcome::Continue)
}
```

No allow-list, no hand-rolled matcher, no `2.0`. The cost is a second tree walk
per request — a `HashMap` hop per path segment: **32–93 ns measured** on a
50-mount tree (2026-08-30, release build, 2–5 segment paths, including the
`Arc` clone and the `actus_expects()` call). The `Request` stamp is therefore an
**optimisation** (one walk instead of two, plus discoverability beside
`rate_limit_class`), and it is queued on the [`2.0` docket](../2.0-docket.md) as
exactly that. The finding generalises: any per-request "stamp X onto `Request`"
whose X is a `Controller` trait method is reachable the same way, because the
stamp field on `Request` is a breaking change for all of them —
`Request` is all-`pub` with no `#[non_exhaustive]` and no private field, so a new
field breaks downstream struct literals (verified 2026-08-30; the full
extensibility finding is the docket's § 1).

### Where the gate belongs when the hook resolves credentials

There is a subtlety the middleware form runs into, and a consumer shaped like ours
will hit it: **"is a credential present *and valid*?" is answered by the shared
`prepare` hook**, with store access — and the hook runs *after* the middleware
chain. `Params::insert`, the per-request state channel, exists only from `prepare`
onward; a `before` middleware sees `&mut Request` and nothing downstream of it. So
a middleware gate must either check mere **presence** (cheap, storeless — and it
already catches the motivating incident, where the caller presented nothing at
all) or resolve the credential a **second** time.

**The prepare hook is the better enforcement point for that consumer — and it
already holds everything it needs.** The hook is a method on the controller, so
`self.actus_expects()` is readable from inside it. The one shared hook body
becomes declaration-aware — *if my floor is `"credential"` and no credential
resolved, refuse* — and every controller that delegates to it enforces its own
label, with the credential resolved once and stashed once. No middleware, no
double lookup, no framework change beyond Phase 1.

The hole in that design is a labelled controller **with no hook** — which is
precisely what Shape A closes: `Mount.prepare` puts hook presence in the
inventory, and the boot rule in the sketch above requires *floor = credential ⇒
hook present*. The loop is then closed end to end:

| the guarantee | checked by | when |
|---|---|---|
| every family member declares a floor | `family_coverage` (Phase 1) or the `families` block (Phase 2) | boot / compile |
| a credential floor has a hook to enforce it | `family_coverage`, via `Mount.prepare` | boot |
| the hook refuses callers below the floor | the shared hook, reading `self.actus_expects()` | per request |
| the floor is *true*, not merely declared | the probe test | CI |

Both gate forms stay application code, and what "a credential" means stays
entirely the application's — the framework supplied a label, an inventory, and an
accessor.

## The mixed controller: the label is the floor

*(Resolves v1's open question #7 — "is 'split it' a good enough answer?" It is
not, and the floor semantics above are the answer.)*

The archetype is an auth controller, and the production consumer has exactly one:
`login`, `register`, verification and reset flows are anonymous — they *establish*
the session — while `logout`, `me` and step-up re-authentication require one. No
single "what this controller requires" value is honest for the whole type. v1's
answer was to split it into an anonymous controller and an authenticated sibling.
For a shipped application that answer is expensive in a way v1 did not price:

- moving the session-required routes to a new mount **changes wire URLs** that
  deployed clients already call; or
- keeping the URLs means **one mount per route**, because longest-prefix routing
  makes each hoisted route its own mount.

**The floor dissolves the dilemma.** The mixed controller declares its weakest
route's floor — `expects = "anonymous"`, honestly: it accepts anonymous callers —
and its stricter routes keep enforcing in their handlers, exactly as they do
today. The family rule accepts `("api", "anonymous")` as the deliberate,
reviewable carve-out. What remains outside the machine-checked floor is then
**small and enumerable**: the `"anonymous"`-labelled controllers inside
credentialed families — a handful in the production consumer — and a reviewer
reads those few by hand. That is the "line of code somebody had to write."

Splitting stays the *better* shape where it is free — two controllers each
carrying a strong claim beat one carrying a weak one — and the README should say
both halves: recommend the split for new surfaces, state the floor semantics for
shipped ones. A per-route label stays out of scope; it would be additive later if
a real consumer outgrows the floor.

## Second-order win: the claim becomes falsifiable

This is the part worth more than the check itself.

Once every controller in a family carries a label, the application can write **one**
test that walks the inventory, fires an unauthenticated request at each mounted
route, and asserts the response agrees with the declared floor — a controller
claiming `"credential"` must not answer `200`; one claiming `"anonymous"` must not
answer `401`.

Actus still verifies nothing about authentication. But it is the only thing that can
**enumerate every route**, and enumeration is what turns a claim from documentation
into something a test can refute. A route added tomorrow is in the inventory the
moment it is mounted, so it is in that test too, with nobody remembering to add it.

Two preconditions the test must honor, or it lies:

- **Pair every probe with an authenticated control.** A handler that looks the
  resource up before checking the caller answers `404` to a synthetic id — and
  "not `200`" passes for a *dead* route just as it does for a guarded one. The
  control request must get its `200`, so a broken route cannot masquerade as a
  working guard. *(The production consumer's fix for the motivating incident
  already carries exactly this control; generalise it.)*
- **Walk `routes()`, not just mounts.** A mount root is often a `404`/`405`, which
  proves nothing; patterns with `{id}` parameters need synthetic values.

That is the honest division of labour: **the framework supplies the enumeration and
insists on the claim; the application supplies the probe that checks it.**

## Implementation sketch

### `actus-controller`

- `Controller::actus_expects(&self) -> Option<&'static str>`, defaulted to `None`.
- `Controller::actus_prepare(&self) -> Option<&'static str>`, defaulted to `None`.
- `pub trait DeclaresExpectation {}` with the `on_unimplemented` diagnostic (Phase 2).
- `pub fn declares_expectation<T: Controller + DeclaresExpectation>(c: T) -> T`
  (Phase 2), re-exported through `actus::__internal`.

### `actus-controller-macros`

- `#[controller]` parses `expects = "…"` alongside `strict` / `lax` / `prepare` /
  `max_body_bytes` / `rate_limit`; emits `actus_expects`, emits `actus_prepare`
  (the stringified hook path) when `prepare` is set, and (Phase 2)
  `impl DeclaresExpectation`.
- `app_routes!` parses an optional `families { "a", "b" }` block. `AppRoutesInput`
  grows one field; `generate_app_routes` wraps constructions whose mount is under a
  listed prefix. **Segment-wise prefix match** — `"api"` covers `"api/auth/oauth"`
  but not `"apiary"` — and it strips the trailing-`*` sugar the way `add_route`
  does, so `"api/*"` and `"api"` constrain identically.
- A `families` entry matching no mount is a compile **error**. *(v1 asked
  warn-or-error; there is no "warn": stable proc macros cannot emit warnings —
  `proc_macro::Diagnostic` is unstable — only errors or a deprecated-item hack.
  Error is also the symmetric choice: a typo'd family silently constraining
  nothing is the same failure this feature exists to prevent, one level up. The
  legitimate "family declared before its first controller" case costs one line
  when that controller lands.)*

### `actus-server`

- `#[non_exhaustive] pub struct Mount`; `Router::mounts()` walking the tree the way
  `routes()` does (children sorted, deterministic DFS), emitting a row for **every**
  node bearing a controller.
- `Server::router(&self) -> Arc<Router>` — clone of the server's `Arc`.
- `rate_limit_classes()` keeps its shape; its rustdoc gains the asymmetry note and
  a pointer to `mounts()`.
- **Queued for `2.0` as an optimisation** (see
  [Enforcement](#enforcement-key-the-gate-to-the-declaration-not-the-path)):
  `#[non_exhaustive]` on `Request` plus `pub expects: Option<&'static str>` set at
  the same site that already sets `rate_limit_class`.

### Tests

- Macro: `expects` parses; combines with every other attribute key; emits the
  trait impl (Phase 2); `actus_prepare` reflects the attribute.
- Router: `mounts()` emits a row for an undeclared controller — *the regression
  test for the blind spot this design exists to fix*; deterministic ordering; `""`
  for a root mount; the full record round-trips the other declarations.
- `Server::router()` returns a router that matches what the server serves.
- `app_routes!`: nested prefixes are covered; `"apiary"` is not covered by `"api"`;
  `"api/*"` behaves as `"api"`; an unlisted mount is unconstrained.
- Compile-fail (Phase 2): a `trybuild` case asserting the missing-label error and
  that its message is the `on_unimplemented` one. This is the first `trybuild`
  dependency in the workspace — see effort, below.
- `examples/advanced` grows `family_coverage` next to `rate_limit_coverage`,
  wired into its existing `--check` flag, with unit tests for a missing label, a
  wrong label, a credential floor with no hook, and a pass.
- `examples/advanced` also grows the **gate** as a `Middleware` beside its rate
  limiter — `Server::router()` + `match_controller` + `actus_expects`, refusing a
  `"credential"` mount with no credential header. Without it the proposal ships a
  *declaration* with no worked example of anything acting on one, which is how a
  feature gets read as documentation-only. An integration test drives a real
  request at an `"anonymous"` mount and a `"credential"` mount and asserts the two
  outcomes. *(A consumer whose hook resolves credentials can enforce in the hook
  instead — the example's README note says so and points here.)*

### Docs

- README: a "Route families" section after "Middleware", framed as *coverage, not
  authorization*, opening with the floor definition, and stating both answers for
  the mixed controller (split when free; floor when shipped).
- The `actus_expects` rustdoc carries the same "label, not a policy" paragraph
  `actus_rate_limit` does — that doc comment is the load-bearing one, because it is
  where a reader decides whether this is an auth feature. It is not.
- CHANGELOG under Added, per phase.

## What this is *not* trying to do

- **Authenticate anything.** No credential parsing, no session concept, no 401
  policy. `WebError::Unauthorized` already exists and stays the application's to
  return.
- **Replace the policy layer.** Resource-aware decisions stay in handlers, where the
  resource is.
- **Per-route labels.** The label is per-controller, matching `max_body_bytes`
  and `rate_limit`; the floor semantics absorb the mixed case
  (§ [The mixed controller](#the-mixed-controller-the-label-is-the-floor)). A
  per-route override would be additive later, if a real consumer outgrows the floor.
- **Enforce anything itself.** Actus supplies the declaration, the inventory, and
  the accessor that lets application code act per request. The gate — middleware or
  hook — is application code, and what "a valid credential" means stays entirely
  the application's. `mounts()` is one tree walk at startup; the Phase 2 bound is
  compile-time; the middleware gate costs one extra tree walk per request only in
  applications that choose it.
- **Guess a floor from the mount.** A controller under `public/` that forgot its
  label is precisely the case being caught; inferring `"anonymous"` for it would
  invert the feature.

## Decisions taken in v2 — formerly the open questions

1. **The word** → `expects`, a credential-posture value; `audience` retired
   (wrong axis on three counts — see [Naming](#naming-the-axis-then-the-word)).
2. **String or type?** → **String.** It matches `rate_limit`, keeps the family
   rule as ordinary data, and stays printable in a route dump. The stronger
   compile-time *value* check does not require marker types anyway: `#[controller]`
   could additionally emit `const EXPECTS: Option<&'static str>` and a `families`
   block with values could emit a post-monomorphisation `const` assertion via a
   `const fn` string compare — queued as a Phase 2 refinement, **to be
   compile-probed against 1.88 before it is relied on**, as the
   `on_unimplemented` mechanism was.
3. **A new `Router` method or extend `routes()`?** → Neither: **ship the inventory
   record now** (`Mount`, `#[non_exhaustive]`, growable in minors), with the
   existing methods kept as documented projections. No third walk with a third
   emptiness semantics.
4. **Fix `rate_limit_classes()`?** → Keep its shape (changing it is breaking),
   **document the asymmetry** in its rustdoc, point at `mounts()` for the
   absence-inclusive view.
5. **`families` entry matching no mount** → **error**; "warn" does not exist on
   stable proc macros (see the sketch).
6. **One type mounted in two families** → dissolved by the axis decision: a
   credential-posture value is true at every mount of the type, and the family
   rule maps a family to a **set** of acceptable floors. No per-mount override.
7. **The mixed controller** → **the label is the floor**
   (§ [The mixed controller](#the-mixed-controller-the-label-is-the-floor));
   split when free, floor when shipped.
8. **The stamp and `#[non_exhaustive]` on `Request`** → on the
   [`2.0` docket](../2.0-docket.md) as **one item, demoted to an optimisation**;
   the additive escape v1 asked for exists and is `Server::router()` +
   `Router::match_controller` (an escape *through* `Request` does not — a typed
   extensions slot or a private field each break the same struct literals).

## For a reviewer — what to attack, and what you can check

This proposal is written to be argued with. A first adversarial review ran on
2026-08-30 — it re-derived every claim below, verified the consumer premises
in the consumer, and produced this v2. Its corrections are folded in; what follows
is updated for the next reviewer.

### The load-bearing assumptions, with the first review's verdicts

1. **That declaration coverage is worth anything at all.** The whole design rests on
   *"a claim that can be omitted silently is the failure; a claim that must be made is
   the fix."* *First review: upheld — the motivating incident was re-examined and was
   precisely an omission, not a wrong policy; and the coverage count itself was
   mis-measured twice in one day, which is the "the join exists nowhere" argument
   made by the consumer's own history.*
2. **That false confidence is manageable.** *First review: only if the label does
   something.* A label nothing reads is documentation; a label the hook or gate reads
   is a floor. The mitigation is the enforcement loop, not naming discipline — and
   the floor definition makes the green check's honest meaning explicit.
3. **That per-controller granularity is the right unit.** *First review: yes, with
   floor semantics — without them, the first real consumer's auth controller breaks
   the model.*
4. **That the label will not be read as authorization.** *First review: the sharper
   risk was the label being read on the wrong axis — resolved by the rename and the
   floor definition.*
5. **That Phase 2's grammar earns its place.** *First review: not yet — Phase 1 plus
   the boot rule gives the same presence guarantee at boot, and the consumer boots in
   CI. Unchanged: decide Phase 2 on a release cycle of evidence.*

### What you can verify yourself, in this repo

Everything in this class is checkable and **should be checked rather than believed**
(all of it re-verified 2026-08-30):

- `Router::rate_limit_classes()` skips undeclared controllers — read `walk_classes`
  in `crates/actus-server/src/router.rs`.
- `Router::match_controller` is public, and `RouteMatch.controller` exposes the
  `Controller` trait methods — same file.
- `Router` is not `Clone`; `Server::new` moves it into a private `Arc`; no accessor
  exists today — `crates/actus-server/src/server.rs`.
- The `Controller` trait surfaces routes, body cap, rate-limit class and name, and
  nothing about `prepare` — read the trait in `crates/actus-controller/src/lib.rs`
  and the methods the macro emits in `crates/actus-controller/macros/src/lib.rs`.
- The `[Access::*]` rejection still exists in that macro (the tombstone the design
  must not reopen).
- `app_routes!` holds the mount literal and the construction *expression*, and emits
  `Arc::new(expr)` — which is what makes Phase 2's pass-through wrapper viable and
  its bound universal — read `generate_app_routes`.
- The middleware `before` chain runs after routing and **before** `Params` exists —
  read the lifecycle comment and body of `handle_request_inner` in
  `crates/actus-server/src/server.rs`.
- **The MSRV probe.** Re-run it; do not take the pasted output on faith:
  `rustc +1.88 --edition 2024` on a file with the `on_unimplemented` trait, a
  conforming type and a non-conforming one.
- **The semver finding** — run the query in [`../2.0-docket.md`](../2.0-docket.md)
  § 1 and confirm that no public enum or all-public struct is `#[non_exhaustive]`.

### What you cannot verify from this repo, and should treat as premise

The claims about the production consumer — the controller and hook counts, the
permissive shared hook, the mixed auth surface, the twice-mounted controller type,
"this has happened in production" — come from a **private codebase this repo does
not contain**. The first review verified each of them *in that codebase* on
2026-08-30, but a reader here still cannot. ⇒ **Do not spend time trying.** Do
challenge them *as premises*: if the motivating failure is better explained by
something other than a missing declaration, the case weakens no matter how sound
the mechanism. Ask for the evidence rather than assuming it.

### Where this proposal has been wrong — the running list

Stated so a reviewer knows the error rate is not zero, and what shape the errors
take (so far: every one is an **unchecked additivity or completeness claim**):

- An early draft folded the `Request` stamp into Phase 1 and called it additive;
  checking the struct disproved it (`Request` is all-`pub`, no `#[non_exhaustive]`).
- v1 then concluded the stamp was the only path to a per-request gate; checking the
  `Router` surface disproved *that* — `match_controller` was public all along, and
  one accessor closes the gap. The stamp is an optimisation.
- v1's inventory struct was itself all-`pub` with no `#[non_exhaustive]` — the
  docket's finding, repeated while citing the docket.
- v1's coverage sketch failed every mount *outside* a family (`"health"`, the `"*"`
  shell), contradicting its own comment two lines up.
- v1 offered "warn" for an unmatched `families` entry; stable proc macros cannot
  warn.
- v1's pasted compiler output was incomplete; v2's is the full paste.
- v1 claimed downstream `Request` struct literals are "the natural shape for a
  middleware unit test in any consumer"; the measured consumer has none. (Actus's
  own tests do — the constructor need on the docket stands, scoped.)

**The same class of error may still be present.** The additivity claims in § Scope
and the claim that `Server::router()` + `match_controller` is gate-sufficient are
the ones to re-derive rather than read.

## Estimated effort

**Phase 1** — roughly 250–350 lines across three crates:
- macro: `expects` parsing + `actus_expects` / `actus_prepare` emission (~50)
- `Mount` + `Router::mounts()` walk + `Server::router()` (~60)
- tests (~80)
- `examples/advanced`: `family_coverage` + the gate middleware + their tests (~90)
- README + rustdoc (prose)

**Queued for `2.0`** — trivial in code, expensive in version, and now merely an
optimisation: `#[non_exhaustive]` on `Request` + the `expects` field + one line at
the stamp site, plus a `Request` constructor for fixtures.

**Phase 2** — roughly 200–300 lines on top:
- `families` parsing in `AppRoutesInput` + prefix matching (with `*`-sugar
  stripping) + construction wrapping (~90)
- `DeclaresExpectation`, the diagnostic, the pass-through fn, `__internal`
  re-export (~40)
- `trybuild` compile-fail tests (~60, plus a new dev-dependency and a `cargo-deny`
  licence pass on it — the workspace has no `trybuild` today, and adding a
  dev-dependency is the one part of this that touches the supply chain)
- tests + docs (~80)

## Why now, why not later

**Neither phase is blocked by the 1.0 freeze**, which is the unusual and comfortable
part: defaulted trait methods, a `#[non_exhaustive]` struct, new `Router`/`Server`
methods, a new attribute key and a new optional macro block are all additive. There
is no forcing function, and this can rest.

The argument for doing it anyway is that the cost of *not* having it is paid by
someone else, quietly, and is only ever discovered in production — as it already
was, once. The production consumer's families are correct today, but that is a fact
established by hand, on one afternoon, and it decays the moment the next controller
lands. Every application with a segregated route surface has this problem and none
of them can solve it without the route tree.

The argument for waiting is Phase 2's grammar: `families` is a permanent addition to
the one macro that is meant to be the whole audit surface, and it should not be
designed twice. Phase 1 buys the information needed to design it well — including
whether the string label survives contact with a second consumer.

⇒ **Suggested order:**

1. **`cargo-semver-checks` in CI first** (the [`2.0` docket](../2.0-docket.md) § 3
   already calls for it). This proposal asserts additivity and has been wrong about
   additivity twice; make the claim machine-checked before the first additive
   release ships.
2. **Ship Phase 1** as a minor release, use it in the production consumer for a
   release cycle — the coverage check, the declaration-aware hook, the probe test
   with its authenticated controls.
3. **Decide Phase 2 on that evidence.** The same discipline the body-cap proposal
   applied to its own Phase 2, and for the same reason.
