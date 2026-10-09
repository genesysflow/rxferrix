//! Admin-only HTTP endpoints for data that lives outside the reactive database: the execution log (`aux.db`) and
//! server stats.
//!
//! - `GET /api/admin/logs?<LogFilter>`: history, newest first. `{ "entries": [LogEntry] }`.
//! - `GET /api/admin/logs/stream?<LogFilter>`: live tail as Server-Sent Events (`event: ready`, then one
//!   `event: log` per entry with the `LogEntry` as JSON, `event: lagged` with the number of skipped entries when the
//!   client fell behind, and `event: expired` right before the stream ends at the access token's expiry).
//! - `GET /api/admin/stats?since=<ms>`: per-function stats since `since` (default: the last 24 hours) and an overview.
//!
//! **Authentication**: every endpoint requires `Authorization: Bearer <access token>` whose `adm` claim is set
//! (an admin session): 401 `unauthenticated` without a token, 401 `invalid_token` for a bad, expired or revoked one,
//! 403 `forbidden` for a user token. This includes the SSE stream: the dashboard reads it with `fetch()` and a
//! streamed body rather than `EventSource` (which can't send headers), so no token ever appears in a URL (and thus
//! in proxy or server logs). The stream ends when the access token expires; the client reconnects with a fresh one.
//!
//! `LogFilter` query parameters: `pathPrefix`, `kind` (`query|mutation|action`), `outcome`
//! (`success|userError|serverError`), `since`, `limit`, `beforeId`, `identity`, `excludeSystem`.

use std::convert::Infallible;
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::auth::{AuthFailure, Authenticated};
use crate::exec_log::LogFilter;
use crate::runtime::App;

/// The `/api/admin/*` routes, merged into the main router by `server::router`.
pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/admin/logs", get(logs))
        .route("/api/admin/logs/stream", get(logs_stream))
        .route("/api/admin/stats", get(stats))
}

fn no_store() -> [(header::HeaderName, &'static str); 1] {
    [(header::CACHE_CONTROL, "no-store")]
}

/// The caller's admin session, or the failure (401 without or with a bad token, 403 for a user token).
pub async fn require_admin(app: &App, headers: &HeaderMap) -> Result<Authenticated, Box<AuthFailure>> {
    let Some(token) = crate::auth::http::bearer_token(headers) else {
        return Err(Box::new(AuthFailure::new(401, crate::auth::UNAUTHENTICATED, "An administrator access token is required")));
    };
    let auth = app.auth().authenticate(app, token).await.map_err(|e| Box::new(crate::auth::http::token_failure(e)))?;
    if auth.identity.admin_id.is_none() {
        return Err(Box::new(AuthFailure::new(403, crate::auth::FORBIDDEN, "Only administrators may do this")));
    }
    Ok(auth)
}

fn bad_query(e: impl std::fmt::Display) -> Response {
    AuthFailure::new(400, "invalid_request", format!("Invalid query string: {e}")).into_response()
}

async fn logs(
    State(app): State<App>,
    headers: HeaderMap,
    filter: Result<Query<LogFilter>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(failure) = require_admin(&app, &headers).await {
        return failure.into_response();
    }
    let Query(filter) = match filter {
        Ok(f) => f,
        Err(e) => return bad_query(e),
    };
    match app.logs().query(filter).await {
        Ok(entries) => (no_store(), axum::Json(json!({ "entries": entries }))).into_response(),
        Err(e) => {
            tracing::error!("admin logs query failed: {e:#}");
            AuthFailure::new(500, "server_error", "Server Error").into_response()
        }
    }
}

async fn logs_stream(
    State(app): State<App>,
    headers: HeaderMap,
    filter: Result<Query<LogFilter>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let auth = match require_admin(&app, &headers).await {
        Ok(a) => a,
        Err(failure) => return failure.into_response(),
    };
    let Query(filter) = match filter {
        Ok(f) => f,
        Err(e) => return bad_query(e),
    };
    let left = auth.expires_at.duration_since(SystemTime::now()).unwrap_or_default();
    let expires = tokio::time::Instant::now() + left;
    struct Tail {
        rx: tokio::sync::broadcast::Receiver<crate::LogEntry>,
        filter: LogFilter,
        expires: tokio::time::Instant,
        done: bool,
    }
    let tail = Tail { rx: app.logs().subscribe(), filter, expires, done: false };
    let entries = futures::stream::unfold(tail, |mut t| async move {
        loop {
            if t.done {
                return None;
            }
            let event = tokio::select! {
                _ = tokio::time::sleep_until(t.expires) => {
                    t.done = true;
                    Event::default().event("expired").data("access token expired")
                }
                received = t.rx.recv() => match received {
                    Ok(entry) if t.filter.matches(&entry) => match Event::default().event("log").json_data(&entry) {
                        Ok(e) => e,
                        Err(_) => continue,
                    },
                    Ok(_) => continue,
                    Err(RecvError::Lagged(n)) => Event::default().event("lagged").data(n.to_string()),
                    Err(RecvError::Closed) => return None,
                }
            };
            return Some((Ok::<_, Infallible>(event), t));
        }
    });
    let ready = futures::stream::once(async { Ok::<_, Infallible>(Event::default().event("ready").data("{}")) });
    let sse = Sse::new(ready.chain(entries)).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)));
    // `X-Accel-Buffering: no` keeps nginx-style proxies from buffering the stream.
    (StatusCode::OK, [(header::CACHE_CONTROL, "no-store"), (header::HeaderName::from_static("x-accel-buffering"), "no")], sse)
        .into_response()
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StatsQuery {
    since: Option<i64>,
}

fn file_size(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

async fn stats(
    State(app): State<App>,
    headers: HeaderMap,
    q: Result<Query<StatsQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(failure) = require_admin(&app, &headers).await {
        return failure.into_response();
    }
    let Query(q) = match q {
        Ok(q) => q,
        Err(e) => return bad_query(e),
    };
    let now = crate::db::wall_clock_ms();
    let since = q.since.unwrap_or(now as i64 - 24 * 3600 * 1000);
    let result: crate::Result<serde_json::Value> = async {
        let functions = app.logs().stats(since).await?;
        let snapshot = app.db().snapshot().await?;
        let (ts, commit_time) = (snapshot.ts, snapshot.time);
        let documents: i64 = snapshot.with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))?))?;
        drop(snapshot);
        let dir = app.dir();
        let files: Vec<_> = ["data.db", "data.db-wal", "aux.db", "aux.db-wal"]
            .iter()
            .map(|name| json!({ "name": name, "bytes": file_size(&dir.join(name)) }))
            .collect();
        let metrics = app.metrics();
        Ok(json!({
            "since": since,
            "functions": functions,
            "overview": {
                "now": now,
                "startedAt": app.started_at(),
                "version": env!("CARGO_PKG_VERSION"),
                "dev": app.options().dev,
                "ts": ts,
                "commitTime": commit_time,
                "tables": app.db().catalog().tables().count(),
                "documents": documents,
                "registeredFunctions": app.registry().list().len(),
                "sockets": metrics.sockets(),
                "subscriptions": metrics.subscriptions(),
                "droppedLogEntries": app.logs().dropped(),
                "files": files,
            },
        }))
    }
    .await;
    match result {
        Ok(body) => (no_store(), axum::Json(body)).into_response(),
        Err(e) => {
            tracing::error!("admin stats failed: {e:#}");
            AuthFailure::new(500, "server_error", "Server Error").into_response()
        }
    }
}
