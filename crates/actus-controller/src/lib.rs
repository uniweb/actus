//! Public API types and utilities for the Actus controller system — the
//! `Controller` trait, the typed [`Params`] / [`ExtractedParams`], the route
//! metadata (`Verb`, `ParamDef`, `RouteDef`), and the route-resolution
//! [`routing`] helpers. This is what user code and the `#[controller]` /
//! `routes!` / `app_routes!` macros' generated code interact with.
#![warn(missing_docs)]

pub use async_trait::async_trait;
use bytes::Bytes;
use serde_json::Value as JsonValue;
use std::any::{Any, TypeId};
use std::collections::HashMap;

// Re-export the controller macro and app_routes! macro from the macros crate.
pub use actus_controller_macros::{app_routes, controller};
pub use actus_reply::prelude::*;

// =========================
// HTTP Verbs
// =========================

/// An HTTP method. Used in `routes!` verb prefixes and for the `Allow` header
/// the framework stamps on `405` responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// The HTTP `GET` method.
    GET,
    /// The HTTP `POST` method.
    POST,
    /// The HTTP `PUT` method.
    PUT,
    /// The HTTP `DELETE` method.
    DELETE,
    /// The HTTP `PATCH` method.
    PATCH,
    /// The HTTP `HEAD` method.
    HEAD,
    /// The HTTP `OPTIONS` method.
    OPTIONS,
}

impl Verb {
    /// The canonical uppercase method token (`"GET"`, `"POST"`, …). Used for
    /// the `Allow` header on `405` responses, among other things.
    pub fn as_str(&self) -> &'static str {
        match self {
            Verb::GET => "GET",
            Verb::POST => "POST",
            Verb::PUT => "PUT",
            Verb::DELETE => "DELETE",
            Verb::PATCH => "PATCH",
            Verb::HEAD => "HEAD",
            Verb::OPTIONS => "OPTIONS",
        }
    }
}

impl core::fmt::Display for Verb {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Verbs accepted by a route declared without a verb prefix. Reflects the
/// "verbs are constraints, not identities" stance: an unmarked route imposes
/// no semantic restriction beyond what HTML forms emit natively.
/// Restrictive verbs (PUT/DELETE/PATCH) and protocol verbs (HEAD/OPTIONS)
/// must be opted into explicitly.
pub const DEFAULT_VERBS: &[Verb] = &[Verb::GET, Verb::POST];

// =========================
// Controller mode
// =========================

/// How a controller treats request parameters it didn't declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerMode {
    /// Reject a request that carries parameters the route didn't declare.
    Strict,
    /// Allow undeclared extra parameters to pass through.
    Lax,
}

// =========================
// Parameter definitions
// =========================

/// The declared type of a route parameter — governs how its raw string value
/// is parsed before reaching the handler.
#[derive(Debug, Clone, Copy)]
pub enum ParamType {
    /// A UTF-8 string.
    String,
    /// A signed 64-bit integer (`i64`).
    Int,
    /// An unsigned 64-bit integer (`u64`).
    U64,
    /// An unsigned 32-bit integer (`u32`).
    U32,
    /// A 64-bit float (`f64`).
    F64,
    /// A boolean.
    Bool,
    /// A repeated parameter collected into `Vec<String>`.
    StringArray,
    /// A JSON value (`serde_json::Value`), parsed from the request body.
    Json,
    /// The raw request body as `bytes::Bytes`.
    Bytes,
}

/// A parameter's default value, applied when the request omits it. Declared
/// in `routes!` as `name: Type = default`.
#[derive(Debug, Clone)]
pub enum ParamDefault {
    /// `&'static str` (not `String`) so route metadata can live in a
    /// `static ROUTES: &[RouteDef]` initializer — `String::from(...)` and
    /// `.to_string()` aren't const on stable Rust. Default-application at
    /// runtime borrows the str, allocating only if/when needed.
    String(&'static str),
    /// A default `i64`.
    Int(i64),
    /// A default `u64`.
    U64(u64),
    /// A default `u32`.
    U32(u32),
    /// A default `f64`.
    F64(f64),
    /// A default `bool`.
    Bool(bool),
}

/// Where a parameter's value is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamSource {
    /// From a `{name}` segment of the URL path.
    Path,
    /// From the query string.
    Query,
    /// From the request body.
    Body,
}

/// The compile-time description of one route parameter, recorded by the
/// `routes!` macro and used to extract and parse it at request time.
#[derive(Debug, Clone)]
pub struct ParamDef {
    /// The parameter name.
    pub name: &'static str,
    /// The declared type the raw value is parsed into.
    pub ty: ParamType,
    /// Where the value is read from (path, query, or body).
    pub source: ParamSource,
    /// The default applied when the request omits the parameter, if any.
    pub default: Option<ParamDefault>,
}

// =========================
// Route definition
// =========================

/// The compile-time description of one route in a controller, recorded by the
/// `routes!` macro. The framework matches and dispatches against these, and
/// tools can introspect them (e.g. the OpenAPI generator).
#[derive(Debug, Clone)]
pub struct RouteDef {
    /// The route pattern relative to the controller's mount (e.g.
    /// `"posts/{id}"`).
    pub pattern: &'static str,
    /// Internal dispatch token (`"handler_0"`, `"handler_1"`, …) produced by
    /// the `#[controller]` macro. Opaque to user code; for the
    /// human-readable handler method name (useful for OpenAPI operationIds
    /// and the like), see [`RouteDef::handler`].
    pub handler_id: &'static str,
    /// Handler method name as written in the controller's `impl` block
    /// (`"list"`, `"get"`, `"create"`, …). Captured at macro-expansion time
    /// so introspection tools (OpenAPI doc generators, route audit scripts)
    /// can identify the handler without grepping.
    pub handler: &'static str,
    /// Verbs this route accepts. Always non-empty: a single-element slice for
    /// an explicitly declared verb, [`DEFAULT_VERBS`] for an unmarked route.
    pub verb: &'static [Verb],
    /// The route's declared parameters, in order.
    pub params: &'static [ParamDef],
    /// The handler method's `///` doc comment, if any — surfaced to tools like
    /// the OpenAPI generator.
    pub doc: Option<&'static str>,
}

// =========================
// Runtime parameter handling
// =========================

/// Raw parameters from the HTTP request, plus headers, the parsed body, and
/// a typed extensions slot for per-request data that prepare hooks (or
/// middleware) want to attach for handlers to read.
///
/// Headers are a **multimap**, just like query: each lowercased name maps to
/// every value seen for it, in request order. Scalar accessor
/// [`Params::header`] reads the first value (the common case);
/// [`Params::header_all`] returns every value (for `Forwarded`, `Via`, etc.
/// which can legitimately appear multiple times — e.g. in a proxy chain).
///
/// Query parameters are a **multimap**: each name maps to *every* value seen
/// for it, in request order — `?tags=a&tags=b` is `{"tags": ["a", "b"]}`.
/// Scalar accessors (`require`, `get_u64`, …) read the first value;
/// [`Params::get_all`] returns the whole list (this is what backs
/// `Vec<String>` handler parameters, so a one-element array works the same
/// as a many-element one). Repeated *keys* are what create multiple values;
/// a comma in a single value (`?tags=a,b`) is just one value `"a,b"`.
///
/// `body` and `raw_body` are populated by the server's `Request::to_params`
/// using `Content-Type` discrimination:
///
/// * `application/json` → `body = Some(parsed)`, `raw_body = original_bytes`.
/// * `application/x-www-form-urlencoded` → fields are appended into `query`
///   (same multimap); `body = None`, `raw_body = original_bytes`.
/// * any other `Content-Type` (including `application/octet-stream`,
///   `application/zip`, …) → `body = None`, `raw_body = original_bytes`.
/// * empty body → `body = None`, `raw_body = Bytes::new()`.
///
/// A non-empty request body without a `Content-Type` header is rejected
/// at ingest with `WebError::BadRequest` — `body` and `raw_body` are
/// therefore never both empty by accident.
pub struct Params {
    verb: Verb,
    query: HashMap<String, Vec<String>>,
    body: Option<JsonValue>,
    raw_body: Bytes,
    headers: HashMap<String, Vec<String>>,
    extensions: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl Params {
    /// Construct a `Params` from the extracted request pieces. Called by the
    /// framework's dispatch path; handlers receive an already-built `Params`.
    pub fn new(
        verb: Verb,
        query: HashMap<String, Vec<String>>,
        body: Option<JsonValue>,
        raw_body: Bytes,
        headers: HashMap<String, Vec<String>>,
    ) -> Self {
        Self {
            verb,
            query,
            body,
            raw_body,
            headers,
            extensions: HashMap::new(),
        }
    }

    /// First value of query parameter `name`, if present. The basis for all
    /// the scalar accessors below.
    fn first(&self, name: &str) -> Option<&str> {
        self.query
            .get(name)
            .and_then(|values| values.first())
            .map(String::as_str)
    }

    /// The entire query multimap — every parameter name and all its values,
    /// in request order, plus any folded `application/x-www-form-urlencoded`
    /// body fields.
    ///
    /// Use this for "catch the rest" handlers — a search endpoint with
    /// open-ended filters, a request proxy, etc.: declare `params: &Params`,
    /// mark the controller `#[controller(lax)]` so strict mode doesn't reject
    /// the undeclared keys, and read `params.query()`. Handlers that know
    /// their parameters up front should declare them as typed arguments
    /// instead; this is the escape hatch, not the default.
    pub fn query(&self) -> &HashMap<String, Vec<String>> {
        &self.query
    }

    /// Raw bytes of the request body, before any content-type-specific
    /// parsing. Always present (`Bytes::new()` for empty bodies).
    ///
    /// Use this when the handler consumes binary uploads (`.uwx`, image
    /// blobs, etc.). For JSON bodies, prefer [`Params::json_body`] or
    /// macro-extracted typed args — those operate on the parsed value
    /// the framework already produced from the same bytes.
    pub fn body_bytes(&self) -> &Bytes {
        &self.raw_body
    }

    /// Stash a value for later retrieval. The value is keyed by its type;
    /// inserting a second value of the same type replaces the first.
    /// Typically called from a `prepare` hook to pass a resolved user (or
    /// other request-scoped state) through to the handler.
    pub fn insert<T: Any + Send + Sync>(&mut self, value: T) -> Option<T> {
        self.extensions
            .insert(TypeId::of::<T>(), Box::new(value))
            .and_then(|prev| prev.downcast::<T>().ok().map(|b| *b))
    }

    /// Look up a value previously inserted with [`Params::insert`].
    pub fn get<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.extensions
            .get(&TypeId::of::<T>())
            .and_then(|b| b.downcast_ref::<T>())
    }

    /// The HTTP verb this request was dispatched with.
    pub fn verb(&self) -> Verb {
        self.verb
    }

    /// Look up a request header (case-insensitive). Returns the *first*
    /// value if the header appears more than once; see [`Params::header_all`]
    /// for every value.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .and_then(|values| values.first())
            .map(String::as_str)
    }

    /// Every value for a request header (case-insensitive), in receipt
    /// order. Empty slice if the header wasn't present. Use this for headers
    /// that can legitimately appear multiple times — `Forwarded`, `Via`,
    /// `X-Forwarded-For` (when proxies emit one entry per hop), etc.
    pub fn header_all(&self, name: &str) -> &[String] {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Convenience: extract a Bearer token from the `Authorization` header.
    /// Returns `None` if the header is missing or doesn't start with `Bearer `.
    pub fn bearer_token(&self) -> Option<&str> {
        let auth = self.header("authorization")?;
        auth.strip_prefix("Bearer ")
            .or_else(|| auth.strip_prefix("bearer "))
    }

    // Core methods for parameter extraction

    /// The first value of query parameter `name`, or a `400 Bad Request` if
    /// it's absent.
    pub fn require(&self, name: &str) -> Result<&str, WebError> {
        self.first(name)
            .ok_or_else(|| WebError::BadRequest(format!("Missing required parameter: {}", name)))
    }

    /// Look up a query parameter's first value; returns `None` if absent.
    pub fn get_optional(&self, name: &str) -> Option<&str> {
        self.first(name)
    }

    /// Parse query parameter `name` as an `i64`; `400` if missing or unparsable.
    pub fn get_int(&self, name: &str) -> Result<i64, WebError> {
        self.require(name)?
            .parse()
            .map_err(|_| WebError::BadRequest(format!("Invalid integer: {}", name)))
    }

    /// Parse optional query parameter `name` as an `i64`; `Ok(None)` if absent,
    /// `400` if present but unparsable.
    pub fn get_int_optional(&self, name: &str) -> Result<Option<i64>, WebError> {
        match self.get_optional(name) {
            Some(s) => s
                .parse()
                .map(Some)
                .map_err(|_| WebError::BadRequest(format!("Invalid integer: {}", name))),
            None => Ok(None),
        }
    }

    // Extended type methods

    /// Parse query parameter `name` as a `u64`; `400` if missing or unparsable.
    pub fn get_u64(&self, name: &str) -> Result<u64, WebError> {
        self.require(name)?
            .parse()
            .map_err(|_| WebError::BadRequest(format!("Invalid u64: {}", name)))
    }

    /// Parse optional query parameter `name` as a `u64`; `Ok(None)` if absent,
    /// `400` if present but unparsable.
    pub fn get_u64_optional(&self, name: &str) -> Result<Option<u64>, WebError> {
        match self.get_optional(name) {
            Some(s) => s
                .parse()
                .map(Some)
                .map_err(|_| WebError::BadRequest(format!("Invalid u64: {}", name))),
            None => Ok(None),
        }
    }

    /// Parse query parameter `name` as a `u32`; `400` if missing or unparsable.
    pub fn get_u32(&self, name: &str) -> Result<u32, WebError> {
        self.require(name)?
            .parse()
            .map_err(|_| WebError::BadRequest(format!("Invalid u32: {}", name)))
    }

    /// Parse optional query parameter `name` as a `u32`; `Ok(None)` if absent,
    /// `400` if present but unparsable.
    pub fn get_u32_optional(&self, name: &str) -> Result<Option<u32>, WebError> {
        match self.get_optional(name) {
            Some(s) => s
                .parse()
                .map(Some)
                .map_err(|_| WebError::BadRequest(format!("Invalid u32: {}", name))),
            None => Ok(None),
        }
    }

    /// Parse query parameter `name` as an `f64`; `400` if missing or unparsable.
    pub fn get_f64(&self, name: &str) -> Result<f64, WebError> {
        self.require(name)?
            .parse()
            .map_err(|_| WebError::BadRequest(format!("Invalid float: {}", name)))
    }

    /// Parse optional query parameter `name` as an `f64`; `Ok(None)` if absent,
    /// `400` if present but unparsable.
    pub fn get_f64_optional(&self, name: &str) -> Result<Option<f64>, WebError> {
        match self.get_optional(name) {
            Some(s) => s
                .parse()
                .map(Some)
                .map_err(|_| WebError::BadRequest(format!("Invalid float: {}", name))),
            None => Ok(None),
        }
    }

    /// Read query parameter `name` as a bool — `false` when absent, empty,
    /// `"false"`, or `"0"`; `true` otherwise.
    pub fn get_bool(&self, name: &str) -> bool {
        self.first(name)
            .map(|s| !s.is_empty() && s != "false" && s != "0")
            .unwrap_or(false)
    }

    /// Like [`Params::get_bool`], but `None` when the parameter is absent
    /// (rather than `false`).
    pub fn get_bool_optional(&self, name: &str) -> Option<bool> {
        self.first(name)
            .map(|s| !s.is_empty() && s != "false" && s != "0")
    }

    /// All values of query parameter `name`, in request order (empty if the
    /// name wasn't present). Backs `Vec<String>` handler parameters.
    pub fn get_all(&self, name: &str) -> Result<Vec<String>, WebError> {
        Ok(self.query.get(name).cloned().unwrap_or_default())
    }

    /// All values of query parameter `name`; `None` if the name wasn't present
    /// at all (vs. `Some(vec![])`, which urlencoding can't actually produce).
    pub fn get_all_optional(&self, name: &str) -> Option<Vec<String>> {
        self.query.get(name).cloned()
    }

    /// The parsed JSON request body, or `400 Bad Request` if there wasn't one.
    pub fn json_body(&self) -> Result<JsonValue, WebError> {
        self.body
            .clone()
            .ok_or_else(|| WebError::BadRequest("Missing JSON body".to_string()))
    }

    /// In strict mode, return any query keys *not* in `expected` (so the caller
    /// can reject the request); `None` if every key was expected.
    pub fn check_unexpected(&self, expected: &[&str]) -> Option<Vec<String>> {
        let unexpected: Vec<String> = self
            .query
            .keys()
            .filter(|k| !expected.contains(&k.as_str()))
            .cloned()
            .collect();

        if unexpected.is_empty() {
            None
        } else {
            Some(unexpected)
        }
    }
}

// =========================
// Extracted parameters (after route resolution)
// =========================

/// Parameters extracted after route resolution — path captures plus the
/// declared query and body values. The `#[controller]` macro reads typed
/// handler arguments out of this; application code rarely touches it directly.
#[derive(Debug)]
pub struct ExtractedParams {
    /// Path captures (single-segment `{name}` or the joined `{...rest}`).
    path: HashMap<String, String>,
    /// Declared query parameters that were present, with *all* their values
    /// (see [`Params`] — query is a multimap).
    query: HashMap<String, Vec<String>>,
    body: Option<JsonValue>,
    raw_body: Bytes,
}

impl ExtractedParams {
    /// The single scalar value for `name`: a path capture takes precedence,
    /// otherwise the first query value. `None` if neither has it.
    fn scalar(&self, name: &str) -> Option<&str> {
        self.path.get(name).map(String::as_str).or_else(|| {
            self.query
                .get(name)
                .and_then(|values| values.first())
                .map(String::as_str)
        })
    }

    fn require_scalar(&self, name: &str) -> Result<&str, WebError> {
        self.scalar(name)
            .ok_or_else(|| WebError::BadRequest(format!("Missing parameter: {}", name)))
    }

    /// The value of path/query parameter `name` as a `String`; `400` if absent.
    pub fn get_string(&self, name: &str) -> Result<String, WebError> {
        self.require_scalar(name).map(str::to_string)
    }

    /// Parse path/query parameter `name` as an `i64`; `400` if missing or
    /// unparsable.
    pub fn get_i64(&self, name: &str) -> Result<i64, WebError> {
        self.require_scalar(name)?
            .parse()
            .map_err(|_| WebError::BadRequest(format!("Invalid integer: {}", name)))
    }

    /// Parse path/query parameter `name` as a `u64`; `400` if missing or
    /// unparsable.
    pub fn get_u64(&self, name: &str) -> Result<u64, WebError> {
        self.require_scalar(name)?
            .parse()
            .map_err(|_| WebError::BadRequest(format!("Invalid u64: {}", name)))
    }

    /// Parse path/query parameter `name` as a `u32`; `400` if missing or
    /// unparsable.
    pub fn get_u32(&self, name: &str) -> Result<u32, WebError> {
        self.require_scalar(name)?
            .parse()
            .map_err(|_| WebError::BadRequest(format!("Invalid u32: {}", name)))
    }

    /// Parse path/query parameter `name` as an `f64`; `400` if missing or
    /// unparsable.
    pub fn get_f64(&self, name: &str) -> Result<f64, WebError> {
        self.require_scalar(name)?
            .parse()
            .map_err(|_| WebError::BadRequest(format!("Invalid float: {}", name)))
    }

    /// Read path/query parameter `name` as a bool — `false` when absent,
    /// empty, `"false"`, or `"0"`; `true` otherwise.
    pub fn get_bool(&self, name: &str) -> Result<bool, WebError> {
        Ok(self
            .scalar(name)
            .map(|s| !s.is_empty() && s != "false" && s != "0")
            .unwrap_or(false))
    }

    /// All values for `name` (in request order; empty if the name wasn't
    /// present). Backs `Vec<String>` handler parameters — a one-element list
    /// (`?tags=a`) and a many-element one (`?tags=a&tags=b`) flow through the
    /// same path.
    pub fn get_string_array(&self, name: &str) -> Result<Vec<String>, WebError> {
        Ok(self.query.get(name).cloned().unwrap_or_default())
    }

    /// The parsed JSON request body, or `400 Bad Request` if there wasn't one.
    pub fn get_json_body(&self) -> Result<JsonValue, WebError> {
        self.body
            .clone()
            .ok_or_else(|| WebError::BadRequest("Missing JSON body".to_string()))
    }

    /// Raw bytes of the request body. See [`Params::body_bytes`] for the
    /// content-type-aware semantics. Used by the macro to extract a
    /// `Bytes` handler argument.
    pub fn get_body_bytes(&self) -> Bytes {
        self.raw_body.clone()
    }
}

// =========================
// Routing utilities module
// =========================

/// Route resolution helpers — matching a request path + verb against a
/// controller's `&[RouteDef]` and extracting the path/query/body parameters.
/// Used by the `#[controller]` macro's generated dispatch; exposed for tools
/// and tests that need to resolve routes directly.
pub mod routing {
    use super::*;
    use std::collections::HashMap;

    /// Main route resolution function. Tries each route in declaration order
    /// and returns the first whose path pattern *and* verb both match, along
    /// with its extracted parameters.
    ///
    /// "Declaration order" matters: when two patterns can match the same
    /// action (e.g. `"special"` and `"{id}"`), the one declared first wins —
    /// list the more specific route earlier.
    ///
    /// When no route fully matches, the error distinguishes:
    /// - `WebError::MethodNotAllowed(methods)`: at least one route's *path*
    ///   pattern matched but its verb didn't; `methods` is the (sorted, deduped)
    ///   set of verbs those routes accept, which the caller surfaces as the
    ///   `Allow` header.
    /// - `WebError::NotFound`: no route's path pattern matched at all.
    #[inline]
    pub fn resolve<'a>(
        routes: &'a [RouteDef],
        action: &str,
        params: &Params,
        mode: ControllerMode,
    ) -> Result<(&'a RouteDef, ExtractedParams), WebError> {
        // Verbs accepted by routes whose *path* matched but whose verb didn't.
        let mut allowed_methods: Vec<&'static str> = Vec::new();

        for route in routes {
            // Pattern match first.
            let path_params = match match_pattern(route.pattern, action) {
                Some(p) => p,
                None => continue,
            };

            // Then verb check. `route.verb` is the (non-empty) set of verbs
            // this route accepts; an unmarked route carries `DEFAULT_VERBS`.
            if !route.verb.contains(&params.verb()) {
                for v in route.verb {
                    let token = v.as_str();
                    if !allowed_methods.contains(&token) {
                        allowed_methods.push(token);
                    }
                }
                continue;
            }

            // Full match: build extracted params and return.
            {
                // Build extracted params
                let mut extracted = ExtractedParams {
                    path: path_params,
                    query: HashMap::new(),
                    body: params.body.clone(),
                    raw_body: params.raw_body.clone(),
                };

                // Extract and validate parameters
                for param_def in route.params {
                    match param_def.source {
                        ParamSource::Path => {
                            // Path-source params come from `{name}` / `{...name}`
                            // tokens, which `match_pattern` always captures when
                            // the pattern matched — so this is already in
                            // `extracted.path`. (The `#[controller]` macro is what
                            // guarantees the `ParamDef` ↔ pattern correspondence;
                            // a hand-built `RouteDef` that violates it would trip
                            // this in debug builds.)
                            debug_assert!(
                                extracted.path.contains_key(param_def.name),
                                "path parameter `{}` not captured by pattern",
                                param_def.name
                            );
                        }
                        ParamSource::Query => {
                            // Extract from query params. A `Vec<String>`
                            // parameter is inherently optional — absent means
                            // the empty list, never a 400. Other scalar types
                            // are required unless they declared a default.
                            if let Some(value) = params.query.get(param_def.name) {
                                extracted
                                    .query
                                    .insert(param_def.name.to_string(), value.clone());
                            } else if param_def.default.is_none()
                                && !matches!(param_def.ty, ParamType::StringArray)
                            {
                                return Err(WebError::BadRequest(format!(
                                    "Missing required parameter: {}",
                                    param_def.name
                                )));
                            }
                            // Defaults are applied in the handler extraction phase.
                        }
                        ParamSource::Body => {
                            // JSON body is already handled
                        }
                    }
                }

                // Check for unexpected parameters in strict mode
                if mode == ControllerMode::Strict {
                    let expected: Vec<&str> = route
                        .params
                        .iter()
                        .filter(|p| p.source == ParamSource::Query)
                        .map(|p| p.name)
                        .collect();

                    if let Some(unexpected) = params.check_unexpected(&expected) {
                        return Err(WebError::BadRequest(format!(
                            "Unexpected parameters: {}",
                            unexpected.join(", ")
                        )));
                    }
                }

                return Ok((route, extracted));
            }
        }

        if allowed_methods.is_empty() {
            Err(WebError::NotFound)
        } else {
            allowed_methods.sort_unstable();
            allowed_methods.dedup();
            Err(WebError::MethodNotAllowed(allowed_methods))
        }
    }

    /// If `segment` is a `{...name}` rest token, returns `name` (non-empty).
    fn rest_token_name(segment: &str) -> Option<&str> {
        segment
            .strip_prefix("{...")
            .and_then(|s| s.strip_suffix('}'))
            .filter(|name| !name.is_empty())
    }

    /// Match one fixed (non-rest) pattern segment against a path segment,
    /// recording a capture for `{name}` tokens. Returns `false` if a literal
    /// segment doesn't match.
    fn match_fixed_segment(
        pattern_part: &str,
        path_part: &str,
        params: &mut HashMap<String, String>,
    ) -> bool {
        if let Some(param_name) = pattern_part
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
        {
            params.insert(param_name.to_string(), path_part.to_string());
            true
        } else {
            pattern_part == path_part
        }
    }

    /// Split a pattern or action into its segments. A pattern/action is a
    /// `/`-joined list of **non-empty** segments; empty segments — from
    /// leading, trailing, or doubled slashes, and notably from the empty
    /// action `""` (which `str::split` would otherwise yield as `[""]`) —
    /// are not segments. This is the same normalization `Request` applies to
    /// the full request path, applied here to the per-controller action and
    /// the route patterns it's matched against.
    fn segments(s: &str) -> Vec<&str> {
        s.split('/').filter(|seg| !seg.is_empty()).collect()
    }

    /// Match a route pattern against an action path.
    /// Returns extracted path parameters if matched.
    ///
    /// Both sides are viewed as lists of non-empty path segments.
    /// A literal segment or `{name}` token is **required** — it has no match
    /// when there's no segment to fill it, so `match_pattern("{id}", "")` is
    /// `None`. (A controller that wants to serve its collection root declares
    /// `"" => …`.)
    ///
    /// A trailing `{...name}` token is a *rest* parameter: it captures the
    /// remainder of the path (slashes included) and matches **zero or more**
    /// segments — `match_pattern("{...path}", "")` is `Some({path: ""})`, and
    /// `"{folder_id}/{...path}"` matches `"abc"` (`path == ""`) and
    /// `"abc/x/y"` (`path == "x/y"`) but **not** `""` (the required
    /// `folder_id` has no segment). The `#[controller]` macro enforces that
    /// `{...name}` appears at most once and only as the final token; this
    /// function trusts that and only inspects the last token.
    #[inline]
    pub fn match_pattern(pattern: &str, path: &str) -> Option<HashMap<String, String>> {
        let pattern_parts = segments(pattern);
        let path_parts = segments(path);
        let mut params = HashMap::new();

        if let Some(rest_name) = pattern_parts.last().and_then(|s| rest_token_name(s)) {
            // Everything before the rest token is a fixed prefix that must
            // match segment-for-segment; the rest token soaks up whatever
            // is left (possibly nothing).
            let fixed = &pattern_parts[..pattern_parts.len() - 1];
            if path_parts.len() < fixed.len() {
                return None;
            }
            for (pattern_part, path_part) in fixed.iter().zip(path_parts.iter()) {
                if !match_fixed_segment(pattern_part, path_part, &mut params) {
                    return None;
                }
            }
            params.insert(rest_name.to_string(), path_parts[fixed.len()..].join("/"));
            return Some(params);
        }

        if pattern_parts.len() != path_parts.len() {
            return None;
        }
        for (pattern_part, path_part) in pattern_parts.iter().zip(path_parts.iter()) {
            if !match_fixed_segment(pattern_part, path_part, &mut params) {
                return None;
            }
        }
        Some(params)
    }
}

// =========================
// Controller trait
// =========================

/// The runtime interface every controller implements. Hand-writing this is
/// possible but unusual — the `#[controller]` macro generates the
/// implementation (dispatch table, parameter extraction, the metadata methods)
/// from a controller's `impl` block and its `routes!` declaration.
#[async_trait]
pub trait Controller: Send + Sync {
    /// Route `action` (the path below this controller's mount) to the matching
    /// handler and run it, returning its [`Reply`]. Generated by the macro.
    async fn actus_dispatch(&self, action: &str, params: Params) -> Reply;

    /// The controller's type name, for diagnostics and route auditing.
    fn __name(&self) -> &'static str;

    /// The controller's declared routes, for introspection (OpenAPI
    /// generation, route audits). Defaults to empty; the macro overrides it.
    fn actus_describe_routes(&self) -> Vec<RouteDef> {
        vec![]
    }

    /// Per-controller maximum buffered body size, in bytes. Returned by the
    /// `#[controller(max_body_bytes = …)]` attribute when set; `None` means the
    /// controller defers to the server-level cap (`Server::with_max_body_bytes`).
    ///
    /// Resolution at request time (see `Server::handle_request_inner`):
    /// controller value if `Some`, otherwise the server-wide cap, otherwise
    /// `DEFAULT_MAX_BODY_BYTES` (2 MiB).
    ///
    /// The framework calls this *before* buffering the body — so a 1 KB
    /// controller cap rejects a 50 KB request before the bytes are
    /// allocated. (A request body big enough to be a memory concern
    /// shouldn't get past the framework regardless of where the handler
    /// would have rejected it.)
    fn actus_max_body_bytes(&self) -> Option<usize> {
        None
    }

    /// Per-controller rate-limit *class* label, as declared by
    /// `#[controller(rate_limit = "name")]`. `None` (the default) means the
    /// controller declared no class.
    ///
    /// This is a **label, not a policy**. Actus is policy-agnostic: it ships
    /// no limiter algorithm, key function, or store, because the framework
    /// can't pick those correctly for someone else (which key — IP / user /
    /// API key? which algorithm — token bucket / sliding window? which store
    /// — in-memory / Redis?). Those are application decisions, so the limiter
    /// itself stays an application `Middleware`.
    ///
    /// What the framework *does* own is auditability and the response shape.
    /// The server stamps this label onto the matched request (surfaced as
    /// `Request::rate_limit_class` in `actus-server`), so a reviewer can read
    /// each endpoint's rate-limit class straight off the `#[controller(...)]`
    /// line, and an application's rate-limit `Middleware` can map class →
    /// policy and reject over-limit requests with
    /// [`WebError::TooManyRequests`] (429 + `Retry-After`, also framework-owned).
    /// Two controllers sharing a class share a limit namespace; what each
    /// class *means* is the application's call.
    ///
    /// Resolution is per-controller, mirroring [`Controller::actus_max_body_bytes`].
    /// A per-route override would be an additive future change (the same shape
    /// as the per-route body-cap proposal).
    fn actus_rate_limit(&self) -> Option<&'static str> {
        None
    }
}

/// A list of `(mount, controller-factory)` pairs — the route-registration
/// shape the `app_routes!` macro builds when wiring controllers into a router.
pub type Routes = Vec<(
    &'static str,
    Box<dyn Fn() -> Box<dyn Controller> + Send + Sync>,
)>;

/// A marker macro to define routes within a `#[controller]` impl block.
/// The `#[controller]` procedural macro is responsible for parsing this.
#[macro_export]
macro_rules! routes {
    ($($tokens:tt)*) => {};
}

#[cfg(test)]
mod match_pattern_tests {
    use super::routing::match_pattern;

    fn cap(pattern: &str, path: &str) -> Option<Vec<(String, String)>> {
        match_pattern(pattern, path).map(|m| {
            let mut v: Vec<_> = m.into_iter().collect();
            v.sort();
            v
        })
    }

    fn pair(k: &str, v: &str) -> (String, String) {
        (k.to_string(), v.to_string())
    }

    #[test]
    fn fixed_patterns_still_work() {
        assert_eq!(cap("", ""), Some(vec![]));
        assert_eq!(cap("{id}", "42"), Some(vec![pair("id", "42")]));
        assert_eq!(
            cap("posts/{id}/comments", "posts/3/comments"),
            Some(vec![pair("id", "3")])
        );
        assert_eq!(cap("a/b", "a/b/c"), None);
        assert_eq!(cap("a/b/c", "a/b"), None);
        assert_eq!(cap("posts/{id}", "users/3"), None);
    }

    #[test]
    fn required_segments_dont_match_the_empty_action() {
        // A `{id}` (or any literal) is required: it has no match when there's
        // no segment for it. The empty action is the empty segment list, not
        // a one-element list containing `""`.
        assert_eq!(cap("{id}", ""), None);
        assert_eq!(cap("posts", ""), None);
        assert_eq!(cap("{a}/{b}", "x"), None);
        // ...but the empty pattern is *defined* as the empty segment list,
        // so it matches the empty action (this is how `"" => index` works).
        assert_eq!(cap("", ""), Some(vec![]));
        assert_eq!(cap("", "x"), None);
    }

    #[test]
    fn rest_param_captures_remainder() {
        assert_eq!(
            cap("{folder_id}/{...path}", "abc/x/y/z"),
            Some(vec![pair("folder_id", "abc"), pair("path", "x/y/z")])
        );
        // zero trailing segments → rest is empty (folder_id is still present)
        assert_eq!(
            cap("{folder_id}/{...path}", "abc"),
            Some(vec![pair("folder_id", "abc"), pair("path", "")])
        );
        // ...but the required folder_id has no segment in the empty action
        assert_eq!(cap("{folder_id}/{...path}", ""), None);
    }

    #[test]
    fn rest_param_as_sole_token() {
        assert_eq!(cap("{...path}", "a/b/c"), Some(vec![pair("path", "a/b/c")]));
        assert_eq!(cap("{...path}", "a"), Some(vec![pair("path", "a")]));
        // a sole rest token explicitly matches zero segments
        assert_eq!(cap("{...path}", ""), Some(vec![pair("path", "")]));
    }

    #[test]
    fn rest_param_after_literal_prefix() {
        assert_eq!(
            cap("files/{...path}", "files/x/y"),
            Some(vec![pair("path", "x/y")])
        );
        assert_eq!(
            cap("files/{...path}", "files"),
            Some(vec![pair("path", "")])
        );
        assert_eq!(cap("files/{...path}", "other/x"), None);
        // a literal prefix longer than the path can't match
        assert_eq!(cap("a/b/{...path}", "a"), None);
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::routing::resolve;
    use super::*;
    use bytes::Bytes;
    use std::collections::HashMap;

    fn params_with(verb: Verb, query: HashMap<String, Vec<String>>) -> Params {
        Params::new(verb, query, None, Bytes::new(), HashMap::new())
    }

    #[test]
    fn headers_are_a_multimap_first_value_wins_for_scalar_access() {
        // Two values for one header name (e.g. a proxy chain stamping
        // `Forwarded` twice). `header()` returns the first; `header_all()`
        // returns both, in receipt order. Absent headers come back as `None`
        // / empty slice respectively.
        let mut headers = HashMap::new();
        headers.insert(
            "forwarded".to_string(),
            vec!["for=1.2.3.4".to_string(), "for=10.0.0.1".to_string()],
        );
        headers.insert("x-trace-id".to_string(), vec!["abc-123".to_string()]);
        let p = Params::new(Verb::GET, HashMap::new(), None, Bytes::new(), headers);

        // Case-insensitive lookup; first value for scalar access.
        assert_eq!(p.header("Forwarded"), Some("for=1.2.3.4"));
        assert_eq!(p.header("FORWARDED"), Some("for=1.2.3.4"));
        assert_eq!(p.header_all("Forwarded"), ["for=1.2.3.4", "for=10.0.0.1"]);

        // Single-value headers still work — header_all yields a one-element
        // slice, header yields the same value.
        assert_eq!(p.header("X-Trace-Id"), Some("abc-123"));
        assert_eq!(p.header_all("X-Trace-Id"), ["abc-123"]);

        // Absent: None / empty slice.
        assert_eq!(p.header("Authorization"), None);
        assert!(p.header_all("Authorization").is_empty());
    }

    #[test]
    fn params_query_exposes_the_whole_multimap() {
        let mut q = HashMap::new();
        q.insert("a".to_string(), vec!["1".to_string(), "2".to_string()]);
        q.insert("b".to_string(), vec!["3".to_string()]);
        let p = params_with(Verb::GET, q);
        assert_eq!(p.query().len(), 2);
        assert_eq!(
            p.query().get("a").unwrap(),
            &["1".to_string(), "2".to_string()]
        );
        // scalar view still takes the first
        assert_eq!(p.get_optional("a"), Some("1"));
    }

    #[test]
    fn verb_mismatch_yields_405_with_sorted_deduped_allow_list() {
        // `""` matches the action `""` for both routes; the request verb
        // (PUT) matches neither, so we get 405 carrying the union of their
        // verbs — sorted and deduped, so the `Allow` header is deterministic.
        static ROUTES: &[RouteDef] = &[
            RouteDef {
                pattern: "",
                handler_id: "create",
                handler: "create",
                verb: &[Verb::POST],
                params: &[],
                doc: None,
            },
            RouteDef {
                pattern: "",
                handler_id: "list",
                handler: "list",
                verb: &[Verb::GET],
                params: &[],
                doc: None,
            },
        ];
        match resolve(
            ROUTES,
            "",
            &params_with(Verb::PUT, HashMap::new()),
            ControllerMode::Strict,
        ) {
            Err(WebError::MethodNotAllowed(methods)) => assert_eq!(methods, ["GET", "POST"]),
            other => panic!("expected 405, got {other:?}"),
        }
        // GET matches the second route → Ok.
        assert!(
            resolve(
                ROUTES,
                "",
                &params_with(Verb::GET, HashMap::new()),
                ControllerMode::Strict
            )
            .is_ok()
        );
    }

    #[test]
    fn no_pattern_match_is_404_not_405() {
        static ROUTES: &[RouteDef] = &[RouteDef {
            pattern: "items",
            handler_id: "h",
            handler: "h",
            verb: &[Verb::GET],
            params: &[],
            doc: None,
        }];
        match resolve(
            ROUTES,
            "other",
            &params_with(Verb::DELETE, HashMap::new()),
            ControllerMode::Strict,
        ) {
            Err(WebError::NotFound) => {}
            other => panic!("expected 404, got {other:?}"),
        }
    }

    #[test]
    fn vec_string_query_param_collects_all_values() {
        static ROUTES: &[RouteDef] = &[RouteDef {
            pattern: "",
            handler_id: "h",
            handler: "h",
            verb: &[Verb::GET],
            params: &[ParamDef {
                name: "tags",
                ty: ParamType::StringArray,
                source: ParamSource::Query,
                default: None,
            }],
            doc: None,
        }];

        let mut q = HashMap::new();
        q.insert(
            "tags".to_string(),
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
        );
        let (_, extracted) = resolve(
            ROUTES,
            "",
            &params_with(Verb::GET, q),
            ControllerMode::Strict,
        )
        .expect("route matches");
        assert_eq!(extracted.get_string_array("tags").unwrap(), ["a", "b", "c"]);

        // A one-element array flows through the same path; a scalar accessor
        // takes the first value.
        let mut q1 = HashMap::new();
        q1.insert("tags".to_string(), vec!["solo".to_string()]);
        let (_, e1) = resolve(
            ROUTES,
            "",
            &params_with(Verb::GET, q1),
            ControllerMode::Strict,
        )
        .expect("route matches");
        assert_eq!(e1.get_string_array("tags").unwrap(), ["solo"]);
        assert_eq!(e1.get_string("tags").unwrap(), "solo");

        // Absent is *not* a 400 for a `Vec<String>` param — it's the empty
        // list (unlike a missing required scalar).
        let (_, e2) = resolve(
            ROUTES,
            "",
            &params_with(Verb::GET, HashMap::new()),
            ControllerMode::Strict,
        )
        .expect("route matches with no query");
        assert!(e2.get_string_array("tags").unwrap().is_empty());
    }
}
