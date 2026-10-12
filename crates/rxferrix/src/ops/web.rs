//! Serving the app's own frontend and extra routes from the same binary (docs/operations/deploying.md).
//!
//! ```ignore
//! #[derive(rxferrix::rust_embed::Embed)]
//! #[folder = "../web/dist/"]
//! #[crate_path = "rxferrix::rust_embed"]
//! struct Assets;
//!
//! RxFerrix::new()
//!     .embed_static::<Assets>()                       // or .static_dir("./web/dist") (read at runtime)
//!     .routes(axum::Router::new().route("/webhooks/stripe", post(stripe_webhook)))
//!     .run()
//! ```
//!
//! **Routing.** RxFerrix's own routes (`/api/*`, `/_/*`, `/metrics`) and the app's [`routes`](WebOptions::routes) come
//! first; static files are the fallback for everything else, and never answer under `/api` or `/_` (those get a
//! plain 404), so they can't shadow the API or the dashboard.
//!
//! **Files.** `GET`/`HEAD` of an existing file serves it; a directory path serves its `index.html`. Any other path is
//! a client-side route and gets the root `index.html` (SPA fallback), unless its last segment has a file extension
//! and the client doesn't accept HTML (a missing `/logo.png` is a 404, not HTML).
//!
//! **Caching.** HTML is `Cache-Control: no-cache` (always revalidated, so a deploy shows up at once); files with a
//! content hash in their name (anything under `assets/`, Vite's output, or a name like `app.3f2a1b9c.js` /
//! `index-D8aSd0Fw.js`) are `public, max-age=31536000, immutable`; everything else is `no-cache`. Every file has an
//! `ETag` (`If-None-Match` → 304).
//!
//! **Precompressed files.** When `<file>.br` or `<file>.gz` exists next to a file and the client accepts that
//! encoding (`Accept-Encoding`), it is served instead with `Content-Encoding` and `Vary: Accept-Encoding` (Brotli
//! preferred). Nothing is compressed on the fly.

use std::borrow::Cow;
use std::path::{Component, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::runtime::App;

/// Looks a file up in an embedded asset bundle (`<A as rust_embed::RustEmbed>::get`).
pub type EmbeddedLookup = fn(&str) -> Option<rust_embed::EmbeddedFile>;

/// Where the frontend's files come from.
#[derive(Clone)]
pub enum StaticFiles {
    /// A directory read at runtime (relative paths are relative to the working directory).
    Dir(PathBuf),
    /// Files embedded in the binary with `rust-embed` ([`crate::RxFerrix::embed_static`]).
    Embedded(EmbeddedLookup),
}

impl std::fmt::Debug for StaticFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StaticFiles::Dir(d) => f.debug_tuple("Dir").field(d).finish(),
            StaticFiles::Embedded(_) => f.write_str("Embedded"),
        }
    }
}

/// The frontend and the extra routes of a server (`RxFerrix::static_dir`, `embed_static`, `routes`).
#[derive(Clone, Debug, Default)]
pub struct WebOptions {
    pub static_files: Option<StaticFiles>,
    /// Extra axum routes (webhooks, custom endpoints), merged with RxFerrix's own. They must not overlap them.
    pub routes: Option<Router>,
}

/// The complete router of a server: RxFerrix's routes, the app's routes, the static frontend as the fallback, and
/// request counting when metrics are enabled.
pub fn router(app: App, web: &WebOptions) -> Router {
    let mut router = crate::server::router(app.clone());
    if let Some(routes) = &web.routes {
        router = router.merge(routes.clone());
    }
    if let Some(files) = &web.static_files {
        let files = Arc::new(files.clone());
        router = router.fallback(move |request: Request| {
            let files = files.clone();
            async move { serve_static(&files, request.method(), request.uri().path(), request.headers()).await }
        });
    }
    if app.options().ops.metrics.enabled {
        router = router.layer(axum::middleware::from_fn_with_state(app, super::metrics::track_requests));
    }
    router
}

/// A file found in the static source.
struct Found {
    data: Cow<'static, [u8]>,
    etag: String,
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, [(header::CACHE_CONTROL, "no-store")], "not found").into_response()
}

/// Percent-decode a URL path; `None` for invalid UTF-8.
fn percent_decode(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The relative file path for a request path, or `None` if it tries to leave the root.
fn relative_path(path: &str) -> Option<String> {
    let decoded = percent_decode(path)?;
    if decoded.contains(['\\', '\0']) {
        return None;
    }
    let mut parts = Vec::new();
    for c in std::path::Path::new(decoded.trim_start_matches('/')).components() {
        match c {
            Component::Normal(p) => parts.push(p.to_str()?.to_string()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    let mut rel = parts.join("/");
    if rel.is_empty() || decoded.ends_with('/') {
        if !rel.is_empty() {
            rel.push('/');
        }
        rel.push_str("index.html");
    }
    Some(rel)
}

/// Whether a file name carries a content hash: under `assets/`, or a `.`/`-`-separated part of 8+ letters and digits
/// with at least one digit (`app.3f2a1b9c.js`, `index-D8aSd0Fw.js`).
pub fn looks_hashed(rel: &str) -> bool {
    if rel.starts_with("assets/") || rel.contains("/assets/") {
        return true;
    }
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    stem.split(['.', '-']).skip(1).any(|part| {
        (8..=64).contains(&part.len())
            && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && part.chars().any(|c| c.is_ascii_digit())
    })
}

fn cache_control(rel: &str) -> &'static str {
    if rel.ends_with(".html") {
        "no-cache"
    } else if looks_hashed(rel) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

fn content_type(rel: &str) -> HeaderValue {
    let mime = mime_guess::from_path(rel).first_or_octet_stream();
    let text = mime.type_() == mime_guess::mime::TEXT
        || matches!(mime.subtype().as_str(), "javascript" | "json" | "xml" | "svg+xml" | "manifest+json");
    let value =
        if text && mime.get_param("charset").is_none() { format!("{mime}; charset=utf-8") } else { mime.to_string() };
    HeaderValue::from_str(&value).unwrap_or(HeaderValue::from_static("application/octet-stream"))
}

async fn lookup(files: &StaticFiles, rel: &str) -> Option<Found> {
    match files {
        StaticFiles::Embedded(get) => {
            let file = get(rel)?;
            let hash = data_encoding::HEXLOWER.encode(&file.metadata.sha256_hash()[..16]);
            Some(Found { data: file.data, etag: format!("\"{hash}\"") })
        }
        StaticFiles::Dir(root) => {
            let path = root.join(rel);
            let meta = tokio::fs::metadata(&path).await.ok().filter(|m| m.is_file())?;
            let data = tokio::fs::read(&path).await.ok()?;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            Some(Found { data: Cow::Owned(data), etag: format!("W/\"{:x}-{mtime:x}\"", meta.len()) })
        }
    }
}

fn accepts(headers: &HeaderMap, coding: &str) -> bool {
    headers.get_all(header::ACCEPT_ENCODING).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).any(
        |item| {
            let mut parts = item.split(';');
            let name = parts.next().unwrap_or("").trim();
            let q_zero = parts.any(|p| {
                let p = p.trim();
                p.strip_prefix("q=").is_some_and(|q| q.trim().parse::<f32>().is_ok_and(|q| q == 0.0))
            });
            (name.eq_ignore_ascii_case(coding) || name == "*") && !q_zero
        },
    )
}

fn etag_matches(headers: &HeaderMap, etag: &str) -> bool {
    let strip = |t: &str| t.trim().trim_start_matches("W/").to_string();
    headers.get_all(header::IF_NONE_MATCH).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).any(|t| {
        let t = t.trim();
        t == "*" || strip(t) == strip(etag)
    })
}

/// Serve `rel` (found as `file`), negotiating a precompressed variant.
async fn respond(files: &StaticFiles, rel: &str, file: Found, headers: &HeaderMap, head: bool) -> Response {
    let mut variant = None;
    let mut vary = false;
    for (coding, ext) in [("br", "br"), ("gzip", "gz")] {
        let candidate = format!("{rel}.{ext}");
        if let Some(found) = lookup(files, &candidate).await {
            vary = true;
            if variant.is_none() && accepts(headers, coding) {
                variant = Some((coding, found));
            }
        }
    }
    let (body, etag, encoding) = match variant {
        Some((coding, found)) => (found.data, format!("{}-{coding}\"", found.etag.trim_end_matches('"')), Some(coding)),
        None => (file.data, file.etag, None),
    };
    let mut response = if etag_matches(headers, &etag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let len = body.len();
        let mut r = if head { Response::new(Body::empty()) } else { Response::new(Body::from(body)) };
        r.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(len));
        r
    };
    let h = response.headers_mut();
    h.insert(header::CONTENT_TYPE, content_type(rel));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache_control(rel)));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if let Some(coding) = encoding {
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static(coding));
    }
    if vary {
        h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    response
}

/// Whether `path` belongs to RxFerrix (never served from static files).
fn reserved(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/") || path == "/_" || path.starts_with("/_/")
}

/// The fallback handler: a file, the SPA's `index.html`, or 404.
pub async fn serve_static(files: &StaticFiles, method: &Method, path: &str, headers: &HeaderMap) -> Response {
    if reserved(path) || (method != Method::GET && method != Method::HEAD) {
        return not_found();
    }
    let head = method == Method::HEAD;
    let Some(rel) = relative_path(path) else { return not_found() };
    if let Some(found) = lookup(files, &rel).await {
        return respond(files, &rel, found, headers, head).await;
    }
    let last = rel.rsplit('/').next().unwrap_or("");
    let wants_html = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|a| a.contains("text/html"));
    if last.contains('.') && !wants_html {
        return not_found();
    }
    match lookup(files, "index.html").await {
        Some(index) => respond(files, "index.html", index, headers, head).await,
        None => not_found(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(relative_path("/").as_deref(), Some("index.html"));
        assert_eq!(relative_path("/a/b.js").as_deref(), Some("a/b.js"));
        assert_eq!(relative_path("/docs/").as_deref(), Some("docs/index.html"));
        assert_eq!(relative_path("/a%20b.txt").as_deref(), Some("a b.txt"));
        assert_eq!(relative_path("/../etc/passwd"), None);
        assert_eq!(relative_path("/a/%2e%2e/%2e%2e/x"), None);
        assert_eq!(relative_path("/a\\b"), None);
        assert_eq!(relative_path("/%ff"), None);
        assert!(reserved("/api") && reserved("/api/x") && reserved("/_") && reserved("/_/data"));
        assert!(!reserved("/apiary") && !reserved("/_x"));
    }

    #[test]
    fn hashed_names_and_cache_control() {
        assert!(looks_hashed("assets/index-D8aSd0Fw.js"));
        assert!(looks_hashed("static/app.3f2a1b9c.js"));
        assert!(looks_hashed("index-D8aSd0Fw.css"));
        assert!(!looks_hashed("favicon.ico"));
        assert!(!looks_hashed("robots.txt"));
        assert!(!looks_hashed("my-component.js"));
        assert_eq!(cache_control("index.html"), "no-cache");
        assert_eq!(cache_control("assets/a-12345678.js"), "public, max-age=31536000, immutable");
        assert_eq!(content_type("a.js"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("a.png"), "image/png");
    }

    #[test]
    fn encodings() {
        let mut h = HeaderMap::new();
        h.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip, deflate, br;q=0"));
        assert!(accepts(&h, "gzip") && !accepts(&h, "br"));
        h.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("*"));
        assert!(accepts(&h, "br"));
        assert!(!accepts(&HeaderMap::new(), "gzip"));
    }
}
