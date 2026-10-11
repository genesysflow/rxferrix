//! Analytics: a columnar copy of the data in an embedded DuckDB, queried with read-only SQL from actions
//! (`ctx.olap`). Design: PLAN.md §7.6; contract: docs/CONTRACT.md §1.11.
//!
//! The copy lives in `<data dir>/olap.duckdb` and follows the live database through the change journal
//! (`crate::db::journal`, consumer `Olap`): batches of commits are applied in DuckDB transactions, a few hundred
//! milliseconds behind. Each table has typed columns from its shape (`crate::shape`, [`columns`]) and is rebuilt from
//! a snapshot when there is no copy yet, after a restore, when its shape changes, when the journal can't continue it,
//! or when it falls more than [`OlapOptions::max_lag_bytes`] behind. Queries see whole commits and report the ts the
//! copy is current to (`as_of_ts`).
//!
//! The DuckDB engine is behind the cargo feature `olap` (it adds ~32 MB to a static binary). Without it the API
//! exists, so app code compiles either way, and `--olap` fails at startup; queries fail with `olap_unavailable`.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::runtime::App;
use crate::{Result, UserError};

pub mod columns;
#[cfg(feature = "olap")]
mod engine;
#[cfg(feature = "olap")]
mod values;

/// The analytics copy's settings (`AppOptions::olap`).
#[derive(Debug, Clone)]
pub struct OlapOptions {
    /// Keep the copy (`--olap`). Needs the `olap` cargo feature.
    pub enabled: bool,
    /// DuckDB's memory limit in bytes (`--olap-memory`). Default: DuckDB's (80% of RAM).
    pub memory_limit: Option<u64>,
    /// DuckDB's worker threads (`--olap-threads`). Default: one per core.
    pub threads: Option<usize>,
    /// A query still running after this long is interrupted (`--olap-timeout`; also the longest `at_least` waits).
    /// Default: 30s.
    pub query_timeout: Duration,
    /// Queries running at once; more wait. Default: 4.
    pub max_concurrent_queries: usize,
    /// Rows a query may return; more is an error (add a `LIMIT`). Default: 100k.
    pub max_result_rows: usize,
    /// Past this many bytes of journal waiting to be applied, the copy gives up following and is rebuilt instead, so
    /// the journal never grows without bound (`--olap-max-lag`). Default: 512 MiB.
    pub max_lag_bytes: u64,
    /// How often the copy looks for new commits. Default: 250ms.
    pub apply_every: Duration,
    /// Follow the database in the background. Off in tests that drive the copy with [`OlapService::sync`].
    pub background: bool,
    /// Rows an export (CSV, Parquet) may write. Default: 10M.
    pub max_export_rows: u64,
    /// An export still running after this long is interrupted. Default: 5 minutes.
    pub export_timeout: Duration,
}

impl Default for OlapOptions {
    fn default() -> Self {
        OlapOptions {
            enabled: false,
            memory_limit: None,
            threads: None,
            query_timeout: Duration::from_secs(30),
            max_concurrent_queries: 4,
            max_result_rows: 100_000,
            max_lag_bytes: 512 << 20,
            apply_every: Duration::from_millis(250),
            background: true,
            max_export_rows: 10_000_000,
            export_timeout: Duration::from_secs(300),
        }
    }
}

/// The name of the copy's file in the data directory.
pub const FILE: &str = "olap.duckdb";

// ---------------------------------------------------------------------------------------------------------------------
// Results and parameters
// ---------------------------------------------------------------------------------------------------------------------

/// A query's result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OlapResult {
    pub columns: Vec<OlapColumn>,
    /// One object per row, keyed by column name. Values: numbers, strings, booleans, `null`; lists and structs as
    /// arrays and objects; `JSON` columns parsed; timestamps and dates as ISO 8601 strings; blobs as
    /// `{"$bytes": "<base64>"}`; integers beyond 2^53 and decimals as numbers when they fit an `f64`.
    pub rows: Vec<Map<String, Value>>,
    /// The copy is current to this commit: it reflects every commit up to it and none after.
    pub as_of_ts: u64,
}

/// A result column.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OlapColumn {
    pub name: String,
    /// DuckDB's type name (`BIGINT`, `VARCHAR`, `STRUCT(...)`, ...).
    pub type_name: String,
}

/// The file format of an export ([`OlapService::export`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OlapExportFormat {
    /// RFC 4180 CSV with a header row: strings as is, `null` empty, anything else as JSON.
    Csv,
    /// Apache Parquet (zstd), with DuckDB's types (lists and structs nested).
    Parquet,
}

impl OlapExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            OlapExportFormat::Csv => "csv",
            OlapExportFormat::Parquet => "parquet",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            OlapExportFormat::Csv => "text/csv; charset=utf-8",
            OlapExportFormat::Parquet => "application/vnd.apache.parquet",
        }
    }
}

/// What an export wrote.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OlapExport {
    pub rows: u64,
    pub columns: Vec<OlapColumn>,
    pub as_of_ts: u64,
}

impl OlapValue {
    /// A JSON value as a parameter: numbers, strings, booleans and `null` as such, arrays and objects as JSON text.
    pub fn from_json(v: Value) -> OlapValue {
        match v {
            Value::Null => OlapValue::Null,
            Value::Bool(b) => OlapValue::Bool(b),
            Value::Number(n) => {
                n.as_i64().map_or_else(|| OlapValue::Float(n.as_f64().unwrap_or(f64::NAN)), OlapValue::Int)
            }
            Value::String(s) => OlapValue::Text(s),
            other => OlapValue::Text(other.to_string()),
        }
    }
}

/// A query parameter (`?` in the SQL, in order).
#[derive(Debug, Clone, PartialEq)]
pub enum OlapValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}

macro_rules! olap_values {
    ($variant:ident => $($t:ty),*) => {
        $(impl From<$t> for OlapValue {
            fn from(v: $t) -> Self {
                OlapValue::$variant(v.into())
            }
        })*
    };
}
olap_values!(Bool => bool);
olap_values!(Int => i8, i16, i32, i64, u8, u16, u32);
olap_values!(Float => f32, f64);
olap_values!(Text => String, &str, &String);
olap_values!(Bytes => Vec<u8>);

impl From<crate::Bytes> for OlapValue {
    fn from(b: crate::Bytes) -> Self {
        OlapValue::Bytes(b.0)
    }
}

impl<T> From<crate::Id<T>> for OlapValue {
    fn from(id: crate::Id<T>) -> Self {
        OlapValue::Text(id.to_string())
    }
}

impl<T: Into<OlapValue>> From<Option<T>> for OlapValue {
    fn from(v: Option<T>) -> Self {
        v.map_or(OlapValue::Null, Into::into)
    }
}

/// The parameters of a query: `()`, a tuple of up to 8 values, or a `Vec<OlapValue>`.
pub trait OlapParams {
    fn into_params(self) -> Vec<OlapValue>;
}

impl OlapParams for () {
    fn into_params(self) -> Vec<OlapValue> {
        Vec::new()
    }
}

impl OlapParams for Vec<OlapValue> {
    fn into_params(self) -> Vec<OlapValue> {
        self
    }
}

macro_rules! olap_tuples {
    ($(($($t:ident),+)),*) => {
        $(impl<$($t: Into<OlapValue>),+> OlapParams for ($($t,)+) {
            #[allow(non_snake_case)]
            fn into_params(self) -> Vec<OlapValue> {
                let ($($t,)+) = self;
                vec![$($t.into()),+]
            }
        })*
    };
}
olap_tuples!(
    (A),
    (A, B),
    (A, B, C),
    (A, B, C, D),
    (A, B, C, D, E),
    (A, B, C, D, E, F),
    (A, B, C, D, E, F, G),
    (A, B, C, D, E, F, G, H)
);

// ---------------------------------------------------------------------------------------------------------------------
// ctx.olap
// ---------------------------------------------------------------------------------------------------------------------

/// `ctx.olap` in actions: read-only SQL over the analytics copy. Not in queries or mutations: the copy lags the data
/// by a few hundred milliseconds and is not reactive.
///
/// One `SELECT` per call (`WITH … SELECT` included); no files, network, extensions or settings; at most
/// [`OlapOptions::max_result_rows`] rows; interrupted after [`OlapOptions::query_timeout`]. Tables are named like
/// ferrix's, with `_id`, `_creationTime` (ms) and `_ts` (commit ts) before their fields. Errors are user errors:
/// `olap_unavailable` (off, not built yet), `olap_query_failed` (DuckDB's message), `olap_timeout`.
pub struct Olap {
    app: App,
    at_least: u64,
}

impl Olap {
    pub(crate) fn new(app: App) -> Olap {
        Olap { app, at_least: 0 }
    }

    /// Wait (up to the query timeout) until the copy has applied commit `ts` before querying: after
    /// `ctx.run_mutation`, pass its commit ts to read its writes (`App::execute_mutation` returns it).
    pub fn at_least(&self, ts: u64) -> Olap {
        Olap { app: self.app.clone(), at_least: ts.max(self.at_least) }
    }

    /// Wait until the copy has applied every commit made so far, then query.
    pub fn latest(&self) -> Olap {
        self.at_least(self.app.db().write_log().latest())
    }

    /// Run one `SELECT` with positional `?` parameters.
    pub async fn query(&self, sql: &str, params: impl OlapParams) -> Result<OlapResult> {
        let service = self.app.olap().ok_or_else(unavailable)?;
        service.query(sql, params.into_params(), self.at_least).await
    }

    /// [`Olap::query`], each row deserialized as `T` (columns are the fields: alias them to match).
    pub async fn query_as<T: DeserializeOwned>(&self, sql: &str, params: impl OlapParams) -> Result<Vec<T>> {
        let result = self.query(sql, params).await?;
        result
            .rows
            .into_iter()
            .map(|row| {
                serde_json::from_value(Value::Object(row)).map_err(|e| {
                    UserError::new("olap_query_failed", format!("A row doesn't deserialize as the requested type: {e}"))
                        .into()
                })
            })
            .collect()
    }
}

fn unavailable() -> crate::Error {
    let why = if cfg!(feature = "olap") {
        "Analytics are off: start the server with --olap (AppOptions::olap.enabled)"
    } else {
        "Analytics are not available: this binary was built without the `olap` cargo feature"
    };
    UserError::new("olap_unavailable", why).into()
}

// ---------------------------------------------------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------------------------------------------------

/// The copy's state for the dashboard, `/metrics` and `GET /api/admin/olap`.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OlapStatus {
    /// `building` (no complete copy yet, or a table is being rebuilt), `ready`, or `failed` (retried).
    pub state: String,
    /// Why the copy is being built, or the last error.
    pub detail: Option<String>,
    /// The copy reflects every commit up to this one.
    pub applied_ts: u64,
    /// The newest commit.
    pub latest_ts: u64,
    /// Commit time (ms) of `applied_ts`, and how far behind the newest commit it is.
    pub applied_time: f64,
    pub lag_ms: f64,
    pub tables: Vec<OlapTableStatus>,
    /// Size of `olap.duckdb` (and its WAL).
    pub size_bytes: u64,
    /// Documents written into the copy since the server started (rebuilds included).
    pub rows_applied: u64,
    /// Rebuilds (of every table, or of the tables whose shape changed) since the server started.
    pub rebuilds: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OlapTableStatus {
    pub name: String,
    pub rows: u64,
    pub columns: Vec<OlapColumn>,
}

/// The running copy (`App::olap`).
pub struct OlapService {
    #[cfg(feature = "olap")]
    engine: Arc<engine::Engine>,
}

impl std::fmt::Debug for OlapService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OlapService").finish_non_exhaustive()
    }
}

/// Open the copy if the app's options ask for it. Called once by `App::open`, before replication starts.
pub(crate) async fn start(app: &App) -> Result<Option<Arc<OlapService>>> {
    if !app.options().olap.enabled {
        return Ok(None);
    }
    #[cfg(feature = "olap")]
    {
        let engine = engine::Engine::open(app).await?;
        if app.options().olap.background {
            tokio::spawn(engine::run(engine.clone(), app.downgrade()));
        }
        Ok(Some(Arc::new(OlapService { engine })))
    }
    #[cfg(not(feature = "olap"))]
    {
        anyhow::bail!("--olap needs the `olap` cargo feature: build with `--features ferrix/olap`")
    }
}

impl OlapService {
    async fn query(&self, sql: &str, params: Vec<OlapValue>, at_least: u64) -> Result<OlapResult> {
        #[cfg(feature = "olap")]
        {
            self.engine.query(sql, params, at_least).await
        }
        #[cfg(not(feature = "olap"))]
        {
            let _ = (sql, params, at_least);
            Err(unavailable())
        }
    }

    /// Run one SELECT (the same sandbox as [`Olap::query`]) and write its whole result to `path` as CSV or Parquet,
    /// up to [`OlapOptions::max_export_rows`] rows, interrupted after [`OlapOptions::export_timeout`].
    pub async fn export(
        &self,
        sql: &str,
        params: Vec<OlapValue>,
        format: OlapExportFormat,
        path: &std::path::Path,
    ) -> Result<OlapExport> {
        #[cfg(feature = "olap")]
        {
            self.engine.export(sql, params, format, path.to_path_buf()).await
        }
        #[cfg(not(feature = "olap"))]
        {
            let _ = (sql, params, format, path);
            Err(unavailable())
        }
    }

    /// Run one SELECT for an admin (the dashboard's console): the same sandbox as [`Olap::query`].
    pub async fn admin_query(&self, sql: &str, params: Vec<OlapValue>) -> Result<OlapResult> {
        self.query(sql, params, 0).await
    }

    /// Bring the copy up to the newest commit now (building it first if needed). For tests and shutdown.
    pub async fn sync(&self, app: &App) -> Result<()> {
        #[cfg(feature = "olap")]
        {
            self.engine.sync(app).await
        }
        #[cfg(not(feature = "olap"))]
        {
            let _ = app;
            Ok(())
        }
    }

    pub async fn status(&self, app: &App) -> OlapStatus {
        #[cfg(feature = "olap")]
        {
            self.engine.status(app).await
        }
        #[cfg(not(feature = "olap"))]
        {
            let _ = app;
            OlapStatus::default()
        }
    }

    /// Ask for every table to be rebuilt from a snapshot; the background loop (or the next [`OlapService::sync`])
    /// does it.
    pub fn request_rebuild(&self, why: &str) {
        #[cfg(feature = "olap")]
        self.engine.request_rebuild(why);
        #[cfg(not(feature = "olap"))]
        let _ = why;
    }

    /// Rebuild every table from a snapshot now (admin; also what a restore or a journal gap does).
    pub async fn rebuild(&self, app: &App) -> Result<()> {
        #[cfg(feature = "olap")]
        {
            self.engine.request_rebuild("requested");
            self.engine.sync(app).await
        }
        #[cfg(not(feature = "olap"))]
        {
            let _ = app;
            Ok(())
        }
    }
}
