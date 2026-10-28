//! `POST /api/storage/upload?token=…[&filename=…]` (upload through a signed URL) and `GET|HEAD /api/storage/{id}`
//! (serving: signed URLs, ETags, single and multiple ranges, thumbnails). See docs/PROTOCOL.md, "File storage".
//!
//! **Security model.** Upload URLs are HMAC-signed with a key derived from `auth_secret.key`, expire after an hour
//! and work once. A file's id carries 96 random bits, so its URL is an unguessable capability: anyone
//! holding it can read the file, and nobody can enumerate files. Signed file URLs (`?exp=…&sig=…`) add an expiry; in
//! private mode they are the only way in. Responses never let an upload run script on the
//! app's origin: `X-Content-Type-Options: nosniff`, `Content-Security-Policy: default-src 'none'; …; sandbox` (an
//! opaque origin even if a browser renders the file), and every type that a browser could render as active content
//! (HTML, SVG, XML, JavaScript, …) is served as an attachment. Only images, audio, video, plain text, JSON, CSV and
//! PDF are served inline.

use std::ops::Range;

use axum::Router;
use axum::body::Body;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use data_encoding::HEXLOWER;
use futures::{StreamExt, TryStreamExt};
use serde::Deserialize;
use serde_json::json;

use super::StoredFile;
use super::service::{StorageService, TokenProblem, WriteError, check_upload_rate};
use super::thumbs::{self, ThumbError, ThumbSpec};
use crate::auth::AuthFailure;
use crate::auth::http::Caller;
use crate::db::query::{QueryDb, Source};
use crate::id::{DocId, Id};
use crate::runtime::App;
use crate::schema::{Doc, Table};

/// The storage routes, merged into the main router by `server::router`.
pub fn routes() -> Router<App> {
    Router::new().route("/api/storage/upload", post(upload)).route("/api/storage/{id}", get(serve_file))
}

fn failure(status: u16, code: &str, message: impl Into<String>) -> Response {
    AuthFailure::new(status, code, message).into_response()
}

fn token_failure(problem: TokenProblem) -> Response {
    failure(problem.status(), problem.code(), problem.to_string())
}

// ---------------------------------------------------------------------------------------------------------------------
// Upload
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct UploadParams {
    token: Option<String>,
    filename: Option<String>,
}

/// A `Content-Type` worth storing: printable ASCII, `type/subtype[; params]`, at most 255 bytes.
pub(crate) fn validate_content_type(ct: &str) -> Result<String, String> {
    let ct = ct.trim();
    let bad = || format!("`{ct}` is not a valid content type");
    if ct.is_empty() || ct.len() > 255 || !ct.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return Err(bad());
    }
    let essence = ct.split(';').next().unwrap_or_default().trim();
    let token = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b));
    match essence.split_once('/') {
        Some((t, s)) if token(t) && token(s) => Ok(ct.to_string()),
        _ => Err(bad()),
    }
}

/// The last path segment of a client-supplied file name, without control characters, at most 255 characters.
pub(crate) fn sanitize_filename(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base.chars().filter(|c| !c.is_control()).take(255).collect();
    let clean = clean.trim();
    (!clean.is_empty() && clean != "." && clean != "..").then(|| clean.to_string())
}

async fn upload(
    State(app): State<App>,
    caller: Caller,
    Query(params): Query<UploadParams>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let service = app.storage().clone();
    if let Err(retry) = check_upload_rate(&service, &caller.rate_limit_key()) {
        let mut failure = AuthFailure::new(429, "rate_limited", "Too many uploads; try again later");
        failure.retry_after = Some(retry);
        return failure.into_response();
    }
    let token = match params.token.as_deref().map(|t| service.verify(t)) {
        Some(Ok(t)) => t,
        Some(Err(p)) => return token_failure(p),
        None => return token_failure(TokenProblem::Invalid),
    };
    let content_type = match headers.get(header::CONTENT_TYPE).map(|v| v.to_str()) {
        None => None,
        Some(Ok(ct)) => match validate_content_type(ct) {
            Ok(ct) => Some(ct),
            Err(m) => return failure(400, "invalid_content_type", m),
        },
        Some(Err(_)) => return failure(400, "invalid_content_type", "The content type is not valid ASCII"),
    };
    let max = service.config.max_upload_size;
    let too_large = || failure(413, "file_too_large", format!("Files may be at most {max} bytes"));
    let declared = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    if declared.is_some_and(|n| n > max) {
        return too_large();
    }
    let _claim = match service.claim(&app, &token).await {
        Ok(c) => c,
        Err(p) => return token_failure(p),
    };
    let (id, _in_flight) = match service.new_id(&app) {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    let stream = body.into_data_stream().map_err(|e| e.to_string());
    let (sha256, size) = match service.write_blob(id, stream, max).await {
        Ok(v) => v,
        Err(WriteError::TooLarge) => return too_large(),
        Err(WriteError::Body(e)) => return failure(400, "upload_failed", format!("Reading the upload failed: {e}")),
        Err(WriteError::Store(e)) => return internal(e.context("writing an uploaded file")),
    };
    let filename = params.filename.as_deref().and_then(sanitize_filename);
    let file = StoredFile { sha256, size, content_type, created_by: token.b.clone(), filename };
    match service.insert_file(&app, id, &file, Some(&token)).await {
        Ok(Ok(id)) => {
            ([(header::CACHE_CONTROL, "no-store")], axum::Json(json!({ "storageId": id.as_str() }))).into_response()
        }
        Ok(Err(problem)) => {
            service.delete_blob(&id.encode()).await.ok();
            token_failure(problem)
        }
        Err(e) => {
            service.delete_blob(&id.encode()).await.ok();
            internal(e)
        }
    }
}

fn internal(e: anyhow::Error) -> Response {
    tracing::error!("storage: {e:#}");
    failure(500, "server_error", "Server Error")
}

// ---------------------------------------------------------------------------------------------------------------------
// Serving
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct ServeParams {
    download: Option<String>,
    /// Signed URLs: expiry (seconds since the epoch) and signature.
    exp: Option<String>,
    sig: Option<String>,
    /// Thumbnails: `WxH`, and `contain` (default) or `cover`.
    thumb: Option<String>,
    fit: Option<String>,
}

/// At most this many ranges in one `Range` header; with more, the header is ignored (the whole file is sent).
pub(crate) const MAX_RANGES: usize = 16;

/// What a `Range` header asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RangeRequest {
    /// No (usable) range: the whole file. Malformed headers and more than [`MAX_RANGES`] ranges are ignored (RFC
    /// 9110 allows this).
    Full,
    Partial(Range<u64>),
    /// Two or more ranges after coalescing: a `multipart/byteranges` response, ascending.
    Multiple(Vec<Range<u64>>),
    Unsatisfiable,
}

/// One `range-spec`: `Ok(Some(range))`, `Ok(None)` when unsatisfiable, `Err(())` when malformed.
fn parse_range_spec(spec: &str, size: u64) -> Result<Option<Range<u64>>, ()> {
    let (start, end) = spec.split_once('-').ok_or(())?;
    let (start, end) = (start.trim(), end.trim());
    let num = |s: &str| if s.bytes().all(|b| b.is_ascii_digit()) { s.parse::<u64>().map_err(|_| ()) } else { Err(()) };
    match (start.is_empty(), end.is_empty()) {
        // -N: the last N bytes.
        (true, false) => match num(end)? {
            0 => Ok(None),
            _ if size == 0 => Ok(None),
            n => Ok(Some(size.saturating_sub(n)..size)),
        },
        // A-
        (false, true) => {
            let a = num(start)?;
            Ok((a < size).then_some(a..size))
        }
        // A-B (inclusive)
        (false, false) => {
            let (a, b) = (num(start)?, num(end)?);
            if a > b {
                return Err(());
            }
            Ok((a < size).then(|| a..b.saturating_add(1).min(size)))
        }
        (true, true) => Err(()),
    }
}

/// Parse a `Range` header (RFC 9110 §14.2) for a file of `size` bytes. Unsatisfiable ranges in a set are dropped;
/// overlapping and adjacent ones are coalesced (§14.6 permits it whatever their order), so parts never repeat bytes
/// and come in ascending order.
pub(crate) fn parse_range(value: &str, size: u64) -> RangeRequest {
    let Some((unit, specs)) = value.trim().split_once('=') else { return RangeRequest::Full };
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        return RangeRequest::Full;
    }
    // Empty list elements are allowed (and ignored) by the `#` rule.
    let specs: Vec<&str> = specs.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if specs.is_empty() || specs.len() > MAX_RANGES {
        return RangeRequest::Full;
    }
    let mut ranges = Vec::with_capacity(specs.len());
    for spec in specs {
        match parse_range_spec(spec, size) {
            Ok(Some(r)) => ranges.push(r),
            Ok(None) => {}
            Err(()) => return RangeRequest::Full,
        }
    }
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<u64>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    match merged.len() {
        0 => RangeRequest::Unsatisfiable,
        1 => RangeRequest::Partial(merged.remove(0)),
        _ => RangeRequest::Multiple(merged),
    }
}

/// `If-None-Match` matches `etag` (weak comparison, `*` matches anything).
fn none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers.get_all(header::IF_NONE_MATCH).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).any(|t| {
        let t = t.trim();
        t == "*" || t.trim_start_matches("W/") == etag
    })
}

/// Types served inline. Everything else (HTML, SVG, XML, scripts, unknown types) is an attachment.
pub(crate) fn inline_safe(content_type: Option<&str>) -> bool {
    let Some(ct) = content_type else { return false };
    let essence = ct.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    matches!(
        essence.as_str(),
        "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "image/avif"
            | "image/bmp"
            | "image/x-icon"
            | "image/vnd.microsoft.icon"
            | "text/plain"
            | "text/csv"
            | "application/json"
            | "application/pdf"
    ) || essence.starts_with("video/")
        || essence.starts_with("audio/")
}

/// `inline` or `attachment`, with the original file name (RFC 6266 / 8187) when known.
pub(crate) fn content_disposition(inline: bool, filename: Option<&str>) -> String {
    let kind = if inline { "inline" } else { "attachment" };
    let Some(name) = filename else { return kind.to_string() };
    let ascii: String =
        name.chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' }).collect();
    let mut encoded = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    format!("{kind}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

/// The CSP for served files: nothing may load, and the file gets an opaque origin if a browser renders it. PDFs are
/// left without it (browsers refuse to show sandboxed PDFs); their viewers run no script with the page's origin.
const FILE_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; sandbox";

async fn load(app: &App, id: &str) -> crate::Result<Option<Doc<StoredFile>>> {
    let Ok(doc_id) = DocId::decode(id) else { return Ok(None) };
    if app.db().catalog().table(StoredFile::NAME)?.number != doc_id.table_number() {
        return Ok(None);
    }
    let snapshot = app.db().snapshot().await?;
    let db = QueryDb::new(Source::Snapshot(snapshot), app.db().catalog().clone());
    db.get(Id::<StoredFile>::from_doc_id(doc_id)).await
}

fn set(h: &mut HeaderMap, name: header::HeaderName, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        h.insert(name, v);
    }
}

fn respond(status: StatusCode, headers: HeaderMap, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

async fn serve_file(
    State(app): State<App>,
    method: Method,
    caller: Caller,
    Path(id): Path<String>,
    Query(params): Query<ServeParams>,
    headers: HeaderMap,
) -> Response {
    let service = app.storage().clone();
    // Signature first (also for ids that don't exist), so private mode reveals nothing without one.
    let now_s = (app.db().clock().now_ms() / 1000.0) as u64;
    let expires = match service.check_signature(&id, params.exp.as_deref(), params.sig.as_deref(), now_s) {
        Ok(expires) => expires,
        Err(problem) => return failure(403, problem.code(), problem.to_string()),
    };
    let thumb = match params.thumb.as_deref() {
        Some(thumb) => match ThumbSpec::parse(thumb, params.fit.as_deref(), &service.config) {
            Ok(spec) => Some(spec),
            Err(message) => return failure(400, "invalid_thumbnail", message),
        },
        None => None,
    };
    let not_found = || failure(404, "not_found", "File not found");
    let file = match load(&app, &id).await {
        Ok(Some(f)) => f,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    let content_type = file.content_type.as_deref();
    let download = params.download.as_deref().is_some_and(|v| v != "0" && v != "false");
    let inline = !download && (thumb.is_some() || inline_safe(content_type));

    let mut h = HeaderMap::new();
    // A signed URL's response is cached at most until the URL expires.
    let max_age = match expires {
        Some(expires) => service.config.cache_max_age.as_secs().min(expires.saturating_sub(now_s)),
        None => service.config.cache_max_age.as_secs(),
    };
    set(&mut h, header::CACHE_CONTROL, &format!("private, max-age={max_age}, immutable"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    if let Some(spec) = thumb {
        return serve_thumbnail(&app, method, &caller, &file, spec, &headers, h, inline).await;
    }

    let size = file.size;
    let etag = format!("\"{}\"", file.sha256);
    set(&mut h, header::ETAG, &etag);
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if none_match(&headers, &etag) {
        return respond(StatusCode::NOT_MODIFIED, h, Body::empty());
    }
    let pdf = content_type.is_some_and(|ct| ct.trim().to_ascii_lowercase().starts_with("application/pdf"));
    if !(inline && pdf) {
        h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(FILE_CSP));
    }
    set(&mut h, header::CONTENT_DISPOSITION, &content_disposition(inline, file.filename.as_deref()));
    let content_type = content_type.unwrap_or("application/octet-stream");

    // A Range applies unless If-Range names another version (with immutable content, only another ETag can).
    let if_range_ok = headers.get(header::IF_RANGE).and_then(|v| v.to_str().ok()).is_none_or(|v| v.trim() == etag);
    let range = match headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        Some(r) if if_range_ok => parse_range(r, size),
        _ => RangeRequest::Full,
    };
    let (status, range) = match range {
        RangeRequest::Full => (StatusCode::OK, None),
        RangeRequest::Partial(r) => {
            set(&mut h, header::CONTENT_RANGE, &format!("bytes {}-{}/{size}", r.start, r.end - 1));
            (StatusCode::PARTIAL_CONTENT, Some(r))
        }
        RangeRequest::Multiple(ranges) => {
            return serve_multipart(&app, method, &file.id.as_str(), size, content_type, ranges, h);
        }
        RangeRequest::Unsatisfiable => {
            set(&mut h, header::CONTENT_RANGE, &format!("bytes */{size}"));
            return respond(StatusCode::RANGE_NOT_SATISFIABLE, h, Body::empty());
        }
    };
    set(&mut h, header::CONTENT_TYPE, content_type);
    let length = range.as_ref().map_or(size, |r| r.end - r.start);
    set(&mut h, header::CONTENT_LENGTH, &length.to_string());
    if method == Method::HEAD || length == 0 {
        return respond(status, h, Body::empty());
    }
    match service.read_blob(&file.id.as_str(), range).await {
        Ok(result) => respond(status, h, Body::from_stream(result.into_stream())),
        Err(object_store::Error::NotFound { .. }) => {
            tracing::warn!("storage: the blob of file {} is missing", file.id);
            not_found()
        }
        Err(e) => internal(anyhow::Error::from(e).context(format!("reading file {}", file.id))),
    }
}

/// A `206` `multipart/byteranges` response (RFC 9110 §14.6): one part per range, each with its `Content-Type` and
/// `Content-Range`, streamed from the store range by range. `Content-Length` is computed up front.
fn serve_multipart(
    app: &App,
    method: Method,
    id: &str,
    size: u64,
    content_type: &str,
    ranges: Vec<Range<u64>>,
    mut h: HeaderMap,
) -> Response {
    let boundary = HEXLOWER.encode(&rand::random::<[u8; 16]>());
    let parts: Vec<(Bytes, Range<u64>)> = ranges
        .into_iter()
        .map(|r| {
            let head = format!(
                "--{boundary}\r\nContent-Type: {content_type}\r\nContent-Range: bytes {}-{}/{size}\r\n\r\n",
                r.start,
                r.end - 1
            );
            (Bytes::from(head), r)
        })
        .collect();
    let closing = Bytes::from(format!("--{boundary}--\r\n"));
    let length: u64 =
        parts.iter().map(|(head, r)| head.len() as u64 + (r.end - r.start) + 2).sum::<u64>() + closing.len() as u64;
    set(&mut h, header::CONTENT_TYPE, &format!("multipart/byteranges; boundary={boundary}"));
    set(&mut h, header::CONTENT_LENGTH, &length.to_string());
    if method == Method::HEAD {
        return respond(StatusCode::PARTIAL_CONTENT, h, Body::empty());
    }
    let service = app.storage().clone();
    let id = id.to_string();
    let body = futures::stream::iter(parts)
        .then(move |(head, range)| {
            let (service, id) = (service.clone(), id.clone());
            async move {
                let data = match service.read_blob(&id, Some(range)).await {
                    Ok(result) => result.into_stream().map_err(std::io::Error::other).boxed(),
                    Err(e) => futures::stream::once(async move { Err(std::io::Error::other(e)) }).boxed(),
                };
                futures::stream::once(async move { Ok(head) })
                    .chain(data)
                    .chain(futures::stream::once(async { Ok(Bytes::from_static(b"\r\n")) }))
            }
        })
        .flatten()
        .chain(futures::stream::once(async move { Ok::<_, std::io::Error>(closing) }));
    respond(StatusCode::PARTIAL_CONTENT, h, Body::from_stream(body))
}

/// Serve (generating and caching it if needed) a thumbnail of `file`. Generating one is rate limited per client IP
/// (`StorageConfig::thumbnail_rate_limit`); serving a cached variant is not.
#[allow(clippy::too_many_arguments)]
async fn serve_thumbnail(
    app: &App,
    method: Method,
    caller: &Caller,
    file: &Doc<StoredFile>,
    spec: ThumbSpec,
    headers: &HeaderMap,
    mut h: HeaderMap,
    inline: bool,
) -> Response {
    let service = app.storage().clone();
    if !thumbs::supported_source(file.content_type.as_deref()) {
        return failure(415, "unsupported_image", ThumbError::Unsupported.to_string());
    }
    let max_bytes = service.config.thumbnail_max_source_bytes;
    if file.size > max_bytes {
        return failure(422, "image_too_large", format!("Thumbnails are only made of files up to {max_bytes} bytes"));
    }
    let etag = spec.etag(&file.sha256);
    set(&mut h, header::ETAG, &etag);
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));
    if none_match(headers, &etag) {
        return respond(StatusCode::NOT_MODIFIED, h, Body::empty());
    }
    let id = file.id.as_str();
    let variant = spec.variant();
    let bytes = match service.cached_thumbnail(&id, &variant).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            if let Err(retry) = super::service::check_thumbnail_rate(&service, &caller.rate_limit_key()) {
                let failure = AuthFailure {
                    status: 429,
                    error: crate::UserError::new("rate_limited", "Too many thumbnails requested; try again later"),
                    retry_after: Some(retry),
                };
                return failure.into_response();
            }
            match generate_thumbnail(&service, &id, spec).await {
                Ok(bytes) => {
                    service.cache_thumbnail(&id, &variant, bytes.clone()).await;
                    bytes
                }
                Err(Ok(ThumbError::Unsupported)) => {
                    return failure(415, "unsupported_image", ThumbError::Unsupported.to_string());
                }
                Err(Ok(e @ ThumbError::TooLarge { .. })) => return failure(422, "image_too_large", e.to_string()),
                Err(Ok(e @ ThumbError::Invalid(_))) => return failure(422, "invalid_image", e.to_string()),
                Err(Err(e)) => return internal(e),
            }
        }
        Err(e) => return internal(e),
    };
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(FILE_CSP));
    set(&mut h, header::CONTENT_TYPE, thumbs::content_type_of(&bytes));
    set(&mut h, header::CONTENT_DISPOSITION, &content_disposition(inline, None));
    set(&mut h, header::CONTENT_LENGTH, &bytes.len().to_string());
    let body = if method == Method::HEAD { Body::empty() } else { Body::from(bytes) };
    respond(StatusCode::OK, h, body)
}

/// Read the source and render the thumbnail on a blocking thread, holding one of the thumbnail permits.
async fn generate_thumbnail(
    service: &StorageService,
    id: &str,
    spec: ThumbSpec,
) -> Result<Bytes, Result<ThumbError, anyhow::Error>> {
    let _permit = service.thumbnail_permits.acquire().await.map_err(|e| Err(e.into()))?;
    let source = match service.read_blob(id, None).await {
        Ok(result) => {
            result.bytes().await.map_err(|e| Err(anyhow::Error::from(e).context(format!("reading file {id}"))))?
        }
        Err(e) => return Err(Err(anyhow::Error::from(e).context(format!("reading file {id}")))),
    };
    let max_pixels = service.config.thumbnail_max_source_pixels;
    match tokio::task::spawn_blocking(move || thumbs::render(&source, spec, max_pixels)).await {
        Ok(Ok(bytes)) => Ok(Bytes::from(bytes)),
        Ok(Err(e)) => Err(Ok(e)),
        Err(e) => Err(Err(anyhow::anyhow!("rendering a thumbnail of {id} failed: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        use RangeRequest::*;
        assert_eq!(parse_range("bytes=0-9", 100), Partial(0..10));
        assert_eq!(parse_range("bytes=90-200", 100), Partial(90..100));
        assert_eq!(parse_range("bytes=95-", 100), Partial(95..100));
        assert_eq!(parse_range("bytes=-10", 100), Partial(90..100));
        assert_eq!(parse_range("bytes=-500", 100), Partial(0..100));
        assert_eq!(parse_range("bytes=100-", 100), Unsatisfiable);
        assert_eq!(parse_range("bytes=100-200", 100), Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 100), Unsatisfiable);
        assert_eq!(parse_range("bytes=0-", 0), Unsatisfiable);
        assert_eq!(parse_range("bytes=0-1,5-6", 100), Multiple(vec![0..2, 5..7]));
        // Unsatisfiable ranges in a set are dropped; overlapping and adjacent ones coalesce, in ascending order.
        assert_eq!(parse_range("bytes=0-1, 500-600", 100), Partial(0..2));
        assert_eq!(parse_range("bytes=500-600,700-", 100), Unsatisfiable);
        assert_eq!(parse_range("bytes=0-9,5-14", 100), Partial(0..15));
        assert_eq!(parse_range("bytes=0-4,5-9", 100), Partial(0..10));
        assert_eq!(parse_range("bytes=50-59, 0-4, 3-6, -5", 100), Multiple(vec![0..7, 50..60, 95..100]));
        assert_eq!(parse_range("bytes=0-1,,5-6,", 100), Multiple(vec![0..2, 5..7]), "empty elements are ignored");
        assert_eq!(parse_range("bytes=0-1,x-6", 100), Full, "one malformed spec invalidates the header");
        let many: Vec<String> = (0..17).map(|i| format!("{}-{}", i * 2, i * 2)).collect();
        assert_eq!(parse_range(&format!("bytes={}", many.join(",")), 100), Full, "more than MAX_RANGES");
        let max: Vec<String> = (0..16).map(|i| format!("{}-{}", i * 2, i * 2)).collect();
        assert!(matches!(parse_range(&format!("bytes={}", max.join(",")), 100), Multiple(r) if r.len() == 16));
        assert_eq!(parse_range("bytes=9-1", 100), Full);
        assert_eq!(parse_range("items=0-1", 100), Full);
        assert_eq!(parse_range("bytes=x-1", 100), Full);
        assert_eq!(parse_range("bytes=+1-2", 100), Full);
    }

    #[test]
    fn content_types_and_names() {
        assert_eq!(validate_content_type(" image/png ").unwrap(), "image/png");
        assert!(validate_content_type("text/plain; charset=utf-8").is_ok());
        assert!(validate_content_type("png").is_err());
        assert!(validate_content_type("text/\u{e9}").is_err());
        assert!(validate_content_type("a b/c").is_err());
        assert!(
            inline_safe(Some("IMAGE/PNG"))
                && inline_safe(Some("video/mp4"))
                && inline_safe(Some("text/plain; charset=utf-8"))
        );
        assert!(!inline_safe(Some("text/html")) && !inline_safe(Some("image/svg+xml")) && !inline_safe(None));
        assert!(!inline_safe(Some("application/xhtml+xml")) && !inline_safe(Some("text/javascript")));
        assert_eq!(sanitize_filename("C:\\Users\\me\\photo.png").as_deref(), Some("photo.png"));
        assert_eq!(sanitize_filename("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(sanitize_filename("a\nb.txt").as_deref(), Some("ab.txt"));
        assert_eq!(sanitize_filename(".."), None);
        assert_eq!(sanitize_filename("dir/"), None);
        assert_eq!(content_disposition(true, None), "inline");
        assert_eq!(
            content_disposition(false, Some("résumé \"v2\".pdf")),
            "attachment; filename=\"r_sum_ _v2_.pdf\"; filename*=UTF-8''r%C3%A9sum%C3%A9%20%22v2%22.pdf"
        );
    }
}
