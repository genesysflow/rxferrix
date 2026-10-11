//! Running migrations: the batch (inside `_system/migrations:run_batch`), the loop that drives batches, dry runs,
//! and crash recovery. See the module docs of `crate::migrations` for the guarantees.

use anyhow::Context as _;
use serde_json::{Map, Value, json};

use super::{
    BatchOutcome, ChangeExample, DocChange, MAX_EXAMPLES, MigrationRecord, MigrationReport, MigrationState, RunOutcome,
    RunResult, StartOutcome, blocked_error, find_row, registry, unmet_dependencies,
};
use crate::db::query::MutationDb;
use crate::db::schema_check;
use crate::private::MigrationDef;
use crate::runtime::{App, Identity, MutationHook};
use crate::schema::{CREATION_TIME_FIELD, ID_FIELD};
use crate::{FunctionRef, MutationCtx, PaginationOpts};

/// How to run a migration.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Run the migration over every document and report what it would change (with examples), writing nothing:
    /// every batch is rolled back, including writes the migration makes through `ctx.db`. A dry run doesn't touch
    /// `_migrations` and always starts at the first document.
    pub dry_run: bool,
    /// Start over from the first document, also if the migration is done (counts are reset).
    pub restart: bool,
    /// Run a migration even if some it runs after (`#[migration(after = [..])]`) are not done.
    pub force: bool,
    /// Stop after this many batches, leaving the migration *running*, as if the process had stopped there (a
    /// restart makes it *pending*; the next run resumes). For tests.
    pub max_batches: Option<u64>,
}

/// A document the migration failed on: the batch is rolled back and the migration marked failed.
#[derive(Debug)]
struct DocFailure {
    document: String,
    error: anyhow::Error,
}

impl std::fmt::Display for DocFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "document {}: {:#}", self.document, self.error)
    }
}

impl std::error::Error for DocFailure {}

/// One batch (the body of `run_batch`).
pub(super) async fn batch(
    ctx: &MutationCtx,
    def: &'static MigrationDef,
    name: &str,
    dry_run: bool,
    cursor: Option<String>,
) -> crate::Result<BatchOutcome> {
    let row = if dry_run {
        None
    } else {
        let row = find_row(&ctx.db, name).await?.with_context(|| format!("migration `{name}` was not started"))?;
        if row.state != MigrationState::Running {
            return Ok(BatchOutcome {
                ran: false,
                state: Some(row.state),
                processed: 0,
                changed: 0,
                done: false,
                cursor: row.value.cursor,
                schema_check: None,
                examples: Vec::new(),
            });
        }
        Some(row)
    };
    let cursor = match &row {
        Some(row) => row.cursor.clone(),
        None => cursor,
    };
    let page = ctx
        .db
        .query_table(def.table)
        .paginate(PaginationOpts { num_items: def.batch_size, cursor, end_cursor: None })
        .await?;
    let mut changed = 0;
    let mut examples = Vec::new();
    for doc in &page.page {
        let id = doc[ID_FIELD].as_str().context("stored document without an _id")?.to_string();
        let applied = async {
            let change = (def.handler)(ctx, doc.clone()).await?;
            apply(&ctx.db, &id, doc, change).await
        }
        .await
        .map_err(|error| DocFailure { document: id.clone(), error })?;
        if applied {
            changed += 1;
            if dry_run && examples.len() < MAX_EXAMPLES {
                let after = ctx.db.get_raw(&id).await?.map(|(_, v)| v);
                examples.push(ChangeExample { id, before: doc.clone(), after });
            }
        }
    }
    let processed = page.page.len() as u64;
    let done = page.is_done;
    let mut outcome = BatchOutcome {
        ran: true,
        state: None,
        processed,
        changed,
        done,
        cursor: Some(page.continue_cursor.clone()),
        schema_check: None,
        examples,
    };
    if let Some(row) = row {
        let id = row.id;
        let mut value = row.value;
        value.cursor = Some(page.continue_cursor);
        value.processed += processed;
        value.changed += changed;
        if done {
            value.state = MigrationState::Done;
            value.finished_at = Some(ctx.now());
            value.schema_check = recheck(ctx, def.table, name).await?;
            outcome.schema_check = value.schema_check.clone();
        }
        outcome.state = Some(value.state);
        ctx.db.replace(id, value).await?;
    }
    Ok(outcome)
}

/// Apply a migration's change to document `id`. True if it wrote something.
async fn apply(db: &MutationDb, id: &str, doc: &Value, change: DocChange) -> crate::Result<bool> {
    match change {
        DocChange::Keep => Ok(false),
        DocChange::Patch(Value::Object(patch)) if patch.is_empty() => Ok(false),
        DocChange::Patch(patch @ Value::Object(_)) => {
            db.patch_raw(id, patch).await?;
            Ok(true)
        }
        DocChange::Replace(Value::Object(fields)) => {
            db.replace_raw(id, Value::Object(user_fields(doc, fields)?)).await?;
            Ok(true)
        }
        DocChange::Delete => {
            // The migration may have deleted it through `ctx.db` already.
            if db.get_raw(id).await?.is_some() {
                db.delete_raw(id).await?;
            }
            Ok(true)
        }
        DocChange::Patch(other) | DocChange::Replace(other) => {
            anyhow::bail!("a migration's patch or replacement must be a JSON object, got {other}")
        }
    }
}

/// A replacement without its system fields, which must be unchanged if present (handy when the migration edits the
/// document it was given and returns it whole).
fn user_fields(doc: &Value, mut fields: Map<String, Value>) -> crate::Result<Map<String, Value>> {
    for sys in [ID_FIELD, CREATION_TIME_FIELD] {
        if let Some(v) = fields.remove(sys)
            && Some(&v) != doc.get(sys)
        {
            anyhow::bail!("a migration can't change the system field `{sys}`");
        }
    }
    Ok(fields)
}

/// When the last pending migration of a migrating table finishes: validate every document of the table (in this
/// transaction, so it sees the batch's writes and nothing can slip in between). Passing records the fingerprint and
/// ends lenient mode; failing is recorded and logged (the next start refuses to open).
async fn recheck(ctx: &MutationCtx, table: &str, name: &str) -> crate::Result<Option<String>> {
    let catalog = ctx.db.catalog().clone();
    if !catalog.is_migrating(table) {
        return Ok(None);
    }
    for (other, _) in registry()?.for_table(table) {
        if other != name && find_row(&ctx.db, other).await?.map(|r| r.state) != Some(MigrationState::Done) {
            tracing::info!(table, migration = other, "the table's schema check waits for its other pending migrations");
            return Ok(None);
        }
    }
    let info = catalog.table(table)?.clone();
    let def = info.def.clone().with_context(|| format!("table `{table}` is not defined in code"))?;
    let bad = ctx.db.txn().with_state(|s| {
        let bad = schema_check::check_table(s.conn(), info.number, &def)?;
        if bad.is_none() {
            schema_check::record_fingerprint(s.conn(), &def)?;
        }
        Ok(bad)
    })?;
    match bad {
        None => {
            // Before the commit: if it failed, the table would just be checked strictly (and again at the next
            // start, its fingerprint not being recorded): never less safe.
            catalog.set_migrating(table, false);
            tracing::info!(table, "migrations done; every document matches the table's type again");
            Ok(Some("passed".to_string()))
        }
        Some(bad) => {
            let report = schema_check::format_table(&bad);
            tracing::error!(
                table,
                "migrations done, but documents still don't match the table's type; the next start will refuse to \
                 open until another migration fixes them:\n{report}"
            );
            Ok(Some(crate::scheduler::store::truncate(format!("failed: {report}"))))
        }
    }
}

/// Rolls a dry run's batch back after it succeeded.
struct RollBack;

impl MutationHook for RollBack {
    fn before<'a>(&'a self, _: &'a MutationDb) -> futures::future::BoxFuture<'a, crate::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn after<'a>(&'a self, _: &'a MutationDb, _: &'a Value) -> futures::future::BoxFuture<'a, crate::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn commit(&self) -> bool {
        false
    }
}

/// Run one batch through the normal mutation path (timeout, panics, execution log, write log).
async fn call_batch(app: &App, name: &str, dry_run: bool, cursor: Option<String>) -> crate::Result<BatchOutcome> {
    let args = json!({ "name": name, "dryRun": dry_run, "cursor": cursor });
    let hook: Option<&dyn MutationHook> = if dry_run { Some(&RollBack) } else { None };
    let path = <super::run_batch as FunctionRef>::path();
    let run = app.call_mutation_hooked(&path, args, &Identity::default(), true, None, hook).await;
    Ok(serde_json::from_value(run.result?.0)?)
}

async fn call<F: crate::MutationFunction>(app: &App, args: F::Args) -> crate::Result<F::Output> {
    let run = app.call_mutation(&F::path(), serde_json::to_value(args)?, &Identity::default(), true, None).await;
    Ok(serde_json::from_value(run.result?.0)?)
}

fn report(def: &MigrationDef, name: &str, outcome: RunOutcome) -> MigrationReport {
    MigrationReport {
        name: name.to_string(),
        table: def.table.to_string(),
        outcome,
        batches: 0,
        processed: 0,
        changed: 0,
        error: None,
        failed_document: None,
        schema_check: None,
        examples: Vec::new(),
    }
}

/// The failing document and message of a batch error.
fn failure(e: &anyhow::Error) -> (String, Option<String>) {
    match e.downcast_ref::<DocFailure>() {
        Some(f) => (format!("{:#}", f.error), Some(f.document.clone())),
        None => (format!("{e:#}"), None),
    }
}

/// Run one migration. A migration whose dependencies aren't done (without `force`) is an error, the
/// `dependencies_not_done` user error.
pub(super) async fn run(app: &App, name: &str, options: &RunOptions) -> crate::Result<MigrationReport> {
    let rep = run_or_block(app, name, options).await?;
    if rep.outcome == RunOutcome::Blocked {
        let def = super::lookup(name)?;
        return Err(blocked_error(name, &unmet(app, def).await?));
    }
    Ok(rep)
}

/// The dependencies of `def` that aren't done, at the latest snapshot.
async fn unmet(app: &App, def: &MigrationDef) -> crate::Result<Vec<(String, MigrationState)>> {
    let snapshot = app.db().snapshot().await?;
    let db = crate::QueryDb::new(crate::db::query::Source::Snapshot(snapshot), app.db().catalog().clone());
    unmet_dependencies(&db, def).await
}

/// Run one migration; a migration whose dependencies aren't done is a `Blocked` report (with the reason in `error`).
async fn run_or_block(app: &App, name: &str, options: &RunOptions) -> crate::Result<MigrationReport> {
    let def = super::lookup(name)?;
    if options.dry_run {
        return dry_run(app, def, name).await;
    }
    match call::<super::start>(app, (name.to_string(), options.restart, Some(options.force)).into()).await? {
        StartOutcome::Started => drive(app, def, name, options.max_batches).await,
        StartOutcome::AlreadyDone => Ok(report(def, name, RunOutcome::AlreadyDone)),
        StartOutcome::AlreadyRunning => Ok(report(def, name, RunOutcome::AlreadyRunning)),
        StartOutcome::Blocked => {
            let mut rep = report(def, name, RunOutcome::Blocked);
            rep.error = Some(format!("{:#}", blocked_error(name, &unmet(app, def).await?)));
            Ok(rep)
        }
    }
}

/// Batches until the migration is done, fails, is canceled, or `max_batches` ran.
async fn drive(
    app: &App,
    def: &'static MigrationDef,
    name: &str,
    max_batches: Option<u64>,
) -> crate::Result<MigrationReport> {
    let mut rep = report(def, name, RunOutcome::Completed);
    tracing::info!(migration = name, table = def.table, "migration started");
    loop {
        if max_batches.is_some_and(|max| rep.batches >= max) {
            rep.outcome = RunOutcome::Stopped;
            return Ok(rep);
        }
        match call_batch(app, name, false, None).await {
            Ok(b) if !b.ran => {
                rep.outcome = match b.state {
                    Some(MigrationState::Done) => RunOutcome::Completed,
                    Some(MigrationState::Failed) => RunOutcome::Failed,
                    _ => RunOutcome::Canceled,
                };
                tracing::info!(migration = name, state = ?b.state, "migration stopped");
                return Ok(rep);
            }
            Ok(b) => {
                rep.batches += 1;
                rep.processed += b.processed;
                rep.changed += b.changed;
                if b.done {
                    rep.schema_check = b.schema_check;
                    tracing::info!(
                        migration = name,
                        processed = rep.processed,
                        changed = rep.changed,
                        "migration done"
                    );
                    return Ok(rep);
                }
            }
            Err(e) => {
                let (error, document) = failure(&e);
                tracing::error!(migration = name, document = ?document, "migration failed: {error}");
                call::<super::fail>(app, (name.to_string(), error.clone(), document.clone()).into()).await?;
                rep.outcome = RunOutcome::Failed;
                rep.error = Some(error);
                rep.failed_document = document;
                return Ok(rep);
            }
        }
        tokio::task::yield_now().await;
    }
}

async fn dry_run(app: &App, def: &'static MigrationDef, name: &str) -> crate::Result<MigrationReport> {
    let mut rep = report(def, name, RunOutcome::DryRun);
    let mut cursor = None;
    loop {
        match call_batch(app, name, true, cursor.take()).await {
            Ok(b) => {
                rep.batches += 1;
                rep.processed += b.processed;
                rep.changed += b.changed;
                let room = MAX_EXAMPLES.saturating_sub(rep.examples.len());
                rep.examples.extend(b.examples.into_iter().take(room));
                if b.done {
                    return Ok(rep);
                }
                cursor = b.cursor;
            }
            Err(e) => {
                let (error, document) = failure(&e);
                rep.outcome = RunOutcome::Failed;
                rep.error = Some(error);
                rep.failed_document = document;
                return Ok(rep);
            }
        }
        tokio::task::yield_now().await;
    }
}

/// The migrations that aren't done, in run order.
async fn not_done(app: &App) -> crate::Result<Vec<String>> {
    let summaries = app.migration_summaries().await?;
    Ok(summaries.into_iter().filter(|m| m.declared && m.state != MigrationState::Done).map(|m| m.name).collect())
}

pub(super) async fn run_pending(app: &App, options: &RunOptions) -> crate::Result<Vec<MigrationReport>> {
    let mut reports = Vec::new();
    for name in not_done(app).await? {
        let rep = run_or_block(app, &name, options).await?;
        let stop = matches!(
            rep.outcome,
            RunOutcome::Failed | RunOutcome::Canceled | RunOutcome::Stopped | RunOutcome::Blocked
        );
        reports.push(rep);
        if stop && !options.dry_run {
            break;
        }
    }
    Ok(reports)
}

/// Start `name` (or every migration that isn't done) and drive it in a background task.
pub(super) async fn run_in_background(
    app: &App,
    name: Option<String>,
    options: RunOptions,
) -> crate::Result<RunResult> {
    let mut result = RunResult { started: Vec::new(), reports: Vec::new() };
    match name {
        Some(name) => {
            let def = super::lookup(&name)?;
            match call::<super::start>(app, (name.clone(), options.restart, Some(options.force)).into()).await? {
                StartOutcome::Started => {
                    let app = app.clone();
                    result.started.push(name.clone());
                    tokio::spawn(async move {
                        if let Err(e) = drive(&app, def, &name, None).await {
                            tracing::error!(migration = %name, "migration runner failed: {e:#}");
                        }
                    });
                }
                StartOutcome::AlreadyDone => result.reports.push(report(def, &name, RunOutcome::AlreadyDone)),
                StartOutcome::AlreadyRunning => result.reports.push(report(def, &name, RunOutcome::AlreadyRunning)),
                StartOutcome::Blocked => return Err(blocked_error(&name, &unmet(app, def).await?)),
            }
        }
        None => {
            result.started = not_done(app).await?;
            let app = app.clone();
            tokio::spawn(async move {
                if let Err(e) = run_pending(&app, &options).await {
                    tracing::error!("migration runner failed: {e:#}");
                }
            });
        }
    }
    Ok(result)
}

/// Rows left *running* by a process that stopped become *pending* (cursor and counts kept). Returns how many.
pub(super) async fn recover_interrupted(app: &App) -> crate::Result<usize> {
    let txn = app.db().begin_write().await?;
    let db = MutationDb::new(txn.clone());
    let result = async {
        let mut n = 0;
        for row in db.query::<MigrationRecord>().collect().await? {
            if row.state == MigrationState::Running {
                let mut value = row.value;
                value.state = MigrationState::Pending;
                db.replace(row.id, value).await?;
                n += 1;
            }
        }
        crate::Result::Ok(n)
    }
    .await;
    match result {
        Ok(n) => {
            txn.commit_with(|_, _| Ok(())).await?;
            Ok(n)
        }
        Err(e) => {
            txn.rollback();
            Err(e)
        }
    }
}
