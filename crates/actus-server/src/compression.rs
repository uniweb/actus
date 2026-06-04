//! Response compression (gzip / brotli). Behind the `compression` feature.
//!
//! Build a [`CompressionLayer`] and hand it to [`crate::Server::with_compression`].
//! For each response Actus picks an encoding from the request's
//! `Accept-Encoding` (preferring brotli when offered), and — if the body is a
//! buffered, compressible type above a size threshold — compresses it,
//! setting `Content-Encoding` and appending `Vary: Accept-Encoding`.
//!
//! Scope: this compresses *buffered* response bodies (the common case — JSON
//! API responses, `reply::bytes`). Streamed responses (`reply!(stream: …)`)
//! and error bodies pass through uncompressed for now.
//!
//! ```ignore
//! use actus::prelude::*;
//! Server::new(router).with_compression(CompressionLayer::new());        // defaults
//! Server::new(router).with_compression(CompressionLayer::new().min_size(256).prefer_gzip());
//! ```

use actus_reply::{ReplyData, ReplySpec};
use http::{HeaderValue, Response, header};
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Write;

/// Negotiated content-encoding for a response.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Encoding {
    Brotli,
    Gzip,
    Identity,
}

/// A response-compression policy. See the [module docs](self).
#[derive(Clone, Debug)]
pub struct CompressionLayer {
    min_size: usize,
    prefer_brotli: bool,
    brotli_quality: u32,
}

/// Default brotli compression quality. 4 is the speed/ratio sweet spot for
/// per-request dynamic content: quality 11 is 10-100× slower for only ~5%
/// additional savings.
const DEFAULT_BROTLI_QUALITY: u32 = 4;

impl Default for CompressionLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl CompressionLayer {
    /// Defaults: compress responses of at least 1 KiB, prefer brotli when
    /// the client's `Accept-Encoding` offers it (brotli compresses tighter;
    /// gzip is the universal fallback), brotli quality 4.
    pub fn new() -> Self {
        Self {
            min_size: 1024,
            prefer_brotli: true,
            brotli_quality: DEFAULT_BROTLI_QUALITY,
        }
    }

    /// Don't compress bodies smaller than `bytes`. Below the threshold the
    /// encoder's framing overhead and the CPU cost outweigh the savings (and
    /// a small response usually fits in one packet anyway).
    pub fn min_size(mut self, bytes: usize) -> Self {
        self.min_size = bytes;
        self
    }

    /// When the client offers both `gzip` and `br`, choose gzip. (Brotli is
    /// the default — it's tighter, especially for JSON, at a small CPU cost.)
    pub fn prefer_gzip(mut self) -> Self {
        self.prefer_brotli = false;
        self
    }

    /// Brotli compression quality (0 = fastest / least compressed,
    /// 11 = slowest / tightest). Default is 4 — the speed/ratio sweet spot
    /// for per-request dynamic content. Quality 11 is 10-100× slower for
    /// roughly 5% additional savings; it's appropriate for pre-compressed
    /// static assets, not for per-request work.
    ///
    /// Values above 11 are clamped to 11. Named for the codec it controls: it
    /// has no effect on gzip (which uses `flate2`'s default level, 6); a
    /// `gzip_level(_)` knob can be added alongside if/when needed.
    pub fn brotli_quality(mut self, q: u32) -> Self {
        self.brotli_quality = q.min(11);
        self
    }

    // -------- internal: used by `Server` --------

    /// Compress `data` if the negotiated encoding, the body type, and the body
    /// size all warrant it; otherwise return it unchanged. The returned reply
    /// carries `Content-Encoding` iff the body was actually encoded.
    pub(crate) fn compress_reply(
        &self,
        data: ReplyData,
        accept_encoding: Option<&str>,
    ) -> ReplyData {
        // Honor `Cache-Control: no-transform` — RFC 7234 §5.2.1.6 / RFC
        // 9111 §5.2.2.6: an intermediary (which we are, when we encode)
        // MUST NOT transform the payload. Common motivations: signed
        // payloads, content-addressed responses, anything where byte-exact
        // transit matters. Handlers opt in by stamping the header on a
        // `Rich` reply (via the builder or `ReplyData::add_header`).
        if let ReplyData::Rich(spec) = &data
            && spec.headers.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case("cache-control")
                    && v.split(',')
                        .any(|t| t.trim().eq_ignore_ascii_case("no-transform"))
            })
        {
            return data;
        }

        let enc = match negotiate(accept_encoding, self.prefer_brotli) {
            Encoding::Identity => return data,
            other => other,
        };
        match data {
            // A handler that built its own `ReplySpec`: compress the payload
            // in place, keeping the status/headers — unless it already set a
            // `Content-Encoding` (don't double-encode).
            ReplyData::Rich(mut spec) => {
                if spec
                    .headers
                    .keys()
                    .any(|k| k.eq_ignore_ascii_case("content-encoding"))
                {
                    return ReplyData::Rich(spec);
                }
                let inner = std::mem::replace(&mut spec.payload, ReplyData::Empty);
                let (payload, encoded_as) = self.compress_payload(inner, enc);
                spec.payload = payload;
                if let Some(name) = encoded_as {
                    spec.headers
                        .insert("content-encoding".to_string(), name.to_string());
                }
                ReplyData::Rich(spec)
            }
            other => match self.compress_payload(other, enc) {
                (payload, Some(name)) => ReplyData::Rich(Box::new(ReplySpec {
                    payload,
                    status: None,
                    headers: HashMap::from([("content-encoding".to_string(), name.to_string())]),
                })),
                (payload, None) => payload,
            },
        }
    }

    /// Compress a single payload. Returns the (possibly transformed) payload
    /// and the encoding name to advertise — `None` means "not encoded" (the
    /// payload may still have changed shape, e.g. `Json` → buffered `Bytes`,
    /// but it's wire-identical). Streams / `Empty` / nested `Rich` pass through.
    fn compress_payload(
        &self,
        payload: ReplyData,
        enc: Encoding,
    ) -> (ReplyData, Option<&'static str>) {
        let name = match enc {
            Encoding::Gzip => "gzip",
            Encoding::Brotli => "br",
            Encoding::Identity => return (payload, None),
        };
        match payload {
            ReplyData::Json(value) => {
                let bytes = match serde_json::to_vec(&value) {
                    Ok(b) => b,
                    // Let the finalizer surface the serialization failure.
                    Err(_) => return (ReplyData::Json(value), None),
                };
                let json: Cow<'static, str> = Cow::Borrowed("application/json");
                if bytes.len() < self.min_size {
                    return (
                        ReplyData::Bytes {
                            content_type: json,
                            data: bytes,
                        },
                        None,
                    );
                }
                match encode(enc, &bytes, self.brotli_quality) {
                    Some(out) if out.len() < bytes.len() => (
                        ReplyData::Bytes {
                            content_type: json,
                            data: out,
                        },
                        Some(name),
                    ),
                    _ => (
                        ReplyData::Bytes {
                            content_type: json,
                            data: bytes,
                        },
                        None,
                    ),
                }
            }
            ReplyData::Bytes { content_type, data } => {
                if data.len() < self.min_size || !is_compressible(&content_type) {
                    return (ReplyData::Bytes { content_type, data }, None);
                }
                match encode(enc, &data, self.brotli_quality) {
                    Some(out) if out.len() < data.len() => (
                        ReplyData::Bytes {
                            content_type,
                            data: out,
                        },
                        Some(name),
                    ),
                    _ => (ReplyData::Bytes { content_type, data }, None),
                }
            }
            // Streams compress-on-the-fly is a future addition; `Empty` and a
            // nested `Rich` have nothing to do here.
            other => (other, None),
        }
    }
}

/// Pick the response encoding from `Accept-Encoding`, per RFC 7231 §5.3.4.
///
/// Parses each token as `name(;q=value)?`. `q` defaults to `1.0`; `q=0` means
/// the encoding is explicitly disallowed. The `*` wildcard supplies a default
/// for any encoding not explicitly named. The highest non-zero `q` among the
/// encodings we support (`br`, `gzip`) wins; on a tie, `prefer_brotli` breaks
/// it. If all our supported encodings score 0 (`*;q=0` with no positive named
/// entries, etc.), we fall back to `identity` — i.e. send the body uncompressed
/// rather than 406.
fn negotiate(accept_encoding: Option<&str>, prefer_brotli: bool) -> Encoding {
    let Some(ae) = accept_encoding else {
        return Encoding::Identity;
    };

    let mut br_q: Option<f32> = None;
    let mut gzip_q: Option<f32> = None;
    let mut star_q: Option<f32> = None;

    for token in ae.split(',') {
        let mut parts = token.split(';');
        let name = parts.next().map(str::trim).unwrap_or("");
        // Per spec, `q=value` is the only widely-used parameter on
        // Accept-Encoding tokens; other parameters are extensions we ignore.
        let mut q: f32 = 1.0;
        for p in parts {
            let p = p.trim();
            if let Some(qs) = p.strip_prefix("q=").or_else(|| p.strip_prefix("Q="))
                && let Ok(v) = qs.parse::<f32>()
                && (0.0..=1.0).contains(&v)
            {
                q = v;
            }
        }
        match name.to_ascii_lowercase().as_str() {
            "br" => br_q = Some(q),
            "gzip" => gzip_q = Some(q),
            "*" => star_q = Some(q),
            // Other encodings (`deflate`, `compress`, `identity`, `x-gzip`, …)
            // we don't produce; record nothing.
            _ => {}
        }
    }

    // Apply the wildcard to encodings we support that weren't named explicitly.
    let br = br_q.or(star_q).unwrap_or(0.0);
    let gzip = gzip_q.or(star_q).unwrap_or(0.0);

    let br_ok = br > 0.0;
    let gzip_ok = gzip > 0.0;
    match (br_ok, gzip_ok) {
        (true, true) => {
            // Equal q → user has no preference between them; honour ours.
            // Otherwise the higher q wins, even if it conflicts with our
            // preference (the client's stated preference is the spec answer).
            if (br - gzip).abs() < f32::EPSILON {
                if prefer_brotli {
                    Encoding::Brotli
                } else {
                    Encoding::Gzip
                }
            } else if br > gzip {
                Encoding::Brotli
            } else {
                Encoding::Gzip
            }
        }
        (true, false) => Encoding::Brotli,
        (false, true) => Encoding::Gzip,
        (false, false) => Encoding::Identity,
    }
}

/// Whether a `Content-Type` is worth compressing — an allowlist of text-ish
/// types, so we never re-compress something already compressed (zip, images,
/// video, fonts, …).
fn is_compressible(content_type: &str) -> bool {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    ct.starts_with("text/")
        || ct == "application/json"
        || ct == "application/javascript"
        || ct == "application/manifest+json"
        || ct == "application/xml"
        || ct == "application/xhtml+xml"
        || ct == "application/rss+xml"
        || ct == "application/atom+xml"
        || ct == "application/wasm"
        || ct == "image/svg+xml"
        || ct.ends_with("+json")
        || ct.ends_with("+xml")
}

fn encode(enc: Encoding, data: &[u8], brotli_quality: u32) -> Option<Vec<u8>> {
    match enc {
        Encoding::Gzip => {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(data).ok()?;
            e.finish().ok()
        }
        Encoding::Brotli => {
            let mut out = Vec::new();
            {
                // (buffer_size, quality 0..=11, lgwin 10..=24). Quality
                // configurable via `CompressionLayer::brotli_quality`; default 4 is
                // the speed/ratio sweet spot for per-request dynamic content.
                let mut w = brotli::CompressorWriter::new(&mut out, 4096, brotli_quality, 22);
                w.write_all(data).ok()?;
            } // drop finalizes the stream and flushes into `out`
            Some(out)
        }
        Encoding::Identity => None,
    }
}

/// If `response` carries a `Content-Encoding` (i.e. we compressed), append
/// `Vary: Accept-Encoding` so caches key on it. Appended, not inserted, so an
/// existing `Vary` (e.g. `Origin` from CORS) is preserved.
pub(crate) fn tag_vary_if_encoded<B>(mut response: Response<B>) -> Response<B> {
    if response.headers().contains_key(header::CONTENT_ENCODING) {
        response
            .headers_mut()
            .append(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn negotiate_picks_the_higher_q_when_client_states_a_preference() {
        // Explicit client preference wins over our prefer_brotli setting:
        // the spec says the highest non-zero q is the chosen encoding.
        assert_eq!(
            negotiate(Some("br;q=0.8, gzip;q=1.0"), true),
            Encoding::Gzip,
            "gzip has higher q; prefer_brotli is only a tie-breaker",
        );
        assert_eq!(
            negotiate(Some("br;q=1.0, gzip;q=0.5"), false),
            Encoding::Brotli,
            "br has higher q; prefer_brotli=false doesn't override it",
        );
    }

    #[test]
    fn negotiate_uses_prefer_brotli_only_on_a_tie() {
        // Equal q → server preference decides.
        assert_eq!(
            negotiate(Some("br;q=0.7, gzip;q=0.7"), true),
            Encoding::Brotli,
        );
        assert_eq!(
            negotiate(Some("br;q=0.7, gzip;q=0.7"), false),
            Encoding::Gzip,
        );
        // Default q (1.0) on both — same as the explicit-tie case.
        assert_eq!(negotiate(Some("gzip, deflate, br"), true), Encoding::Brotli);
        assert_eq!(negotiate(Some("gzip, deflate, br"), false), Encoding::Gzip);
    }

    #[test]
    fn negotiate_treats_q_zero_as_explicit_disallow() {
        // `br;q=0` → use gzip even though brotli is named.
        assert_eq!(negotiate(Some("br;q=0, gzip"), true), Encoding::Gzip);
        // Both disallowed → identity (we don't 406; we just send uncompressed).
        assert_eq!(
            negotiate(Some("br;q=0, gzip;q=0"), true),
            Encoding::Identity
        );
    }

    #[test]
    fn negotiate_wildcard_applies_to_unnamed_encodings() {
        assert_eq!(negotiate(Some("*"), true), Encoding::Brotli);
        assert_eq!(negotiate(Some("*;q=0.5"), true), Encoding::Brotli);
        // Wildcard disallows everything; nothing positive remains.
        assert_eq!(negotiate(Some("*;q=0"), true), Encoding::Identity);
        // Named gzip + wildcard → only named is positive; pick it.
        assert_eq!(negotiate(Some("gzip, *;q=0"), true), Encoding::Gzip);
        // Wildcard supplies fallback q for the not-named encoding.
        // Here gzip is named (q=1) and br falls back to the wildcard q=0.5.
        // Gzip wins on the higher q.
        assert_eq!(negotiate(Some("gzip, *;q=0.5"), true), Encoding::Gzip);
    }

    #[test]
    fn negotiate_handles_only_one_offered() {
        assert_eq!(negotiate(Some("gzip"), true), Encoding::Gzip);
        assert_eq!(negotiate(Some("br"), false), Encoding::Brotli);
    }

    #[test]
    fn negotiate_identity_only_means_no_encoding() {
        // `identity` is "send the body untouched." We don't compress.
        assert_eq!(negotiate(Some("identity"), true), Encoding::Identity);
    }

    #[test]
    fn negotiate_missing_header_means_no_compression() {
        // Per spec: a request with no Accept-Encoding header doesn't
        // forbid encodings, but conservatively we don't send a body the
        // client didn't ask for.
        assert_eq!(negotiate(None, true), Encoding::Identity);
    }

    #[test]
    fn negotiate_ignores_unknown_encodings() {
        // `deflate`, `compress`, `x-gzip` — we don't produce them, so they
        // contribute nothing; fall through to identity.
        assert_eq!(
            negotiate(Some("deflate, compress, x-gzip"), true),
            Encoding::Identity,
        );
    }

    #[test]
    fn negotiate_tolerates_whitespace_and_casing() {
        assert_eq!(
            negotiate(Some(" BR ; Q=0.9 , GZip ; q=0.5 "), true),
            Encoding::Brotli,
            "case-insensitive name + Q=; tolerated whitespace",
        );
    }

    #[test]
    fn negotiate_rejects_out_of_range_q_silently() {
        // RFC restricts q to [0, 1]. Out-of-range values are ignored
        // (fall back to default q=1.0 for the token).
        assert_eq!(negotiate(Some("br;q=2.0"), true), Encoding::Brotli);
        assert_eq!(negotiate(Some("br;q=-1"), true), Encoding::Brotli);
    }

    #[test]
    fn is_compressible_allowlist() {
        assert!(is_compressible("application/json"));
        assert!(is_compressible("application/vnd.api+json; charset=utf-8"));
        assert!(is_compressible("text/html"));
        assert!(is_compressible("image/svg+xml"));
        assert!(!is_compressible("image/png"));
        assert!(!is_compressible("application/zip"));
        assert!(!is_compressible("application/octet-stream"));
    }

    #[test]
    fn small_json_is_buffered_but_not_encoded() {
        let out = CompressionLayer::new()
            .compress_reply(ReplyData::Json(json!({"ok": true})), Some("br"));
        match out {
            ReplyData::Bytes { content_type, .. } => assert_eq!(content_type, "application/json"),
            other => panic!("expected buffered Bytes, got {other:?}"),
        }
    }

    #[test]
    fn large_json_is_brotli_encoded_and_smaller() {
        // A repetitive ~50 KiB JSON array — very compressible.
        let big = json!({ "rows": (0..2000).map(|i| json!({"id": i, "name": "User Name"})).collect::<Vec<_>>() });
        let original_len = serde_json::to_vec(&big).unwrap().len();
        assert!(original_len > 10_000);
        let out = CompressionLayer::new().compress_reply(ReplyData::Json(big), Some("br, gzip"));
        match out {
            ReplyData::Rich(spec) => {
                assert_eq!(
                    spec.headers.get("content-encoding").map(String::as_str),
                    Some("br")
                );
                match &spec.payload {
                    ReplyData::Bytes { data, .. } => assert!(data.len() < original_len / 2),
                    other => panic!("expected Bytes payload, got {other:?}"),
                }
            }
            other => panic!("expected Rich(compressed), got {other:?}"),
        }
    }

    #[test]
    fn no_accept_encoding_leaves_json_alone() {
        let out = CompressionLayer::new().compress_reply(ReplyData::Json(json!({"a": 1})), None);
        assert!(matches!(out, ReplyData::Json(_)));
    }

    #[test]
    fn does_not_double_encode_an_already_encoded_reply() {
        let big = json!({ "rows": (0..2000).map(|i| json!({"id": i})).collect::<Vec<_>>() });
        let pre = ReplyData::Rich(Box::new(ReplySpec {
            payload: ReplyData::Bytes {
                content_type: "application/json".into(),
                data: serde_json::to_vec(&big).unwrap(),
            },
            status: None,
            headers: HashMap::from([("content-encoding".to_string(), "gzip".to_string())]),
        }));
        let out = CompressionLayer::new().compress_reply(pre, Some("br"));
        match out {
            ReplyData::Rich(spec) => {
                assert_eq!(
                    spec.headers.get("content-encoding").map(String::as_str),
                    Some("gzip")
                ); // unchanged — not re-encoded as br
            }
            other => panic!("expected Rich, got {other:?}"),
        }
    }

    #[test]
    fn tag_vary_appends_only_when_content_encoding_present() {
        let with_ce = Response::builder()
            .header(header::CONTENT_ENCODING, "br")
            .body(())
            .unwrap();
        let tagged = tag_vary_if_encoded(with_ce);
        assert_eq!(
            tagged.headers().get(header::VARY).unwrap(),
            "Accept-Encoding"
        );

        let without = Response::builder().body(()).unwrap();
        let untagged = tag_vary_if_encoded(without);
        assert!(untagged.headers().get(header::VARY).is_none());
    }

    // ===== `Cache-Control: no-transform` =====

    fn big_compressible_rich(headers: HashMap<String, String>) -> ReplyData {
        // A ~50 KiB JSON payload that would absolutely get compressed if
        // the layer were running normally — use this to prove the
        // no-transform short-circuit really is bypassing it.
        let big = json!({ "rows": (0..2000).map(|i| json!({"id": i})).collect::<Vec<_>>() });
        ReplyData::Rich(Box::new(ReplySpec {
            payload: ReplyData::Json(big),
            status: None,
            headers,
        }))
    }

    #[test]
    fn no_transform_directive_skips_compression_entirely() {
        // Per RFC 7234 §5.2.1.6: `Cache-Control: no-transform` tells
        // intermediaries (including us) not to alter the payload.
        // A handler that sets it should get its body through untouched.
        let pre = big_compressible_rich(HashMap::from([(
            "Cache-Control".into(),
            "no-transform".into(),
        )]));
        let out = CompressionLayer::new().compress_reply(pre, Some("br, gzip"));
        // The reply is unchanged: still a Rich wrapping a Json payload,
        // no Content-Encoding stamped, no compression performed.
        match out {
            ReplyData::Rich(spec) => {
                assert!(
                    !spec
                        .headers
                        .keys()
                        .any(|k| k.eq_ignore_ascii_case("content-encoding")),
                    "no-transform forbids compression; no Content-Encoding should be set",
                );
                assert!(
                    matches!(spec.payload, ReplyData::Json(_)),
                    "payload should be untouched (still Json, not lifted to Bytes)",
                );
            }
            other => panic!("expected Rich passing through unchanged, got {other:?}"),
        }
    }

    #[test]
    fn no_transform_is_case_insensitive_and_robust_to_other_directives() {
        // The header name is matched case-insensitively, and the
        // directive list is parsed token-by-token so `no-cache, no-transform`
        // and `private, no-transform, max-age=0` all trigger it. The
        // directive name itself is also case-insensitive.
        for header_name in ["cache-control", "Cache-Control", "CACHE-CONTROL"] {
            for value in [
                "no-transform",
                "no-cache, no-transform",
                "private, no-transform, max-age=0",
                "  no-transform  ", // tolerated whitespace
                "no-cache, NO-TRANSFORM",
            ] {
                let pre =
                    big_compressible_rich(HashMap::from([(header_name.into(), value.into())]));
                let out = CompressionLayer::new().compress_reply(pre, Some("br"));
                match out {
                    ReplyData::Rich(spec) => assert!(
                        !spec
                            .headers
                            .keys()
                            .any(|k| k.eq_ignore_ascii_case("content-encoding")),
                        "no-transform should suppress compression for header `{header_name}: {value}`",
                    ),
                    other => panic!("expected Rich, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn other_cache_control_directives_do_not_disable_compression() {
        // `no-cache`, `no-store`, etc. say nothing about transformation.
        // The body should still be compressed.
        for value in ["no-cache", "no-store", "private", "max-age=0"] {
            let pre =
                big_compressible_rich(HashMap::from([("Cache-Control".into(), value.into())]));
            let out = CompressionLayer::new().compress_reply(pre, Some("br"));
            match out {
                ReplyData::Rich(spec) => assert_eq!(
                    spec.headers.get("content-encoding").map(String::as_str),
                    Some("br"),
                    "compression should still run for header `Cache-Control: {value}`",
                ),
                other => panic!("expected Rich, got {other:?}"),
            }
        }
    }

    #[test]
    fn no_transform_only_applies_to_rich_replies() {
        // A bare `ReplyData::Json` can't carry headers, so it goes
        // through the normal compression path. The no-transform check is
        // a `Rich`-only short-circuit. (The handler that wants
        // no-transform builds a Rich; everyone else gets the default.)
        let big = json!({ "rows": (0..2000).map(|i| json!({"id": i})).collect::<Vec<_>>() });
        let out = CompressionLayer::new().compress_reply(ReplyData::Json(big), Some("br"));
        match out {
            ReplyData::Rich(spec) => {
                assert_eq!(
                    spec.headers.get("content-encoding").map(String::as_str),
                    Some("br"),
                );
            }
            other => panic!("expected Rich (compressed), got {other:?}"),
        }
    }

    // ===== `brotli_quality()` =====

    #[test]
    fn quality_setting_changes_brotli_output() {
        // Quality 0 and quality 11 should produce different outputs for
        // the same input — we don't assert specific sizes (brotli can
        // surprise), only that the *result differs*, which proves the
        // setting is plumbed through to the encoder.
        let payload = json!({ "rows": (0..2000).map(|i| json!({"id": i})).collect::<Vec<_>>() });
        let bytes = serde_json::to_vec(&payload).unwrap();
        let fast = encode(Encoding::Brotli, &bytes, 0).unwrap();
        let best = encode(Encoding::Brotli, &bytes, 11).unwrap();
        assert_ne!(
            fast, best,
            "quality 0 and quality 11 should produce different brotli outputs",
        );
        // Quality 11 is at least as tight as quality 0 (usually tighter).
        assert!(best.len() <= fast.len());
    }

    #[test]
    fn quality_clamps_to_eleven() {
        // Out-of-range values are clamped, not rejected — config-file
        // ergonomics. A caller passing `brotli_quality(99)` gets quality 11.
        let layer = CompressionLayer::new().brotli_quality(99);
        // Internal state check: we made the field private, so use the
        // observable behavior — encoding a body should succeed (it would
        // panic in brotli with an out-of-range quality otherwise).
        let payload = json!({"x": "y".repeat(2000)});
        let out = layer.compress_reply(ReplyData::Json(payload), Some("br"));
        assert!(matches!(out, ReplyData::Rich(_)));
    }
}
