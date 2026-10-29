# SQL analytics

Use analytics for reports with joins, groupings, or time series over many documents. RxFerrix keeps a separate DuckDB copy of selected app data and lets actions query it with read-only SQL.

The copy follows committed changes asynchronously. It is usually a little behind the live database, and results are not reactive subscriptions. Use [aggregates](/guide/aggregates) for counts and sums that must update transactionally with every write.

## Enable both build and runtime support

Enable the `olap` feature on your app's `rxferrix` dependency, then start with `serve --olap`. The Cargo feature compiles DuckDB; the flag starts the copy. The first feature-enabled build can take several minutes.

The repository examples forward this feature, so you can run:

```sh
cargo run -p todo-server --features olap -- serve --dev --olap \
  --codegen-out examples/todo/web/src/lib/rxferrix
```

Without build support or an enabled runtime, analytics calls return `olap_unavailable`.

## Query from an action

This example reports on the todo example's table:

```rust
use rxferrix::prelude::*;

#[object]
pub struct DayStats {
    pub day: String,
    pub added: i64,
    pub completed: i64,
}

#[action]
pub async fn stats_by_day(ctx: &ActionCtx) -> Result<Vec<DayStats>> {
    ctx.olap.query_as::<DayStats>(
        "SELECT strftime(to_timestamp(_creationTime / 1000), '%Y-%m-%d') AS day,
                count(*) AS added,
                count(*) FILTER (WHERE done) AS completed
         FROM todos GROUP BY day ORDER BY day",
        (),
    ).await
}
```

`query_as` deserializes rows into your result type. SQL column names must match its serialized field names. Use `?` parameters for input values rather than constructing SQL from user text.

The SQL sandbox does not provide row-level app authorization. A public analytics action must check who may run the report and scope its SQL to the data they can read. The chat example's `insights.rs` demonstrates channel-scoped reports.

## Understand the copied schema

Tables keep their app table names. Columns include `_id`, `_creationTime` in milliseconds, `_ts` for commit time, and fields derived from the Rust type. `Bytes` becomes a BLOB, vectors/lists become lists, and supported nested objects become STRUCTs; other values use JSON.

Selected system tables such as `_users`, `_storage`, and `_scheduled_jobs` are included. Tables containing credentials, sessions, settings, environment secrets, and the audit log are excluded.

## Control freshness and limits

The general query result includes `as_of_ts`, the commit timestamp represented by the copy. After an action runs a mutation, use `ctx.olap.latest()` for a query that waits to include that work. Do not assume the ordinary copy already includes the immediately preceding write.

Each call accepts one read-only SELECT. File access, network access, extensions, and changing settings are disabled. App queries return at most 100,000 rows and default to a 30-second timeout. Errors include `olap_query_failed` and `olap_timeout`.

Tune `--olap-memory`, `--olap-threads`, `--olap-timeout`, and `--olap-max-lag` for the server. A copy that falls too far behind rebuilds from a snapshot. It also rebuilds as needed after schema changes and restores.

## Inspect and test analytics

The dashboard's **Analytics** page shows copy state, lag, tables, and a SQL editor with CSV/Parquet downloads. `/api/admin/olap` and Prometheus metrics expose state for operations.

Tests can enable `OlapOptions { enabled: true, background: false, ..Default::default() }` inside `AppOptions`, then call `t.olap_sync().await?` before querying. This makes copy progress explicit.

The copy lives in `<data dir>/olap.duckdb`. It is derived data, so backups skip it. See the [analytics reference](/CONTRACT#_1-11-analytics-olap) for parameters, freshness barriers, and admin export limits.
