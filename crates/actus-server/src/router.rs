//! [`Router`] and [`RouterBuilder`] — the longest-prefix route tree that maps
//! a request path to the controller mounted at its deepest matching prefix.

use actus_controller::{Controller, Params, RouteDef};
use actus_reply::{Reply, WebError};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, warn};

/// A node in the routing tree, representing a segment of a URL path.
#[derive(Default)]
struct RouteNode {
    /// Child nodes for sub-paths (e.g., "api" -> "v2").
    children: HashMap<String, RouteNode>,
    /// A controller mounted at this exact path. Because routing is
    /// longest-prefix, a mounted controller also handles every path *below*
    /// it that no deeper mount claims — it receives the unconsumed path as
    /// its "action". So a single mount at `"foo"` serves `/foo`, `/foo/x`,
    /// `/foo/x/y`, … unless something is mounted deeper.
    controller: Option<Arc<dyn Controller>>,
}

/// The result of [`Router::match_controller`]: the matched controller and
/// the leftover path segments joined as the controller's action. Returned
/// from a path-only resolve (no verb / parameter checks; those happen
/// inside the controller's dispatch).
#[derive(Clone)]
pub struct RouteMatch {
    /// The controller mounted at the matched longest prefix.
    pub controller: Arc<dyn Controller>,
    /// The unconsumed path below that prefix, joined with `/` — the
    /// controller's "action".
    pub action: String,
}

/// One controller's declared rate-limit class, as returned by
/// [`Router::rate_limit_classes`]: the controller's `mount` path and the
/// `class` label it declared via `#[controller(rate_limit = "…")]`. Used by a
/// startup coverage check that asserts every declared class has a policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateLimitClass {
    /// The controller's mount path (e.g. `"api/auth"`).
    pub mount: String,
    /// The rate-limit class label the controller declared.
    pub class: &'static str,
}

/// The Actus router. Dispatches requests to controllers by longest-prefix
/// match over the route tree at arbitrary depth.
pub struct Router {
    root: RouteNode,
}

impl Router {
    /// Find which controller a path would hit and the leftover-segments
    /// action, without invoking the controller. Returns `None` if no
    /// controller matches (→ 404).
    ///
    /// The verb-level resolution (which `routes!` line the action will
    /// dispatch to, or 405 if none match the verb) happens inside the
    /// controller's `actus_dispatch`; this method only does the
    /// path-tree walk.
    pub fn match_controller(&self, path_parts: &[String]) -> Option<RouteMatch> {
        let mut current_node = &self.root;
        // (controller, prefix_len) — `prefix_len` is how many leading path
        // segments the matched controller's mount point consumed.
        let mut longest_match: Option<(Arc<dyn Controller>, usize)> = None;

        // A controller mounted at the root ("" or "*") handles "/" and, as
        // the loop below shows, anything not claimed by a deeper mount.
        if let Some(ref controller) = current_node.controller {
            longest_match = Some((controller.clone(), 0));
        }

        for (i, segment) in path_parts.iter().enumerate() {
            match current_node.children.get(segment) {
                Some(child_node) => {
                    current_node = child_node;
                    if let Some(ref controller) = current_node.controller {
                        longest_match = Some((controller.clone(), i + 1));
                    }
                }
                // The path diverges from the tree here; the deepest mount we
                // passed through (held in `longest_match`) takes the rest.
                None => break,
            }
        }

        longest_match.map(|(controller, prefix_len)| {
            // Action is the joined path remaining after the matched
            // controller's mount point — all of it, so multi-segment
            // routes like `"posts/{id}/comments"` (and `{...rest}` params)
            // can match. Fully-consumed path → action `""`, which is how
            // a controller registers a root handler via `routes! { "" => … }`.
            let action: String = if prefix_len >= path_parts.len() {
                String::new()
            } else {
                path_parts[prefix_len..].join("/")
            };
            RouteMatch { controller, action }
        })
    }

    /// Routes a request to the appropriate controller and action: thin
    /// wrapper around [`Router::match_controller`] that also invokes
    /// `actus_dispatch`. Kept for callers (mostly tests) that don't need
    /// to inspect the matched controller before dispatch — the server
    /// uses the two-step shape so it can buffer the body with the right
    /// cap *between* match and dispatch.
    pub async fn route(&self, path_parts: &[String], params: Params) -> Reply {
        match self.match_controller(path_parts) {
            Some(rm) => {
                debug!(action = %rm.action, "routing to controller");
                rm.controller.actus_dispatch(&rm.action, params).await
            }
            None => {
                debug!(?path_parts, "no route matched");
                Err(WebError::NotFound)
            }
        }
    }

    /// Walk the route tree and return every `(mount_path, RouteDef)` pair.
    ///
    /// `mount_path` is the slash-joined path *segments* where the controller
    /// was mounted (no leading or trailing slash) — `""` for a root mount,
    /// `"api/users"` for `app_routes!{ "api/users" => UserController, ... }`,
    /// etc. Each controller contributes one entry per `RouteDef` it
    /// describes via [`Controller::actus_describe_routes`].
    ///
    /// Used by introspection tools (OpenAPI doc generators, route audit
    /// scripts). Order matches a DFS over the route tree; within a single
    /// controller, routes are emitted in the order
    /// `Controller::actus_describe_routes` returns them (which is the order
    /// they were declared in the `routes!` block).
    pub fn routes(&self) -> Vec<(String, RouteDef)> {
        let mut out = Vec::new();
        let mut prefix: Vec<String> = Vec::new();
        Self::walk(&self.root, &mut prefix, &mut out);
        out
    }

    fn walk(node: &RouteNode, prefix: &mut Vec<String>, out: &mut Vec<(String, RouteDef)>) {
        if let Some(controller) = &node.controller {
            let mount = prefix.join("/");
            for rd in controller.actus_describe_routes() {
                out.push((mount.clone(), rd));
            }
        }
        // Sorting the children gives a deterministic traversal order across
        // runs (HashMap iteration is otherwise nondeterministic), which
        // matters for stable OpenAPI output / test assertions.
        let mut children: Vec<(&String, &RouteNode)> = node.children.iter().collect();
        children.sort_by(|a, b| a.0.cmp(b.0));
        for (seg, child) in children {
            prefix.push(seg.clone());
            Self::walk(child, prefix, out);
            prefix.pop();
        }
    }

    /// Walk the route tree and return `(mount_path, class)` for every mounted
    /// controller that declared a rate-limit class via
    /// `#[controller(rate_limit = "…")]`. Controllers with no class are
    /// skipped. `mount_path` follows the same convention as [`Router::routes`]
    /// (`""` for a root mount); order is a deterministic DFS (children sorted),
    /// so diagnostics and tests get stable output.
    ///
    /// This exposes the *declared* half of the rate-limit picture — the half
    /// only the router knows. An application's rate-limit middleware holds the
    /// other half (the classes it has a *policy* for), so `main()` can diff the
    /// two at startup and assert every declared class is covered. That turns a
    /// typo'd class (`"ath"` for `"auth"`) into a boot failure instead of a
    /// silently-unlimited controller. One tree walk; no per-request cost.
    pub fn rate_limit_classes(&self) -> Vec<RateLimitClass> {
        let mut out = Vec::new();
        let mut prefix: Vec<String> = Vec::new();
        Self::walk_classes(&self.root, &mut prefix, &mut out);
        out
    }

    fn walk_classes(node: &RouteNode, prefix: &mut Vec<String>, out: &mut Vec<RateLimitClass>) {
        if let Some(controller) = &node.controller
            && let Some(class) = controller.actus_rate_limit()
        {
            out.push(RateLimitClass {
                mount: prefix.join("/"),
                class,
            });
        }
        let mut children: Vec<(&String, &RouteNode)> = node.children.iter().collect();
        children.sort_by(|a, b| a.0.cmp(b.0));
        for (seg, child) in children {
            prefix.push(seg.clone());
            Self::walk_classes(child, prefix, out);
            prefix.pop();
        }
    }
}

/// A builder for constructing the router.
#[derive(Default)]
pub struct RouterBuilder {
    root: RouteNode,
}

impl RouterBuilder {
    /// Create an empty builder with no mounts.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a route, creating the necessary nodes in the tree.
    ///
    /// A **trailing `*` segment is optional sugar** for mounting the
    /// controller at the prefix before it: `"foo/*"` routes identically to
    /// `"foo"`, and `"*"` identically to `""` (the root) — a mounted
    /// controller already receives the entire unconsumed path as its action,
    /// so a "catch-all" is just a mount. The `*` is there so a reader of
    /// `app_routes!` can see "this controller is the catch-all here" at a
    /// glance. A `*` anywhere but the last segment is meaningless; such a
    /// route is dropped with a warning.
    pub fn add_route(mut self, path: &str, controller: Arc<dyn Controller>) -> Self {
        let path = path.trim_matches('/');
        let parts: Vec<&str> = if path.is_empty() {
            Vec::new()
        } else {
            path.split('/').collect()
        };

        // Drop a trailing `*` (it's sugar — see the doc comment).
        let segments: &[&str] = match parts.split_last() {
            Some((last, head)) if *last == "*" => head,
            _ => parts.as_slice(),
        };

        if segments.contains(&"*") {
            warn!(
                route = path,
                "'*' is only meaningful as the last segment of a route path; ignoring route"
            );
            return self;
        }

        let mut current_node = &mut self.root;
        for part in segments {
            current_node = current_node
                .children
                .entry((*part).to_string())
                .or_default();
        }
        // Surface accidental double-mounts loudly instead of silently
        // letting the later registration overwrite the earlier one. The
        // most common cause is two `app_routes!` entries with the same
        // path literal — easy to introduce when copy-pasting versioned
        // mounts (`api/v1/x`, `api/v2/x`, …).
        if let Some(prev) = &current_node.controller {
            warn!(
                route = path,
                previous = prev.__name(),
                new = controller.__name(),
                "duplicate route mount: the later controller overwrites the earlier one",
            );
        }
        current_node.controller = Some(controller);
        self
    }

    /// Finalize the builder into an immutable [`Router`].
    pub fn build(self) -> Router {
        Router { root: self.root }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actus_controller::Verb;
    use bytes::Bytes;
    use std::sync::Mutex;

    /// `(mounted path, action)` pairs recorded by [`Spy`] controllers.
    type SpyLog = Arc<Mutex<Vec<(String, String)>>>;

    /// A controller that records `(its registered path, the action it got)`
    /// into a shared log instead of doing real work, so a test can assert
    /// which controller a request reached and with what action.
    struct Spy {
        path: String,
        log: SpyLog,
    }

    #[actus_controller::async_trait]
    impl Controller for Spy {
        async fn actus_dispatch(&self, action: &str, _params: Params) -> Reply {
            self.log
                .lock()
                .unwrap()
                .push((self.path.clone(), action.to_string()));
            Err(WebError::NotFound) // return value is irrelevant to these tests
        }
        fn __name(&self) -> &'static str {
            "spy"
        }
    }

    fn build(routes: &[&str]) -> (Router, SpyLog) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut b = RouterBuilder::new();
        for r in routes {
            b = b.add_route(
                r,
                Arc::new(Spy {
                    path: r.to_string(),
                    log: log.clone(),
                }),
            );
        }
        (b.build(), log)
    }

    fn empty_params() -> Params {
        Params::new(
            Verb::GET,
            HashMap::new(),
            None,
            Bytes::new(),
            HashMap::new(),
        )
    }

    /// Returns `Some((mounted_path, action))` for the controller the request
    /// reached, or `None` if nothing matched (→ 404).
    async fn hit(router: &Router, log: &SpyLog, path: &str) -> Option<(String, String)> {
        log.lock().unwrap().clear();
        let pp: Vec<String> = path
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        let _ = router.route(&pp, empty_params()).await;
        log.lock().unwrap().last().cloned()
    }

    fn at(path: &str, action: &str) -> Option<(String, String)> {
        Some((path.to_string(), action.to_string()))
    }

    #[tokio::test]
    async fn longest_prefix_wins_and_passes_the_remainder() {
        let (r, log) = build(&["api/users", "api/users/admin"]);
        assert_eq!(hit(&r, &log, "/api/users").await, at("api/users", ""));
        assert_eq!(hit(&r, &log, "/api/users/42").await, at("api/users", "42"));
        assert_eq!(
            hit(&r, &log, "/api/users/42/posts").await,
            at("api/users", "42/posts")
        );
        // a deeper mount takes precedence over a shallower one
        assert_eq!(
            hit(&r, &log, "/api/users/admin").await,
            at("api/users/admin", "")
        );
        assert_eq!(
            hit(&r, &log, "/api/users/admin/x").await,
            at("api/users/admin", "x")
        );
        // nothing mounted at or above `/api` alone
        assert_eq!(hit(&r, &log, "/api").await, None);
        assert_eq!(hit(&r, &log, "/nope").await, None);
        assert_eq!(hit(&r, &log, "/").await, None);
    }

    #[tokio::test]
    async fn trailing_star_is_sugar_for_mounting_at_the_prefix() {
        let (star, log_s) = build(&["api/folder/*"]);
        let (plain, log_p) = build(&["api/folder"]);
        for path in ["/api/folder", "/api/folder/x", "/api/folder/x/y/z"] {
            assert_eq!(
                hit(&star, &log_s, path).await.map(|(_, a)| a),
                hit(&plain, &log_p, path).await.map(|(_, a)| a),
                "`api/folder/*` and `api/folder` must route `{path}` identically"
            );
        }
        // and in particular the bare prefix is matched (not a 404)
        assert_eq!(hit(&star, &log_s, "/api/folder").await.unwrap().1, "");
        assert_eq!(
            hit(&star, &log_s, "/api/folder/deep/path").await.unwrap().1,
            "deep/path"
        );
    }

    #[tokio::test]
    async fn root_star_is_the_global_fallback() {
        let (r, log) = build(&["*", "api/users"]);
        // specific routes still win
        assert_eq!(hit(&r, &log, "/api/users").await, at("api/users", ""));
        assert_eq!(hit(&r, &log, "/api/users/9").await, at("api/users", "9"));
        // everything else falls to the root catch-all, with the full path
        assert_eq!(hit(&r, &log, "/").await, at("*", ""));
        assert_eq!(
            hit(&r, &log, "/anything/here").await,
            at("*", "anything/here")
        );
        // including a divergence partway down a known prefix
        assert_eq!(hit(&r, &log, "/api/missing").await, at("*", "api/missing"));
        // `""` is equivalent to `"*"`
        let (r2, log2) = build(&["", "api/users"]);
        assert_eq!(hit(&r2, &log2, "/whatever").await, at("", "whatever"));
        assert_eq!(hit(&r2, &log2, "/api/users").await, at("api/users", ""));
    }

    #[test]
    fn match_controller_returns_some_for_matched_path_and_none_for_404() {
        // Build a router with two mounts. match_controller walks the tree
        // and returns the deepest mounted controller's match — or None
        // if the path doesn't reach any mount.
        let log = Arc::new(Mutex::new(Vec::new()));
        let router = RouterBuilder::new()
            .add_route(
                "api/users",
                Arc::new(Spy {
                    path: "users".to_string(),
                    log: log.clone(),
                }),
            )
            .add_route(
                "api/users/admin",
                Arc::new(Spy {
                    path: "admin".to_string(),
                    log: log.clone(),
                }),
            )
            .build();

        // Deeper mount wins for the deeper path.
        let pp: Vec<String> = vec!["api".into(), "users".into(), "admin".into()];
        let rm = router.match_controller(&pp).expect("matches");
        assert_eq!(rm.action, "");
        assert_eq!(rm.controller.__name(), "spy");

        // Shallower mount catches the unconsumed remainder.
        let pp: Vec<String> = vec!["api".into(), "users".into(), "42".into()];
        let rm = router.match_controller(&pp).expect("matches");
        assert_eq!(rm.action, "42");

        // Diverged from the tree → None (404 territory).
        let pp: Vec<String> = vec!["api".into(), "missing".into()];
        assert!(router.match_controller(&pp).is_none());

        // Empty path with nothing mounted at root → None.
        let pp: Vec<String> = vec![];
        assert!(router.match_controller(&pp).is_none());
    }

    #[tokio::test]
    async fn duplicate_mount_overwrites_earlier_one() {
        // Two controllers registered at the same path — the second wins.
        // (A `tracing::warn!` fires too; we don't assert on it here, but
        // the override behavior is what callers actually observe.)
        let log = Arc::new(Mutex::new(Vec::new()));
        let first = Arc::new(Spy {
            path: "first".to_string(),
            log: log.clone(),
        });
        let second = Arc::new(Spy {
            path: "second".to_string(),
            log: log.clone(),
        });
        let router = RouterBuilder::new()
            .add_route("api/users", first)
            .add_route("api/users", second)
            .build();
        let pp: Vec<String> = vec!["api".into(), "users".into()];
        let _ = router.route(&pp, empty_params()).await;
        assert_eq!(
            log.lock().unwrap().last().map(|(p, _)| p.as_str()),
            Some("second"),
        );
    }

    /// A `Controller` that exposes a fixed slice of `RouteDef`s via
    /// `actus_describe_routes()` — used to exercise `Router::routes()`
    /// without depending on the `#[controller]` macro.
    struct Described {
        routes: &'static [RouteDef],
    }

    #[actus_controller::async_trait]
    impl Controller for Described {
        async fn actus_dispatch(&self, _action: &str, _params: Params) -> Reply {
            Err(WebError::NotFound)
        }
        fn __name(&self) -> &'static str {
            "described"
        }
        fn actus_describe_routes(&self) -> Vec<RouteDef> {
            self.routes.to_vec()
        }
    }

    #[tokio::test]
    async fn routes_introspection_returns_mount_paths_and_routedefs() {
        static USERS_ROUTES: &[RouteDef] = &[
            RouteDef {
                pattern: "",
                handler_id: "handler_0",
                handler: "list",
                verb: &[Verb::GET],
                params: &[],
                doc: None,
            },
            RouteDef {
                pattern: "{id}",
                handler_id: "handler_1",
                handler: "get",
                verb: &[Verb::GET],
                params: &[],
                doc: None,
            },
        ];
        static HEALTH_ROUTES: &[RouteDef] = &[RouteDef {
            pattern: "",
            handler_id: "handler_0",
            handler: "ping",
            verb: actus_controller::DEFAULT_VERBS,
            params: &[],
            doc: None,
        }];

        let router = RouterBuilder::new()
            .add_route(
                "api/users",
                Arc::new(Described {
                    routes: USERS_ROUTES,
                }),
            )
            .add_route(
                "health",
                Arc::new(Described {
                    routes: HEALTH_ROUTES,
                }),
            )
            .build();

        let pairs = router.routes();
        // 2 routes from api/users + 1 from health = 3 total.
        let mounts: Vec<&str> = pairs.iter().map(|(m, _)| m.as_str()).collect();
        let handlers: Vec<&str> = pairs.iter().map(|(_, r)| r.handler).collect();
        assert_eq!(
            mounts,
            vec!["api/users", "api/users", "health"],
            "DFS sorts child segments alphabetically — 'api' before 'health'",
        );
        assert_eq!(handlers, vec!["list", "get", "ping"]);
    }

    /// A controller that declares a rate-limit class — overrides
    /// `actus_rate_limit` the way the `#[controller(rate_limit = …)]` macro
    /// would, without depending on the macro in this crate.
    struct Classed(&'static str);

    #[actus_controller::async_trait]
    impl Controller for Classed {
        async fn actus_dispatch(&self, _action: &str, _params: Params) -> Reply {
            Err(WebError::NotFound)
        }
        fn __name(&self) -> &'static str {
            "classed"
        }
        fn actus_rate_limit(&self) -> Option<&'static str> {
            Some(self.0)
        }
    }

    #[test]
    fn rate_limit_classes_lists_only_classed_controllers_with_mounts() {
        // Two classed controllers and one unclassed (`Spy`, default `None`).
        // The walk returns the classed ones with their mounts, in sorted-DFS
        // order ('auth' < 'health' < 'tasks'), and omits the unclassed one.
        let log = Arc::new(Mutex::new(Vec::new()));
        let router = RouterBuilder::new()
            .add_route("api/auth", Arc::new(Classed("auth")))
            .add_route(
                "api/health",
                Arc::new(Spy {
                    path: "health".into(),
                    log: log.clone(),
                }),
            )
            .add_route("api/tasks", Arc::new(Classed("tasks")))
            .build();

        assert_eq!(
            router.rate_limit_classes(),
            vec![
                RateLimitClass {
                    mount: "api/auth".to_string(),
                    class: "auth",
                },
                RateLimitClass {
                    mount: "api/tasks".to_string(),
                    class: "tasks",
                },
            ],
        );
        // The unclassed controller contributes nothing.
        assert!(
            router
                .rate_limit_classes()
                .iter()
                .all(|rlc| rlc.mount != "api/health"),
        );
    }

    #[tokio::test]
    async fn star_only_in_last_position() {
        // A `*` that isn't the final segment is meaningless; the route is
        // dropped (with a warning to stderr) rather than treating `*` as a
        // literal segment.
        let (r, log) = build(&["a/*/b"]);
        assert_eq!(hit(&r, &log, "/a/x/b").await, None);
        assert_eq!(hit(&r, &log, "/a/*/b").await, None);
    }
}
