//! Data migrations: `#[migration]` functions that rewrite the documents of a table in batches (PLAN.md §4.2,
//! docs/CONTRACT.md §1.9).
//!
//! ```ignore
//! #[migration(table = Todo, batch = 100)]
//! async fn add_priority(ctx: &MutationCtx, doc: serde_json::Value) -> Result<Option<serde_json::Value>> {
//!     Ok(doc.get("priority").is_none().then(|| json!({ "priority": 0 })))   // a patch, or None to keep it
//! }
//! ```
//!
//! **Lifecycle.** A migration is *pending* until it first runs (no `_migrations` row). A run marks it *running*
//! (`start`), then processes the table in batches, in `by_creation_time` order. Each batch is one mutation
//! transaction (`_system/migrations:run_batch`): it reads the cursor from the migration's `_migrations` row, runs
//! the migration on up to `batch` documents after it, applies the changes through the normal (untyped) write path,
//! and stores the new cursor and counts in the same transaction. So a batch either fully happens or not at all, and
//! each document is processed **exactly once per run**, even across crashes (a restart turns a *running* row back
//! into *pending*; the next run resumes at its cursor) and even with two runners. When the scan reaches the end, the
//! row becomes *done*, and running it again does nothing (unless restarted from scratch). A document the migration
//! fails on (an error, a panic, a write rejected by validation, the batch timing out) rolls its batch back and marks
//! the row *failed* with the error and the document id; the next run retries from that batch. *Canceling* stops a
//! run after the current batch; the next run resumes.
//!
//! Documents inserted while a migration runs (by the app or by the migration itself) come after the cursor and are
//! processed by that run too, so a migration must leave documents that are already in the new shape alone.
//!
//! **Interplay with the startup schema check** (`db::schema_check`). A table whose documents don't match its Rust
//! type refuses to open, unless migrations for it are pending (not *done*). Such a table opens *migrating*
//! ([`Catalog::is_migrating`](crate::db::catalog::Catalog::is_migrating)): its fingerprint is not recorded, and
//! writes to it are validated leniently: a patch or replace may leave a document that doesn't match the type only
//! if it didn't match before either (so migrations can rewrite old documents step by step, and the app can still
//! touch not-yet-migrated documents). Inserts, and writes to matching documents, stay strict, so the number of
//! invalid documents can only go down. Typed reads of not-yet-migrated documents fail as before. In the
//! transaction where the table's last pending migration finishes, every document of the table is validated again:
//! if all match, the fingerprint is recorded and the table leaves the migrating state; if not, the failure is
//! recorded on the migration (`schemaCheck`) and logged, the table stays migrating until the process stops, and the
//! next start refuses to open (unless another migration for it is pending). Invalid documents therefore never
//! survive a restart unnoticed.
//!
//! Migrations run in name order (`ferrix migrate`, `serve --migrate`, the dashboard's `run` without a name), stopping
//! at the first failure; give them sortable names (`name = "0002_split_name"`) when the order matters.

pub mod tables;
mod runner;

use std::collections::BTreeMap;
use std::sync::OnceLock;

use anyhow::Context as _;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::catalog::Catalog;
use crate::db::schema_check;
use crate::prelude::*;
use crate::private::{MIGRATIONS, MigrationDef};
use crate::runtime::App;
use crate::schema::TableDef;

pub use runner::RunOptions;
pub use tables::{MigrationRecord, MigrationState};

/// Default `batch` of `#[migration]`.
pub const DEFAULT_BATCH_SIZE: usize = 100;
/// Largest `batch` (a page scans at most this many documents, `db::pagination::MAX_PAGE_SCAN`).
pub const MAX_BATCH_SIZE: usize = crate::db::pagination::MAX_PAGE_SCAN;
/// Dry runs return up to this many examples of changed documents per migration.
pub const MAX_EXAMPLES: usize = 5;

// ---------------------------------------------------------------------------------------------------------------------
// The user-facing types
// ---------------------------------------------------------------------------------------------------------------------

/// What a migration does to one document.
#[derive(Debug, Clone, PartialEq)]
pub enum DocChange {
    /// Leave it as it is.
    Keep,
    /// Shallow-merge this JSON object into it (`null` sets a field to null; system fields can't be patched).
    Patch(Value),
    /// Replace its fields with this JSON object (`_id` / `_creationTime`, if present, must be unchanged).
    Replace(Value),
    /// Delete it.
    Delete,
}

/// What a `#[migration]` may return (inside `Result`): `Option<serde_json::Value>` (a patch, `None` = keep),
/// [`DocChange`], or `()` (keep; e.g. when the migration wrote through `ctx.db` itself).
#[diagnostic::on_unimplemented(
    message = "a #[migration] must return `Result<Option<serde_json::Value>>` (a patch or None), `Result<DocChange>` or `Result<()>`, not `Result<{Self}>`",
    label = "not a migration result"
)]
pub trait MigrationOutput {
    fn into_change(self) -> DocChange;
}

impl MigrationOutput for DocChange {
    fn into_change(self) -> DocChange {
        self
    }
}

impl MigrationOutput for Option<Value> {
    fn into_change(self) -> DocChange {
        self.map_or(DocChange::Keep, DocChange::Patch)
    }
}

impl MigrationOutput for () {
    fn into_change(self) -> DocChange {
        DocChange::Keep
    }
}

/// A typed reference to a `#[migration]`, implemented by the macro on a type-namespace-only struct named like the
/// function: `app.run_migration::<add_priority>(RunOptions::default())`.
pub trait MigrationRef: 'static {
    /// The migration's stable id: `#[migration(name = "...")]`, or its function path (e.g. `"todos:add_priority"`).
    fn name() -> String;
    /// The name of the table it migrates.
    fn table() -> &'static str;
}

// ---------------------------------------------------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------------------------------------------------

/// Every migration linked into the binary, by name.
pub struct Migrations {
    by_name: BTreeMap<String, &'static MigrationDef>,
}

impl Migrations {
    fn build(defs: &'static [MigrationDef]) -> Result<Migrations, String> {
        let mut by_name: BTreeMap<String, &'static MigrationDef> = BTreeMap::new();
        for def in defs {
            let name = def.name();
            check_name(&name).map_err(|e| format!("migration `{}::{}`: {e}", def.module_path, def.fn_name))?;
            if !(1..=MAX_BATCH_SIZE).contains(&def.batch_size) {
                return Err(format!("migration `{name}`: batch must be between 1 and {MAX_BATCH_SIZE}"));
            }
            if let Some(prev) = by_name.insert(name.clone(), def) {
                return Err(format!(
                    "two migrations are named `{name}`: `{}::{}` and `{}::{}` (names must be unique)",
                    prev.module_path, prev.fn_name, def.module_path, def.fn_name
                ));
            }
        }
        Ok(Migrations { by_name })
    }

    pub fn get(&self, name: &str) -> Option<&'static MigrationDef> {
        self.by_name.get(name).copied()
    }

    /// Every migration with its name, in name order (the order they run in).
    pub fn iter(&self) -> impl Iterator<Item = (&str, &'static MigrationDef)> {
        self.by_name.iter().map(|(n, d)| (n.as_str(), *d))
    }

    /// The migrations of one table, in name order.
    pub fn for_table<'a>(&'a self, table: &'a str) -> impl Iterator<Item = (&'a str, &'static MigrationDef)> + 'a {
        self.iter().filter(move |(_, d)| d.table == table)
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// Names are stable ids: 1..=128 characters out of `A-Z a-z 0-9 _ - . : /`.
fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 128 {
        return Err(format!("the name `{name}` must have 1 to 128 characters"));
    }
    if let Some(c) = name.chars().find(|c| !(c.is_ascii_alphanumeric() || "_-.:/".contains(*c))) {
        return Err(format!("the name `{name}` contains `{c}` (allowed: letters, digits and `_ - . : /`)"));
    }
    Ok(())
}

/// The migrations linked into the binary, validated once (unique names, valid names and batch sizes).
pub fn registry() -> crate::Result<&'static Migrations> {
    static REGISTRY: OnceLock<Result<Migrations, String>> = OnceLock::new();
    REGISTRY.get_or_init(|| Migrations::build(&MIGRATIONS)).as_ref().map_err(|e| anyhow::anyhow!("{e}"))
}

/// The declared migrations as `(name, table)`, checked against the tables defined in code. `App::open` passes them
/// to the database for the startup schema check.
pub(crate) fn declared(tables: &[TableDef]) -> crate::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (name, def) in registry()?.iter() {
        if !tables.iter().any(|t| t.name == def.table) {
            anyhow::bail!("migration `{name}` migrates table `{}`, which is not defined in code", def.table);
        }
        out.push((name.to_string(), def.table.to_string()));
    }
    Ok(out)
}

/// Which declared migrations (`(name, table)`) are done and which still have to run, from the `_migrations` rows
/// (read raw: this runs while the database opens).
pub(crate) fn coverage(
    conn: &Connection,
    catalog: &Catalog,
    declared: &[(String, String)],
) -> crate::Result<schema_check::Migrations> {
    let mut states: BTreeMap<String, String> = BTreeMap::new();
    if let Ok(table) = catalog.table(<MigrationRecord as crate::Table>::NAME) {
        for (_, json) in crate::db::storage::table_documents(conn, table.number)? {
            let row: Value = serde_json::from_str(&json)?;
            if let (Some(name), Some(state)) = (row["name"].as_str(), row["state"].as_str()) {
                states.insert(name.to_string(), state.to_string());
            }
        }
    }
    let mut out = schema_check::Migrations::default();
    for (name, table) in declared {
        let done = states.get(name).is_some_and(|s| s == MigrationState::Done.as_str());
        let map = if done { &mut out.done } else { &mut out.pending };
        map.entry(table.clone()).or_default().push(name.clone());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------------------------------
// Reports and summaries
// ---------------------------------------------------------------------------------------------------------------------

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub enum RunOutcome {
    /// Every document was processed; the migration is done.
    Completed,
    /// It failed on a document (see `error`, `failedDocument`); fix it and run again to resume.
    Failed,
    /// It was canceled during the run.
    Canceled,
    /// It was already done: nothing ran.
    AlreadyDone,
    /// Another runner is working on it: nothing ran.
    AlreadyRunning,
    /// It stopped after `RunOptions::max_batches` batches, still running (as if the process had stopped).
    Stopped,
    /// A dry run went through every document; nothing was written.
    DryRun,
}

/// A document a dry run would change.
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct ChangeExample {
    pub id: String,
    pub before: Value,
    /// `null` when the migration would delete it.
    pub after: Option<Value>,
}

/// The result of running one migration (or of a dry run).
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct MigrationReport {
    pub name: String,
    pub table: String,
    pub outcome: RunOutcome,
    /// Batches committed by this run (a dry run: batches run and rolled back).
    pub batches: u64,
    /// Documents processed by this run.
    pub processed: u64,
    /// Documents patched, replaced or deleted by this run (a dry run: that would be).
    pub changed: u64,
    pub error: Option<String>,
    pub failed_document: Option<String>,
    /// The table's schema check after its last migration finished (see `MigrationRecord::schema_check`).
    pub schema_check: Option<String>,
    /// Dry runs: up to [`MAX_EXAMPLES`] documents that would change, before and after.
    pub examples: Vec<ChangeExample>,
}

/// A migration as the dashboard lists it: declared in code and/or recorded in `_migrations`.
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct MigrationSummary {
    pub name: String,
    pub table: String,
    /// Still declared in code (a row can outlive its migration).
    pub declared: bool,
    /// `null` when not declared.
    pub batch_size: Option<u64>,
    #[ts(type = "\"pending\" | \"running\" | \"done\" | \"failed\" | \"canceled\"")]
    pub state: MigrationState,
    pub processed: u64,
    pub changed: u64,
    /// Documents in the table now (for a progress bar: `processed / documents`).
    pub documents: u64,
    pub started_at: Option<f64>,
    pub finished_at: Option<f64>,
    pub error: Option<String>,
    pub failed_document: Option<String>,
    pub schema_check: Option<String>,
    /// The table is migrating: it opened with documents that don't match its type (lenient writes).
    pub table_migrating: bool,
}

async fn find_row(db: &crate::QueryDb, name: &str) -> crate::Result<Option<Doc<MigrationRecord>>> {
    db.query::<MigrationRecord>()
        .with_index(MigrationRecord::by_name, |q| q.eq(MigrationRecord::name, name.to_string()))
        .first()
        .await
}

/// Every declared migration (in name order), then rows of migrations no longer declared.
async fn summaries(db: &crate::QueryDb) -> crate::Result<Vec<MigrationSummary>> {
    let mut rows: BTreeMap<String, Doc<MigrationRecord>> =
        db.query::<MigrationRecord>().collect().await?.into_iter().map(|r| (r.name.clone(), r)).collect();
    let catalog = db.catalog().clone();
    let mut out = Vec::new();
    let entry = async |name: &str, table: &str, def: Option<&MigrationDef>, row: Option<Doc<MigrationRecord>>| {
        let documents = match catalog.table(table) {
            Ok(_) => db.count(table).await?,
            Err(_) => 0,
        };
        let row = row.map(|r| r.value);
        Ok::<_, Error>(MigrationSummary {
            name: name.to_string(),
            table: table.to_string(),
            declared: def.is_some(),
            batch_size: def.map(|d| d.batch_size as u64),
            state: row.as_ref().map_or(MigrationState::Pending, |r| r.state),
            processed: row.as_ref().map_or(0, |r| r.processed),
            changed: row.as_ref().map_or(0, |r| r.changed),
            documents,
            started_at: row.as_ref().and_then(|r| r.started_at),
            finished_at: row.as_ref().and_then(|r| r.finished_at),
            error: row.as_ref().and_then(|r| r.error.clone()),
            failed_document: row.as_ref().and_then(|r| r.failed_document.clone()),
            schema_check: row.as_ref().and_then(|r| r.schema_check.clone()),
            table_migrating: catalog.is_migrating(table),
        })
    };
    for (name, def) in registry()?.iter() {
        let row = rows.remove(name);
        out.push(entry(name, def.table, Some(def), row).await?);
    }
    for (name, row) in rows {
        let table = row.table.clone();
        out.push(entry(&name, &table, None, Some(row)).await?);
    }
    Ok(out)
}

impl App {
    /// Run a migration by reference (see [`RunOptions`]): `app.run_migration::<add_priority>(Default::default())`.
    pub async fn run_migration<M: MigrationRef>(&self, options: RunOptions) -> crate::Result<MigrationReport> {
        self.run_migration_named(&M::name(), options).await
    }

    /// Run the migration named `name`. Errors only for an unknown name or a database failure; a migration that fails
    /// on a document is a report with `outcome: Failed`.
    pub async fn run_migration_named(&self, name: &str, options: RunOptions) -> crate::Result<MigrationReport> {
        runner::run(self, name, &options).await
    }

    /// Run every migration that isn't done, in name order, stopping after the first that doesn't complete (fails or
    /// is canceled). With `dry_run`, every one is dry-run (each sees the data as it is now).
    pub async fn run_pending_migrations(&self, options: RunOptions) -> crate::Result<Vec<MigrationReport>> {
        runner::run_pending(self, &options).await
    }

    /// Every migration with its state, as `_system/migrations:list` returns it.
    pub async fn migration_summaries(&self) -> crate::Result<Vec<MigrationSummary>> {
        let snapshot = self.db().snapshot().await?;
        let db = crate::QueryDb::new(crate::db::query::Source::Snapshot(snapshot), self.db().catalog().clone());
        summaries(&db).await
    }
}

/// Startup work, run by `App::open`: rows still *running* (the process stopped during a run) become *pending*; the
/// next run resumes at their cursor.
pub(crate) async fn startup(app: &App) -> crate::Result<()> {
    let recovered = runner::recover_interrupted(app).await?;
    if recovered > 0 {
        tracing::warn!("{recovered} migration(s) were interrupted by a restart; they resume where they stopped on the next run");
    }
    let migrating = app.db().catalog().migrating_tables();
    if !migrating.is_empty() {
        tracing::warn!(
            tables = ?migrating,
            "tables with documents that don't match their type are waiting for their migrations; run them with \
             `ferrix migrate`, `serve --migrate` or the dashboard"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------------
// System functions (`_system/migrations:*`)
// ---------------------------------------------------------------------------------------------------------------------

fn user_error(code: &str, message: impl Into<String>) -> Error {
    UserError::new(code, message).into()
}

fn lookup(name: &str) -> crate::Result<&'static MigrationDef> {
    registry()?.get(name).ok_or_else(|| user_error("not_found", format!("No migration is named `{name}`")))
}

/// What `start` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub enum StartOutcome {
    Started,
    AlreadyRunning,
    AlreadyDone,
}

/// Mark a migration running (creating its row), unless it is running or done. With `restart`, a migration in any
/// state but running starts over from the first document (cursor and counts reset).
#[internal_mutation]
pub async fn start(ctx: &MutationCtx, name: String, restart: bool) -> Result<StartOutcome> {
    let def = lookup(&name)?;
    let now = ctx.now();
    let Some(row) = find_row(&ctx.db, &name).await? else {
        ctx.db
            .insert(MigrationRecord {
                name,
                table: def.table.to_string(),
                state: MigrationState::Running,
                cursor: None,
                processed: 0,
                changed: 0,
                started_at: Some(now),
                finished_at: None,
                error: None,
                failed_document: None,
                schema_check: None,
            })
            .await?;
        return Ok(StartOutcome::Started);
    };
    let id = row.id;
    let mut value = row.value;
    match value.state {
        MigrationState::Running => return Ok(StartOutcome::AlreadyRunning),
        MigrationState::Done if !restart => return Ok(StartOutcome::AlreadyDone),
        _ => {}
    }
    if restart {
        value.cursor = None;
        value.processed = 0;
        value.changed = 0;
        value.started_at = Some(now);
        value.finished_at = None;
        value.schema_check = None;
    }
    value.table = def.table.to_string();
    value.state = MigrationState::Running;
    value.started_at = value.started_at.or(Some(now));
    value.error = None;
    value.failed_document = None;
    ctx.db.replace(id, value).await?;
    Ok(StartOutcome::Started)
}

/// What one batch did.
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct BatchOutcome {
    /// The batch ran (false: the migration is no longer running, e.g. canceled; nothing was done).
    pub ran: bool,
    #[ts(type = "\"pending\" | \"running\" | \"done\" | \"failed\" | \"canceled\" | null")]
    pub state: Option<MigrationState>,
    pub processed: u64,
    pub changed: u64,
    /// The scan reached the end of the table.
    pub done: bool,
    /// Where the next batch starts.
    pub cursor: Option<String>,
    pub schema_check: Option<String>,
    pub examples: Vec<ChangeExample>,
}

/// One batch of a migration, in this mutation's transaction (see the module docs). A real run reads and advances
/// the cursor stored in `_migrations`; a dry run (whose transaction the runner rolls back) starts at `cursor`.
#[internal_mutation]
pub async fn run_batch(ctx: &MutationCtx, name: String, dry_run: bool, cursor: Option<String>) -> Result<BatchOutcome> {
    runner::batch(ctx, lookup(&name)?, &name, dry_run, cursor).await
}

/// Mark a migration failed (its failing batch was rolled back).
#[internal_mutation]
pub async fn fail(ctx: &MutationCtx, name: String, error: String, document: Option<String>) -> Result<()> {
    let row = find_row(&ctx.db, &name).await?.with_context(|| format!("migration `{name}` has no _migrations row"))?;
    let mut value = row.value;
    value.state = MigrationState::Failed;
    value.error = Some(crate::scheduler::store::truncate(error));
    value.failed_document = document;
    ctx.db.replace(row.id, value).await
}

/// Every migration: those declared in code (in name order, the order they run in), then rows of migrations no
/// longer declared. Live: progress updates with every batch. Admin only.
#[query]
pub async fn list(ctx: &QueryCtx) -> Result<Vec<MigrationSummary>> {
    ctx.auth.require_admin()?;
    summaries(&ctx.db).await
}

/// One migration by name, or null. Admin only.
#[query]
pub async fn status(ctx: &QueryCtx, name: String) -> Result<Option<MigrationSummary>> {
    ctx.auth.require_admin()?;
    Ok(summaries(&ctx.db).await?.into_iter().find(|m| m.name == name))
}

/// The result of `run`.
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct RunResult {
    /// Migrations now running in the background (follow them with `list`).
    pub started: Vec<String>,
    /// Dry runs: one report per migration. Real runs: the migrations that didn't start (`alreadyDone`,
    /// `alreadyRunning`).
    pub reports: Vec<MigrationReport>,
}

/// Run the migration `name`, or every migration that isn't done (in name order) when `name` is omitted. Real runs
/// continue in the background (`started`); dry runs run to the end and return their reports. Admin only.
#[action]
pub async fn run(ctx: &ActionCtx, name: Option<String>, dry_run: Option<bool>, restart: Option<bool>) -> Result<RunResult> {
    ctx.auth.require_admin()?;
    let options = RunOptions { dry_run: dry_run.unwrap_or(false), restart: restart.unwrap_or(false), ..Default::default() };
    if let Some(name) = &name {
        lookup(name)?;
    }
    if options.dry_run {
        let reports = match name {
            Some(name) => vec![ctx.app.run_migration_named(&name, options).await?],
            None => ctx.app.run_pending_migrations(options).await?,
        };
        return Ok(RunResult { started: Vec::new(), reports });
    }
    runner::run_in_background(&ctx.app, name, options).await
}

/// Cancel a running migration: it stops after its current batch; the next run resumes from there. Returns
/// `"canceled"`, or `"notRunning"` if it wasn't running. Admin only.
#[mutation]
pub async fn cancel(ctx: &MutationCtx, name: String) -> Result<String> {
    ctx.auth.require_admin()?;
    lookup(&name)?;
    let Some(row) = find_row(&ctx.db, &name).await? else { return Ok("notRunning".into()) };
    if row.state != MigrationState::Running {
        return Ok("notRunning".into());
    }
    let mut value = row.value;
    value.state = MigrationState::Canceled;
    ctx.db.replace(row.id, value).await?;
    Ok("canceled".into())
}
