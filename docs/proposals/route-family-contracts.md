# Proposal: route-family contracts

**Owner:** Diego Macrini

**Status:** PROPOSAL — 2026-08-30. Nothing here has shipped. Two phases, separately
shippable; Phase 1 stands alone and solves the reported problem, Phase 2 moves the
same check from boot to compile time.
**Scope:** post-1.0 API addition. Both phases are **purely additive** — a trait
method with a default body, a `Router` method, a `#[controller]` attribute key, and
an optional `app_routes!` block. Neither needs a `2.0`. One refinement does, and
is queued rather than folded in: stamping the audience onto `Request`
(§ [Enforcement](#enforcement-key-the-gate-to-the-declaration-not-the-path)) — the
reason is a finding about the 1.0 surface that outlives this proposal.

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
serves, and it is wrong. This has happened in production.

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

In the production consumer, measured 2026-08-30 (re-run with a `#[controller` grep
over its controller modules): 47 `#[controller]` attributes across 49 controller
modules. 36 declare a `prepare` hook; **11 declare none**. All 11 are legitimate —
the anonymous content lanes, the webhook receiver, the SPA shell, and one
deliberately-anonymous discovery route sitting inside an otherwise-authenticated
family. But establishing that took grepping 49 files and hand-joining the result
against a mount table two hundred lines long in a different file. **That join
exists nowhere in the codebase, in any form, for any reader.**

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

What closes the remaining distance is a test, not a static check — and the proposal
makes that test *possible*; see [Second-order win](#second-order-win-the-claim-becomes-falsifiable).

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
- **Principle 3 (explicit over magic).** A controller's audience is written on the
  controller. It is never inferred from the mount, from the presence of a `prepare`
  hook, or from anything else the framework could cleverly deduce.

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
the entire failure mode**. Any inventory added here must emit a row for every
mounted controller, including the ones that declared nothing. See
[Open questions](#open-questions-for-the-review) #4 on whether to fix the
rate-limit method to match.

## Shapes considered

### Shape A — expose the `prepare` hook, change nothing else

The `#[controller(prepare = Self::auth)]` attribute is parsed by the macro and
compiled directly into `actus_dispatch`. It is never surfaced: the `Controller`
trait exposes `actus_describe_routes`, `actus_max_body_bytes`, `actus_rate_limit`
and `__name`, and nothing about `prepare`. Add `actus_prepare() -> Option<&'static str>`
returning the hook's path as a string, and an application can audit "every
controller under `api/` has a hook."

**Cheapest possible change, and genuinely worth doing for its own sake** — a route
dump that omits the hook name is hiding a fact the framework has. But it does not
solve the problem: as established above, a `prepare` hook's presence says nothing
about what it enforces. Auditing for it would produce a green check over an
unenforced family, which is worse than no check.

*Verdict: do it, for auditability. Do not call it the answer.*

### Shape B — an audience label plus an absence-inclusive inventory

`#[controller(audience = "…")]` records an opaque label; `Router::audiences()`
returns one row **per mounted controller**, carrying `Option<&'static str>`. The
application writes the family rule itself, in `main()`, and fails boot on a
violation.

Follows the rate-limit precedent exactly, fixes its blind spot, adds no new
grammar, and leaves every judgement with the application.

*Verdict: **recommended, as Phase 1.***

### Shape C — a compile-time family contract in `app_routes!`

A `families` block names the prefixes that require a declaration; the macro emits a
trait-bound assertion per mount under one. A controller with no `audience` mounted
under a listed prefix **fails to compile**.

Catches the failure at the moment the developer adds the mount, in their editor,
rather than at the next boot. Zero runtime cost. Costs new macro grammar and one
public marker trait.

*Verdict: **recommended, as Phase 2.***

### Shape D — the framework owns the family policy

`Server::with_route_families([("api", Auth::Required), ("public", Auth::None)])`,
and Actus enforces it per request.

**Rejected.** This is the `Access` enum with a new spelling. It requires Actus to
have a concept of authentication, to define what "required" means, to decide what a
failure returns, and to be wrong about all three for somebody. It is the exact trade
Principle 7 exists to refuse.

### Shape E — a prefix middleware, no framework change at all

The obvious objection, and the strongest one: an application can write a
`Middleware` today that inspects `request.path`, and rejects anything under `api/`
without a valid session. That is *real enforcement*, not merely a declaration, and
it needs nothing from Actus.

**It is complementary, and most applications should have one.** It is not a
substitute, for three reasons:

1. **The carve-outs move the bug rather than fixing it.** Every family has them —
   the login routes that must be anonymous inside an authenticated family; the
   unauthenticated discovery route on a client lane. Encoding those in a middleware
   means a path allow-list: a second out-of-band list, maintained by hand, drifting
   from the routes exactly the way the prose invariant does today. The failure mode
   is preserved and relocated.
2. **It enforces a floor, not the policy.** The real decisions are resource-aware
   ("may *this* caller modify *this* record") and stay in handlers. A middleware
   that tried to subsume them would duplicate the policy layer.
3. **It is silent about new routes, which is the reported failure.** A new mount
   under a covered prefix is simply swept up. That is fine when the new route wanted
   the family default and invisible when it did not — and *"invisible when it did
   not"* is the case that shipped wrong.

The mechanism below ties the declaration to the controller, in the file the author
already has open, and puts the carve-outs in one reviewable `match`.

⭐ **The sharper way to say it: the middleware is the right mechanism with the wrong
key.** A gate keyed on `request.path` protects a *location*; a gate keyed on the
controller's declaration protects a *claim*, needs no allow-list, and covers a
controller mounted anywhere — including somewhere nobody anticipated. Keep the
middleware; change what it reads. See
[Enforcement](#enforcement-key-the-gate-to-the-declaration-not-the-path).

## Recommended shape

**Phase 1 — the label and the inventory.**

```rust
// The controller states what it expects of callers. An opaque label; Actus
// compares it for presence and hands it back untouched.
#[controller(audience = "session", prepare = Self::auth)]
impl ThingController {
    routes! { GET "" => list() }
}
```

```rust
// actus-controller — a new trait method, defaulted, so every existing
// controller keeps compiling.
fn actus_audience(&self) -> Option<&'static str> { None }
```

```rust
// actus-server — one row per MOUNTED CONTROLLER, not per declaration.
pub struct MountAudience {
    pub mount: String,                    // "api/things"; "" for a root mount
    pub controller: &'static str,         // Controller::__name()
    pub audience: Option<&'static str>,   // None = declared nothing
}

impl Router {
    pub fn audiences(&self) -> Vec<MountAudience>;
}
```

⭐ **`None` is a row, not a skip.** This is the one place the design departs from
`rate_limit_classes()`, and it is the entire point: the omission has to be
representable before it can be caught.

The application owns the rule, and writes it once:

```rust
fn audience_coverage(router: &Router) -> Result<(), String> {
    let mut bad = Vec::new();
    for row in router.audiences() {
        let family = row.mount.split('/').next().unwrap_or("");
        match (family, row.audience) {
            ("public", Some("anonymous")) => {}
            ("api",    Some("session"))   => {}
            ("api",    Some("anonymous")) => {}   // the login routes — deliberate
            ("admin",  Some("session"))   => {}
            ("hooks",  Some("signature")) => {}
            (_, None) => bad.push(format!(
                "  - {} at `{}` declares no audience", row.controller, row.mount)),
            (f, Some(a)) => bad.push(format!(
                "  - {} at `{}` declares {:?}, which family `{}` does not accept",
                row.controller, row.mount, a, f)),
        }
    }
    if bad.is_empty() { Ok(()) } else { Err(bad.join("\n")) }
}
```

Every carve-out is now **a line of code somebody had to write**, in one file, in a
`match` a reviewer reads top to bottom. That is the artifact that does not exist
today in any form.

**Phase 2 — the same check, moved to compile time.**

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
.add_route("api/things", Arc::new(::actus::__internal::declares_audience(ThingController { db })))
```

```rust
// actus-controller
pub fn declares_audience<T: Controller + DeclaresAudience>(c: T) -> T { c }

#[diagnostic::on_unimplemented(
    message = "`{Self}` is mounted under a route family that requires a declared audience",
    label = "this controller declares no audience",
    note = "add `audience = \"…\"` to its `#[controller(...)]` attribute, or drop the \
            prefix from the `families` block in `app_routes!`"
)]
pub trait DeclaresAudience {}
```

`#[controller(audience = "…")]` emits `impl DeclaresAudience for ThingController {}`.
A controller with no audience, mounted under a declared family, is a compile error.

**Verified, not assumed** — compile-probed against the 1.88 MSRV toolchain on
2026-08-30 (`rustc +1.88 --edition 2024`), which is what the mechanism produces:

```text
error[E0277]: `SpaController` is mounted under a route family that requires a declared audience
   |
24 |     let _ = declares_audience(SpaController);
   |             ----------------- ^^^^^^^^^^^^^ this controller declares no audience
   |             |
   |             required by a bound introduced by this call
   |
   = note: add `audience = "…"` to its `#[controller(...)]` attribute
```

Both halves hold at MSRV: `#[diagnostic::on_unimplemented]` is accepted (stabilized
in Rust 1.78, comfortably inside 1.88), and the custom `message` / `label` / `note`
all render — so the error a developer meets is the one written above, not a bare
unsatisfied-trait-bound.

Wrapping the **expression** rather than naming the type is what makes this work for
any construction form — `Foo { db }`, `Foo::new(db)`, `make_foo()` — with no type
extraction in the macro and no runtime cost.

### Which phase does what

| | Phase 1 | Phase 2 |
|---|---|---|
| catches a **missing** declaration | at boot | **at compile time** |
| catches a **wrong** declaration | at boot | at boot (unchanged) |
| new public API | 1 trait method, 1 struct, 1 `Router` method, 1 attribute key | + 1 marker trait, + 1 `app_routes!` block |
| the check can be skipped by | not calling it | nothing |
| enables a **runtime** gate | yes — via a boot-built map (see below) | unchanged |

Phase 2 does not replace Phase 1's startup check — it only guarantees *presence*.
Whether the declared value is the right one for the family stays a string
comparison, and that stays at boot.

## Enforcement: key the gate to the declaration, not the path

A declaration is worth more than an audit if something can *act* on it per request.
This section is what a real design question produced, and the answer generalises past
the case that raised it.

**The question.** An application with client-segregated top-level prefixes hits an
awkwardness: a family reserved for authenticated callers must still expose the routes
that *establish* authentication — login, registration, credential reset, an OAuth
callback. So the family has a hole in it, and the invariant "everything under this
prefix is authenticated" is false by design. The tempting fix is to **hoist**: move
those routes to a top-level `auth/` family, leaving the original prefix uniform and
gateable by a blanket path rule.

**Hoisting is the wrong lever, and the reason is worth stating in general terms:** it
restructures the URL space to buy a property that a declaration buys for free. Three
costs, none of which the hoist can avoid:

1. **It merges nothing.** The per-client login endpoints stay distinct after the move
   (they have different transports — one sets a cookie, one is bearer-native). The
   endpoint set is unchanged; only the segment order is. The whole cost buys a
   re-parenting.
2. **The hoisted surface is not uniform either.** An auth surface is *irreducibly*
   mixed: `login` and `reset` are anonymous, but `logout`, `me`, and step-up
   re-authentication all require an existing session. Hoisting therefore creates a
   top-level family with **no invariant at all**, containing some of the most
   sensitive authenticated routes in the system, under a prefix every reader parses
   as "the anonymous lane." That inverts the problem rather than solving it.
3. **It spends the wrong segment.** The first path segment is the one every
   intermediary keys on — an edge rewrite, CORS, any prefix-scoped policy. If the
   model's premise is that the top level names the audience, a hoist demotes the
   audience to second place in favour of a function name the rest of the path already
   carries, and gives each client two base URLs where it had one.

⭐ **The general rule: the top-level segment is already spent. Anything else you want
to segregate belongs at controller granularity, where a declaration is cheap and the
framework can check it.** Restructuring URLs to obtain a checkable invariant is paying
in architecture for something a label provides.

### What the gate then looks like

Once the audience is declared, an application `Middleware` can enforce a floor with
**no allow-list at all**:

```rust
// The gate reads a CLAIM, not a location.
if request_audience == Some("session") && !has_valid_session(&request) {
    return Err(WebError::Unauthorized);
}
```

Be fair to the alternative: a prefix gate *does* cover its own subtree
automatically, new mounts included, and that is its real strength. Two things it
cannot do. It cannot express an **exception** without enumerating it by path — a
second list, maintained beside the routes, drifting from them. And it cannot follow a
controller mounted **outside** the prefix anyone thought to cover. The
declaration-keyed gate has neither limitation, because the claim travels with the
controller rather than with its address: the exception is the controller declaring
`"anonymous"` in its own file, and a controller carrying `"session"` is gated at every
mount it ever appears at.

### Reading the declaration: today, and the way it should work

**Today, additively (Phase 1, no framework change beyond the label).** Build the map
once at boot from the inventory, and look up in the middleware:

```rust
// at startup, from the declarations themselves — not hand-maintained
let gate: HashMap<String, &'static str> = router.audiences()
    .into_iter()
    .filter_map(|r| r.audience.map(|a| (r.mount, a)))
    .collect();
```

This keeps the property that matters — **the gate is derived from declarations, never
from a hand-written path list** — at the cost of the application re-implementing
longest-prefix matching over `request.path_parts`, which the router already does
correctly. That duplication is a real wart: mounts nest, and a subtly different match
is a subtly different security boundary.

**The right shape: the framework stamps it.** Actus already does exactly this for the
sibling label — `server.rs` sets `request.rate_limit_class` from the matched
controller right after routing and before the `before` chain. An `audience` field
alongside it would remove the duplicated matching entirely and cost one line at the
stamp site.

### ⛔ Why the stamp is queued and not in Phase 1

**Adding that field is a breaking change**, verified against the code on 2026-08-30:

- `Request` carries six fields, **every one `pub`**, with **no private field** and
  **no `#[non_exhaustive]`** (`crates/actus-server/src/request.rs`).
- Downstream code can therefore construct it with an exhaustive struct literal — and
  *does*: Actus's own tests build one that way (`request.rs`, the `req(...)` helper),
  which is the natural shape for a middleware unit test in any consumer.
- Adding a public field to a struct with no private fields and no `#[non_exhaustive]`
  is a **major** change under Cargo's SemVer rules. Marking it `#[non_exhaustive]`
  now is equally breaking, for the same reason.

⭐ **The finding is larger than this proposal, and worth recording on its own:**
`Request` cannot receive *any* new routing-derived projection during `1.x`.
`rate_limit_class` got in before the freeze; nothing can follow it. Every future
"stamp the matched route's X onto the request" idea is now a `2.0` item. The 1.0
freeze audit did not surface this, because it reviewed the *shape* of the public
surface rather than its *extensibility*.

⇒ **Recommendation: ship Phase 1 without the stamp, and put `#[non_exhaustive]` on
`Request` plus the `audience` field on the `2.0` docket as one item.** The boot-built
map above is a working gate in the meantime, and the day a `2.0` happens the wart
disappears without the application changing its middleware's logic — only where it
reads the label from.

## Second-order win: the claim becomes falsifiable

This is the part worth more than the check itself.

Once every controller in a family carries a label, the application can write **one**
test that walks `Router::audiences()`, fires an unauthenticated request at each
mount, and asserts the response agrees with the declared label — a controller
claiming `"session"` must not answer `200`; one claiming `"anonymous"` must not
answer `401`.

Actus still verifies nothing about authentication. But it is the only thing that can
**enumerate every route**, and enumeration is what turns a claim from documentation
into something a test can refute. A route added tomorrow is in the inventory the
moment it is mounted, so it is in that test too, with nobody remembering to add it.

That is the honest division of labour: **the framework supplies the enumeration and
insists on the claim; the application supplies the probe that checks it.**

## Implementation sketch

### `actus-controller`

- `Controller::actus_audience(&self) -> Option<&'static str>`, defaulted to `None`.
- `pub trait DeclaresAudience {}` with the `on_unimplemented` diagnostic (Phase 2).
- `pub fn declares_audience<T: Controller + DeclaresAudience>(c: T) -> T` (Phase 2),
  re-exported through `actus::__internal`.
- Shape A, if taken: `Controller::actus_prepare(&self) -> Option<&'static str>`.

### `actus-controller-macros`

- `#[controller]` parses `audience = "…"` alongside `strict` / `lax` / `prepare` /
  `max_body_bytes` / `rate_limit`; emits `actus_audience` and (Phase 2)
  `impl DeclaresAudience`.
- `app_routes!` parses an optional `families { "a", "b" }` block. `AppRoutesInput`
  grows one field; `generate_app_routes` wraps constructions whose mount is under a
  listed prefix. **Segment-wise prefix match** — `"api"` covers `"api/auth/oauth"`
  but not `"apiary"`.
- A `families` entry matching no mount should be a compile **warning** (or error —
  open question #5): it is usually a typo, and a typo'd family silently constrains
  nothing.

### `actus-server`

- `pub struct MountAudience`; `Router::audiences()` walking the tree the way
  `routes()` does (children sorted, deterministic DFS), emitting a row for **every**
  node bearing a controller.
- **Queued for `2.0`, not Phase 1** (§ [Enforcement](#enforcement-key-the-gate-to-the-declaration-not-the-path)):
  `#[non_exhaustive]` on `Request`, plus
  `pub audience: Option<&'static str>` set from `route_match.controller.actus_audience()`
  at the same site that already sets `rate_limit_class`. One line at the stamp site;
  the cost is entirely in the major version it forces.

### Tests

- Macro: `audience` parses; combines with every other attribute key; emits the trait
  impl (Phase 2).
- Router: `audiences()` emits a row for an undeclared controller — *the regression
  test for the blind spot this design exists to fix*; deterministic ordering; `""`
  for a root mount.
- `app_routes!`: nested prefixes are covered; `"apiary"` is not covered by `"api"`;
  an unlisted mount is unconstrained.
- Compile-fail (Phase 2): a `trybuild` case asserting the missing-audience error, and
  that its message is the `on_unimplemented` one. This is the first `trybuild`
  dependency in the workspace — see effort, below.
- `examples/advanced` grows an `audience_coverage` check next to `rate_limit_coverage`,
  wired into its existing `--check` flag, with unit tests for a violation and a pass —
  mirroring the two `rate_limit_coverage` tests already there.
- `examples/advanced` also grows the **gate** from
  [Enforcement](#enforcement-key-the-gate-to-the-declaration-not-the-path) as a
  `Middleware` beside its rate limiter: the boot-built `mount → audience` map, a
  longest-prefix lookup, `WebError::Unauthorized` on a `"session"` mount with no
  credential. Without it the proposal ships a *declaration* with no worked example of
  anything acting on one, which is how a feature gets read as documentation-only. An
  integration test drives a real request at an `"anonymous"` mount and a `"session"`
  mount and asserts the two outcomes.

### Docs

- README: a "Route families" section after "Middleware", framed as *coverage, not
  authorization*, with the limit stated in the first paragraph.
- The `actus_audience` rustdoc carries the same "label, not a policy" paragraph
  `actus_rate_limit` does — that doc comment is the load-bearing one, because it is
  where a reader decides whether this is an auth feature. It is not.
- CHANGELOG under Added, per phase.

## What this is *not* trying to do

- **Authenticate anything.** No credential parsing, no session concept, no 401
  policy. `WebError::Unauthorized` already exists and stays the application's to
  return.
- **Replace the policy layer.** Resource-aware decisions stay in handlers, where the
  resource is.
- **Per-route audiences.** The label is per-controller, matching `max_body_bytes`
  and `rate_limit`. A route needing a different audience gets its own controller —
  the same answer the body-cap proposal gives, and for the same reason. A per-route
  override would be additive later, if a real consumer hits it.
- **Enforce anything itself.** Actus supplies the declaration and — once a `2.0`
  allows the stamp — makes it readable per request. The gate that *acts* on it is
  application `Middleware`, and what "a valid session" means stays entirely the
  application's. `audiences()` is one tree walk at startup and the Phase 2 bound is
  compile-time; neither adds per-request work of its own.
- **Guess an audience from the mount.** A controller under `public/` that forgot its
  label is precisely the case being caught; inferring `"anonymous"` for it would
  invert the feature.

## Open questions for the review

1. **Is `audience` the right word?** It names the *caller*, which is the axis being
   segregated, and it is the word the production consumer's own design document
   already uses. Alternatives: `caller` (equally good, less established), `lane`
   (the informal term in conversation; vague on its own), `posture` (overloaded —
   it already means something else in that consumer's deployment vocabulary),
   `access` (⛔ **reject** — it is the removed enum's word and would read as a
   revived authorization feature, which is the one misreading this design must not
   invite).

2. **String label or type?** A string matches `rate_limit` and keeps the family rule
   as ordinary data. A type (`#[controller(audience = Session)]`, family rule as
   `HasAudience<Session>`) would move the *value* check to compile time too, not just
   presence — strictly stronger. It costs the application a set of marker types, makes
   the label unprintable in a route dump without extra work, and it is a much larger
   macro change. I lean string, but the stronger guarantee deserves a hearing.

3. **A new `Router` method, or extend `routes()`?** Principle 2 dislikes method
   sprawl, and `Router` would then carry `routes()`, `rate_limit_classes()` and
   `audiences()` — three walks over one tree, each answering a slice. The honest
   alternative is one `Router::inventory()` returning a per-mount record (controller
   name, audience, rate-limit class, body cap, `prepare` name, routes) with the
   existing methods kept as thin projections. That is a better long-run surface and a
   bigger change. **My inclination: build `audiences()` now, and open a separate
   consolidation proposal** rather than growing the API twice.

4. **Fix `rate_limit_classes()` to include absences?** It has the same blind spot and
   the same argument applies weakly (an unlimited controller is usually intended). But
   leaving two sibling methods with opposite emptiness semantics is a trap for the next
   reader. Options: change it (a **breaking** change to its output — needs a `2.0`, so
   realistically not); add `rate_limit_coverage()` alongside; or document the asymmetry
   loudly and accept it. I lean on documenting it and pointing at `audiences()`.

5. **A `families` entry matching no mount — warn or error?** A typo there silently
   constrains nothing, which is the same failure the feature exists to prevent, one
   level up. Erroring is tempting and symmetric; it also breaks the legitimate case of
   a family declared before its first controller is written.

6. **One controller type mounted in two families.** Observed in the production consumer:
   a single controller type mounted at two prefixes belonging to different client
   classes (same code, different base path). One type carries one label, so the family
   rule must map a family to a **set** of acceptable audiences, not a single value —
   the sketch above already does. Under Phase 2 the marker trait is per-type, so
   presence still works; only the value needs the set. Is that sufficient, or is a
   per-mount override in `app_routes!` warranted? I think the set is sufficient and the
   override is speculative.

7. **The genuinely mixed controller — is "split it" a good enough answer?** This is
   the case that will come up first in any real application, and the label is
   per-controller. An auth controller is the archetype: `login`, `register` and
   `reset` are anonymous while `logout`, `me` and step-up re-authentication require an
   existing session, so no single label is honest for the whole thing. Three answers:
   **(a)** split it into an anonymous controller and an authenticated sibling — the
   same answer the body-cap proposal gives, it costs only a second mount, and it
   yields two controllers that each carry a *strong* claim; **(b)** allow a
   deliberately weak label (`"mixed"`) that the family rule accepts, honest but
   nearly contentless; **(c)** per-route audiences, which is real scope. I lean (a)
   and think it should be stated as the recommended shape in the README, because the
   alternative is every consumer independently discovering that their auth controller
   does not fit.

8. **Do the stamp and `#[non_exhaustive]` go on the `2.0` docket as one item?** The
   [Enforcement](#enforcement-key-the-gate-to-the-declaration-not-the-path) finding
   says `Request` can take no new framework-populated field during `1.x`. If that is
   accepted, the `2.0` list starts here and this is its first entry — and it is worth
   asking whether an additive escape exists that this review missed (a typed
   extensions slot on `Request` mirroring `Params::insert`/`get` would itself be a new
   field, so it does not escape; nor does a private field, which breaks the same
   literals).

## Estimated effort

**Phase 1** — roughly 150–250 lines across three crates:
- macro attribute parsing + `actus_audience` emission (~40)
- `MountAudience` + `Router::audiences()` + walk (~50)
- tests (~60)
- `examples/advanced` coverage check + its two tests (~50)
- README + rustdoc (prose)

**Queued for `2.0`** — trivial in code, expensive in version: `#[non_exhaustive]` on
`Request` + the `audience` field + one line at the stamp site (~10), plus updating
every `Request` literal in the workspace's own tests (~6 sites).

**Phase 2** — roughly 200–300 lines on top:
- `families` parsing in `AppRoutesInput` + prefix matching + construction wrapping (~90)
- `DeclaresAudience`, the diagnostic, the pass-through fn, `__internal` re-export (~40)
- `trybuild` compile-fail tests (~60, plus a new dev-dependency and a `cargo-deny`
  licence pass on it — the workspace has no `trybuild` today, and adding a
  dev-dependency is the one part of this that touches the supply chain)
- tests + docs (~80)

## Why now, why not later

**Neither phase is blocked by the 1.0 freeze**, which is the unusual and comfortable
part: a defaulted trait method, a new `Router` method, a new attribute key and a new
optional macro block are all additive. There is no forcing function, and this can rest.

The argument for doing it anyway is that the cost of *not* having it is paid by
someone else, quietly, and is only ever discovered in production. The production
consumer's families are correct today — but that is a fact established by hand, by
reading 49 files, on one afternoon, and it decays the moment the next controller
lands. Every application with a segregated route surface has this problem and none
of them can solve it without the route tree.

The argument for waiting is Phase 2's grammar: `families` is a permanent addition to
the one macro that is meant to be the whole audit surface, and it should not be
designed twice. Phase 1 buys the information needed to design it well — including
whether the string label survives contact with a second consumer.

⚠️ **One thing found while writing this does not wait for either phase.** `Request`
is a plain all-public struct, so it can take no new framework-populated field before a
`2.0` (§ [Enforcement](#enforcement-key-the-gate-to-the-declaration-not-the-path)).
That constraint exists whether or not this proposal is ever built, it applies to every
future "stamp the matched route's X onto the request" idea, and it is cheaper to
record now than to rediscover from a failed `cargo semver-checks` later.

⇒ **Suggested order: ship Phase 1, use it in the production consumer for a release
cycle, then decide Phase 2 on evidence.** That is the same discipline the body-cap
proposal applied to its own Phase 2, and for the same reason.
