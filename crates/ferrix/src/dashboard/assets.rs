//! The dashboard SPA, embedded in the binary and served at `/_/` (cargo feature `dashboard`, on by default).
//!
//! The built assets live in `crates/ferrix/dashboard-dist/` and are committed, so `cargo build` needs no JS
//! toolchain. Rebuild them from `packages/dashboard` with `bun run build:dashboard` at the repository root; that also
//! writes `dashboard-dist/source-hash.txt`, a hash of the sources the bundle was built from, which the test
//! [`tests::committed_dist_is_fresh`] compares with the current sources so a stale bundle fails `cargo test`.
//!
//! Routing: a request for an existing file is answered with it (hashed `assets/*` files are cached forever);
//! anything else under `/_/` gets `index.html`, and the SPA routes on the client (`/_/data/todos`, `/_/setup`, ...).

use axum::Router;
use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;

use crate::runtime::App;

#[derive(rust_embed::Embed)]
#[folder = "dashboard-dist/"]
struct Assets;

/// The `/_/` routes.
pub fn routes() -> Router<App> {
    Router::new()
        .route("/_", get(|| async { Redirect::permanent("/_/") }))
        .route("/_/", get(|| async { serve("index.html") }))
        .route("/_/{*path}", get(|Path(path): Path<String>| async move { serve(&path) }))
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Content Security Policy of the SPA: only its own scripts; connections to this origin (HTTP and WebSocket);
/// images from anywhere over http(s) (user avatars).
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
                   img-src 'self' data: https: http:; connect-src 'self' ws: wss:; frame-ancestors 'none'; \
                   base-uri 'none'; form-action 'self'";

fn serve(path: &str) -> Response {
    let path = path.trim_start_matches('/');
    let (name, file) = match Assets::get(path) {
        Some(f) if !path.is_empty() => (path, f),
        // Unknown asset files are real 404s; every other path is a client-side route.
        _ if path.starts_with("assets/") => return (StatusCode::NOT_FOUND, "not found").into_response(),
        _ => match Assets::get("index.html") {
            Some(f) => ("index.html", f),
            None => {
                return (StatusCode::NOT_FOUND, "The dashboard was not built into this binary (see dashboard-dist/).")
                    .into_response();
            }
        },
    };
    let cache = if name.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    let mut response = (StatusCode::OK, file.data.into_owned()).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type(name)));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    if name == "index.html" {
        headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    }
    response
}

/// Hash of the dashboard's sources, as computed by `packages/dashboard/scripts/source-hash.ts`: SHA-256 over every
/// input file in path order, each as `<path relative to the repo root>\0<content with CRLF normalized to LF>\0`.
/// Inputs: the dashboard package (`index.html`, configs, `src/**`) and the sources it bundles from
/// `packages/client/src/**` and `packages/svelte/src/lib/**`.
#[cfg(test)]
pub(crate) fn source_hash(repo: &std::path::Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                walk(&path, out)?;
            } else {
                out.push(path);
            }
        }
        Ok(())
    }
    let dash = repo.join("packages/dashboard");
    let mut files: Vec<std::path::PathBuf> = ["index.html", "package.json", "vite.config.ts", "tsconfig.json", "svelte.config.js"]
        .iter()
        .map(|f| dash.join(f))
        .filter(|p| p.exists())
        .collect();
    for dir in [dash.join("src"), repo.join("packages/client/src"), repo.join("packages/svelte/src/lib")] {
        walk(&dir, &mut files)?;
    }
    let mut rel: Vec<(String, std::path::PathBuf)> = files
        .into_iter()
        .map(|p| (p.strip_prefix(repo).expect("under repo").to_string_lossy().replace('\\', "/"), p))
        .collect();
    rel.sort();
    let mut h = Sha256::new();
    for (name, path) in rel {
        let content = std::fs::read_to_string(&path)?.replace("\r\n", "\n");
        h.update(name.as_bytes());
        h.update([0]);
        h.update(content.as_bytes());
        h.update([0]);
    }
    Ok(data_encoding::HEXLOWER.encode(&h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed bundle was built from the current sources. If this fails, run `bun run build:dashboard` at the
    /// repository root and commit `crates/ferrix/dashboard-dist`.
    #[test]
    fn committed_dist_is_fresh() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        if !repo.join("packages/dashboard").exists() {
            return; // Built from a published crate: no sources to compare with.
        }
        let expected = source_hash(&repo).expect("hashing dashboard sources");
        let stamp = Assets::get("source-hash.txt").expect("dashboard-dist/source-hash.txt is missing");
        let committed = String::from_utf8_lossy(&stamp.data).trim().to_string();
        assert_eq!(
            committed, expected,
            "crates/ferrix/dashboard-dist is stale: run `bun run build:dashboard` and commit the result"
        );
        assert!(Assets::get("index.html").is_some());
    }

    #[test]
    fn spa_fallback_and_assets() {
        assert_eq!(serve("data/todos").status(), StatusCode::OK);
        assert_eq!(serve("setup").headers()[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(serve("assets/missing.js").status(), StatusCode::NOT_FOUND);
        assert!(serve("").headers().contains_key(header::CONTENT_SECURITY_POLICY));
    }
}
