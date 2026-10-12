# Analytics (OLAP)

For group-bys, joins and time series over many documents, `serve --olap` keeps a columnar copy of the data in an
embedded DuckDB (`<data dir>/olap.duckdb`), a few hundred milliseconds behind, and actions query it with read-only
SQL. It needs the cargo feature `olap` (DuckDB is built from source: a static binary grows from ~25 to ~57 MB, and a
first build takes a few minutes): `ferrix = { version = "…", features = ["olap"] }`.

```rust
#[object]
pub struct DayStats { pub day: String, pub added: i64, pub completed: i64 }

#[action]
pub async fn stats_by_day(ctx: &ActionCtx) -> Result<Vec<DayStats>> {
    ctx.olap.query_as::<DayStats>(
        "SELECT strftime(to_timestamp(_creationTime / 1000), '%Y-%m-%d') AS day,
                count(*) AS added, count(*) FILTER (WHERE done) AS completed
         FROM todos GROUP BY day ORDER BY day",
        (),
    ).await
}
```

- Tables are named like yours, with `_id`, `_creationTime` (ms) and `_ts` (commit ts) first, then the fields with
  types from the Rust struct: numbers, booleans, strings, `Bytes` as BLOBs, `Vec<T>` as lists, nested `#[object]` structs as STRUCTs
  (`address.city`), anything else as JSON. `_users`, `_storage`, `_scheduled_jobs` and a few other system tables are
  there too; tables holding secrets (credentials, sessions, settings, environment variables) and the audit log never are.
- Actions only, one SELECT per call, `?` parameters. No files, network, extensions or settings; at most 100k rows;
  interrupted after `--olap-timeout` (30s). Errors are user errors: `olap_unavailable`, `olap_query_failed`,
  `olap_timeout`.
- The copy follows the change journal and reports `as_of_ts` with each result. After `ctx.run_mutation`, query
  through `ctx.olap.latest()` to see its writes. It rebuilds itself from a snapshot when there is none yet, after a
  restore, when a table's Rust type changes (that table only), or when it falls more than `--olap-max-lag` (512 MB of
  changes) behind. Backups skip it: it is derived data.
- Cost on writes: the journal (~8% on tiny inserts, as with replication) plus ~2% for the copy following; measured
  47k → 43k one-document inserts/s at 16 writers.
- `--olap-memory 2GB` and `--olap-threads N` bound DuckDB. `GET /api/admin/olap` shows the state, lag, tables and
  size (also on the dashboard Overview and in `/metrics`); `POST /api/admin/olap/rebuild` starts over. The dashboard's
  Analytics page runs SQL as an administrator, and `POST /api/admin/olap/query` (`format` csv or parquet) downloads a
  whole result (up to 10M rows, written from DuckDB's Arrow batches; the sandbox has no file access).
- In tests: `AppOptions { olap: OlapOptions { enabled: true, background: false, ..Default::default() }, .. }`, then
  `t.olap_sync().await?` before querying. The todo example has `todos:stats_by_day`
  (`cargo run -p todo-server --features olap -- serve --dev --olap`). Details: docs/CONTRACT.md §1.11.
