//! Cross-Origin Resource Sharing (CORS).
//!
//! Build a [`CorsLayer`] and hand it to [`crate::Server::with_cors`]. The
//! server then answers preflight (`OPTIONS`) requests itself and stamps the
//! `Access-Control-*` headers onto every cross-origin response — including
//! error responses, so the browser can read 4xx/5xx bodies.
//!
//! ```ignore
//! use actus::prelude::*;
//! use std::time::Duration;
//!
//! // Development: anything goes.
//! Server::new(router).with_cors(CorsLayer::permissive());
//!
//! // Production: pin it down.
//! Server::new(router).with_cors(
//!     CorsLayer::new()
//!         .allow_origin("https://app.example.com")
//!         .allow_methods([Verb::GET, Verb::POST, Verb::DELETE])
//!         .allow_headers([http::header::CONTENT_TYPE, http::header::AUTHORIZATION])
//!         .allow_credentials(true)
//!         .max_age(Duration::from_secs(3600)),
//! );
//! ```

use actus_controller::Verb;
use http::{HeaderMap, HeaderName, HeaderValue, Method, header};
use std::time::Duration;

#[derive(Clone, Debug)]
enum OriginRule {
    /// Any origin is allowed; responses echo the request's concrete `Origin`
    /// (never the literal `*`, so this stays valid alongside credentials).
    Any,
    /// An explicit allow-list — exact, case-sensitive match on `Origin`.
    List(Vec<String>),
}

#[derive(Clone, Debug)]
enum HeaderRule {
    /// Mirror the browser's `Access-Control-Request-Headers` verbatim (always
    /// valid, including with credentials); send nothing if it asked for none.
    MirrorRequest,
    /// An explicit allow-list.
    List(Vec<HeaderName>),
}

/// A CORS policy. See the [module docs](self).
#[derive(Clone, Debug)]
pub struct CorsLayer {
    origins: OriginRule,
    methods: Vec<Verb>,
    headers: HeaderRule,
    expose: Vec<HeaderName>,
    credentials: bool,
    max_age: Option<Duration>,
}

impl Default for CorsLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl CorsLayer {
    /// A *closed* policy — no origin allowed yet. Add some with
    /// [`allow_origin`](Self::allow_origin) / [`allow_any_origin`](Self::allow_any_origin)
    /// (and methods/headers/credentials as your API needs). Defaults: methods
    /// GET + POST, no extra headers, no credentials, no preflight cache.
    pub fn new() -> Self {
        Self {
            origins: OriginRule::List(Vec::new()),
            methods: vec![Verb::GET, Verb::POST],
            headers: HeaderRule::List(Vec::new()),
            expose: Vec::new(),
            credentials: false,
            max_age: None,
        }
    }

    /// A *wide-open* policy — any origin, the common verbs
    /// (GET/POST/PUT/DELETE/PATCH), any request header, no credentials, a
    /// one-day preflight cache. Handy for development; tighten it for production.
    pub fn permissive() -> Self {
        Self {
            origins: OriginRule::Any,
            methods: vec![Verb::GET, Verb::POST, Verb::PUT, Verb::DELETE, Verb::PATCH],
            headers: HeaderRule::MirrorRequest,
            expose: Vec::new(),
            credentials: false,
            max_age: Some(Duration::from_secs(86_400)),
        }
    }

    /// Allow any origin. The response still echoes the concrete `Origin`, so
    /// this composes correctly with [`allow_credentials`](Self::allow_credentials).
    pub fn allow_any_origin(mut self) -> Self {
        self.origins = OriginRule::Any;
        self
    }

    /// Add an allowed origin (exact match, e.g. `"https://app.example.com"`).
    /// Calling this after [`allow_any_origin`](Self::allow_any_origin) narrows
    /// back to a list.
    pub fn allow_origin(mut self, origin: impl Into<String>) -> Self {
        match &mut self.origins {
            OriginRule::List(list) => list.push(origin.into()),
            OriginRule::Any => self.origins = OriginRule::List(vec![origin.into()]),
        }
        self
    }

    /// Set the methods advertised in preflight responses
    /// (`Access-Control-Allow-Methods`). Default: GET, POST.
    pub fn allow_methods(mut self, methods: impl IntoIterator<Item = Verb>) -> Self {
        self.methods = methods.into_iter().collect();
        self
    }

    /// Allow any request header — mirrors `Access-Control-Request-Headers`.
    pub fn allow_any_header(mut self) -> Self {
        self.headers = HeaderRule::MirrorRequest;
        self
    }

    /// Set the allowed request headers explicitly (`Access-Control-Allow-Headers`).
    pub fn allow_headers(mut self, headers: impl IntoIterator<Item = HeaderName>) -> Self {
        self.headers = HeaderRule::List(headers.into_iter().collect());
        self
    }

    /// Response headers JS may read beyond the CORS-safelisted ones
    /// (`Access-Control-Expose-Headers`).
    pub fn expose_headers(mut self, headers: impl IntoIterator<Item = HeaderName>) -> Self {
        self.expose = headers.into_iter().collect();
        self
    }

    /// Whether credentialed requests (cookies, HTTP auth) are permitted
    /// (`Access-Control-Allow-Credentials`). When on, the wildcard `*` token
    /// is never sent — the policy always echoes concrete origin/header values,
    /// which is what the spec requires.
    pub fn allow_credentials(mut self, yes: bool) -> Self {
        self.credentials = yes;
        self
    }

    /// How long a browser may cache a preflight result (`Access-Control-Max-Age`).
    pub fn max_age(mut self, age: Duration) -> Self {
        self.max_age = Some(age);
        self
    }

    // -------- internal: used by `Server` --------

    /// `Some(value)` to send as `Access-Control-Allow-Origin` for `origin`
    /// (always the concrete origin), or `None` if it isn't allowed.
    fn allow_origin_value(&self, origin: &str) -> Option<HeaderValue> {
        let allowed = match &self.origins {
            OriginRule::Any => true,
            OriginRule::List(list) => list.iter().any(|o| o == origin),
        };
        if allowed {
            HeaderValue::from_str(origin).ok()
        } else {
            None
        }
    }

    fn allow_methods_value(&self) -> HeaderValue {
        let joined = self
            .methods
            .iter()
            .map(Verb::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        HeaderValue::from_str(&joined).unwrap_or_else(|_| HeaderValue::from_static("GET, POST"))
    }

    fn allow_headers_value(&self, requested: Option<&HeaderValue>) -> Option<HeaderValue> {
        match &self.headers {
            HeaderRule::MirrorRequest => requested.cloned(),
            HeaderRule::List(list) if list.is_empty() => None,
            HeaderRule::List(list) => {
                let joined = list
                    .iter()
                    .map(HeaderName::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                HeaderValue::from_str(&joined).ok()
            }
        }
    }

    fn expose_headers_value(&self) -> Option<HeaderValue> {
        if self.expose.is_empty() {
            return None;
        }
        let joined = self
            .expose
            .iter()
            .map(HeaderName::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        HeaderValue::from_str(&joined).ok()
    }

    /// True iff `(method, headers)` look like a CORS preflight: `OPTIONS` with
    /// both `Origin` and `Access-Control-Request-Method`.
    pub(crate) fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
        *method == Method::OPTIONS
            && headers.contains_key(header::ORIGIN)
            && headers.contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
    }

    fn preflight_headers(&self, request_headers: &HeaderMap) -> Vec<(HeaderName, HeaderValue)> {
        let mut out = Vec::new();
        let Some(origin) = request_headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
        else {
            return out;
        };
        let Some(allow_origin) = self.allow_origin_value(origin) else {
            return out;
        };
        out.push((header::ACCESS_CONTROL_ALLOW_ORIGIN, allow_origin));
        out.push((header::VARY, HeaderValue::from_static("Origin")));
        out.push((
            header::ACCESS_CONTROL_ALLOW_METHODS,
            self.allow_methods_value(),
        ));
        if let Some(h) =
            self.allow_headers_value(request_headers.get(header::ACCESS_CONTROL_REQUEST_HEADERS))
        {
            out.push((header::ACCESS_CONTROL_ALLOW_HEADERS, h));
        }
        if self.credentials {
            out.push((
                header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                HeaderValue::from_static("true"),
            ));
        }
        if let Some(age) = self.max_age
            && let Ok(v) = HeaderValue::from_str(&age.as_secs().to_string())
        {
            out.push((header::ACCESS_CONTROL_MAX_AGE, v));
        }
        out
    }

    fn response_headers(&self, request_headers: &HeaderMap) -> Vec<(HeaderName, HeaderValue)> {
        let mut out = Vec::new();
        let Some(origin) = request_headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
        else {
            return out;
        };
        let Some(allow_origin) = self.allow_origin_value(origin) else {
            return out;
        };
        out.push((header::ACCESS_CONTROL_ALLOW_ORIGIN, allow_origin));
        out.push((header::VARY, HeaderValue::from_static("Origin")));
        if self.credentials {
            out.push((
                header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                HeaderValue::from_static("true"),
            ));
        }
        if let Some(v) = self.expose_headers_value() {
            out.push((header::ACCESS_CONTROL_EXPOSE_HEADERS, v));
        }
        out
    }

    /// Add the CORS headers to `into`. With `preflight = true` this is the
    /// response to a preflight `OPTIONS` (otherwise an empty `204`); with
    /// `false` it's an ordinary response. No-op when the request had no
    /// `Origin`, or it isn't allowed (the browser then blocks the call).
    ///
    /// `Vary` is *appended* (so an existing `Vary: Accept-Encoding` isn't
    /// clobbered); the `Access-Control-*` headers are set outright.
    pub(crate) fn apply(&self, request_headers: &HeaderMap, into: &mut HeaderMap, preflight: bool) {
        let pairs = if preflight {
            self.preflight_headers(request_headers)
        } else {
            self.response_headers(request_headers)
        };
        for (name, value) in pairs {
            if name == header::VARY {
                into.append(name, value);
            } else {
                into.insert(name, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (name, value) in pairs {
            h.insert(name.clone(), HeaderValue::from_str(value).unwrap());
        }
        h
    }

    fn names(pairs: &[(HeaderName, HeaderValue)], name: &HeaderName) -> Vec<String> {
        pairs
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.to_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn no_origin_header_is_a_noop() {
        assert!(
            CorsLayer::permissive()
                .response_headers(&HeaderMap::new())
                .is_empty()
        );
        assert!(
            CorsLayer::permissive()
                .preflight_headers(&HeaderMap::new())
                .is_empty()
        );
    }

    #[test]
    fn permissive_echoes_any_origin_with_vary() {
        let out = CorsLayer::permissive()
            .response_headers(&headers(&[(header::ORIGIN, "https://x.example")]));
        assert_eq!(
            names(&out, &header::ACCESS_CONTROL_ALLOW_ORIGIN),
            ["https://x.example"]
        );
        assert_eq!(names(&out, &header::VARY), ["Origin"]);
        // no credentials by default
        assert!(names(&out, &header::ACCESS_CONTROL_ALLOW_CREDENTIALS).is_empty());
    }

    #[test]
    fn allow_list_rejects_unlisted_origin() {
        let cors = CorsLayer::new().allow_origin("https://app.example");
        assert!(
            cors.response_headers(&headers(&[(header::ORIGIN, "https://evil.example")]))
                .is_empty()
        );
        assert_eq!(
            names(
                &cors.response_headers(&headers(&[(header::ORIGIN, "https://app.example")])),
                &header::ACCESS_CONTROL_ALLOW_ORIGIN
            ),
            ["https://app.example"]
        );
    }

    #[test]
    fn preflight_advertises_methods_mirrored_headers_and_max_age() {
        let out = CorsLayer::permissive().preflight_headers(&headers(&[
            (header::ORIGIN, "https://x.example"),
            (header::ACCESS_CONTROL_REQUEST_METHOD, "POST"),
            (
                header::ACCESS_CONTROL_REQUEST_HEADERS,
                "content-type, authorization",
            ),
        ]));
        let methods = &names(&out, &header::ACCESS_CONTROL_ALLOW_METHODS)[0];
        assert!(methods.contains("POST") && methods.contains("DELETE"));
        assert_eq!(
            names(&out, &header::ACCESS_CONTROL_ALLOW_HEADERS),
            ["content-type, authorization"]
        );
        assert_eq!(names(&out, &header::ACCESS_CONTROL_MAX_AGE), ["86400"]);
    }

    #[test]
    fn credentials_never_sends_star() {
        let cors = CorsLayer::permissive().allow_credentials(true);
        let out = cors.response_headers(&headers(&[(header::ORIGIN, "https://x.example")]));
        assert_eq!(
            names(&out, &header::ACCESS_CONTROL_ALLOW_ORIGIN),
            ["https://x.example"]
        );
        assert_eq!(
            names(&out, &header::ACCESS_CONTROL_ALLOW_CREDENTIALS),
            ["true"]
        );
    }

    #[test]
    fn apply_appends_vary_but_replaces_acao() {
        let cors = CorsLayer::permissive();
        let mut into = HeaderMap::new();
        into.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        cors.apply(
            &headers(&[(header::ORIGIN, "https://x.example")]),
            &mut into,
            false,
        );
        let vary: Vec<_> = into
            .get_all(header::VARY)
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        assert_eq!(vary, ["Accept-Encoding", "Origin"]);
        assert_eq!(
            into.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
            "https://x.example"
        );
    }
}
