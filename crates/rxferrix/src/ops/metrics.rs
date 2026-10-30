//! Prometheus metrics: `GET /metrics` in the text exposition format (version 0.0.4).
//!
//! Off by default: `serve --metrics` (or `AppOptions::ops.metrics.enabled`) turns it on, and `--metrics-token <T>`
//! (env `RXFERRIX_METRICS_TOKEN`) requires `Authorization: Bearer <T>`. While off, `/metrics` answers 404 and nothing
//! is counted.
//!
//! | Metric | Type | Labels |
//! |---|---|---|
//! | `rxferrix_build_info` | gauge (1) | `version` |
//! | `rxferrix_start_time_seconds` | gauge | |
//! | `rxferrix_http_requests_total` | counter | `route` (the matched route, e.g. `/api/storage/{id}`; `fallback` for static files and unmatched paths), `status` |
//! | `rxferrix_function_executions_total` | counter | `path`, `kind` (`query`, `mutation`, `action`), `outcome` (`success`, `userError`, `serverError`) |
//! | `rxferrix_function_duration_seconds` | histogram | `path`, `kind` |
//! | `rxferrix_commit_ts` | gauge | the latest commit ts |
//! | `rxferrix_commits_total` | counter | write transactions committed since the database was created (= the commit ts; `rate()` it) |
//! | `rxferrix_commits_per_second` | gauge | commits per second over the last 10 seconds |
//! | `rxferrix_websocket_connections` | gauge | open WebSockets |
//! | `rxferrix_sync_connections` | gauge | connections known to the subscription manager |
//! | `rxferrix_subscriptions` | gauge | live `(connection, queryId)` subscriptions |
//! | `rxferrix_shared_queries` | gauge | distinct queries the server reruns |
//! | `rxferrix_subscription_rounds_total`, `rxferrix_query_reruns_total` | counter | invalidation rounds and query reruns |
//! | `rxferrix_query_cache_entries`, `rxferrix_query_cache_bytes` | gauge | cached results nobody watches; their size |
//! | `rxferrix_query_jobs` | gauge | query runs queued or running |
//! | `rxferrix_query_cache_hits_total`, `_misses_total` | counter | subscriptions, HTTP queries, `ctx.run_query`: cached, or not |
//! | `rxferrix_query_cache_evictions_total` | counter | cached results dropped for the memory budget or their age |
//! | `rxferrix_db_file_bytes` | gauge | `file` (`data.db`, `data.db-wal`, `aux.db`, `aux.db-wal`) |
//! | `rxferrix_scheduler_queue_depth` | gauge | pending scheduled jobs (due or not) |
//! | `rxferrix_scheduler_running_jobs` | gauge | actions in progress |
//! | `rxferrix_exec_log_dropped_total` | counter | execution log entries dropped because the log writer fell behind |
//!
//! With `--replicate-to` (`super::replicate`):
//!
//! | Metric | Type | |
//! |---|---|---|
//! | `rxferrix_replication_shipped_ts` | gauge | the newest commit ts stored in the replication target |
//! | `rxferrix_replication_lag_commits` | gauge | commits not shipped yet |
//! | `rxferrix_replication_lag_seconds` | gauge | age of the oldest unshipped commit (0 when caught up) |
//! | `rxferrix_replication_pending_bytes` | gauge | journal bytes kept locally |
//! | `rxferrix_replication_journaling` | gauge | 1 while commits are journaled (0 after the journal was dropped) |
//! | `rxferrix_replication_failures` | gauge | consecutive failed attempts (0 when healthy) |
//! | `rxferrix_replication_segments_shipped_total`, `rxferrix_replication_snapshots_total` | counter | since the start |
//! | `rxferrix_replication_last_snapshot_timestamp_seconds` | gauge | when the newest snapshot of the current generation was taken |
//! | `rxferrix_olap_ready` | gauge | 1 while the analytics copy answers queries (`--olap`) |
//! | `rxferrix_olap_applied_ts`, `rxferrix_olap_lag_commits`, `rxferrix_olap_lag_seconds` | gauge | how far the copy is behind |
//! | `rxferrix_olap_size_bytes` | gauge | `olap.duckdb` and its WAL |
//! | `rxferrix_olap_rows_applied_total`, `rxferrix_olap_rebuilds_total` | counter | since the start |

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Duration;

use axum::extract::{MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;

use crate::Table as _;
use crate::exec_log::LogOutcome;
use crate::private::FunctionKind;
use crate::runtime::App;

/// `GET /metrics` settings (`AppOptions::ops.metrics`).
#[derive(Clone, Default)]
pub struct MetricsOptions {
    /// Serve `/metrics` and count requests and executions.
    pub enabled: bool,
    /// Require `Authorization: Bearer <token>`.
    pub token: Option<String>,
}

impl std::fmt::Debug for MetricsOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsOptions")
            .field("enabled", &self.enabled)
            .field("token", &self.token.as_ref().map(|_| "…"))
            .finish()
    }
}

/// Upper bounds (seconds) of the duration histogram's buckets.
const BUCKETS: [f64; 14] = [0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

#[derive(Debug, Default, Clone)]
struct Histogram {
    /// Non-cumulative counts per bucket, plus one for +Inf.
    counts: [u64; BUCKETS.len() + 1],
    sum: f64,
    count: u64,
}

impl Histogram {
    fn observe(&mut self, seconds: f64) {
        let i = BUCKETS.iter().position(|b| seconds <= *b).unwrap_or(BUCKETS.len());
        self.counts[i] += 1;
        self.sum += seconds;
        self.count += 1;
    }
}

/// Request and execution counters of an app (in `App::metrics()`), filled only while metrics are enabled.
#[derive(Debug, Default)]
pub struct Counters {
    requests: Mutex<HashMap<(String, u16), u64>>,
    executions: Mutex<HashMap<(String, &'static str, &'static str), u64>>,
    durations: Mutex<HashMap<(String, &'static str), Histogram>>,
}

fn outcome_label(outcome: LogOutcome) -> &'static str {
    match outcome {
        LogOutcome::Success => "success",
        LogOutcome::UserError => "userError",
        LogOutcome::ServerError => "serverError",
    }
}

impl Counters {
    pub(crate) fn record_request(&self, route: String, status: u16) {
        *self.requests.lock().unwrap_or_else(|e| e.into_inner()).entry((route, status)).or_default() += 1;
    }

    pub(crate) fn record_execution(&self, path: &str, kind: FunctionKind, outcome: LogOutcome, duration: Duration) {
        let kind = kind.as_str();
        *self
            .executions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry((path.to_string(), kind, outcome_label(outcome)))
            .or_default() += 1;
        self.durations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry((path.to_string(), kind))
            .or_default()
            .observe(duration.as_secs_f64());
    }
}

/// Count a finished execution (called by the runtime for every execution it logs).
pub(crate) fn record_execution(app: &App, path: &str, kind: FunctionKind, outcome: LogOutcome, duration: Duration) {
    if app.options().ops.metrics.enabled {
        app.metrics().ops.record_execution(path, kind, outcome, duration);
    }
}

/// Middleware counting requests by matched route and status.
pub async fn track_requests(State(app): State<App>, request: Request, next: Next) -> Response {
    let route = request.extensions().get::<MatchedPath>().map(|p| p.as_str().to_string());
    let response = next.run(request).await;
    app.metrics().ops.record_request(route.unwrap_or_else(|| "fallback".into()), response.status().as_u16());
    response
}

/// Escape a label value (backslash, double quote, newline).
fn label(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn header(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

fn file_size(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Pending and in-progress scheduled jobs, counted on the `(state, runAt, scheduledAt)` index.
pub(crate) async fn scheduler_counts(app: &App) -> crate::Result<(u64, u64)> {
    let catalog = app.db().catalog().clone();
    let Ok(table) = catalog.table(crate::scheduler::ScheduledJob::NAME) else { return Ok((0, 0)) };
    let Some(index) = table.index("by_state_run_at") else { return Ok((0, 0)) };
    let index_id = index.id;
    let snapshot = app.db().snapshot().await?;
    snapshot.with_conn(|conn| {
        let count = |state: &str| -> crate::Result<u64> {
            let start = crate::keys::encode(&serde_json::Value::String(state.into()));
            let end = crate::keys::prefix_successor(&start).unwrap_or_else(|| vec![0xff; start.len() + 1]);
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM index_entries WHERE index_id = ?1 AND key >= ?2 AND key < ?3",
                rusqlite::params![index_id, start, end],
                |r| r.get(0),
            )?;
            Ok(n as u64)
        };
        Ok((count("pending")?, count("inProgress")?))
    })
}

/// The metrics of `app` in the Prometheus text format.
pub async fn render(app: &App) -> crate::Result<String> {
    let mut out = String::with_capacity(8192);
    header(&mut out, "rxferrix_build_info", "gauge", "RxFerrix version.");
    let _ = writeln!(out, "rxferrix_build_info{{version=\"{}\"}} 1", env!("CARGO_PKG_VERSION"));
    header(&mut out, "rxferrix_start_time_seconds", "gauge", "When the app was opened, in seconds since the epoch.");
    let _ = writeln!(out, "rxferrix_start_time_seconds {:.3}", app.started_at() / 1000.0);

    let counters = &app.metrics().ops;
    header(&mut out, "rxferrix_http_requests_total", "counter", "HTTP requests by matched route and status.");
    let mut requests: Vec<_> =
        counters.requests.lock().unwrap_or_else(|e| e.into_inner()).clone().into_iter().collect();
    requests.sort();
    for ((route, status), n) in requests {
        let _ = writeln!(out, "rxferrix_http_requests_total{{route=\"{}\",status=\"{status}\"}} {n}", label(&route));
    }

    header(&mut out, "rxferrix_function_executions_total", "counter", "Function executions by path, kind and outcome.");
    let mut executions: Vec<_> =
        counters.executions.lock().unwrap_or_else(|e| e.into_inner()).clone().into_iter().collect();
    executions.sort();
    for ((path, kind, outcome), n) in executions {
        let _ = writeln!(
            out,
            "rxferrix_function_executions_total{{path=\"{}\",kind=\"{kind}\",outcome=\"{outcome}\"}} {n}",
            label(&path)
        );
    }

    header(&mut out, "rxferrix_function_duration_seconds", "histogram", "Function execution time.");
    let mut durations: Vec<_> =
        counters.durations.lock().unwrap_or_else(|e| e.into_inner()).clone().into_iter().collect();
    durations.sort_by(|a, b| a.0.cmp(&b.0));
    for ((path, kind), h) in durations {
        let labels = format!("path=\"{}\",kind=\"{kind}\"", label(&path));
        let mut cumulative = 0;
        for (i, bound) in BUCKETS.iter().enumerate() {
            cumulative += h.counts[i];
            let _ = writeln!(out, "rxferrix_function_duration_seconds_bucket{{{labels},le=\"{bound}\"}} {cumulative}");
        }
        let _ = writeln!(out, "rxferrix_function_duration_seconds_bucket{{{labels},le=\"+Inf\"}} {}", h.count);
        let _ = writeln!(out, "rxferrix_function_duration_seconds_sum{{{labels}}} {}", h.sum);
        let _ = writeln!(out, "rxferrix_function_duration_seconds_count{{{labels}}} {}", h.count);
    }

    let log = app.db().write_log();
    let ts = log.latest();
    header(&mut out, "rxferrix_commit_ts", "gauge", "The latest commit ts.");
    let _ = writeln!(out, "rxferrix_commit_ts {ts}");
    header(
        &mut out,
        "rxferrix_commits_total",
        "counter",
        "Write transactions committed since the database was created.",
    );
    let _ = writeln!(out, "rxferrix_commits_total {ts}");
    header(&mut out, "rxferrix_commits_per_second", "gauge", "Commits per second over the last 10 seconds.");
    let _ = writeln!(out, "rxferrix_commits_per_second {}", log.commits_within(Duration::from_secs(10)) as f64 / 10.0);

    let m = app.metrics();
    let stats = m.subscription_stats();
    let gauges: [(&str, &str, u64); 7] = [
        ("rxferrix_websocket_connections", "Open WebSocket connections.", m.sockets()),
        ("rxferrix_sync_connections", "Connections known to the subscription manager.", stats.connections),
        ("rxferrix_subscriptions", "Live (connection, queryId) subscriptions.", stats.subscriptions),
        ("rxferrix_shared_queries", "Distinct shared queries the server reruns.", stats.shared_queries),
        ("rxferrix_query_cache_entries", "Cached results of queries nobody watches.", stats.idle_queries),
        ("rxferrix_query_cache_bytes", "Estimated memory of the cached results.", stats.idle_bytes),
        ("rxferrix_query_jobs", "Query runs queued or running.", stats.jobs),
    ];
    for (name, help, v) in gauges {
        header(&mut out, name, "gauge", help);
        let _ = writeln!(out, "{name} {v}");
    }
    header(&mut out, "rxferrix_subscription_rounds_total", "counter", "Subscription invalidation rounds.");
    let _ = writeln!(out, "rxferrix_subscription_rounds_total {}", stats.rounds);
    header(&mut out, "rxferrix_query_reruns_total", "counter", "Query reruns caused by commits.");
    let _ = writeln!(out, "rxferrix_query_reruns_total {}", stats.reruns);
    let counters = [
        (
            "rxferrix_query_cache_hits_total",
            "Subscriptions, HTTP queries and ctx.run_query calls served cached.",
            stats.cache_hits,
        ),
        (
            "rxferrix_query_cache_misses_total",
            "Subscriptions, HTTP queries and ctx.run_query calls not served cached.",
            stats.cache_misses,
        ),
        ("rxferrix_query_cache_evictions_total", "Cached results dropped for memory or age.", stats.cache_evictions),
    ];
    for (name, help, v) in counters {
        header(&mut out, name, "counter", help);
        let _ = writeln!(out, "{name} {v}");
    }

    header(&mut out, "rxferrix_db_file_bytes", "gauge", "Size of the database files.");
    for name in ["data.db", "data.db-wal", "aux.db", "aux.db-wal"] {
        let _ = writeln!(out, "rxferrix_db_file_bytes{{file=\"{name}\"}} {}", file_size(&app.dir().join(name)));
    }

    let (pending, running) = scheduler_counts(app).await?;
    header(&mut out, "rxferrix_scheduler_queue_depth", "gauge", "Pending scheduled jobs.");
    let _ = writeln!(out, "rxferrix_scheduler_queue_depth {pending}");
    header(&mut out, "rxferrix_scheduler_running_jobs", "gauge", "Scheduled actions in progress.");
    let _ = writeln!(out, "rxferrix_scheduler_running_jobs {running}");

    header(
        &mut out,
        "rxferrix_exec_log_dropped_total",
        "counter",
        "Execution log entries dropped because the writer fell behind.",
    );
    let _ = writeln!(out, "rxferrix_exec_log_dropped_total {}", app.logs().dropped());

    if let Some(replicator) = app.replication() {
        let st = replicator.status(app).await;
        let gauges: [(&str, &str, &str, f64); 8] = [
            (
                "rxferrix_replication_shipped_ts",
                "gauge",
                "The newest commit ts stored in the replication target.",
                st.shipped_ts as f64,
            ),
            (
                "rxferrix_replication_lag_commits",
                "gauge",
                "Commits not shipped to the replication target yet.",
                st.lag_commits as f64,
            ),
            (
                "rxferrix_replication_lag_seconds",
                "gauge",
                "Age of the oldest unshipped commit (0 when caught up).",
                st.lag_ms / 1000.0,
            ),
            (
                "rxferrix_replication_pending_bytes",
                "gauge",
                "Journal bytes kept locally until shipped.",
                st.pending_bytes as f64,
            ),
            (
                "rxferrix_replication_journaling",
                "gauge",
                "1 while commits are journaled.",
                if st.journaling { 1.0 } else { 0.0 },
            ),
            ("rxferrix_replication_failures", "gauge", "Consecutive failed replication attempts.", st.failures as f64),
            (
                "rxferrix_replication_segments_shipped_total",
                "counter",
                "Journal segments shipped since the start.",
                st.segments_shipped as f64,
            ),
            (
                "rxferrix_replication_snapshots_total",
                "counter",
                "Snapshots stored since the start.",
                st.snapshots_taken as f64,
            ),
        ];
        for (name, kind, help, v) in gauges {
            header(&mut out, name, kind, help);
            let _ = writeln!(out, "{name} {v}");
        }
        header(
            &mut out,
            "rxferrix_replication_last_snapshot_timestamp_seconds",
            "gauge",
            "When the newest snapshot was taken.",
        );
        let last = st.last_snapshot.map(|s| s.created_at / 1000.0).unwrap_or(0.0);
        let _ = writeln!(out, "rxferrix_replication_last_snapshot_timestamp_seconds {last:.3}");
    }
    if let Some(olap) = app.olap() {
        let st = olap.status(app).await;
        let gauges: [(&str, &str, &str, f64); 7] = [
            (
                "rxferrix_olap_ready",
                "gauge",
                "1 while the analytics copy answers queries.",
                if st.state == "ready" { 1.0 } else { 0.0 },
            ),
            (
                "rxferrix_olap_applied_ts",
                "gauge",
                "The analytics copy reflects every commit up to this ts.",
                st.applied_ts as f64,
            ),
            (
                "rxferrix_olap_lag_commits",
                "gauge",
                "Commits not in the analytics copy yet.",
                st.latest_ts.saturating_sub(st.applied_ts) as f64,
            ),
            (
                "rxferrix_olap_lag_seconds",
                "gauge",
                "How far the analytics copy is behind the newest commit.",
                st.lag_ms / 1000.0,
            ),
            ("rxferrix_olap_size_bytes", "gauge", "Size of olap.duckdb and its WAL.", st.size_bytes as f64),
            (
                "rxferrix_olap_rows_applied_total",
                "counter",
                "Documents written into the analytics copy since the start.",
                st.rows_applied as f64,
            ),
            ("rxferrix_olap_rebuilds_total", "counter", "Analytics copy rebuilds since the start.", st.rebuilds as f64),
        ];
        for (name, kind, help, v) in gauges {
            header(&mut out, name, kind, help);
            let _ = writeln!(out, "{name} {v}");
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_buckets_and_labels() {
        let mut h = Histogram::default();
        h.observe(0.0001);
        h.observe(0.003);
        h.observe(30.0);
        assert_eq!(h.counts[0], 1);
        assert_eq!(h.counts[3], 1);
        assert_eq!(h.counts[BUCKETS.len()], 1);
        assert_eq!(h.count, 3);
        assert_eq!(label("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }
}
