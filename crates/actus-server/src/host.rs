//! Host aliases — a host as a name for a mounted controller.
//!
//! [`RouterBuilder::host_alias`](crate::RouterBuilder::host_alias) registers
//! them (the `hosts` block of `app_routes!` is the declarative form), and the
//! server applies them before routing. A request whose host an alias names is
//! routed as the request for the alias's target — the aliased mount, then the
//! host's captured labels — followed by the request's own path, unless that
//! path lies under one of the alias's shared mounts, which keep their own
//! paths.
//!
//! From then on it *is* that request: [`Request::path_parts`] holds the
//! aliased path, so the router, every middleware and the handler see one
//! address, and a middleware that re-derives the matched controller with
//! [`Router::match_controller`] gets the one the server dispatched to. An
//! aliased request is identical, apart from `Host`, to a request any client
//! could send to the aliased path directly: an alias adds addresses, never
//! capabilities.
//!
//! [`Request::path_parts`]: crate::Request::path_parts
//! [`Router::match_controller`]: crate::Router::match_controller

use actus_controller::routing;
use http::{HeaderMap, Uri, header};
use std::fmt;

/// Why [`RouterBuilder::host_alias`](crate::RouterBuilder::host_alias)
/// refused an alias. The message names the entry and what is wrong with it;
/// `app_routes!` returns it from the generated `init()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAliasError {
    message: String,
}

impl HostAliasError {
    fn new(host: Option<&str>, target: &str, problem: impl fmt::Display) -> Self {
        let host = match host {
            Some(h) => format!("{h:?}"),
            None => "(not configured)".to_string(),
        };
        Self {
            message: format!("host alias {host} => {target:?}: {problem}"),
        }
    }
}

impl fmt::Display for HostAliasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HostAliasError {}

/// One label of a host pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Label {
    /// Matches this label, ignoring ASCII case. Stored lowercase.
    Literal(String),
    /// `{name}` — matches any one hostname label.
    Capture(String),
}

/// A registered host alias. Built, and checked against the mounts, by
/// [`HostAlias::new`].
pub(crate) struct HostAlias {
    /// The host pattern's labels, leftmost first.
    labels: Vec<Label>,
    /// The aliased mount's path segments.
    mount: Vec<String>,
    /// For each `{name}` segment of the target, in order: the position of
    /// the host label that fills it.
    fills: Vec<usize>,
    /// The shared mounts' path segments.
    shares: Vec<Vec<String>>,
}

impl HostAlias {
    /// Parse `host`, `target` and `shares`, and check them against the mounts
    /// `is_mount` recognises. `Ok(None)` when `host` is `None`: the entry is
    /// still checked, so a wrong path fails every deployment, but no alias
    /// exists in this one.
    pub(crate) fn new(
        host: Option<&str>,
        target: &str,
        shares: &[&str],
        is_mount: impl Fn(&[String]) -> bool,
    ) -> Result<Option<Self>, HostAliasError> {
        let err = |problem: String| HostAliasError::new(host, target, problem);

        // The target: a mount path, then only `{name}` segments.
        let segments: Vec<&str> = target.split('/').filter(|s| !s.is_empty()).collect();
        let first_capture = segments
            .iter()
            .position(|s| s.starts_with('{'))
            .unwrap_or(segments.len());
        let (mount, captures) = segments.split_at(first_capture);
        let mount: Vec<String> = mount.iter().map(|s| s.to_string()).collect();
        let mut names: Vec<&str> = Vec::new();
        for segment in captures {
            match capture_name(segment) {
                Some(name) if names.contains(&name) => {
                    return Err(err(format!("`{{{name}}}` appears twice in the target")));
                }
                Some(name) => names.push(name),
                None => {
                    return Err(err(format!(
                        "`{segment}` follows a capture — a target is a mount path, then only \
                         `{{name}}` segments"
                    )));
                }
            }
        }
        // An exact mount is what keeps every aliased path inside it: a path
        // that named no mount would fall through to a shallower one — a root
        // catch-all, typically — and reach what the alias exists to confine.
        if !is_mount(&mount) {
            return Err(err(format!(
                "`{}` is not a mounted controller — the target must name one",
                mount.join("/")
            )));
        }

        let mut shared = Vec::with_capacity(shares.len());
        for share in shares {
            let segs: Vec<String> = routing::family_segments(share)
                .into_iter()
                .map(String::from)
                .collect();
            if segs.is_empty() {
                return Err(err(
                    "a shared mount cannot be the root — that would share every mount".to_string(),
                ));
            }
            if mount.starts_with(&segs) {
                return Err(err(format!(
                    "shared `{share}` contains the aliased mount — sharing it would reach every \
                     path the alias confines"
                )));
            }
            if !is_mount(&segs) {
                return Err(err(format!("shared `{share}` is not a mounted controller")));
            }
            shared.push(segs);
        }

        let Some(host) = host else {
            return Ok(None);
        };
        let labels = parse_pattern(host).map_err(err)?;
        let mut fills = Vec::with_capacity(names.len());
        for name in &names {
            match labels
                .iter()
                .position(|l| matches!(l, Label::Capture(c) if c == name))
            {
                Some(position) => fills.push(position),
                None => {
                    return Err(err(format!(
                        "the target's `{{{name}}}` is not captured by the host pattern"
                    )));
                }
            }
        }
        if let Some(Label::Capture(unused)) = labels
            .iter()
            .find(|l| matches!(l, Label::Capture(c) if !names.contains(&c.as_str())))
        {
            return Err(err(format!(
                "the host pattern captures `{{{unused}}}`, but the target does not use it"
            )));
        }

        Ok(Some(Self {
            labels,
            mount,
            fills,
            shares: shared,
        }))
    }

    /// This alias applied to `host` (as [`request_host`] returns it), or
    /// `None` if the alias does not name that host.
    pub(crate) fn hit(&self, host: &str) -> Option<AliasHit<'_>> {
        if host.split('.').count() != self.labels.len() {
            return None;
        }
        let names_host = host
            .split('.')
            .zip(&self.labels)
            .all(|(label, pattern)| match pattern {
                Label::Literal(literal) => label.eq_ignore_ascii_case(literal),
                Label::Capture(_) => is_hostname_label(label),
            });
        if !names_host {
            return None;
        }
        let labels: Vec<&str> = host.split('.').collect();
        Some(AliasHit {
            alias: self,
            captures: self
                .fills
                .iter()
                .map(|&i| labels[i].to_ascii_lowercase())
                .collect(),
        })
    }
}

/// An alias that names a request's host, with the labels the host filled in.
pub(crate) struct AliasHit<'r> {
    alias: &'r HostAlias,
    captures: Vec<String>,
}

impl AliasHit<'_> {
    /// Route `path_parts` — the request's own path — under the alias. A path
    /// under a shared mount is left as it is, and `None` returned. Any other
    /// path becomes the target, with the captures filled in, followed by the
    /// path; the target is returned joined with `/`, which is what
    /// `Params::alias_prefix` reports.
    pub(crate) fn apply(self, path_parts: &mut Vec<String>) -> Option<String> {
        if self
            .alias
            .shares
            .iter()
            .any(|share| path_parts.starts_with(share))
        {
            return None;
        }
        let prefix: Vec<String> = self
            .alias
            .mount
            .iter()
            .cloned()
            .chain(self.captures)
            .collect();
        let joined = prefix.join("/");
        path_parts.splice(0..0, prefix);
        Some(joined)
    }
}

/// The host a request is addressed to, as aliases match it: the authority of
/// an absolute-form request-target — which RFC 9112 §3.2.2 has a server use
/// instead of `Host` — else the `Host` header; without its port, and without
/// a trailing dot. `None` when there is none, or it is an IP literal
/// (`[::1]`), which no pattern names.
pub(crate) fn request_host<'a>(uri: &'a Uri, headers: &'a HeaderMap) -> Option<&'a str> {
    let host = match uri.authority() {
        Some(authority) => authority.host(),
        None => {
            let value = headers.get(header::HOST)?.to_str().ok()?;
            value.rsplit_once(':').map_or(value, |(host, _port)| host)
        }
    };
    if host.starts_with('[') {
        return None;
    }
    let host = host.strip_suffix('.').unwrap_or(host);
    (!host.is_empty()).then_some(host)
}

/// Parse a host pattern: dot-separated labels, each a hostname label or a
/// `{name}` capture. One trailing dot is allowed.
fn parse_pattern(pattern: &str) -> Result<Vec<Label>, String> {
    let body = pattern.strip_suffix('.').unwrap_or(pattern);
    if body.is_empty() {
        return Err("the host pattern is empty".to_string());
    }
    let mut labels: Vec<Label> = Vec::new();
    for raw in body.split('.') {
        let label = if let Some(name) = capture_name(raw) {
            if labels
                .iter()
                .any(|l| matches!(l, Label::Capture(c) if c == name))
            {
                return Err(format!("`{{{name}}}` appears twice in the host pattern"));
            }
            Label::Capture(name.to_string())
        } else if is_hostname_label(raw) {
            Label::Literal(raw.to_ascii_lowercase())
        } else {
            return Err(format!(
                "`{raw}` in the host pattern is neither a hostname label (letters, digits and \
                 hyphens) nor a `{{name}}` capture — a pattern names hosts only, without a port"
            ));
        };
        labels.push(label);
    }
    Ok(labels)
}

/// `name` when `segment` is exactly `{name}` with a capture name: letters,
/// digits and `_`.
fn capture_name(segment: &str) -> Option<&str> {
    segment
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
}

/// One hostname label: 1–63 letters, digits and hyphens (RFC 1123). A
/// capture matches only these, so what it puts in the path is always a
/// single, plain segment.
fn is_hostname_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mounts every test below checks against.
    fn mounted(segments: &[String]) -> bool {
        const MOUNTS: &[&[&str]] = &[
            &["tenants"],
            &["assets"],
            &["shop", "stores"],
            &["shop", "assets"],
            &["api"],
        ];
        MOUNTS
            .iter()
            .any(|m| m.len() == segments.len() && m.iter().zip(segments).all(|(a, b)| a == b))
    }

    fn alias(host: &str, target: &str, shares: &[&str]) -> HostAlias {
        HostAlias::new(Some(host), target, shares, mounted)
            .expect("valid alias")
            .expect("configured")
    }

    fn refused(host: Option<&str>, target: &str, shares: &[&str]) -> String {
        match HostAlias::new(host, target, shares, mounted) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("{host:?} => {target:?} {shares:?} must be refused"),
        }
    }

    fn path(p: &str) -> Vec<String> {
        p.split('/')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    }

    /// Route `path` on `host` through `alias`: `Some((aliased path, prefix))`
    /// when the alias names the host, `None` when it does not.
    fn route(alias: &HostAlias, host: &str, p: &str) -> Option<(String, Option<String>)> {
        let hit = alias.hit(host)?;
        let mut parts = path(p);
        let prefix = hit.apply(&mut parts);
        Some((parts.join("/"), prefix))
    }

    #[test]
    fn a_capture_becomes_the_first_action_segment_under_the_mount() {
        let a = alias("{tenant}.example.test", "tenants/{tenant}", &[]);
        assert_eq!(
            route(&a, "acme.example.test", "orders/7"),
            Some(("tenants/acme/orders/7".into(), Some("tenants/acme".into())))
        );
        // The host's root is the target itself.
        assert_eq!(
            route(&a, "acme.example.test", ""),
            Some(("tenants/acme".into(), Some("tenants/acme".into())))
        );
    }

    #[test]
    fn shared_mounts_keep_their_paths_and_everything_else_goes_under_the_target() {
        let a = alias(
            "{store}.localhost",
            "shop/stores/{store}",
            &["shop/assets", "assets"],
        );
        // Shared: untouched, and not reported as aliased.
        assert_eq!(
            route(&a, "s1.localhost", "shop/assets/logo.svg"),
            Some(("shop/assets/logo.svg".into(), None))
        );
        assert_eq!(
            route(&a, "s1.localhost", "assets"),
            Some(("assets".into(), None))
        );
        // Segment-aligned: `assetsx` is not under `assets`.
        assert_eq!(
            route(&a, "s1.localhost", "assetsx/y").map(|(p, _)| p),
            Some("shop/stores/s1/assetsx/y".into())
        );
        // Every other mount's path lands under the target instead.
        assert_eq!(
            route(&a, "s1.localhost", "api/things").map(|(p, _)| p),
            Some("shop/stores/s1/api/things".into())
        );
    }

    #[test]
    fn hosts_match_by_label_ignoring_case_and_capture_only_hostname_labels() {
        let a = alias("{tenant}.example.test", "tenants/{tenant}", &[]);
        // Case-insensitive, and the capture arrives lowercased.
        assert_eq!(
            route(&a, "ACME.Example.TEST", "").map(|(p, _)| p),
            Some("tenants/acme".into())
        );
        // One label per capture: more or fewer labels is another host.
        assert!(a.hit("a.b.example.test").is_none());
        assert!(a.hit("example.test").is_none());
        // A capture matches letters, digits and hyphens only — never a
        // character that could split or escape a path segment.
        assert!(a.hit("0c7e-11ef.example.test").is_some());
        for bad in [
            "a_b.example.test",
            "a%2fb.example.test",
            "a b.example.test",
            "a/b.example.test",
        ] {
            assert!(a.hit(bad).is_none(), "{bad:?} must not be captured");
        }
        assert!(a.hit(&format!("{}.example.test", "x".repeat(64))).is_none());
        // Literal labels must match.
        assert!(a.hit("acme.example.org").is_none());
    }

    #[test]
    fn a_pattern_without_captures_names_one_host() {
        let a = alias("admin.example.test", "api", &[]);
        assert_eq!(
            route(&a, "admin.example.test", "x"),
            Some(("api/x".into(), Some("api".into())))
        );
        assert!(a.hit("other.example.test").is_none());
    }

    #[test]
    fn the_request_host_is_the_authority_else_host_without_port_or_trailing_dot() {
        let origin: Uri = "/orders/7".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "acme.example.test:8080".parse().unwrap());
        assert_eq!(request_host(&origin, &headers), Some("acme.example.test"));

        headers.insert(header::HOST, "acme.example.test.".parse().unwrap());
        assert_eq!(request_host(&origin, &headers), Some("acme.example.test"));

        // Absolute form: the request-target's authority wins over `Host`.
        let absolute: Uri = "http://acme.example.test:8080/orders/7".parse().unwrap();
        headers.insert(header::HOST, "elsewhere.test".parse().unwrap());
        assert_eq!(request_host(&absolute, &headers), Some("acme.example.test"));

        // IP literals and a missing host name no host.
        headers.insert(header::HOST, "[::1]:8080".parse().unwrap());
        assert_eq!(request_host(&origin, &headers), None);
        assert_eq!(request_host(&origin, &HeaderMap::new()), None);
    }

    #[test]
    fn an_unconfigured_alias_is_checked_but_not_registered() {
        assert!(
            HostAlias::new(None, "tenants/{tenant}", &["assets"], mounted)
                .expect("the paths are valid")
                .is_none()
        );
        // …and its paths are still checked, in every deployment.
        assert!(refused(None, "tenantz/{tenant}", &[]).contains("not a mounted controller"));
    }

    #[test]
    fn a_target_must_name_a_mount_and_use_every_capture() {
        let e = refused(Some("{t}.example.test"), "tenantz/{t}", &[]);
        assert!(e.contains("`tenantz` is not a mounted controller"), "{e}");
        assert!(e.contains(r#""{t}.example.test""#), "names the entry: {e}");

        // A mount *below* a mounted prefix is not itself a mount.
        let e = refused(Some("{t}.example.test"), "tenants/x/{t}", &[]);
        assert!(e.contains("not a mounted controller"), "{e}");

        let e = refused(Some("{t}.example.test"), "tenants/{t}/extra", &[]);
        assert!(e.contains("follows a capture"), "{e}");

        let e = refused(Some("{t}.example.test"), "tenants/{u}", &[]);
        assert!(e.contains("not captured by the host pattern"), "{e}");

        let e = refused(Some("{t}.{u}.example.test"), "tenants/{t}", &[]);
        assert!(e.contains("does not use it"), "{e}");

        let e = refused(Some("{t}.example.test"), "tenants/{t}/{t}", &[]);
        assert!(e.contains("appears twice in the target"), "{e}");
    }

    #[test]
    fn a_share_must_be_a_mount_outside_the_aliased_one() {
        let e = refused(Some("{t}.example.test"), "tenants/{t}", &["asets"]);
        assert!(
            e.contains("shared `asets` is not a mounted controller"),
            "{e}"
        );

        let e = refused(Some("{t}.example.test"), "tenants/{t}", &["tenants"]);
        assert!(e.contains("contains the aliased mount"), "{e}");

        // An ancestor of the aliased mount would reach it too.
        let e = refused(Some("{s}.localhost"), "shop/stores/{s}", &["shop"]);
        assert!(e.contains("contains the aliased mount"), "{e}");

        let e = refused(Some("{t}.example.test"), "tenants/{t}", &["*"]);
        assert!(e.contains("cannot be the root"), "{e}");
    }

    #[test]
    fn malformed_patterns_are_refused() {
        for (pattern, why) in [
            ("", "is empty"),
            ("{t}.localhost:8080", "without a port"),
            ("a..localhost", "neither a hostname label"),
            ("x{t}.localhost", "neither a hostname label"),
            ("{t}.{t}.localhost", "appears twice in the host pattern"),
            ("{bad name}.localhost", "neither a hostname label"),
        ] {
            let e = refused(Some(pattern), "tenants/{t}", &[]);
            assert!(e.contains(why), "{pattern:?}: {e}");
        }
    }
}
