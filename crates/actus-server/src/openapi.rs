//! OpenAPI 3.1 doc generation. Behind the `openapi` feature.
//!
//! Walk a built [`Router`] and emit a `serde_json::Value`
//! shaped like an OpenAPI 3.1 document. The generator pulls structural data
//! directly from the route tree — every `(mount_path, RouteDef)` pair the
//! `#[controller]` and `app_routes!` macros recorded — so the spec reflects
//! the code, not a hand-maintained YAML file.
//!
//! ```ignore
//! use actus::prelude::*;
//! use actus::openapi;
//!
//! let router = init().await?;
//! let spec = openapi::generate(
//!     &router,
//!     &openapi::Options::new("My API", "1.0.0").description("…"),
//!     // Document only `/api/...` — hide internal mounts.
//!     |mount| mount.starts_with("api/"),
//! );
//! println!("{}", openapi::to_string_pretty(&spec));
//! ```
//!
//! ## Scope
//!
//! * **Mapping is structural, not semantic.** Verbs, path params, query
//!   params (typed, with defaults, optional `Vec<String>`), JSON / Bytes
//!   request bodies, and the handler's `///` doc as summary + description.
//!   No response-body schema is inferred — handlers can return anything,
//!   and the framework's `Reply` shape doesn't carry that information.
//!   Operations get a `default` response with a generic description; if you
//!   need richer responses, post-process the generated `Value`.
//! * **Trailing rest parameters** (`{...name}`) don't have a clean OpenAPI
//!   form — the spec's path templating is a single segment per `{name}`.
//!   The generator strips the `...` and adds `x-actus-rest-param: true`
//!   plus a `description` noting "captures the trailing path (slashes
//!   included)" on the parameter, so clients and tooling can recognise it
//!   if they want to.
//! * **`DEFAULT_VERBS` routes** (no verb prefix in `routes!` — accepts
//!   `GET` and `POST`) emit *two* operations on the path, one per verb.
//! * **Route selection.** The `filter` predicate runs on the mount path
//!   (the controller's prefix, no leading slash, no trailing slash). A
//!   route is included iff its controller's mount passes the predicate.
//!   The flexible form is a closure; the most common shape is
//!   `|mount| mount.starts_with("api/")`.

use actus_controller::{DEFAULT_VERBS, ParamDefault, ParamSource, ParamType, RouteDef, Verb};
use serde_json::{Map, Value, json};

use crate::router::Router;

/// Top-level options for the generated spec. The OpenAPI `info` object plus
/// an optional `servers` list.
#[derive(Clone, Debug)]
pub struct Options {
    /// The API title (OpenAPI `info.title`).
    pub title: String,
    /// The API version (OpenAPI `info.version`).
    pub version: String,
    /// An optional API description (OpenAPI `info.description`).
    pub description: Option<String>,
    /// The base URLs the API is served at (OpenAPI `servers`).
    pub servers: Vec<ServerInfo>,
}

/// One entry in the OpenAPI `servers` array — a base URL the API is
/// reachable at, plus an optional human description.
#[derive(Clone, Debug)]
pub struct ServerInfo {
    /// The server base URL (e.g. `https://api.example.com`).
    pub url: String,
    /// An optional human-readable description of this server entry.
    pub description: Option<String>,
}

impl Options {
    /// New `Options` with the given `info.title` and `info.version`. Both
    /// are required by the OpenAPI 3.1 spec.
    pub fn new(title: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            version: version.into(),
            description: None,
            servers: Vec::new(),
        }
    }

    /// Set `info.description`.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Add an entry to the `servers` array.
    pub fn server(
        mut self,
        url: impl Into<String>,
        description: Option<impl Into<String>>,
    ) -> Self {
        self.servers.push(ServerInfo {
            url: url.into(),
            description: description.map(Into::into),
        });
        self
    }
}

/// Walk `router` and emit an OpenAPI 3.1 `Value`. `filter` is consulted with
/// the mount path of each controller (no leading or trailing slash); only
/// routes from controllers whose mount passes are included.
///
/// See the [module docs](self) for the mapping conventions and the
/// limitations on rest parameters / response schemas.
pub fn generate<F>(router: &Router, options: &Options, filter: F) -> Value
where
    F: Fn(&str) -> bool,
{
    let mut paths: Map<String, Value> = Map::new();

    for (mount, route) in router.routes() {
        if !filter(mount.as_str()) {
            continue;
        }
        let path = compose_path(&mount, route.pattern);
        let methods = methods_for(&route);
        let entry = paths.entry(path.clone()).or_insert_with(|| json!({}));
        let entry_obj = entry
            .as_object_mut()
            .expect("path entry is always a JSON object");
        for method in methods {
            // Last-writer-wins for collisions on the same (path, method) —
            // two routes with the same shape is a configuration error
            // (the runtime router uses declaration order to pick one). The
            // spec only knows the latest.
            entry_obj.insert(method.to_string(), build_operation(&path, method, &route));
        }
    }

    let mut info = Map::new();
    info.insert("title".into(), Value::String(options.title.clone()));
    info.insert("version".into(), Value::String(options.version.clone()));
    if let Some(d) = &options.description {
        info.insert("description".into(), Value::String(d.clone()));
    }

    let mut spec = Map::new();
    spec.insert("openapi".into(), Value::String("3.1.0".into()));
    spec.insert("info".into(), Value::Object(info));
    if !options.servers.is_empty() {
        let servers: Vec<Value> = options
            .servers
            .iter()
            .map(|s| {
                let mut obj = Map::new();
                obj.insert("url".into(), Value::String(s.url.clone()));
                if let Some(d) = &s.description {
                    obj.insert("description".into(), Value::String(d.clone()));
                }
                Value::Object(obj)
            })
            .collect();
        spec.insert("servers".into(), Value::Array(servers));
    }
    spec.insert("paths".into(), Value::Object(paths));
    Value::Object(spec)
}

/// Pretty-printed JSON of a generated spec. Convenience for serving the
/// document at e.g. `/openapi.json`.
pub fn to_string_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).expect("serde_json::Value is always serializable")
}

// ---------- internal: mapping logic --------------------------------------

/// Join a mount path and a route pattern into the OpenAPI path, replacing
/// `{...name}` rest tokens with plain `{name}` (the rest-vs-segment
/// distinction is communicated by `x-actus-rest-param` on the parameter,
/// since OpenAPI path templating only knows about segment-sized variables).
fn compose_path(mount: &str, pattern: &str) -> String {
    let mount = mount.trim_matches('/');
    let pattern = pattern.trim_matches('/').replace("{...", "{");
    match (mount.is_empty(), pattern.is_empty()) {
        (true, true) => "/".to_string(),
        (true, false) => format!("/{pattern}"),
        (false, true) => format!("/{mount}"),
        (false, false) => format!("/{mount}/{pattern}"),
    }
}

/// HTTP method names this route advertises as OpenAPI operations.
fn methods_for(route: &RouteDef) -> Vec<&'static str> {
    // A "no verb prefix" route accepts the framework's default verb set;
    // the macro encodes that by reusing the `DEFAULT_VERBS` static slice.
    // Identity comparison is enough since the macro never constructs a
    // fresh equivalent slice for the default case.
    if std::ptr::eq(route.verb, DEFAULT_VERBS) {
        return DEFAULT_VERBS.iter().map(verb_method).collect();
    }
    route.verb.iter().map(verb_method).collect()
}

fn verb_method(v: &Verb) -> &'static str {
    match v {
        Verb::GET => "get",
        Verb::POST => "post",
        Verb::PUT => "put",
        Verb::DELETE => "delete",
        Verb::PATCH => "patch",
        Verb::HEAD => "head",
        Verb::OPTIONS => "options",
    }
}

fn build_operation(path: &str, method: &str, route: &RouteDef) -> Value {
    let mut op = Map::new();
    op.insert(
        "operationId".into(),
        Value::String(operation_id(path, method, route.handler)),
    );

    if let Some(doc) = route.doc {
        let trimmed = doc.trim();
        if !trimmed.is_empty() {
            // First non-empty line → `summary`; the full doc → `description`.
            // Matches what most OpenAPI consumers (Swagger UI, redoc) render.
            let summary = trimmed
                .lines()
                .find(|l| !l.trim().is_empty())
                .map(str::trim)
                .unwrap_or("");
            if !summary.is_empty() {
                op.insert("summary".into(), Value::String(summary.to_string()));
            }
            op.insert("description".into(), Value::String(trimmed.to_string()));
        }
    }

    let (parameters, request_body) = split_params(route);
    if !parameters.is_empty() {
        op.insert("parameters".into(), Value::Array(parameters));
    }
    if let Some(body) = request_body {
        op.insert("requestBody".into(), body);
    }

    // Every operation needs a `responses` object. Actus's `Reply` shape
    // doesn't carry response-schema info, so we emit a generic `default`
    // entry covering "any response not otherwise specified" (RFC 9110 /
    // OpenAPI 3.1 §responses-object).
    op.insert(
        "responses".into(),
        json!({
            "default": { "description": "Response from the handler." }
        }),
    );

    Value::Object(op)
}

/// `{sanitized_path}_{handler}_{method}` — guaranteed unique because the
/// path is unique within the router and the handler/method tokens make the
/// id readable.
fn operation_id(path: &str, method: &str, handler: &str) -> String {
    let sanitized: String = path
        .chars()
        .map(|c| match c {
            '/' => '_',
            '{' | '}' => '_',
            other => other,
        })
        .collect();
    let trimmed = sanitized.trim_matches('_');
    if trimmed.is_empty() {
        format!("{handler}_{method}")
    } else {
        // Collapse runs of `_` so e.g. `_api_users_{id}_` doesn't turn into
        // `api_users__id__handler_method`.
        let mut collapsed = String::with_capacity(trimmed.len());
        let mut prev_us = false;
        for c in trimmed.chars() {
            if c == '_' {
                if !prev_us {
                    collapsed.push('_');
                }
                prev_us = true;
            } else {
                collapsed.push(c);
                prev_us = false;
            }
        }
        format!("{collapsed}_{handler}_{method}")
    }
}

/// Split a route's `params` into `(parameters[], Option<requestBody>)`.
fn split_params(route: &RouteDef) -> (Vec<Value>, Option<Value>) {
    let mut params: Vec<Value> = Vec::new();
    let mut body: Option<Value> = None;

    let pattern_has_rest = route.pattern.contains("{...");

    for p in route.params {
        match p.source {
            ParamSource::Path => {
                let mut entry = Map::new();
                entry.insert("name".into(), Value::String(p.name.to_string()));
                entry.insert("in".into(), Value::String("path".into()));
                entry.insert("required".into(), Value::Bool(true));
                entry.insert("schema".into(), schema_for(p.ty, p.default.as_ref()));
                // Mark `{...rest}` for clients that want to know.
                if pattern_has_rest && matches!(p.ty, ParamType::String) {
                    // Heuristic: the rest param is always typed `String` and
                    // is the only Path-source `String` declared by a
                    // rest-containing pattern. (The macro enforces typing.)
                    if route
                        .pattern
                        .contains(&format!("{{...{name}}}", name = p.name))
                    {
                        entry.insert("x-actus-rest-param".into(), Value::Bool(true));
                        entry.insert(
                            "description".into(),
                            Value::String(
                                "Captures the trailing path (slashes included). Not natively \
                                 representable in OpenAPI path templating; treated as a single \
                                 segment here."
                                    .into(),
                            ),
                        );
                    }
                }
                params.push(Value::Object(entry));
            }
            ParamSource::Query => {
                let mut entry = Map::new();
                entry.insert("name".into(), Value::String(p.name.to_string()));
                entry.insert("in".into(), Value::String("query".into()));
                // A `Vec<String>` is inherently optional — absent → `[]`,
                // never a 400 (see `Params::get_all`). Anything else is
                // required iff no default.
                let required = !matches!(p.ty, ParamType::StringArray) && p.default.is_none();
                entry.insert("required".into(), Value::Bool(required));
                entry.insert("schema".into(), schema_for(p.ty, p.default.as_ref()));
                params.push(Value::Object(entry));
            }
            ParamSource::Body => {
                // Json / Bytes — wrap into a requestBody. Two body params
                // shouldn't happen (the macro emits one body param at most),
                // but if it does we last-writer-wins.
                let (content_type, schema): (&str, Value) = match p.ty {
                    ParamType::Json => ("application/json", json!({})),
                    ParamType::Bytes => (
                        "application/octet-stream",
                        json!({ "type": "string", "format": "binary" }),
                    ),
                    _ => continue, // shouldn't reach here for other ParamTypes
                };
                body = Some(json!({
                    "required": true,
                    "content": {
                        content_type: { "schema": schema }
                    }
                }));
            }
        }
    }

    (params, body)
}

/// OpenAPI 3.1 schema fragment for a `ParamType`, including `default` if
/// the macro recorded one.
fn schema_for(ty: ParamType, default: Option<&ParamDefault>) -> Value {
    let mut schema = base_schema(ty);
    if let Some(d) = default {
        let obj = schema
            .as_object_mut()
            .expect("base schema is always object");
        obj.insert("default".into(), default_to_value(d));
    }
    schema
}

fn base_schema(ty: ParamType) -> Value {
    match ty {
        ParamType::String => json!({ "type": "string" }),
        ParamType::Int => json!({ "type": "integer", "format": "int64" }),
        ParamType::U64 => json!({ "type": "integer", "format": "int64", "minimum": 0 }),
        ParamType::U32 => json!({ "type": "integer", "format": "int32", "minimum": 0 }),
        ParamType::F64 => json!({ "type": "number" }),
        ParamType::Bool => json!({ "type": "boolean" }),
        ParamType::StringArray => json!({
            "type": "array",
            "items": { "type": "string" }
        }),
        ParamType::Json => json!({}), // any
        ParamType::Bytes => json!({ "type": "string", "format": "binary" }),
    }
}

fn default_to_value(d: &ParamDefault) -> Value {
    match d {
        ParamDefault::String(s) => Value::String((*s).to_string()),
        ParamDefault::Int(i) => Value::from(*i),
        ParamDefault::U64(u) => Value::from(*u),
        ParamDefault::U32(u) => Value::from(*u),
        ParamDefault::F64(f) => Value::from(*f),
        ParamDefault::Bool(b) => Value::from(*b),
    }
}

// =========================
// Tests
// =========================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::RouterBuilder;
    use actus_controller::{Controller, ParamDef, Params};
    use actus_reply::{Reply, WebError};
    use std::sync::Arc;

    /// A `Controller` that exposes a fixed slice of `RouteDef`s via
    /// `actus_describe_routes()` — sidesteps the `#[controller]` macro so
    /// the test crate doesn't need an `::actus` self-dep.
    struct Stub {
        routes: &'static [RouteDef],
    }

    #[actus_controller::async_trait]
    impl Controller for Stub {
        async fn actus_dispatch(&self, _action: &str, _params: Params) -> Reply {
            Err(WebError::NotFound)
        }
        fn __name(&self) -> &'static str {
            "stub"
        }
        fn actus_describe_routes(&self) -> Vec<RouteDef> {
            self.routes.to_vec()
        }
    }

    fn build_router(mounts: &[(&str, &'static [RouteDef])]) -> Router {
        let mut b = RouterBuilder::new();
        for (mount, routes) in mounts {
            b = b.add_route(mount, Arc::new(Stub { routes }));
        }
        b.build()
    }

    fn opts() -> Options {
        Options::new("Test API", "1.0.0")
    }

    #[test]
    fn shape_basics() {
        static R: &[RouteDef] = &[RouteDef {
            pattern: "",
            handler_id: "handler_0",
            handler: "list",
            verb: &[Verb::GET],
            params: &[],
            doc: None,
        }];
        let router = build_router(&[("api/users", R)]);
        let spec = generate(&router, &opts(), |_| true);

        assert_eq!(spec["openapi"], "3.1.0");
        assert_eq!(spec["info"]["title"], "Test API");
        assert_eq!(spec["info"]["version"], "1.0.0");
        assert!(spec["paths"]["/api/users"]["get"].is_object());
        assert_eq!(
            spec["paths"]["/api/users"]["get"]["operationId"],
            "api_users_list_get"
        );
        // Every operation has a responses object.
        assert!(spec["paths"]["/api/users"]["get"]["responses"]["default"].is_object());
    }

    #[test]
    fn mount_filter_excludes_non_matching_controllers() {
        static R: &[RouteDef] = &[RouteDef {
            pattern: "",
            handler_id: "handler_0",
            handler: "h",
            verb: &[Verb::GET],
            params: &[],
            doc: None,
        }];
        let router = build_router(&[("api/users", R), ("internal/debug", R)]);
        let spec = generate(&router, &opts(), |mount| mount.starts_with("api/"));

        assert!(spec["paths"]["/api/users"].is_object());
        assert!(
            spec["paths"]["/internal/debug"].is_null(),
            "filter excluded"
        );
    }

    #[test]
    fn default_verbs_route_emits_both_get_and_post() {
        static R: &[RouteDef] = &[RouteDef {
            pattern: "",
            handler_id: "handler_0",
            handler: "either",
            verb: DEFAULT_VERBS, // identity comparison detects this
            params: &[],
            doc: None,
        }];
        let router = build_router(&[("api/things", R)]);
        let spec = generate(&router, &opts(), |_| true);

        assert!(spec["paths"]["/api/things"]["get"].is_object());
        assert!(spec["paths"]["/api/things"]["post"].is_object());
    }

    #[test]
    fn path_param_marked_required_and_query_default_marked_optional() {
        static R: &[RouteDef] = &[RouteDef {
            pattern: "{id}",
            handler_id: "handler_0",
            handler: "get",
            verb: &[Verb::GET],
            params: &[
                ParamDef {
                    name: "id",
                    ty: ParamType::U64,
                    source: ParamSource::Path,
                    default: None,
                },
                ParamDef {
                    name: "expand",
                    ty: ParamType::Bool,
                    source: ParamSource::Query,
                    default: Some(ParamDefault::Bool(false)),
                },
                ParamDef {
                    name: "fields",
                    ty: ParamType::StringArray,
                    source: ParamSource::Query,
                    default: None,
                },
            ],
            doc: None,
        }];
        let router = build_router(&[("api/users", R)]);
        let spec = generate(&router, &opts(), |_| true);

        let params = spec["paths"]["/api/users/{id}"]["get"]["parameters"]
            .as_array()
            .expect("parameters array");
        // id (path, required, u64 → integer/int64 min 0)
        let id = &params[0];
        assert_eq!(id["name"], "id");
        assert_eq!(id["in"], "path");
        assert_eq!(id["required"], true);
        assert_eq!(id["schema"]["type"], "integer");
        assert_eq!(id["schema"]["format"], "int64");
        assert_eq!(id["schema"]["minimum"], 0);

        // expand (query, optional because of default, bool with default)
        let expand = &params[1];
        assert_eq!(expand["name"], "expand");
        assert_eq!(expand["in"], "query");
        assert_eq!(expand["required"], false);
        assert_eq!(expand["schema"]["type"], "boolean");
        assert_eq!(expand["schema"]["default"], false);

        // fields (query, StringArray → optional, array of string)
        let fields = &params[2];
        assert_eq!(fields["required"], false);
        assert_eq!(fields["schema"]["type"], "array");
        assert_eq!(fields["schema"]["items"]["type"], "string");
    }

    #[test]
    fn rest_param_is_marked_with_extension() {
        static R: &[RouteDef] = &[RouteDef {
            pattern: "{drive}/{...path}",
            handler_id: "handler_0",
            handler: "read",
            verb: &[Verb::GET],
            params: &[
                ParamDef {
                    name: "drive",
                    ty: ParamType::String,
                    source: ParamSource::Path,
                    default: None,
                },
                ParamDef {
                    name: "path",
                    ty: ParamType::String,
                    source: ParamSource::Path,
                    default: None,
                },
            ],
            doc: None,
        }];
        let router = build_router(&[("files", R)]);
        let spec = generate(&router, &opts(), |_| true);

        // `{...path}` is reduced to `{path}` for OpenAPI path templating.
        let op = &spec["paths"]["/files/{drive}/{path}"]["get"];
        assert!(
            op.is_object(),
            "rest token stripped to /files/{{drive}}/{{path}}"
        );

        let params = op["parameters"].as_array().unwrap();
        let drive = &params[0];
        let path = &params[1];
        // `drive` is a normal path param — no rest extension.
        assert!(drive["x-actus-rest-param"].is_null());
        // `path` is the rest param — marked.
        assert_eq!(path["x-actus-rest-param"], true);
        assert!(
            path["description"]
                .as_str()
                .unwrap_or("")
                .contains("trailing path"),
        );
    }

    #[test]
    fn body_params_become_request_body() {
        static R: &[RouteDef] = &[RouteDef {
            pattern: "",
            handler_id: "handler_0",
            handler: "create",
            verb: &[Verb::POST],
            params: &[ParamDef {
                name: "data",
                ty: ParamType::Json,
                source: ParamSource::Body,
                default: None,
            }],
            doc: None,
        }];
        let router = build_router(&[("api/users", R)]);
        let spec = generate(&router, &opts(), |_| true);

        let body = &spec["paths"]["/api/users"]["post"]["requestBody"];
        assert!(body.is_object());
        assert_eq!(body["required"], true);
        assert!(body["content"]["application/json"]["schema"].is_object());

        // Bytes body → application/octet-stream / string-binary.
        static R2: &[RouteDef] = &[RouteDef {
            pattern: "upload",
            handler_id: "handler_0",
            handler: "upload",
            verb: &[Verb::POST],
            params: &[ParamDef {
                name: "body",
                ty: ParamType::Bytes,
                source: ParamSource::Body,
                default: None,
            }],
            doc: None,
        }];
        let router = build_router(&[("api/files", R2)]);
        let spec = generate(&router, &opts(), |_| true);
        let body = &spec["paths"]["/api/files/upload"]["post"]["requestBody"];
        assert!(body["content"]["application/octet-stream"]["schema"]["format"] == "binary");
    }

    #[test]
    fn doc_becomes_summary_first_line_and_description_full() {
        static R: &[RouteDef] = &[RouteDef {
            pattern: "",
            handler_id: "handler_0",
            handler: "list",
            verb: &[Verb::GET],
            params: &[],
            doc: Some(
                " List items.\n\nThe long form: paginated, sorted by creation time.\nUse `?page=`.",
            ),
        }];
        let router = build_router(&[("api/items", R)]);
        let spec = generate(&router, &opts(), |_| true);
        let op = &spec["paths"]["/api/items"]["get"];
        assert_eq!(op["summary"], "List items.");
        // Description carries the full trimmed doc (multi-line).
        let desc = op["description"].as_str().unwrap();
        assert!(desc.starts_with("List items."));
        assert!(desc.contains("paginated"));
    }

    #[test]
    fn options_servers_and_description_round_trip() {
        static R: &[RouteDef] = &[RouteDef {
            pattern: "",
            handler_id: "handler_0",
            handler: "h",
            verb: &[Verb::GET],
            params: &[],
            doc: None,
        }];
        let router = build_router(&[("api", R)]);
        let options = Options::new("My API", "2.1.0")
            .description("Awesome")
            .server("https://api.example.com", Some("prod"))
            .server("https://staging.api.example.com", None::<&str>);
        let spec = generate(&router, &options, |_| true);

        assert_eq!(spec["info"]["description"], "Awesome");
        let servers = spec["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0]["url"], "https://api.example.com");
        assert_eq!(servers[0]["description"], "prod");
        assert!(servers[1]["description"].is_null());
    }

    #[test]
    fn to_string_pretty_is_deterministic_json() {
        static R: &[RouteDef] = &[RouteDef {
            pattern: "",
            handler_id: "handler_0",
            handler: "h",
            verb: &[Verb::GET],
            params: &[],
            doc: None,
        }];
        let router = build_router(&[("api", R)]);
        let spec = generate(&router, &opts(), |_| true);
        let pretty = to_string_pretty(&spec);
        assert!(pretty.starts_with("{\n"));
        assert!(pretty.contains("\"openapi\": \"3.1.0\""));
    }
}
