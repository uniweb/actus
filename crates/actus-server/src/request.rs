//! [`Request`] — the incoming HTTP request as seen by middleware, and the
//! body-limiting / parameter-extraction that turns it into a handler's
//! [`Params`].

use actus_controller::{Params, Verb};
use actus_reply::WebError;
use bytes::Bytes;
use http::HeaderMap;
use http_body_util::{BodyExt, LengthLimitError, Limited};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// An incoming HTTP request, as seen by middleware and used to build the
/// typed [`Params`] handed to a handler.
#[derive(Clone, Debug)]
pub struct Request {
    /// The HTTP method (`GET`, `POST`, …).
    pub method: http::Method,
    /// The request path split on `/` into non-empty segments (no leading
    /// empty segment); e.g. `/api/users` → `["api", "users"]`.
    pub path_parts: Vec<String>,
    /// Query parameters as a multimap (each name → all its values, in order).
    /// `application/x-www-form-urlencoded` body fields are appended into the
    /// same map by [`Request::to_params`].
    pub query_params: HashMap<String, Vec<String>>,
    /// The raw request body. Empty for bodyless requests.
    pub body: Bytes,
    /// The request headers.
    pub headers: HeaderMap,
    /// The rate-limit *class* of the controller this request matched, as
    /// declared by `#[controller(rate_limit = "…")]` (see
    /// [`actus_controller::Controller::actus_rate_limit`]). `None` until the
    /// server has matched a controller (it's set right after routing, before
    /// the middleware `before` chain runs) and for controllers that declared
    /// no class.
    ///
    /// This is the routing-derived projection a rate-limit [`crate::Middleware`]
    /// reads to apply per-class policy — the framework supplies the label and
    /// the `429` plumbing ([`WebError::TooManyRequests`]); the limiter and its
    /// store stay application code. Unlike the wire-derived fields above, it's
    /// populated by the framework rather than parsed from the request.
    pub rate_limit_class: Option<&'static str>,
}

/// Fold `(name, value)` pairs into a multimap, preserving order for repeated
/// keys (`a=1&a=2` → `{"a": ["1", "2"]}`).
fn collect_pairs(pairs: Vec<(String, String)>) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for (name, value) in pairs {
        map.entry(name).or_default().push(value);
    }
    map
}

/// Buffer a request body, refusing to hold more than `max_bytes` in memory:
/// an over-limit body becomes `WebError::PayloadTooLarge` (→ 413) instead of
/// growing unbounded. Any other read failure (a truncated/aborted body) is a
/// `400`.
///
/// If `budget` is `Some`, the per-request cap is *also* reserved from a
/// shared semaphore — when the server-wide budget is exhausted, the request
/// is refused with `WebError::Busy` (→ 503) and a short `Retry-After`. The
/// reservation is conservative (pre-reserves `max_bytes` even for requests
/// whose body turns out to be smaller); the alternative — per-chunk byte
/// accounting — is more code for the same effective ceiling.
async fn collect_body_capped<B>(
    body: B,
    max_bytes: usize,
    budget: Option<&Arc<Semaphore>>,
) -> Result<Bytes, WebError>
where
    B: hyper::body::Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    // Reserve from the global byte budget before touching the body. Tokio
    // `Semaphore::acquire_many` takes a `u32`; we cap accordingly. The
    // permit stays alive for the duration of this function — when we
    // return (Ok or Err) it's released.
    let _permit = match budget {
        Some(s) => {
            let n = u32::try_from(max_bytes).unwrap_or(u32::MAX);
            match s.clone().try_acquire_many_owned(n) {
                Ok(p) => Some(p),
                Err(_) => return Err(WebError::Busy(Some(Duration::from_secs(1)))),
            }
        }
        None => None,
    };

    match Limited::new(body, max_bytes).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(e) if e.downcast_ref::<LengthLimitError>().is_some() => Err(WebError::PayloadTooLarge),
        Err(e) => Err(WebError::BadRequest(format!(
            "could not read request body: {e}"
        ))),
    }
}

impl Request {
    /// Build the `Request` skeleton from a hyper request *without*
    /// consuming the body. The body stream is returned alongside; the
    /// caller passes it back to [`Request::collect_body`] once it has
    /// resolved the right body-size cap (which may depend on the matched
    /// route — see `Server::handle_request_inner`).
    ///
    /// This is the cheap half of [`Request::from_hyper`]: the per-request
    /// allocations are just the `path_parts` Vec and `query_params`
    /// HashMap; no IO happens. Use it when you need to inspect the
    /// request shape before deciding how (or whether) to read the body.
    pub fn from_hyper_parts(
        req: hyper::Request<hyper::body::Incoming>,
    ) -> (Self, hyper::body::Incoming) {
        let (parts, body) = req.into_parts();

        let path_parts: Vec<String> = parts
            .uri
            .path()
            .trim_matches('/')
            .split('/')
            .map(String::from)
            .filter(|s| !s.is_empty())
            .collect();

        let query_params = parts
            .uri
            .query()
            .map(|q| {
                collect_pairs(
                    serde_urlencoded::from_str::<Vec<(String, String)>>(q).unwrap_or_default(),
                )
            })
            .unwrap_or_default();

        let skeleton = Self {
            method: parts.method,
            path_parts,
            query_params,
            // Filled by `collect_body`. Left empty on error so the
            // skeleton-on-error contract holds.
            body: Bytes::new(),
            headers: parts.headers,
            // Stamped by the server once the controller is matched (the
            // skeleton predates routing, so it starts `None`).
            rate_limit_class: None,
        };
        (skeleton, body)
    }

    /// Consume the body stream into this skeleton, capped at
    /// `max_body_bytes` and (optionally) reserved against the
    /// framework-wide inflight-bytes budget.
    ///
    /// Returns `Err((self, err))` on body failure (413, 400 truncated,
    /// 503 budget exhausted) — `self` is the same skeleton (with `body`
    /// still empty) the caller passed in, so the error response still
    /// has the request headers etc. and flows through the after-chain
    /// like every other reply.
    pub async fn collect_body(
        mut self,
        body: hyper::body::Incoming,
        max_body_bytes: usize,
        inflight_budget: Option<&Arc<Semaphore>>,
    ) -> Result<Self, (Self, WebError)> {
        match collect_body_capped(body, max_body_bytes, inflight_budget).await {
            Ok(body_bytes) => {
                self.body = body_bytes;
                Ok(self)
            }
            Err(e) => Err((self, e)),
        }
    }

    /// Creates a new `Request` from a `hyper::Request`, buffering its body
    /// (capped at `max_body_bytes` — see `Server::with_max_body_bytes`).
    ///
    /// Convenience wrapper around [`Request::from_hyper_parts`] +
    /// [`Request::collect_body`]: useful for tests and for callers that
    /// don't need to inspect the request shape before deciding the cap.
    /// The server uses the two-step form directly so it can resolve the
    /// cap from the matched controller (Phase 1) or route (Phase 2)
    /// before reading the body.
    ///
    /// Returns `Err((skeleton, err))` when body collection fails (413
    /// over the cap, 400 truncated, or 503 when the framework-level
    /// `with_max_inflight_body_bytes` budget is exhausted). The returned
    /// `Request` skeleton is populated with method / path / query /
    /// headers — only `body` is empty — so the caller can route the
    /// error through the normal reply pipeline.
    pub async fn from_hyper(
        req: hyper::Request<hyper::body::Incoming>,
        max_body_bytes: usize,
        inflight_budget: Option<&Arc<Semaphore>>,
    ) -> Result<Self, (Self, WebError)> {
        let (skeleton, body) = Self::from_hyper_parts(req);
        skeleton
            .collect_body(body, max_body_bytes, inflight_budget)
            .await
    }

    /// Converts the request into a `Params` object for controller methods.
    ///
    /// Body handling is content-type discriminated. Each non-empty body
    /// must declare its type via `Content-Type`; the four outcomes are:
    ///
    /// * `application/json` (or `application/...+json`) → parse JSON;
    ///   `body = Some(value)`, `raw_body = original bytes`. A parse
    ///   error becomes `WebError::BadRequest`.
    /// * `application/x-www-form-urlencoded` → parse into fields and
    ///   *append* them to the query multimap (so a form field with the
    ///   same name as a query parameter accumulates rather than
    ///   clobbering); `body = None`, `raw_body = bytes`. A parse error
    ///   becomes `WebError::BadRequest`.
    /// * any other content-type (`application/octet-stream`,
    ///   `application/zip`, …) → `body = None`, `raw_body = bytes`.
    ///   The handler reads `params.body_bytes()` directly.
    /// * empty body → `body = None`, `raw_body = Bytes::new()`. The
    ///   content-type header is ignored when there's nothing to parse.
    ///
    /// A non-empty body with **no** `Content-Type` header is rejected:
    /// "discipline tightening" per design — the framework refuses to
    /// guess, and the handler never sees a body whose shape it can't
    /// trust. (This is a behavior change from the previous
    /// auto-JSON-sniff path; auto-sniff silently dropped binary
    /// payloads on the floor.)
    pub fn to_params(&self) -> Result<Params, WebError> {
        let mut all_params = self.query_params.clone();

        let content_type = self
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| {
                s.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
            });

        let json_body = if self.body.is_empty() {
            None
        } else {
            match content_type.as_deref() {
                Some(ct) if is_json_content_type(ct) => {
                    Some(serde_json::from_slice(&self.body).map_err(|e| {
                        WebError::BadRequest(format!("body is not valid JSON: {e}"))
                    })?)
                }
                Some("application/x-www-form-urlencoded") => {
                    let form_pairs: Vec<(String, String)> =
                        serde_urlencoded::from_bytes(&self.body).map_err(|e| {
                            WebError::BadRequest(format!("body is not valid form-urlencoded: {e}"))
                        })?;
                    for (name, value) in form_pairs {
                        all_params.entry(name).or_default().push(value);
                    }
                    None
                }
                Some(_) => None,
                None => {
                    return Err(WebError::BadRequest(
                        "non-empty request body requires a Content-Type header".into(),
                    ));
                }
            }
        };

        // Propagate headers as a lowercase-keyed multimap so controllers can
        // do case-insensitive lookup *and* see every value when a header
        // appears more than once (`Forwarded`, `Via`, etc. — common when a
        // proxy chain prepends one entry per hop). Values that aren't valid
        // UTF-8 are skipped. Receipt order within a name is preserved.
        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        for (name, value) in self.headers.iter() {
            if let Ok(v) = value.to_str() {
                headers
                    .entry(name.as_str().to_ascii_lowercase())
                    .or_default()
                    .push(v.to_string());
            }
        }

        Ok(Params::new(
            method_to_verb(&self.method),
            all_params,
            json_body,
            self.body.clone(),
            headers,
        ))
    }
}

/// Returns true for the JSON media types we parse: bare
/// `application/json` and the `application/<subtype>+json`
/// structured-suffix family (per RFC 6839 §3.1) so callers can use
/// e.g. `application/vnd.example+json` without surprise.
fn is_json_content_type(ct: &str) -> bool {
    ct == "application/json" || ct.ends_with("+json")
}

fn method_to_verb(method: &http::Method) -> Verb {
    match method {
        m if m == http::Method::GET => Verb::GET,
        m if m == http::Method::POST => Verb::POST,
        m if m == http::Method::PUT => Verb::PUT,
        m if m == http::Method::DELETE => Verb::DELETE,
        m if m == http::Method::PATCH => Verb::PATCH,
        m if m == http::Method::HEAD => Verb::HEAD,
        m if m == http::Method::OPTIONS => Verb::OPTIONS,
        _ => Verb::GET,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn req(content_type: Option<&str>, body: &[u8]) -> Request {
        let mut headers = HeaderMap::new();
        if let Some(ct) = content_type {
            headers.insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_str(ct).unwrap(),
            );
        }
        Request {
            method: http::Method::POST,
            path_parts: vec!["whatever".into()],
            query_params: HashMap::new(),
            body: Bytes::copy_from_slice(body),
            headers,
            rate_limit_class: None,
        }
    }

    #[test]
    fn empty_body_no_content_type_is_ok() {
        let params = req(None, b"").to_params().expect("empty + no CT is fine");
        assert!(params.body_bytes().is_empty());
    }

    #[test]
    fn json_body_is_parsed() {
        let params = req(Some("application/json"), br#"{"x":1}"#)
            .to_params()
            .expect("valid JSON");
        // Round-trip through the parsed view.
        let v = params.json_body().expect("body present");
        assert_eq!(v["x"], 1);
        // Raw bytes are also preserved.
        assert_eq!(params.body_bytes().as_ref(), br#"{"x":1}"#);
    }

    #[test]
    fn json_body_with_charset_param_is_parsed() {
        // `Content-Type: application/json; charset=utf-8` is canonical.
        // The discriminator splits on `;` and trims, so the parameter is
        // ignored as long as the media type is json.
        let params = req(Some("application/json; charset=utf-8"), br#"{"x":1}"#)
            .to_params()
            .expect("valid JSON with charset");
        assert_eq!(params.json_body().unwrap()["x"], 1);
    }

    #[test]
    fn vendor_plus_json_subtype_is_parsed() {
        // RFC 6839 structured suffix: `application/vnd.example+json` is
        // semantically JSON. We support that family for compatibility
        // with API conventions that use vendor media types.
        let params = req(Some("application/vnd.example+json"), br#"{"x":1}"#)
            .to_params()
            .expect("valid +json");
        assert_eq!(params.json_body().unwrap()["x"], 1);
    }

    #[test]
    fn malformed_json_is_a_400() {
        match req(Some("application/json"), b"not json").to_params() {
            Err(WebError::BadRequest(msg)) => assert!(msg.contains("valid JSON"), "msg = {msg:?}"),
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("must reject malformed JSON"),
        }
    }

    #[test]
    fn form_body_merges_into_query() {
        let params = req(
            Some("application/x-www-form-urlencoded"),
            b"username=alice&kind=admin",
        )
        .to_params()
        .expect("valid form");
        // Form fields are merged into the query map; `body` (parsed
        // JSON view) is `None` because the body wasn't JSON.
        assert!(params.json_body().is_err());
        // Raw body still travels for handlers that want it.
        assert_eq!(params.body_bytes().as_ref(), b"username=alice&kind=admin");
    }

    #[test]
    fn binary_body_round_trips() {
        let zip_bytes = b"PK\x03\x04\x00\x00fake-zip";
        let params = req(Some("application/zip"), zip_bytes)
            .to_params()
            .expect("opaque CT is fine");
        // Not parsed as JSON — the parsed view is empty.
        assert!(params.json_body().is_err());
        // Raw bytes are preserved verbatim for `params.body_bytes()`.
        assert_eq!(params.body_bytes().as_ref(), zip_bytes);
    }

    #[test]
    fn non_empty_body_without_content_type_is_a_400() {
        match req(None, b"some payload").to_params() {
            Err(WebError::BadRequest(msg)) => {
                assert!(msg.contains("Content-Type"), "msg = {msg:?}")
            }
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("must reject non-empty body without Content-Type"),
        }
    }

    #[test]
    fn collect_pairs_keeps_repeated_keys_in_order() {
        let m = collect_pairs(vec![
            ("tag".into(), "a".into()),
            ("page".into(), "1".into()),
            ("tag".into(), "b".into()),
        ]);
        assert_eq!(m.get("tag").unwrap(), &["a".to_string(), "b".to_string()]);
        assert_eq!(m.get("page").unwrap(), &["1".to_string()]);
    }

    #[tokio::test]
    async fn body_within_limit_round_trips() {
        let body = http_body_util::Full::new(Bytes::from_static(b"hello"));
        assert_eq!(
            collect_body_capped(body, 1024, None).await.unwrap(),
            Bytes::from_static(b"hello")
        );
    }

    #[tokio::test]
    async fn body_over_limit_is_413() {
        let body = http_body_util::Full::new(Bytes::from(vec![0u8; 2048]));
        match collect_body_capped(body, 1024, None).await {
            Err(WebError::PayloadTooLarge) => {}
            other => panic!("expected PayloadTooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn body_refused_when_inflight_budget_exhausted() {
        // Budget = 100 bytes total. First request reserves its per-request
        // cap of 80 bytes → ok. Second request also wants 80 bytes → 503.
        let budget = Arc::new(Semaphore::new(100));
        // Reserve some so we know the budget is partially used. Hold the
        // permit to simulate an in-flight body.
        let _hold = budget.clone().try_acquire_many_owned(80).expect("acquire");
        let body = http_body_util::Full::new(Bytes::from_static(b"hello"));
        match collect_body_capped(body, 80, Some(&budget)).await {
            Err(WebError::Busy(Some(d))) => assert_eq!(d, Duration::from_secs(1)),
            other => panic!("expected Busy, got {other:?}"),
        }
        // After the held permit drops, a request fits again.
        drop(_hold);
        let body = http_body_util::Full::new(Bytes::from_static(b"hello"));
        assert!(collect_body_capped(body, 80, Some(&budget)).await.is_ok());
    }

    #[test]
    fn duplicate_request_headers_survive_into_params() {
        // hyper's HeaderMap preserves duplicates; `to_params` must too. A
        // proxy chain stamping `Forwarded` twice is the canonical example.
        let mut headers = HeaderMap::new();
        headers.append(
            http::HeaderName::from_static("forwarded"),
            HeaderValue::from_static("for=1.2.3.4"),
        );
        headers.append(
            http::HeaderName::from_static("forwarded"),
            HeaderValue::from_static("for=10.0.0.1"),
        );
        let request = Request {
            method: http::Method::GET,
            path_parts: vec![],
            query_params: HashMap::new(),
            body: Bytes::new(),
            headers,
            rate_limit_class: None,
        };
        let params = request.to_params().expect("ok");
        assert_eq!(params.header("Forwarded"), Some("for=1.2.3.4"));
        assert_eq!(
            params.header_all("Forwarded"),
            ["for=1.2.3.4", "for=10.0.0.1"]
        );
    }

    #[test]
    fn form_body_appends_to_query_multimap_instead_of_overwriting() {
        let mut query = HashMap::new();
        query.insert("tag".to_string(), vec!["from-query".to_string()]);
        let request = Request {
            method: http::Method::POST,
            path_parts: vec!["whatever".into()],
            query_params: query,
            body: Bytes::from_static(b"tag=from-body&note=hi"),
            headers: {
                let mut h = HeaderMap::new();
                h.insert(
                    http::header::CONTENT_TYPE,
                    HeaderValue::from_static("application/x-www-form-urlencoded"),
                );
                h
            },
            rate_limit_class: None,
        };
        let params = request.to_params().expect("valid form");
        // Query value survives; the form field with the same name is appended.
        assert_eq!(params.get_all("tag").unwrap(), ["from-query", "from-body"]);
        // First-wins for the scalar view.
        assert_eq!(params.require("tag").unwrap(), "from-query");
        assert_eq!(params.get_all("note").unwrap(), ["hi"]);
    }
}
