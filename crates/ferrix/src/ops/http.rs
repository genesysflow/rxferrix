//! HTTP endpoints of the operations features. Every `/api/admin/*` endpoint requires an admin access token
//! (`Authorization: Bearer`, as in `crate::dashboard::http`): 401 `unauthenticated` / `invalid_token`, 403
//! `forbidden` for a user's token. Errors have the auth endpoints' shape; responses carry `Cache-Control: no-store`.
//!
//! - `GET /api/admin/backups` → `{ target, backups: BackupInfo[] }`, newest first.
//! - `POST /api/admin/backups` with `{ includeAux?, includeFiles? }` → `BackupInfo` (201), once the archive is stored.
//! - `GET /api/admin/backups/{name}` → the archive (`application/zstd`, attachment).
//! - `DELETE /api/admin/backups/{name}` → `{ deleted: true }`, or 404 `not_found`.
//! - `GET /api/admin/export/{table}?format=jsonl|csv` → the table, streamed from one consistent snapshot (secrets of
//!   system tables masked as in the dashboard).
//! - `POST /api/admin/import/{table}?mode=insert|upsert|replace` with a JSONL body (at most 256 MiB) →
//!   `ImportReport`. App tables only.
//! - `GET /metrics` → Prometheus text (404 unless enabled; bearer token if configured). See [`super::metrics`].

use std::io::Write;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use subtle::ConstantTimeEq;

use super::backup::{self, BackupSpec, BackupTarget};
use super::export::{self, ExportFormat, ImportMode, ImportOptions};
use crate::auth::AuthFailure;
use crate::dashboard::http::require_admin;
use crate::runtime::App;

/// Largest accepted import body.
pub const MAX_IMPORT_BYTES: u64 = 256 << 20;

/// The routes, merged into the main router by `server::router`.
pub fn routes() -> Router<App> {
    Router::new()
        .route("/metrics", get(metrics))
        .route("/api/admin/backups", get(list_backups).post(create_backup))
        .route("/api/admin/backups/{name}", get(download_backup).delete(delete_backup))
        .route("/api/admin/export/{table}", get(export_table))
        .route("/api/admin/import/{table}", post(import_table))
}

fn no_store() -> [(header::HeaderName, &'static str); 1] {
    [(header::CACHE_CONTROL, "no-store")]
}

/// A failure as a response: user errors are 400 with their code, anything else 500 with the full message (only
/// admins reach these endpoints).
fn failure(e: crate::Error) -> Response {
    if let Some(u) = e.chain().find_map(|c| c.downcast_ref::<crate::UserError>()) {
        let status = if u.code == "not_found" { 404 } else { 400 };
        return AuthFailure::new(status, &u.code, u.message.clone()).into_response();
    }
    tracing::error!("admin operation failed: {e:#}");
    AuthFailure::new(500, "server_error", format!("{e:#}")).into_response()
}

fn not_found(what: impl Into<String>) -> Response {
    AuthFailure::new(404, "not_found", what).into_response()
}

// ---------------------------------------------------------------------------------------------------------------------
// /metrics
// ---------------------------------------------------------------------------------------------------------------------

async fn metrics(State(app): State<App>, headers: HeaderMap) -> Response {
    let options = &app.options().ops.metrics;
    if !options.enabled {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    if let Some(token) = options.token.as_deref().filter(|t| !t.is_empty()) {
        let given = crate::auth::http::bearer_token(&headers).unwrap_or("");
        if !bool::from(given.as_bytes().ct_eq(token.as_bytes())) {
            return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], "unauthorized").into_response();
        }
    }
    match super::metrics::render(&app).await {
        Ok(text) => (
            [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8"), (header::CACHE_CONTROL, "no-store")],
            text,
        )
            .into_response(),
        Err(e) => {
            tracing::error!("rendering metrics failed: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, "metrics failed").into_response()
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Backups
// ---------------------------------------------------------------------------------------------------------------------

macro_rules! admin {
    ($app:expr, $headers:expr) => {
        if let Err(f) = require_admin(&$app, &$headers).await {
            return f.into_response();
        }
    };
}

macro_rules! try_or {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return failure(e.into()),
        }
    };
}

async fn list_backups(State(app): State<App>, headers: HeaderMap) -> Response {
    admin!(app, headers);
    let target = try_or!(BackupTarget::for_app(&app));
    let backups = try_or!(target.list().await);
    (no_store(), axum::Json(json!({ "target": target.location, "backups": backups }))).into_response()
}

async fn create_backup(State(app): State<App>, headers: HeaderMap, body: Bytes) -> Response {
    admin!(app, headers);
    let spec: BackupSpec = if body.is_empty() {
        BackupSpec::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(s) => s,
            Err(e) => return AuthFailure::new(400, "invalid_request", format!("Invalid body: {e}")).into_response(),
        }
    };
    if spec.include_secret {
        return AuthFailure::new(
            400,
            "invalid_request",
            "auth_secret.key is never included in backups made over HTTP; use `ferrix backup --include-secret`",
        )
        .into_response();
    }
    let target = try_or!(BackupTarget::for_app(&app));
    let info = try_or!(backup::create(&app, &spec, &target, false).await);
    (StatusCode::CREATED, no_store(), axum::Json(info)).into_response()
}

async fn download_backup(State(app): State<App>, headers: HeaderMap, Path(name): Path<String>) -> Response {
    admin!(app, headers);
    let target = try_or!(BackupTarget::for_app(&app));
    let Some((size, stream)) = try_or!(target.open(&name).await) else {
        return not_found(format!("Backup {name} does not exist"));
    };
    let body = Body::from_stream(stream.map(|r| r.map_err(std::io::Error::other)));
    let mut response = Response::new(body);
    let h = response.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/zstd"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    response
}

async fn delete_backup(State(app): State<App>, headers: HeaderMap, Path(name): Path<String>) -> Response {
    admin!(app, headers);
    let target = try_or!(BackupTarget::for_app(&app));
    if try_or!(target.delete(&name).await) {
        (no_store(), axum::Json(json!({ "deleted": true }))).into_response()
    } else {
        not_found(format!("Backup {name} does not exist"))
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Export and import
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
struct ExportQuery {
    format: Option<ExportFormat>,
}

/// A `Write` that sends 64 KiB chunks into a channel (from a blocking task to a streamed response body).
struct ChannelWriter {
    tx: tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    buf: Vec<u8>,
}

impl Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= 64 * 1024 {
            self.flush()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            let chunk = Bytes::from(std::mem::take(&mut self.buf));
            self.tx.blocking_send(Ok(chunk)).map_err(|_| std::io::Error::other("client went away"))?;
        }
        Ok(())
    }
}

async fn export_table(
    State(app): State<App>,
    headers: HeaderMap,
    Path(table): Path<String>,
    q: Result<Query<ExportQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    admin!(app, headers);
    let Ok(Query(q)) = q else {
        return AuthFailure::new(400, "invalid_request", "format must be jsonl or csv").into_response();
    };
    let format = q.format.unwrap_or_default();
    let Ok(info) = app.db().catalog().table(&table).cloned() else {
        return not_found(format!("Table `{table}` does not exist"));
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    let dir = app.dir().to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut out = ChannelWriter { tx: tx.clone(), buf: Vec::new() };
        let name = info.name.clone();
        let mask = move |doc: serde_json::Value| crate::dashboard::secrets::mask(&name, doc);
        let transform: Option<&dyn Fn(serde_json::Value) -> serde_json::Value> =
            if crate::dashboard::secrets::is_system_table(&info.name) { Some(&mask) } else { None };
        let result = export::ReadTxn::open(&dir)
            .and_then(|txn| export::write_table(txn.conn(), info.number, format, transform, &mut out))
            .and_then(|_| Ok(out.flush()?));
        if let Err(e) = result {
            tracing::warn!("export of `{}` failed: {e:#}", info.name);
            let _ = tx.blocking_send(Err(std::io::Error::other(format!("{e:#}"))));
        }
    });
    let stream = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
    let mut response = Response::new(Body::from_stream(stream));
    let h = response.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(format.content_type()));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    let file = format!("{table}.{}", format.extension());
    let safe: String = file.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')).collect();
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{safe}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    response
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ImportQuery {
    mode: Option<ImportMode>,
}

async fn import_table(
    State(app): State<App>,
    headers: HeaderMap,
    Path(table): Path<String>,
    q: Result<Query<ImportQuery>, axum::extract::rejection::QueryRejection>,
    body: Body,
) -> Response {
    admin!(app, headers);
    let Ok(Query(q)) = q else {
        return AuthFailure::new(400, "invalid_request", "mode must be insert, upsert or replace").into_response();
    };
    if table.starts_with('_') {
        return AuthFailure::new(400, "read_only_table", format!("`{table}` is a system table; it can't be imported"))
            .into_response();
    }
    if app.db().catalog().table(&table).is_err() {
        return not_found(format!("Table `{table}` does not exist"));
    }
    // Spool the body to a file (the import reads it twice: validation, then writes).
    let spool = try_or!(tempfile::Builder::new().prefix(".ferrix-import-").tempfile_in(app.dir()));
    let mut file = try_or!(tokio::fs::File::create(spool.path()).await);
    let mut stream = body.into_data_stream();
    let mut size = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => return AuthFailure::new(400, "invalid_request", format!("reading the body failed: {e}")).into_response(),
        };
        size += chunk.len() as u64;
        if size > MAX_IMPORT_BYTES {
            return AuthFailure::new(413, "too_large", format!("imports over HTTP are limited to {} MiB", MAX_IMPORT_BYTES >> 20))
                .into_response();
        }
        try_or!(tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await);
    }
    try_or!(tokio::io::AsyncWriteExt::flush(&mut file).await);
    drop(file);
    let options = ImportOptions { mode: q.mode.unwrap_or_default(), ..Default::default() };
    let mut report = export::ImportReport::default();
    let result =
        export::import_file(&app, &table, spool.path(), None, &export::IdRemap::default(), &options, &mut report).await;
    match result {
        Ok(done) => {
            report.tables.push(done);
            (no_store(), axum::Json(report)).into_response()
        }
        Err(e) => AuthFailure::new(400, "import_failed", format!("{e:#}")).into_response(),
    }
}
