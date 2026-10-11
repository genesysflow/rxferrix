//! Admin-only HTTP endpoints for data that lives outside the reactive database: the execution log (`aux.db`) and
//! server stats.
//!
//! - `GET /api/admin/logs?<LogFilter>`: history, newest first. `{ "entries": [LogEntry] }`.
//! - `GET /api/admin/logs/stream?<LogFilter>`: live tail as Server-Sent Events (`event: ready`, then one
//!   `event: log` per entry with the `LogEntry` as JSON, `event: lagged` with the number of skipped entries when the
//!   client fell behind, `event: expired` right before the stream ends at the access token's expiry, and
//!   `event: revoked` when it ends because the admin's session ended: signed out or revoked).
//! - `GET /api/admin/stats?since=<ms>`: per-function stats since `since` (default: the last 24 hours), an overview,
//!   and the analytics copy's status (`olap`, null without `--olap`).
//! - `GET /api/admin/stats/timeseries?path=<fn>&bucket=<1m|15m|1h|ms>&since=<ms>&until=<ms>`: calls, errors and
//!   p50/p95/max duration per time bucket for one function (or every app function without `path`), oldest first,
//!   only buckets with executions: `{ path, bucket, since, until, points: [TimePoint] }`. Defaults: the last hour in
//!   1-minute buckets; at most 1500 buckets (400 `invalid_request` otherwise).
//!
//! **Authentication**: every endpoint requires `Authorization: Bearer <access token>` whose `adm` claim is set
//! (an admin session): 401 `unauthenticated` without a token, 401 `invalid_token` for a bad, expired or revoked one,
//! 403 `forbidden` for a user token. This includes the SSE stream: the dashboard reads it with `fetch()` and a
//! streamed body rather than `EventSource` (which can't send headers), so no token ever appears in a URL (and thus
//! in proxy or server logs). The stream ends when the access token expires; the client reconnects with a fresh one.
//!
//! `LogFilter` query parameters: `pathPrefix`, `path` (one function), `kind` (`query|mutation|action`), `outcome`
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

use crate::auth::{AuthFailure, Authenticated, SessionWatch};
use crate::exec_log::LogFilter;
use crate::runtime::App;

/// The `/api/admin/*` routes, merged into the main router by `server::router`.
pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/admin/logs", get(logs))
        .route("/api/admin/logs/stream", get(logs_stream))
        .route("/api/admin/stats", get(stats))
        .route("/api/admin/stats/timeseries", get(timeseries))
        .route("/api/admin/impersonate", axum::routing::post(super::impersonate::impersonate))
}

fn no_store() -> [(header::HeaderName, &'static str); 1] {
    [(header::CACHE_CONTROL, "no-store")]
}

/// The caller's admin session, or the failure (401 without or with a bad token, 403 for a user token).
pub async fn require_admin(app: &App, headers: &HeaderMap) -> Result<Authenticated, Box<AuthFailure>> {
    require_admin_watched(app, headers, false).await.map(|(a, _)| a)
}

/// [`require_admin`], plus (with `watch`) a watch on the admin's session for streams that must end when it does.
pub(crate) async fn require_admin_watched(
    app: &App,
    headers: &HeaderMap,
    watch: bool,
) -> Result<(Authenticated, Option<SessionWatch>), Box<AuthFailure>> {
    let Some(token) = crate::auth::http::bearer_token(headers) else {
        return Err(Box::new(AuthFailure::new(
            401,
            crate::auth::UNAUTHENTICATED,
            "An administrator access token is required",
        )));
    };
    let failure = |e| Box::new(crate::auth::http::token_failure(e));
    let (auth, session) = if watch {
        app.auth().authenticate_watched(app, token).await.map_err(failure)?
    } else {
        (app.auth().authenticate(app, token).await.map_err(failure)?, None)
    };
    if auth.identity.admin_id.is_none() {
        return Err(Box::new(AuthFailure::new(403, crate::auth::FORBIDDEN, "Only administrators may do this")));
    }
    Ok((auth, session))
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
    let (auth, session) = match require_admin_watched(&app, &headers, true).await {
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
        /// Ends the stream (`event: revoked`) as soon as the admin's session ends.
        session: Option<SessionWatch>,
        done: bool,
    }
    let tail = Tail { rx: app.logs().subscribe(), filter, expires, session, done: false };
    let entries = futures::stream::unfold(tail, |mut t| async move {
        loop {
            if t.done {
                return None;
            }
            let revoked = t.session.as_ref().map(|s| s.ended());
            let event = tokio::select! {
                _ = tokio::time::sleep_until(t.expires) => {
                    t.done = true;
                    Event::default().event("expired").data("access token expired")
                }
                _ = async { match revoked { Some(r) => r.await, None => std::future::pending().await } } => {
                    t.done = true;
                    Event::default().event("revoked").data(crate::auth::revocation::SESSION_ENDED)
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
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store"), (header::HeaderName::from_static("x-accel-buffering"), "no")],
        sse,
    )
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
        let documents: i64 =
            snapshot.with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))?))?;
        drop(snapshot);
        let dir = app.dir();
        let files: Vec<_> = ["data.db", "data.db-wal", "aux.db", "aux.db-wal"]
            .iter()
            .map(|name| json!({ "name": name, "bytes": file_size(&dir.join(name)) }))
            .collect();
        let metrics = app.metrics();
        let subscriptions = metrics.subscription_stats();
        let (queue_depth, running_jobs) = crate::ops::metrics::scheduler_counts(&app).await?;
        let commits_per_second = app.db().write_log().commits_within(Duration::from_secs(10)) as f64 / 10.0;
        let olap = match app.olap() {
            Some(o) => json!(o.status(&app).await),
            None => serde_json::Value::Null,
        };
        Ok(json!({
            "since": since,
            "functions": functions,
            "olap": olap,
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
                "sharedQueries": metrics.shared_queries(),
                "droppedLogEntries": app.logs().dropped(),
                "files": files,
                "commitsPerSecond": commits_per_second,
                "queueDepth": queue_depth,
                "runningJobs": running_jobs,
                "subscriptionRounds": subscriptions.rounds,
                "queryReruns": subscriptions.reruns,
                "cachedQueries": subscriptions.idle_queries,
                "cacheBytes": subscriptions.idle_bytes,
                "cacheHits": subscriptions.cache_hits,
                "cacheMisses": subscriptions.cache_misses,
                "metricsEnabled": app.options().ops.metrics.enabled,
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

#[derive(Deserialize, Default)]
#[serde(default)]
struct TimeseriesQuery {
    path: Option<String>,
    bucket: Option<String>,
    since: Option<i64>,
    until: Option<i64>,
}

/// A bucket size: milliseconds (`60000`) or a number with a unit (`30s`, `1m`, `15m`, `1h`, `1d`).
fn parse_bucket(s: &str) -> Option<i64> {
    let s = s.trim();
    let (digits, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = digits.parse().ok().filter(|n| *n > 0)?;
    let ms = match unit {
        "" | "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    n.checked_mul(ms)
}

async fn timeseries(
    State(app): State<App>,
    headers: HeaderMap,
    q: Result<Query<TimeseriesQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(failure) = require_admin(&app, &headers).await {
        return failure.into_response();
    }
    let Query(q) = match q {
        Ok(q) => q,
        Err(e) => return bad_query(e),
    };
    let Some(bucket_ms) = q.bucket.as_deref().map_or(Some(60_000), parse_bucket) else {
        return bad_query("`bucket` must be milliseconds or a duration such as 30s, 1m, 15m, 1h or 1d");
    };
    let until = q.until.unwrap_or(crate::db::wall_clock_ms() as i64 + 1);
    // Times before the epoch can't hold executions; clamping also keeps the bucket arithmetic in range.
    let since = q.since.unwrap_or(until - 3_600_000).max(0);
    let path = q.path.filter(|p| !p.is_empty());
    let spec = crate::exec_log::TimeseriesSpec { path: path.clone(), since, until, bucket_ms };
    if until <= since {
        return bad_query("`until` must be after `since`");
    }
    let buckets = crate::exec_log::bucket_count(since, until, bucket_ms);
    if buckets > crate::exec_log::MAX_TIMESERIES_BUCKETS as i128 {
        return bad_query(format!(
            "{buckets} buckets requested; at most {} (use a larger bucket or a shorter range)",
            crate::exec_log::MAX_TIMESERIES_BUCKETS
        ));
    }
    match app.logs().timeseries(spec).await {
        Ok(points) => (
            no_store(),
            axum::Json(json!({
                "path": path,
                "bucket": bucket_ms,
                "since": since.div_euclid(bucket_ms) * bucket_ms,
                "until": until,
                "points": points,
            })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("admin timeseries failed: {e:#}");
            AuthFailure::new(500, "server_error", "Server Error").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn bucket_sizes() {
        use super::parse_bucket;
        assert_eq!(parse_bucket("60000"), Some(60_000));
        assert_eq!(parse_bucket("250ms"), Some(250));
        assert_eq!(parse_bucket("30s"), Some(30_000));
        assert_eq!(parse_bucket("15m"), Some(900_000));
        assert_eq!(parse_bucket("2h"), Some(7_200_000));
        assert_eq!(parse_bucket("1d"), Some(86_400_000));
        for bad in ["", "0", "m", "5x", "-1m", "1.5m", "99999999999999999999"] {
            assert_eq!(parse_bucket(bad), None, "{bad}");
        }
    }
}
