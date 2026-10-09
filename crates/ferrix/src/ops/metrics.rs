//! Prometheus metrics: `GET /metrics` in the text exposition format (version 0.0.4).
//!
//! Off by default: `serve --metrics` (or `AppOptions::ops.metrics.enabled`) turns it on, and `--metrics-token <T>`
//! (env `FERRIX_METRICS_TOKEN`) requires `Authorization: Bearer <T>`. While off, `/metrics` answers 404 and nothing
//! is counted.
//!
//! | Metric | Type | Labels |
//! |---|---|---|
//! | `ferrix_build_info` | gauge (1) | `version` |
//! | `ferrix_start_time_seconds` | gauge | |
//! | `ferrix_http_requests_total` | counter | `route` (the matched route, e.g. `/api/storage/{id}`; `fallback` for static files and unmatched paths), `status` |
//! | `ferrix_function_executions_total` | counter | `path`, `kind` (`query`, `mutation`, `action`), `outcome` (`success`, `userError`, `serverError`) |
//! | `ferrix_function_duration_seconds` | histogram | `path`, `kind` |
//! | `ferrix_commit_ts` | gauge | the latest commit ts |
//! | `ferrix_commits_total` | counter | write transactions committed since the database was created (= the commit ts; `rate()` it) |
//! | `ferrix_commits_per_second` | gauge | commits per second over the last 10 seconds |
//! | `ferrix_websocket_connections` | gauge | open WebSockets |
//! | `ferrix_sync_connections` | gauge | connections known to the subscription manager |
//! | `ferrix_subscriptions` | gauge | live `(connection, queryId)` subscriptions |
//! | `ferrix_shared_queries` | gauge | distinct queries the server reruns |
//! | `ferrix_subscription_rounds_total`, `ferrix_query_reruns_total` | counter | invalidation rounds and query reruns |
//! | `ferrix_db_file_bytes` | gauge | `file` (`data.db`, `data.db-wal`, `aux.db`, `aux.db-wal`) |
//! | `ferrix_scheduler_queue_depth` | gauge | pending scheduled jobs (due or not) |
//! | `ferrix_scheduler_running_jobs` | gauge | actions in progress |
//! | `ferrix_exec_log_dropped_total` | counter | execution log entries dropped because the log writer fell behind |

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Duration;

use axum::extract::{MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;

use crate::exec_log::LogOutcome;
use crate::private::FunctionKind;
use crate::Table as _;
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
        f.debug_struct("MetricsOptions").field("enabled", &self.enabled).field("token", &self.token.as_ref().map(|_| "…")).finish()
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
async fn scheduler_counts(app: &App) -> crate::Result<(u64, u64)> {
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
    header(&mut out, "ferrix_build_info", "gauge", "Ferrix version.");
    let _ = writeln!(out, "ferrix_build_info{{version=\"{}\"}} 1", env!("CARGO_PKG_VERSION"));
    header(&mut out, "ferrix_start_time_seconds", "gauge", "When the app was opened, in seconds since the epoch.");
    let _ = writeln!(out, "ferrix_start_time_seconds {:.3}", app.started_at() / 1000.0);

    let counters = &app.metrics().ops;
    header(&mut out, "ferrix_http_requests_total", "counter", "HTTP requests by matched route and status.");
    let mut requests: Vec<_> = counters.requests.lock().unwrap_or_else(|e| e.into_inner()).clone().into_iter().collect();
    requests.sort();
    for ((route, status), n) in requests {
        let _ = writeln!(out, "ferrix_http_requests_total{{route=\"{}\",status=\"{status}\"}} {n}", label(&route));
    }

    header(&mut out, "ferrix_function_executions_total", "counter", "Function executions by path, kind and outcome.");
    let mut executions: Vec<_> = counters.executions.lock().unwrap_or_else(|e| e.into_inner()).clone().into_iter().collect();
    executions.sort();
    for ((path, kind, outcome), n) in executions {
        let _ = writeln!(
            out,
            "ferrix_function_executions_total{{path=\"{}\",kind=\"{kind}\",outcome=\"{outcome}\"}} {n}",
            label(&path)
        );
    }

    header(&mut out, "ferrix_function_duration_seconds", "histogram", "Function execution time.");
    let mut durations: Vec<_> = counters.durations.lock().unwrap_or_else(|e| e.into_inner()).clone().into_iter().collect();
    durations.sort_by(|a, b| a.0.cmp(&b.0));
    for ((path, kind), h) in durations {
        let labels = format!("path=\"{}\",kind=\"{kind}\"", label(&path));
        let mut cumulative = 0;
        for (i, bound) in BUCKETS.iter().enumerate() {
            cumulative += h.counts[i];
            let _ = writeln!(out, "ferrix_function_duration_seconds_bucket{{{labels},le=\"{bound}\"}} {cumulative}");
        }
        let _ = writeln!(out, "ferrix_function_duration_seconds_bucket{{{labels},le=\"+Inf\"}} {}", h.count);
        let _ = writeln!(out, "ferrix_function_duration_seconds_sum{{{labels}}} {}", h.sum);
        let _ = writeln!(out, "ferrix_function_duration_seconds_count{{{labels}}} {}", h.count);
    }

    let log = app.db().write_log();
    let ts = log.latest();
    header(&mut out, "ferrix_commit_ts", "gauge", "The latest commit ts.");
    let _ = writeln!(out, "ferrix_commit_ts {ts}");
    header(&mut out, "ferrix_commits_total", "counter", "Write transactions committed since the database was created.");
    let _ = writeln!(out, "ferrix_commits_total {ts}");
    header(&mut out, "ferrix_commits_per_second", "gauge", "Commits per second over the last 10 seconds.");
    let _ = writeln!(out, "ferrix_commits_per_second {}", log.commits_within(Duration::from_secs(10)) as f64 / 10.0);

    let m = app.metrics();
    let stats = m.subscription_stats();
    let gauges: [(&str, &str, u64); 4] = [
        ("ferrix_websocket_connections", "Open WebSocket connections.", m.sockets()),
        ("ferrix_sync_connections", "Connections known to the subscription manager.", stats.connections),
        ("ferrix_subscriptions", "Live (connection, queryId) subscriptions.", stats.subscriptions),
        ("ferrix_shared_queries", "Distinct shared queries the server reruns.", stats.shared_queries),
    ];
    for (name, help, v) in gauges {
        header(&mut out, name, "gauge", help);
        let _ = writeln!(out, "{name} {v}");
    }
    header(&mut out, "ferrix_subscription_rounds_total", "counter", "Subscription invalidation rounds.");
    let _ = writeln!(out, "ferrix_subscription_rounds_total {}", stats.rounds);
    header(&mut out, "ferrix_query_reruns_total", "counter", "Query reruns caused by commits.");
    let _ = writeln!(out, "ferrix_query_reruns_total {}", stats.reruns);

    header(&mut out, "ferrix_db_file_bytes", "gauge", "Size of the database files.");
    for name in ["data.db", "data.db-wal", "aux.db", "aux.db-wal"] {
        let _ = writeln!(out, "ferrix_db_file_bytes{{file=\"{name}\"}} {}", file_size(&app.dir().join(name)));
    }

    let (pending, running) = scheduler_counts(app).await?;
    header(&mut out, "ferrix_scheduler_queue_depth", "gauge", "Pending scheduled jobs.");
    let _ = writeln!(out, "ferrix_scheduler_queue_depth {pending}");
    header(&mut out, "ferrix_scheduler_running_jobs", "gauge", "Scheduled actions in progress.");
    let _ = writeln!(out, "ferrix_scheduler_running_jobs {running}");

    header(&mut out, "ferrix_exec_log_dropped_total", "counter", "Execution log entries dropped because the writer fell behind.");
    let _ = writeln!(out, "ferrix_exec_log_dropped_total {}", app.logs().dropped());
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
