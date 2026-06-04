//! Procedural macros for the Actus controller system: the `#[controller]`
//! attribute and the `app_routes!` macro. (The `routes!` macro that appears
//! inside a `#[controller]` impl is a `macro_rules!` in `actus-controller`;
//! `#[controller]` consumes the block it produces.)
//!
//! This crate is an implementation detail — depend on `actus` (or
//! `actus-controller`) and use the macros through their prelude re-exports
//! rather than depending on this crate directly.
//!
//! Supports HTTP verb constraints, path parameters (including a trailing
//! `{...rest}`), strict/lax parameter modes, `prepare` hooks, per-controller
//! `rate_limit` / `max_body_bytes`, and per-route docs sourced from each handler's
//! `///` comment.
#![warn(missing_docs)]

use proc_macro::TokenStream;
use quote::{ToTokens, quote};
use std::collections::BTreeMap; // NEW: used to gather handler docs
use syn::{
    Expr, Ident, ImplItem, ItemImpl, LitStr, Token,
    ext::IdentExt,
    parse::{Parse, ParseStream},
    parse_macro_input,
    punctuated::Punctuated,
};

// =========================
// Core types for route definitions
// =========================

struct AllRoutes {
    routes: Vec<RouteDefinition>,
}

struct RouteDefinition {
    verb: Option<Verb>,
    pattern: LitStr,
    handler: Ident,
    params: Punctuated<Param, Token![,]>,
}

struct Param {
    name: Ident,
    ty: syn::Type,
    default: Option<Expr>,
}

#[derive(Debug, Clone)]
enum Verb {
    GET,
    POST,
    PUT,
    DELETE,
    PATCH,
    HEAD,
    OPTIONS,
}

impl Verb {
    fn from_ident(ident: &Ident) -> Option<Self> {
        match ident.to_string().as_str() {
            "GET" => Some(Verb::GET),
            "POST" => Some(Verb::POST),
            "PUT" => Some(Verb::PUT),
            "DELETE" => Some(Verb::DELETE),
            "PATCH" => Some(Verb::PATCH),
            "HEAD" => Some(Verb::HEAD),
            "OPTIONS" => Some(Verb::OPTIONS),
            _ => None,
        }
    }

    fn to_tokens(&self) -> proc_macro2::TokenStream {
        match self {
            Verb::GET => quote! { ::actus::__internal::Verb::GET },
            Verb::POST => quote! { ::actus::__internal::Verb::POST },
            Verb::PUT => quote! { ::actus::__internal::Verb::PUT },
            Verb::DELETE => quote! { ::actus::__internal::Verb::DELETE },
            Verb::PATCH => quote! { ::actus::__internal::Verb::PATCH },
            Verb::HEAD => quote! { ::actus::__internal::Verb::HEAD },
            Verb::OPTIONS => quote! { ::actus::__internal::Verb::OPTIONS },
        }
    }
}

// =========================
// Controller attributes (strict/lax, prepare function)
// =========================

#[derive(Debug, Clone, Copy)]
enum ControllerMode {
    Strict,
    Lax,
}

struct ControllerAttrs {
    mode: ControllerMode,
    prepare: Option<syn::ExprPath>,
    /// `#[controller(max_body_bytes = <expr>)]` — per-controller maximum
    /// buffered body, in bytes. `None` means inherit the server-level cap.
    max_body_bytes: Option<syn::Expr>,
    /// `#[controller(rate_limit = <expr>)]` — per-controller rate-limit
    /// *class* label (an `&'static str`). `None` means the controller
    /// declares no class. A label, not a policy: the application's
    /// rate-limit middleware maps class → limits (see the `Controller`
    /// trait's `actus_rate_limit` docs).
    rate_limit: Option<syn::Expr>,
}

// =========================
// Parser implementations
// =========================

impl Parse for AllRoutes {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut routes = Vec::new();

        while !input.is_empty() {
            // Reject the legacy `[Access::X]` section syntax with a clear
            // pointer. Actus is now policy-agnostic; access decisions live
            // in the application's `prepare` hook (and its policy layer).
            if input.peek(syn::token::Bracket) {
                let bracket_span = input.fork().parse::<proc_macro2::TokenTree>()?.span();
                return Err(syn::Error::new(
                    bracket_span,
                    "actus no longer ships an `Access` enum or `[Access::*]` section syntax. \
                     Authorization belongs in your `#[controller(prepare = …)]` hook, where \
                     you can call into your own policy layer (e.g. `services::policy::*`).",
                ));
            }

            // Check for optional HTTP verb prefix (e.g., GET, POST)
            let verb = if input.peek2(LitStr) {
                if let Ok(ident) = input.parse::<Ident>() {
                    if let Some(v) = Verb::from_ident(&ident) {
                        Some(v)
                    } else {
                        return Err(syn::Error::new(
                            ident.span(),
                            format!(
                                "Unknown HTTP verb: {}. Expected GET, POST, PUT, DELETE, PATCH, HEAD, or OPTIONS",
                                ident
                            ),
                        ));
                    }
                } else {
                    None
                }
            } else {
                None
            };

            // Parse the route pattern (e.g., "posts/{id}")
            let pattern: LitStr = input.parse()?;
            validate_pattern(&pattern)?;
            input.parse::<Token![=>]>()?;
            let handler: Ident = input.parse()?;

            // Parse handler parameters
            let params_content;
            syn::parenthesized!(params_content in input);
            let params = Punctuated::parse_terminated(&params_content)?;

            routes.push(RouteDefinition {
                verb,
                pattern,
                handler,
                params,
            });

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(AllRoutes { routes })
    }
}

impl Parse for Param {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let name: Ident = input.parse()?;
        input.parse::<Token![:]>()?;
        let ty: syn::Type = input.parse()?;

        let default = if input.peek(Token![=]) {
            input.parse::<Token![=]>()?;
            Some(input.parse()?)
        } else {
            None
        };

        Ok(Param { name, ty, default })
    }
}

impl Parse for ControllerAttrs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut mode = ControllerMode::Strict;
        let mut prepare = None;
        let mut max_body_bytes = None;
        let mut rate_limit = None;

        while !input.is_empty() {
            let ident: Ident = input.parse()?;
            match ident.to_string().as_str() {
                "strict" => mode = ControllerMode::Strict,
                "lax" => mode = ControllerMode::Lax,
                "prepare" => {
                    input.parse::<Token![=]>()?;
                    prepare = Some(input.parse()?);
                }
                "max_body_bytes" => {
                    input.parse::<Token![=]>()?;
                    // Accept any expression — a literal (`4096`), a const
                    // reference (`MAX_BODY`), or an arithmetic expression
                    // (`4 * 1024`). Resolved at handler-build time, so
                    // const-fn / static const are both fine.
                    max_body_bytes = Some(input.parse()?);
                }
                "rate_limit" => {
                    input.parse::<Token![=]>()?;
                    // Accept any expression that evaluates to `&'static str` —
                    // a string literal (`"auth"`) is the common case; a const
                    // reference (`AUTH_CLASS`) works too. It's a *label*, not a
                    // policy: the app's rate-limit middleware maps it to limits.
                    rate_limit = Some(input.parse()?);
                }
                _ => {
                    return Err(syn::Error::new(
                        ident.span(),
                        "Expected 'strict', 'lax', 'prepare = <fn>', 'max_body_bytes = <expr>', \
                         or 'rate_limit = <expr>'",
                    ));
                }
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(ControllerAttrs {
            mode,
            prepare,
            max_body_bytes,
            rate_limit,
        })
    }
}

// =========================
// Helper functions
// =========================

fn type_to_string(ty: &syn::Type) -> String {
    quote!(#ty).to_string().replace(" ", "")
}

fn extract_path_params(pattern: &str) -> Vec<String> {
    let mut params = Vec::new();
    let mut chars = pattern.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '{' {
            let mut param = String::new();
            for ch in chars.by_ref() {
                if ch == '}' {
                    break;
                }
                param.push(ch);
            }
            // `{...name}` is a "rest" parameter (captures the path remainder);
            // its handler-side binding is just `name`. `{name}` is unchanged.
            let name = param.strip_prefix("...").unwrap_or(param.as_str());
            if !name.is_empty() {
                params.push(name.to_string());
            }
        }
    }

    params
}

/// If `segment` is a well-formed `{...name}` rest token, returns `Some(name)`.
/// Returns `None` for ordinary `{name}` tokens and for literals.
fn rest_param_name(segment: &str) -> Option<&str> {
    segment
        .strip_prefix("{...")
        .and_then(|s| s.strip_suffix('}'))
        .filter(|name| !name.is_empty())
}

/// Validate a route pattern at macro-expansion time. Enforces the rules the
/// runtime matcher ([`actus_controller::routing::match_pattern`]) relies on:
/// a `{...name}` rest parameter, if present, must be the *last* `/`-segment,
/// must appear at most once, and must have a non-empty name. Also rejects the
/// near-miss `{..name}` / `{...}` shapes with a pointed message.
fn validate_pattern(pattern: &LitStr) -> syn::Result<()> {
    let value = pattern.value();
    let segments: Vec<&str> = value.split('/').collect();

    for (i, seg) in segments.iter().enumerate() {
        // Only consider segments that look like a single `{...}` token.
        let Some(inner) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
            continue;
        };

        if !inner.starts_with('.') {
            continue; // ordinary `{name}` token — nothing to check here.
        }

        // It starts with a dot, so the author meant a rest parameter.
        if rest_param_name(seg).is_none() {
            return Err(syn::Error::new(
                pattern.span(),
                format!(
                    "malformed rest parameter `{{{inner}}}` in route pattern `{value}`; \
                     write it as `{{...name}}` (three dots, then a non-empty name)"
                ),
            ));
        }

        if i != segments.len() - 1 {
            return Err(syn::Error::new(
                pattern.span(),
                format!(
                    "rest parameter `{{{inner}}}` must be the last segment of route \
                     pattern `{value}` (it captures the entire remaining path)"
                ),
            ));
        }

        // Last segment and well-formed; make sure it's the only one. (Any
        // earlier rest token would already have errored on the position
        // check above, so reaching here twice is impossible — but a literal
        // earlier segment that merely *contains* `{...}` text wouldn't, so
        // be explicit about "at most one".)
        let earlier_rest = segments[..i]
            .iter()
            .filter(|s| rest_param_name(s).is_some())
            .count();
        if earlier_rest > 0 {
            return Err(syn::Error::new(
                pattern.span(),
                format!("route pattern `{value}` has more than one `{{...name}}` rest parameter"),
            ));
        }
    }

    Ok(())
}

// NEW: Collect `///` docs from methods in the impl and join them by newlines.
fn collect_method_docs(item_impl: &syn::ItemImpl) -> BTreeMap<String, String> {
    use syn::{Attribute, ImplItem, Meta};

    fn doc_from_attrs(attrs: &[Attribute]) -> String {
        attrs
            .iter()
            .filter(|a| a.path().is_ident("doc"))
            .filter_map(|a| {
                match &a.meta {
                    Meta::NameValue(nv) => {
                        // #[doc = "..."]  → nv.value is an Expr
                        if let syn::Expr::Lit(expr_lit) = &nv.value
                            && let syn::Lit::Str(ls) = &expr_lit.lit
                        {
                            return Some(ls.value());
                        }
                        None
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    let mut map = BTreeMap::new();
    for it in &item_impl.items {
        if let ImplItem::Fn(m) = it {
            let name = m.sig.ident.to_string();
            let doc = doc_from_attrs(&m.attrs);
            if !doc.trim().is_empty() {
                map.insert(name, doc);
            }
        }
    }
    map
}

// =========================
// Main macro entry point
// =========================

/// Attribute macro for a controller's `impl` block.
///
/// Reads the `routes! { … }` block inside the impl and generates the
/// controller's `Controller` implementation — its route table, parameter
/// extraction, and dispatch. Attribute options: `prepare = Self::method` (a
/// hook run before every handler in the controller), `lax` (relax strict
/// parameter rejection), `rate_limit = "class"` (stamp a rate-limit class onto
/// matched requests), and `max_body_bytes = N` (per-controller request-body cap, in
/// bytes).
#[proc_macro_attribute]
pub fn controller(attr: TokenStream, item: TokenStream) -> TokenStream {
    // Parse attributes (strict/lax mode, prepare function)
    let attrs = if attr.is_empty() {
        ControllerAttrs {
            mode: ControllerMode::Strict,
            prepare: None,
            max_body_bytes: None,
            rate_limit: None,
        }
    } else {
        match syn::parse::<ControllerAttrs>(attr) {
            Ok(a) => a,
            Err(e) => return e.to_compile_error().into(),
        }
    };

    let item_impl = parse_macro_input!(item as ItemImpl);

    // NEW: collect method docs (by handler name)
    let docs_map = collect_method_docs(&item_impl);

    // Find the routes! macro inside the impl block
    let routes_macro = item_impl
        .items
        .iter()
        .find_map(|item| {
            if let ImplItem::Macro(m) = item
                && m.mac.path.is_ident("routes") {
                    return Some(m);
                }
            None
        })
        .expect("A `routes!` macro invocation is required inside an `impl` block marked with `#[controller]`");

    // Parse the routes
    let all_routes: AllRoutes = match syn::parse2(routes_macro.mac.tokens.clone()) {
        Ok(routes) => routes,
        Err(e) => return e.to_compile_error().into(),
    };

    // Generate code (passing docs_map)
    let generated = generate_controller_impl(&item_impl, &all_routes, &attrs, &docs_map);

    generated.into()
}

// =========================
// Code generation
// =========================

fn generate_controller_impl(
    item_impl: &ItemImpl,
    all_routes: &AllRoutes,
    attrs: &ControllerAttrs,
    docs_map: &BTreeMap<String, String>, // NEW
) -> proc_macro2::TokenStream {
    let self_ty = &item_impl.self_ty;

    // Generate route definitions and handler dispatch arms
    let mut route_defs = Vec::new();
    let mut handler_arms = Vec::new();

    for (idx, route) in all_routes.routes.iter().enumerate() {
        let pattern = &route.pattern;
        let pattern_str = pattern.value();
        let handler = &route.handler;
        let handler_id = format!("handler_{}", idx);

        // Extract path parameters from the pattern
        let path_params = extract_path_params(&pattern_str);
        // The (at most one) `{...name}` rest parameter, if the pattern has one.
        let rest_param: Option<String> = pattern_str
            .split('/')
            .find_map(|s| rest_param_name(s).map(str::to_string));

        // Build parameter definitions and extraction code
        let mut param_defs = Vec::new();
        let mut param_extractions = Vec::new();
        let mut param_names = Vec::new();

        for param in &route.params {
            let name = &param.name;
            // The *wire* name (query key / path-segment name) is the bare
            // identifier — `r#`-strip raw identifiers so a handler can bind a
            // keyword-named parameter (`r#type: Vec<String>` reads `?type=`).
            // The handler-call identifier (`param_names`) keeps the `r#`.
            let name_str = name.unraw().to_string();
            let ty_str = type_to_string(&param.ty);

            // Collect parameter names for handler call
            param_names.push(name.clone());

            // Special pass-through: a handler may declare `_: &Params` to
            // receive a borrow of the per-request `Params` (typically to
            // read state stashed by a `prepare` hook via `params.insert`).
            // No `ParamDef` is emitted for this — it's framework plumbing,
            // not a request input.
            //
            // Naming-collision note: if a route's pattern has a `{name}`
            // capture *and* the handler also declares `name: &Params`, the
            // `&Params` short-circuit wins — the path capture is silently
            // discarded. Don't name a `&Params` binding the same as a path
            // token. (The reader, not the compiler, has to catch it.)
            if ty_str == "&Params" {
                param_extractions.push(quote! { &params });
                continue;
            }

            let is_rest = rest_param.as_deref() == Some(name_str.as_str());

            // A `{...name}` rest parameter always carries the joined path
            // remainder, so it must be typed `String`. (A `{name}` segment
            // can be `u64`/`u32`/… because it's a single segment; the rest
            // token can't.)
            if is_rest && ty_str != "String" {
                let msg = format!(
                    "rest parameter `{{...{name_str}}}` must be typed `String` (it holds the \
                     joined remaining path); found `{ty_str}`"
                );
                param_defs.push(quote! { compile_error!(#msg) });
                param_extractions.push(quote! { compile_error!(#msg) });
                continue;
            }

            // Determine parameter source (path, query, or body)
            let source = if path_params.contains(&name_str) {
                quote! { ::actus::__internal::ParamSource::Path }
            } else if ty_str == "JsonValue" || ty_str == "Bytes" {
                quote! { ::actus::__internal::ParamSource::Body }
            } else {
                quote! { ::actus::__internal::ParamSource::Query }
            };

            // Generate parameter type and default value
            let (param_type, default_value) =
                generate_param_type_and_default(&ty_str, &param.default);

            param_defs.push(quote! {
                ::actus::__internal::ParamDef {
                    name: #name_str,
                    ty: #param_type,
                    source: #source,
                    default: #default_value,
                }
            });

            // Generate extraction code for this parameter
            let extraction = generate_param_extraction(&name_str, &ty_str, &param.default);
            param_extractions.push(extraction);
        }

        // Build route definition. `RouteDef.verb` is `&'static [Verb]`:
        // a single-element slice for an explicit verb, or
        // `DEFAULT_VERBS` (= [GET, POST]) for an unmarked route.
        let verb_expr = match &route.verb {
            Some(v) => {
                let verb_tokens = v.to_tokens();
                quote! { &[#verb_tokens] }
            }
            None => quote! { ::actus::__internal::DEFAULT_VERBS },
        };

        // NEW: look up handler docs and attach to RouteDef
        let handler_name_str = handler.to_string();
        let doc_val = docs_map.get(&handler_name_str).cloned().unwrap_or_default();
        let doc_lit = syn::LitStr::new(&doc_val, proc_macro2::Span::call_site());

        route_defs.push(quote! {
            ::actus::__internal::RouteDef {
                pattern: #pattern_str,
                handler_id: #handler_id,
                handler: #handler_name_str,
                verb: #verb_expr,
                params: &[ #(#param_defs),* ],
                doc: if #doc_lit.is_empty() { None } else { Some(#doc_lit) },
            }
        });

        // Build handler dispatch arm
        handler_arms.push(quote! {
            #handler_id => {
                #(let #param_names = #param_extractions;)*
                self.#handler(#(#param_names),*).await
            }
        });
    }

    // Generate prepare function call if specified.
    //
    // Signature contract:
    //     async fn prepare(&self, route: &RouteDef, params: &mut Params)
    //         -> Result<Option<ReplyData>, WebError>;
    //
    // - `Ok(None)` continues to the handler.
    // - `Ok(Some(reply))` short-circuits with that reply (any HTTP status the
    //   hook chose).
    // - `Err(WebError::*)` short-circuits with the corresponding error response.
    //
    // We pass `&mut params` so the hook can both *read* the request (headers,
    // body, undeclared query params) and *attach* per-request state via
    // `params.insert(...)` for handlers to read via a `&Params` parameter.
    let prepare_call = attrs
        .prepare
        .as_ref()
        .map(|prepare_fn| {
            quote! {
                if let ::core::option::Option::Some(__actus_early_reply) =
                    #prepare_fn(self, &matched_route, &mut params).await?
                {
                    return ::core::result::Result::Ok(__actus_early_reply);
                }
            }
        })
        .unwrap_or_default();

    // Generate mode configuration
    let mode_value = match attrs.mode {
        ControllerMode::Strict => quote! { ::actus::__internal::ControllerMode::Strict },
        ControllerMode::Lax => quote! { ::actus::__internal::ControllerMode::Lax },
    };

    let mode_str = match attrs.mode {
        ControllerMode::Strict => "strict",
        ControllerMode::Lax => "lax",
    };

    let _ = mode_str;

    // The prepare hook needs `&mut params` so it can stash per-request state
    // for handlers via `params.insert(...)`. When no prepare is configured,
    // omit `mut` to avoid an "unused_mut" warning in the user's crate.
    let params_binding = if attrs.prepare.is_some() {
        quote! { mut params: ::actus::__internal::Params }
    } else {
        quote! { params: ::actus::__internal::Params }
    };

    // `#[controller(max_body_bytes = …)]` — emit an `actus_max_body_bytes` override
    // returning `Some(<expr>)`. When not set, the trait's default impl
    // returns `None` and the server falls back to its own cap.
    let max_body_bytes_impl = attrs.max_body_bytes.as_ref().map(|expr| {
        quote! {
            fn actus_max_body_bytes(&self) -> ::core::option::Option<usize> {
                ::core::option::Option::Some(#expr)
            }
        }
    });

    // `#[controller(rate_limit = "class")]` — emit an `actus_rate_limit`
    // override returning `Some("class")`. When not set, the trait's default
    // impl returns `None` (the controller declares no rate-limit class). The
    // server stamps this label onto the matched request so an application's
    // rate-limit middleware can read it; the framework owns no policy.
    let rate_limit_impl = attrs.rate_limit.as_ref().map(|expr| {
        quote! {
            fn actus_rate_limit(&self) -> ::core::option::Option<&'static str> {
                ::core::option::Option::Some(#expr)
            }
        }
    });

    // Generate main Controller trait implementation
    let controller_impl = quote! {
        #[::actus::__internal::async_trait]
        impl ::actus::__internal::Controller for #self_ty {
            async fn actus_dispatch(&self, action: &str, #params_binding) -> ::actus::__internal::Reply {
                // Define routes as static data inside the method
                // This works with dyn Controller since it's not an associated const
                static ROUTES: &[::actus::__internal::RouteDef] = &[ #(#route_defs),* ];

                // Use shared routing utilities to resolve the route
                let (matched_route, extracted) = ::actus::__internal::routing::resolve(
                    ROUTES,
                    action,
                    &params,
                    #mode_value
                )?;

                // Call prepare function if configured
                #prepare_call

                // Type-safe dispatch to handlers. `resolve` only ever returns
                // a route from `ROUTES`, and every route there has a matching
                // arm below (both are keyed by the macro-assigned handler id),
                // so the catch-all is genuinely unreachable — `match` on `&str`
                // just can't prove it.
                match matched_route.handler_id {
                    #(#handler_arms),*
                    other => ::core::unreachable!(
                        "dispatch: no handler for route id {:?}", other
                    ),
                }
            }

            fn __name(&self) -> &'static str {
                stringify!(#self_ty)
            }

            /// Returns the static route definitions for this controller.
            /// Useful for introspection (e.g., generating API documentation).
            fn actus_describe_routes(&self) -> Vec<::actus::__internal::RouteDef> {
                static ROUTES: &[::actus::__internal::RouteDef] = &[ #(#route_defs),* ];
                ROUTES.to_vec()
            }

            #max_body_bytes_impl
            #rate_limit_impl
        }
    };

    quote! {
        // Original impl block unchanged
        #item_impl

        // Generated Controller implementation
        #controller_impl
    }
}

fn generate_param_type_and_default(
    ty_str: &str,
    default: &Option<Expr>,
) -> (proc_macro2::TokenStream, proc_macro2::TokenStream) {
    match (ty_str, default) {
        ("String", Some(d)) => (
            quote! { ::actus::__internal::ParamType::String },
            quote! { Some(::actus::__internal::ParamDefault::String(#d)) },
        ),
        ("String", None) => (
            quote! { ::actus::__internal::ParamType::String },
            quote! { None },
        ),
        ("i64", Some(d)) => (
            quote! { ::actus::__internal::ParamType::Int },
            quote! { Some(::actus::__internal::ParamDefault::Int(#d)) },
        ),
        ("i64", None) => (
            quote! { ::actus::__internal::ParamType::Int },
            quote! { None },
        ),
        ("u64", Some(d)) => (
            quote! { ::actus::__internal::ParamType::U64 },
            quote! { Some(::actus::__internal::ParamDefault::U64(#d)) },
        ),
        ("u64", None) => (
            quote! { ::actus::__internal::ParamType::U64 },
            quote! { None },
        ),
        ("u32", Some(d)) => (
            quote! { ::actus::__internal::ParamType::U32 },
            quote! { Some(::actus::__internal::ParamDefault::U32(#d)) },
        ),
        ("u32", None) => (
            quote! { ::actus::__internal::ParamType::U32 },
            quote! { None },
        ),
        ("f64", Some(d)) => (
            quote! { ::actus::__internal::ParamType::F64 },
            quote! { Some(::actus::__internal::ParamDefault::F64(#d)) },
        ),
        ("f64", None) => (
            quote! { ::actus::__internal::ParamType::F64 },
            quote! { None },
        ),
        ("bool", Some(d)) => (
            quote! { ::actus::__internal::ParamType::Bool },
            quote! { Some(::actus::__internal::ParamDefault::Bool(#d)) },
        ),
        ("bool", None) => (
            quote! { ::actus::__internal::ParamType::Bool },
            quote! { None },
        ),
        ("Vec<String>", _) => (
            quote! { ::actus::__internal::ParamType::StringArray },
            quote! { None },
        ),
        ("JsonValue", _) => (
            quote! { ::actus::__internal::ParamType::Json },
            quote! { None },
        ),
        ("Bytes", _) => (
            quote! { ::actus::__internal::ParamType::Bytes },
            quote! { None },
        ),
        _ => (
            quote! { compile_error!(concat!("Unsupported type: ", #ty_str)) },
            quote! { None },
        ),
    }
}

fn generate_param_extraction(
    name_str: &str,
    ty_str: &str,
    default: &Option<Expr>,
) -> proc_macro2::TokenStream {
    match (ty_str, default) {
        ("String", Some(d)) => {
            quote! {
                extracted.get_string(#name_str)
                    .unwrap_or_else(|_| #d.to_string())
            }
        }
        ("String", None) => {
            quote! { extracted.get_string(#name_str)? }
        }
        ("i64", Some(d)) => {
            quote! {
                extracted.get_i64(#name_str).unwrap_or(#d)
            }
        }
        ("i64", None) => {
            quote! { extracted.get_i64(#name_str)? }
        }
        ("u64", Some(d)) => {
            quote! {
                extracted.get_u64(#name_str).unwrap_or(#d)
            }
        }
        ("u64", None) => {
            quote! { extracted.get_u64(#name_str)? }
        }
        ("u32", Some(d)) => {
            quote! {
                extracted.get_u32(#name_str).unwrap_or(#d)
            }
        }
        ("u32", None) => {
            quote! { extracted.get_u32(#name_str)? }
        }
        ("f64", Some(d)) => {
            quote! {
                extracted.get_f64(#name_str).unwrap_or(#d)
            }
        }
        ("f64", None) => {
            quote! { extracted.get_f64(#name_str)? }
        }
        ("bool", Some(d)) => {
            quote! {
                extracted.get_bool(#name_str).unwrap_or(#d)
            }
        }
        ("bool", None) => {
            quote! { extracted.get_bool(#name_str)? }
        }
        ("Vec<String>", _) => {
            quote! { extracted.get_string_array(#name_str)? }
        }
        ("JsonValue", _) => {
            quote! { extracted.get_json_body()? }
        }
        // Raw request-body bytes. Use for binary uploads (e.g. `.uwx`
        // packages). The framework discriminates JSON/form/binary at
        // ingest by `Content-Type`; declaring `body: Bytes` is the
        // signal that this handler wants the unparsed payload.
        ("Bytes", _) => {
            quote! { extracted.get_body_bytes() }
        }
        _ => {
            quote! { compile_error!(concat!("Unsupported type: ", #ty_str)) }
        }
    }
}

// =========================
// app_routes! — application-level route map with deps + per-route service injection
// =========================
//
// Grammar:
//
//     app_routes! {
//         // Optional. The `deps(...)` parens declare *inputs* — values
//         // constructed by the caller (typically in `main()`) and passed
//         // into the generated `init()` function. The brace block is the
//         // `let`-block of dependencies built inside `init()`.
//         deps(store: Arc<Store>) {
//             cache = Cache::redis(...).await?,
//         }
//         routes {
//             "api/entities" => EntityController { store },
//             "api/cache"    => CacheController { cache },
//             "health"       => HealthController,
//             "*"            => SpaController,
//         }
//     }
//
// Generates `pub async fn init(<inputs>) -> actus::InitResult<actus::Router>`,
// where `InitResult<T> = Result<T, anyhow::Error>` — `?` on any error type
// implementing `std::error::Error + Send + Sync + 'static` works inside.
// The `deps` block is optional; the `(<inputs>)` clause inside it is
// optional too. All four shapes are valid:
//
//     deps { ... }                            // only let-bindings
//     deps(a: T, b: U) { ... }                // both inputs and let-bindings
//     deps(a: T, b: U) {}                     // only inputs
//     // (no deps block at all)               // neither
//
// In each route's controller construction, struct-literal shorthand
// (`{ store, cache }`) and rest-spread (`..base`) are auto-cloned, since
// deps and inputs are typically `Arc`-wrapped and shared across multiple
// controllers. Non-struct-literal expressions pass through unchanged.

struct AppRoutesInput {
    inputs: Vec<InputParam>,
    deps: Vec<DepBinding>,
    routes: Vec<RouteBinding>,
}

struct InputParam {
    name: Ident,
    ty: syn::Type,
}

struct DepBinding {
    name: Ident,
    value: Expr,
}

struct RouteBinding {
    path: LitStr,
    construction: Expr,
}

impl Parse for AppRoutesInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut inputs: Vec<InputParam> = Vec::new();
        let mut deps: Vec<DepBinding> = Vec::new();
        let mut routes: Option<Vec<RouteBinding>> = None;

        while !input.is_empty() {
            let kw: Ident = input.parse()?;
            let kw_str = kw.to_string();

            match kw_str.as_str() {
                "deps" => {
                    // Optional `(name: Type, ...)` declaring init() inputs.
                    if input.peek(syn::token::Paren) {
                        let paren_content;
                        syn::parenthesized!(paren_content in input);
                        while !paren_content.is_empty() {
                            let name: Ident = paren_content.parse()?;
                            paren_content.parse::<Token![:]>()?;
                            let ty: syn::Type = paren_content.parse()?;
                            inputs.push(InputParam { name, ty });
                            if !paren_content.is_empty() {
                                paren_content.parse::<Token![,]>()?;
                            }
                        }
                    }
                    // Then the `{ name = expr, ... }` block of let bindings
                    // (may be empty).
                    let content;
                    syn::braced!(content in input);
                    while !content.is_empty() {
                        let name: Ident = content.parse()?;
                        content.parse::<Token![=]>()?;
                        let value: Expr = content.parse()?;
                        deps.push(DepBinding { name, value });
                        if !content.is_empty() {
                            content.parse::<Token![,]>()?;
                        }
                    }
                }
                "routes" => {
                    let content;
                    syn::braced!(content in input);
                    let mut rs = Vec::new();
                    while !content.is_empty() {
                        let path: LitStr = content.parse()?;
                        content.parse::<Token![=>]>()?;
                        let construction: Expr = content.parse()?;
                        rs.push(RouteBinding { path, construction });
                        if !content.is_empty() {
                            content.parse::<Token![,]>()?;
                        }
                    }
                    routes = Some(rs);
                }
                other => {
                    return Err(syn::Error::new(
                        kw.span(),
                        format!("expected 'deps' or 'routes', got '{}'", other),
                    ));
                }
            }
        }

        let routes = routes.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "app_routes! requires a 'routes { ... }' block",
            )
        })?;

        Ok(Self {
            inputs,
            deps,
            routes,
        })
    }
}

/// Declares the application's URL blueprint and generates its `init()`.
///
/// Takes an optional `deps( … ) { … }` block — constructor-injected services
/// and `let`-bindings shared across controllers — and a `routes { mount =>
/// Controller … }` map. Expands to an async `init(…)` returning the built
/// `Router`: it constructs every controller, wires its dependencies, and
/// registers each mount. See the `actus` crate's top-level docs for a worked
/// example.
#[proc_macro]
pub fn app_routes(input: TokenStream) -> TokenStream {
    let parsed = parse_macro_input!(input as AppRoutesInput);
    generate_app_routes(parsed).into()
}

fn generate_app_routes(parsed: AppRoutesInput) -> proc_macro2::TokenStream {
    let init_params = parsed.inputs.iter().map(|p| {
        let name = &p.name;
        let ty = &p.ty;
        quote! { #name: #ty }
    });

    let dep_lets = parsed.deps.iter().map(|d| {
        let name = &d.name;
        let value = &d.value;
        quote! { let #name = #value; }
    });

    let route_calls = parsed.routes.iter().map(|r| {
        let path = &r.path;
        let construction = rewrite_construction(&r.construction);
        quote! {
            .add_route(#path, ::std::sync::Arc::new(#construction))
        }
    });

    quote! {
        pub async fn init(#(#init_params),*) -> ::actus::InitResult<::actus::Router> {
            #(#dep_lets)*

            let router = ::actus::RouterBuilder::new()
                #(#route_calls)*
                .build();

            ::std::result::Result::Ok(router)
        }
    }
}

/// In a struct-literal controller construction, auto-clone simple references
/// to bound names so the same value can be threaded into multiple
/// controllers without each call site spelling `.clone()`.
///
/// Three cases get auto-cloned, all gated on the right-hand side being a
/// bare unqualified identifier (no path segments, no generic args, no
/// `qself`). The escape hatch in every case is the same: write any
/// non-ident expression — method call, function call, qualified path, an
/// already-`.clone()`d value — and it passes through unchanged.
///
/// * **Shorthand** — `Foo { db }` → `Foo { db: db.clone() }`.
/// * **Bare-ident explicit form** — `Foo { svc: store }` →
///   `Foo { svc: store.clone() }`.
/// * **Bare-ident rest spread** — `Foo { ..base }` → `Foo { ..(base).clone() }`.
///   Non-ident rest expressions (`..base.clone()`, `..self.template()`)
///   pass through verbatim — no double-cloning.
fn rewrite_construction(expr: &Expr) -> proc_macro2::TokenStream {
    let Expr::Struct(s) = expr else {
        return expr.to_token_stream();
    };

    let path = &s.path;
    let mut inner = proc_macro2::TokenStream::new();
    let mut wrote_field = false;

    for f in s.fields.iter() {
        if wrote_field {
            inner.extend(quote! { , });
        }
        wrote_field = true;

        let member = &f.member;
        if f.colon_token.is_none() {
            // Shorthand: `name` → `name: name.clone()`
            inner.extend(quote! { #member: #member.clone() });
        } else if is_bare_ident(&f.expr) {
            // Explicit `target: source` where `source` is a simple ident:
            // treat like shorthand and auto-clone. Any non-ident expression
            // (method call, function call, qualified path, …) passes
            // through unchanged so callers retain a clean escape hatch.
            let value = &f.expr;
            inner.extend(quote! { #member: #value.clone() });
        } else {
            let value = &f.expr;
            inner.extend(quote! { #member: #value });
        }
    }

    if let Some(rest) = &s.rest {
        if wrote_field {
            inner.extend(quote! { , });
        }
        if is_bare_ident(rest) {
            inner.extend(quote! { ..(#rest).clone() });
        } else {
            inner.extend(quote! { ..#rest });
        }
    }

    quote! { #path { #inner } }
}

/// Whether `expr` is a single, unqualified identifier path (no qself, no
/// leading `::`, exactly one segment, no generic args). The criterion the
/// auto-clone rule uses to decide that an explicit field assignment looks
/// "shorthand-like."
fn is_bare_ident(expr: &Expr) -> bool {
    let Expr::Path(p) = expr else { return false };
    p.qself.is_none()
        && p.path.leading_colon.is_none()
        && p.path.segments.len() == 1
        && p.path.segments[0].arguments.is_none()
}
