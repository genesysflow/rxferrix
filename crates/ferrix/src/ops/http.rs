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
//! - `POST /api/admin/import/{table}?mode=insert|upsert|replace&format=jsonl|csv` with a JSONL or CSV body (at most
//!   256 MiB) → `ImportReport`. App tables only.
//! - `GET /api/admin/olap` → `{ enabled: false }` or `{ enabled: true, ...OlapStatus }` (`crate::olap`);
//!   `POST /api/admin/olap/rebuild` → 202: every table is rebuilt from a snapshot in the background;
//!   `POST /api/admin/olap/query` `{ sql, params?, format? }` → an `OlapResult` (`format` json, the default), or the
//!   whole result as a CSV or Parquet download (`format` csv / parquet); the same sandbox as `ctx.olap`, audited.
//! - `GET /api/admin/replication` → `{ enabled: false }` or `{ enabled: true, ...ReplicationStatus }`
//!   (`super::replicate`); `POST /api/admin/replication/snapshot` → `SnapshotInfo` (201).
//! - `POST /api/admin/restore/point-in-time` with `{ toTs?, toTime?, generation? }` → `{ pendingRestore, restart }`:
//!   a point-in-time restore from the replication target, checked and staged like an archive.
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
use super::restore;
use crate::audit;
use crate::auth::{Admin, AuthFailure};
use crate::id::Id;
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
        .route("/api/admin/backups/{name}/restore", post(stage_backup))
        .route("/api/admin/restore", get(get_restore).post(stage_upload).delete(cancel_restore))
        .route("/api/admin/restore/restart", post(restart))
        .route("/api/admin/restore/point-in-time", post(stage_point_in_time))
        .route("/api/admin/olap", get(olap_status))
        .route("/api/admin/olap/rebuild", post(olap_rebuild))
        .route("/api/admin/olap/query", post(olap_query))
        .route("/api/admin/replication", get(replication_status))
        .route("/api/admin/replication/snapshot", post(replication_snapshot))
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
        let status = match u.code.as_str() {
            "not_found" => 404,
            "restore_pending" | "no_pending_restore" => 409,
            "replication_off" | "olap_off" => 404,
            _ => 400,
        };
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

/// The pending restore and whether the server can restart itself, as `GET /api/admin/backups` and
/// `GET /api/admin/restore` report them.
fn restore_state(app: &App) -> crate::Result<serde_json::Value> {
    let restart = &app.options().ops.restart;
    Ok(json!({
        "pendingRestore": restore::pending(app.dir())?.map(restore::PendingRestore::public),
        "restart": { "available": restart.exit_code().is_some(), "exitCode": restart.exit_code() },
    }))
}

async fn list_backups(State(app): State<App>, headers: HeaderMap) -> Response {
    admin!(app, headers);
    let target = try_or!(BackupTarget::for_app(&app));
    let backups = try_or!(target.list().await);
    let mut body = try_or!(restore_state(&app));
    body["target"] = json!(target.location);
    body["backups"] = json!(backups);
    body["replication"] = match app.replication() {
        Some(r) => json!(r.status(&app).await),
        None => serde_json::Value::Null,
    };
    (no_store(), axum::Json(body)).into_response()
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
// Staged restores (`super::restore`)
// ---------------------------------------------------------------------------------------------------------------------

/// Largest archive accepted by `POST /api/admin/restore`.
pub const MAX_RESTORE_UPLOAD_BYTES: u64 = 64 << 30;

/// The administrator behind `headers`, or the refusal.
async fn admin_of(app: &App, headers: &HeaderMap) -> Result<Option<Id<Admin>>, Box<AuthFailure>> {
    match require_admin(app, headers).await {
        Ok(a) => Ok(a.identity.admin_id.as_deref().and_then(|id| id.parse().ok())),
        Err(f) => Err(f),
    }
}

/// Record an operations action in the audit log (best effort: the action already happened).
async fn audit_ops(app: &App, admin: Option<Id<Admin>>, action: &'static str, details: serde_json::Value) {
    let rec = audit::Record::new(action).details(details);
    if let Err(e) = crate::auth::store::write(app, move |db| async move { audit::record(&db, admin, rec).await }).await {
        tracing::warn!("recording `{action}` in the audit log failed: {e:#}");
    }
}

async fn staged(app: &App, admin: Option<Id<Admin>>, archive: &std::path::Path, source: &str) -> Response {
    let marker = restore::stage(app.dir(), archive, source, admin.map(|a| a.to_string())).await;
    staged_marker(app, admin, marker).await
}

async fn staged_marker(app: &App, admin: Option<Id<Admin>>, marker: crate::Result<restore::PendingRestore>) -> Response {
    let marker = match marker {
        Ok(m) => m,
        Err(e) => return failure(e),
    };
    let details = json!({
        "source": marker.source,
        "ts": marker.ts,
        "secretIgnored": marker.secret_ignored,
        "pointInTime": marker.point_in_time,
    });
    audit_ops(app, admin, "restore_stage", details).await;
    let mut body = try_or!(restore_state(app));
    body["pendingRestore"] = json!(marker.public());
    (no_store(), axum::Json(body)).into_response()
}

/// `POST /api/admin/backups/{name}/restore`: stage a backup of the target.
async fn stage_backup(State(app): State<App>, headers: HeaderMap, Path(name): Path<String>) -> Response {
    let admin = match admin_of(&app, &headers).await {
        Ok(a) => a,
        Err(f) => return f.into_response(),
    };
    try_or!(backup::validate_name(&name));
    let target = try_or!(BackupTarget::for_app(&app));
    let work = try_or!(tempfile::Builder::new().prefix(".ferrix-download-").tempdir_in(app.dir()));
    let local = work.path().join(&name);
    if try_or!(target.open(&name).await).is_none() {
        return not_found(format!("Backup {name} does not exist"));
    }
    try_or!(target.download(&name, &local).await);
    staged(&app, admin, &local, &name).await
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct UploadQuery {
    name: Option<String>,
}

/// `POST /api/admin/restore?name=<file name>` with a backup archive as the body: stage it.
async fn stage_upload(
    State(app): State<App>,
    headers: HeaderMap,
    q: Result<Query<UploadQuery>, axum::extract::rejection::QueryRejection>,
    body: Body,
) -> Response {
    let admin = match admin_of(&app, &headers).await {
        Ok(a) => a,
        Err(f) => return f.into_response(),
    };
    let name = q.ok().and_then(|Query(q)| q.name).unwrap_or_default();
    let mut source: String =
        name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')).take(200).collect();
    if source.is_empty() {
        source = "uploaded archive".into();
    }
    if restore::pending(app.dir()).ok().flatten().is_some() {
        return failure(crate::UserError::new("restore_pending", "A restore is staged already; cancel it first").into());
    }
    let spool = try_or!(tempfile::Builder::new().prefix(".ferrix-upload-").tempfile_in(app.dir()));
    let mut file = try_or!(tokio::fs::File::create(spool.path()).await);
    let mut stream = body.into_data_stream();
    let mut size = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => return AuthFailure::new(400, "invalid_request", format!("reading the body failed: {e}")).into_response(),
        };
        size += chunk.len() as u64;
        if size > MAX_RESTORE_UPLOAD_BYTES {
            let message = format!("uploaded archives are limited to {} GiB; use `ferrix restore`", MAX_RESTORE_UPLOAD_BYTES >> 30);
            return AuthFailure::new(413, "too_large", message).into_response();
        }
        try_or!(tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await);
    }
    try_or!(tokio::io::AsyncWriteExt::flush(&mut file).await);
    drop(file);
    if size == 0 {
        return AuthFailure::new(400, "invalid_request", "send the backup archive as the request body").into_response();
    }
    staged(&app, admin, spool.path(), &source).await
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct PointInTimeBody {
    to_ts: Option<u64>,
    /// RFC 3339, or ms since the epoch.
    to_time: Option<serde_json::Value>,
    generation: Option<String>,
}

fn replication_off() -> Response {
    let message = "Replication is off: start the server with --replicate-to <directory | s3://bucket/prefix>";
    AuthFailure::new(404, "replication_off", message).into_response()
}

/// `POST /api/admin/restore/point-in-time`: stage the state of a past commit (or time) from the replication target.
async fn stage_point_in_time(State(app): State<App>, headers: HeaderMap, body: Bytes) -> Response {
    let admin = match admin_of(&app, &headers).await {
        Ok(a) => a,
        Err(f) => return f.into_response(),
    };
    let Some(replicator) = app.replication().cloned() else { return replication_off() };
    let body: PointInTimeBody = if body.is_empty() {
        PointInTimeBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(b) => b,
            Err(e) => return AuthFailure::new(400, "invalid_request", format!("Invalid body: {e}")).into_response(),
        }
    };
    let to_time = match &body.to_time {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Number(n)) => n.as_f64(),
        Some(serde_json::Value::String(s)) => match super::parse_time(s) {
            Ok(t) => Some(t),
            Err(e) => return AuthFailure::new(400, "invalid_request", e).into_response(),
        },
        Some(_) => return AuthFailure::new(400, "invalid_request", "toTime must be an RFC 3339 string or ms").into_response(),
    };
    if restore::pending(app.dir()).ok().flatten().is_some() {
        return failure(crate::UserError::new("restore_pending", "A restore is staged already; cancel it first").into());
    }
    // The newest commits first, so "the newest point" includes them (best effort: the target may be down).
    if let Err(e) = replicator.flush(&app).await {
        tracing::warn!("replication: shipping before a point-in-time restore failed: {e:#}");
    }
    let point = super::replicate::PointInTime { to_ts: body.to_ts, to_time, generation: body.generation };
    let marker = restore::stage_point_in_time(app.dir(), replicator.target(), &point, admin.map(|a| a.to_string())).await;
    staged_marker(&app, admin, marker).await
}

/// `GET /api/admin/replication`.
async fn replication_status(State(app): State<App>, headers: HeaderMap) -> Response {
    admin!(app, headers);
    let body = match app.replication() {
        Some(r) => {
            let mut v = json!(r.status(&app).await);
            v["enabled"] = json!(true);
            v
        }
        None => json!({ "enabled": false }),
    };
    (no_store(), axum::Json(body)).into_response()
}

async fn olap_status(State(app): State<App>, headers: HeaderMap) -> Response {
    admin!(app, headers);
    let body = match app.olap() {
        Some(o) => {
            let mut v = json!(o.status(&app).await);
            v["enabled"] = json!(true);
            v
        }
        None => json!({ "enabled": false }),
    };
    (no_store(), axum::Json(body)).into_response()
}

/// `POST /api/admin/olap/rebuild`: rebuild the analytics copy from a snapshot, in the background (202).
async fn olap_rebuild(State(app): State<App>, headers: HeaderMap) -> Response {
    let admin = match admin_of(&app, &headers).await {
        Ok(a) => a,
        Err(f) => return f.into_response(),
    };
    let Some(olap) = app.olap().cloned() else {
        return AuthFailure::new(404, "olap_off", "Analytics are off: start the server with --olap").into_response();
    };
    olap.request_rebuild("an administrator asked for it");
    audit_ops(&app, admin, "olap_rebuild", json!({})).await;
    (StatusCode::ACCEPTED, no_store(), axum::Json(json!({ "rebuilding": true }))).into_response()
}

#[derive(Deserialize)]
struct OlapQueryBody {
    sql: String,
    #[serde(default)]
    params: Vec<serde_json::Value>,
    #[serde(default)]
    format: Option<String>,
}

/// `POST /api/admin/olap/query`: one SELECT over the analytics copy, as JSON or as a CSV / Parquet download.
async fn olap_query(State(app): State<App>, headers: HeaderMap, body: Bytes) -> Response {
    use crate::olap::{OlapExportFormat, OlapValue};
    let admin = match admin_of(&app, &headers).await {
        Ok(a) => a,
        Err(f) => return f.into_response(),
    };
    let Some(olap) = app.olap().cloned() else {
        return AuthFailure::new(404, "olap_off", "Analytics are off: start the server with --olap").into_response();
    };
    let req: OlapQueryBody = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return AuthFailure::new(400, "invalid_request", format!("Expected {{ sql, params?, format? }}: {e}")).into_response(),
    };
    let params: Vec<OlapValue> = req.params.into_iter().map(OlapValue::from_json).collect();
    let logged: String = req.sql.chars().take(2000).collect();
    let format = match req.format.as_deref().unwrap_or("json") {
        "json" => None,
        "csv" => Some(OlapExportFormat::Csv),
        "parquet" => Some(OlapExportFormat::Parquet),
        other => {
            return AuthFailure::new(400, "invalid_request", format!("Unknown format `{other}` (json, csv or parquet)")).into_response();
        }
    };
    let Some(format) = format else {
        let result = try_or!(olap.admin_query(&req.sql, params).await);
        audit_ops(&app, admin, "olap_query", json!({ "sql": logged, "rows": result.rows.len() })).await;
        return (no_store(), axum::Json(result)).into_response();
    };
    // Written to a temporary file next to the data (deleted once sent), then streamed.
    let file = try_or!(
        tempfile::Builder::new()
            .prefix(".olap-export-")
            .suffix(&format!(".{}", format.extension()))
            .tempfile_in(app.dir())
            .map_err(anyhow::Error::from)
    );
    let export = try_or!(olap.export(&req.sql, params, format, file.path()).await);
    audit_ops(&app, admin, "olap_export", json!({ "sql": logged, "rows": export.rows, "format": format.extension() })).await;
    let reader = try_or!(tokio::fs::File::open(file.path()).await.map_err(anyhow::Error::from));
    let size = try_or!(reader.metadata().await.map_err(anyhow::Error::from)).len();
    let stream = futures::stream::unfold((reader, file, false), |(mut reader, file, done)| async move {
        use tokio::io::AsyncReadExt;
        if done {
            return None;
        }
        let mut buf = vec![0u8; 64 << 10];
        match reader.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(Bytes::from(buf)), (reader, file, false)))
            }
            Err(e) => Some((Err(e), (reader, file, true))),
        }
    });
    let mut response = Response::new(Body::from_stream(stream));
    let h = response.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(format.content_type()));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"query.{}\"", format.extension())) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert("x-ferrix-rows", HeaderValue::from(export.rows));
    h.insert("x-ferrix-as-of-ts", HeaderValue::from(export.as_of_ts));
    response
}

/// `POST /api/admin/replication/snapshot`: take a snapshot of the current generation now.
async fn replication_snapshot(State(app): State<App>, headers: HeaderMap) -> Response {
    let admin = match admin_of(&app, &headers).await {
        Ok(a) => a,
        Err(f) => return f.into_response(),
    };
    let Some(replicator) = app.replication().cloned() else { return replication_off() };
    let snapshot = try_or!(replicator.snapshot_now(&app).await);
    audit_ops(&app, admin, "replication_snapshot", json!({ "name": snapshot.name, "ts": snapshot.ts })).await;
    (StatusCode::CREATED, no_store(), axum::Json(snapshot)).into_response()
}

/// `GET /api/admin/restore`: `{ pendingRestore, restart }`.
async fn get_restore(State(app): State<App>, headers: HeaderMap) -> Response {
    admin!(app, headers);
    (no_store(), axum::Json(try_or!(restore_state(&app)))).into_response()
}

/// `DELETE /api/admin/restore`: cancel the pending restore.
async fn cancel_restore(State(app): State<App>, headers: HeaderMap) -> Response {
    let admin = match admin_of(&app, &headers).await {
        Ok(a) => a,
        Err(f) => return f.into_response(),
    };
    let before = try_or!(restore::pending(app.dir()));
    if !try_or!(restore::cancel(app.dir())) {
        return not_found("No restore is staged");
    }
    audit_ops(&app, admin, "restore_cancel", json!({ "source": before.map(|p| p.source) })).await;
    (no_store(), axum::Json(json!({ "canceled": true }))).into_response()
}

/// `POST /api/admin/restore/restart`: shut down gracefully and exit with `--restart-exit-code`, so the supervisor
/// starts the server again and the staged restore is applied.
async fn restart(State(app): State<App>, headers: HeaderMap) -> Response {
    let admin = match admin_of(&app, &headers).await {
        Ok(a) => a,
        Err(f) => return f.into_response(),
    };
    let handle = app.options().ops.restart.clone();
    let Some(code) = handle.exit_code() else {
        let message = "This server can't restart itself (start it with --restart-exit-code under systemd or Docker); \
                       restart it by hand to apply the restore";
        return AuthFailure::new(400, "restart_unavailable", message).into_response();
    };
    match try_or!(restore::pending(app.dir())) {
        None => return failure(crate::UserError::new("no_pending_restore", "No restore is staged").into()),
        Some(p) if p.error.is_some() => {
            return AuthFailure::new(400, "invalid_backup", "The staged restore can't be applied; cancel it").into_response();
        }
        Some(_) => {}
    }
    audit_ops(&app, admin, "restore_restart", json!({ "exitCode": code })).await;
    tracing::warn!("restart requested by an administrator to apply the staged restore; exiting with code {code}");
    // Answer first; the shutdown lets open requests finish.
    let response = (StatusCode::ACCEPTED, no_store(), axum::Json(json!({ "restarting": true, "exitCode": code }))).into_response();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        handle.request();
    });
    response
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
    format: Option<ExportFormat>,
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
        return AuthFailure::new(400, "invalid_request", "mode must be insert, upsert or replace; format jsonl or csv")
            .into_response();
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
    let format = q.format.unwrap_or_default();
    let options = ImportOptions { mode: q.mode.unwrap_or_default(), format: Some(format), ..Default::default() };
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
