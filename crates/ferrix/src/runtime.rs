//! The function runtime: the registry of functions, the three contexts, and [`App`], which runs functions against
//! the database.

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Context;
use futures::FutureExt;
use serde_json::Value;

use crate::db::dedup::{self, IdempotencyKey};
use crate::db::query::{MutationDb, QueryDb, Source};
use crate::db::readset::ReadSet;
use crate::db::{Database, Snapshot};
use crate::error::{FunctionNotFound, MutationTimeout};
use crate::exec_log::{LogRetention, Logs};
use crate::sync::subscriptions::{SubscriptionManager, SubscriptionStats};
use crate::private::{FUNCTIONS, FunctionDef, FunctionKind, Handler, TABLES, Visibility};
use crate::{ActionFunction, MutationFunction, QueryFunction, TableDef};

// ---------------------------------------------------------------------------------------------------------------------
// Identity and contexts
// ---------------------------------------------------------------------------------------------------------------------

// `Identity` (who is calling) and `Auth` (`ctx.auth`) live in the auth module.
pub use crate::auth::{Auth, Identity};
use crate::auth::identity::AuthDeps;

/// Context of a query: read-only access to one consistent snapshot.
pub struct QueryCtx {
    pub db: QueryDb,
    pub auth: Auth,
    now: f64,
}

impl QueryCtx {
    /// The commit time (ms since the epoch) of the snapshot the query runs at. Deterministic per ts.
    pub fn now(&self) -> f64 {
        self.now
    }
}

/// Context of a mutation: a serializable read-write transaction.
pub struct MutationCtx {
    pub db: MutationDb,
    pub auth: Auth,
    now: f64,
}

impl MutationCtx {
    /// The time (ms since the epoch) this mutation commits at.
    pub fn now(&self) -> f64 {
        self.now
    }
}

/// Context of an action: no transaction; can call queries, mutations and other actions (including internal ones).
pub struct ActionCtx {
    pub auth: Auth,
    pub(crate) app: App,
    identity: Identity,
}

impl ActionCtx {
    /// Run a query (public or internal) in its own snapshot: `ctx.run_query::<list>((x,))`.
    pub async fn run_query<F: QueryFunction>(&self, args: impl Into<F::Args>) -> crate::Result<F::Output> {
        self.app.typed_query::<F>(args.into(), &self.identity).await
    }

    /// Run a mutation (public or internal) in its own transaction. Its log lines also appear in this action's.
    pub async fn run_mutation<F: MutationFunction>(&self, args: impl Into<F::Args>) -> crate::Result<F::Output> {
        Ok(self.app.typed_mutation::<F>(args.into(), &self.identity).await?.0)
    }

    /// Run another action (public or internal).
    pub async fn run_action<F: ActionFunction>(&self, args: impl Into<F::Args>) -> crate::Result<F::Output> {
        self.app.typed_action::<F>(args.into(), &self.identity).await
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------------------------------------------------

/// A registered function as listed for codegen and the `functions` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionInfo {
    pub path: String,
    pub kind: FunctionKind,
    pub visibility: Visibility,
    /// TS type of the args object.
    pub args_ts: String,
    /// TS type of the return value.
    pub output_ts: String,
}

/// All functions registered through `__private::FUNCTIONS`, keyed by path.
pub struct Registry {
    by_path: BTreeMap<String, &'static FunctionDef>,
}

impl Registry {
    fn build(defs: &'static [FunctionDef]) -> Result<Registry, String> {
        let mut by_path = BTreeMap::new();
        for def in defs {
            let path = def.path();
            if by_path.insert(path.clone(), def).is_some() {
                return Err(format!("two functions are registered at path `{path}`"));
            }
        }
        Ok(Registry { by_path })
    }

    pub fn get(&self, path: &str) -> Option<&'static FunctionDef> {
        self.by_path.get(path).copied()
    }

    /// Every function definition with its path, sorted by path.
    pub(crate) fn defs(&self) -> impl Iterator<Item = (&str, &'static FunctionDef)> {
        self.by_path.iter().map(|(path, def)| (path.as_str(), *def))
    }

    /// Every function with its kind, visibility and TS signature, sorted by path.
    pub fn list(&self) -> Vec<FunctionInfo> {
        self.by_path
            .iter()
            .map(|(path, def)| {
                let (args_ts, output_ts) = (def.ts)();
                FunctionInfo { path: path.clone(), kind: def.handler.kind(), visibility: def.visibility, args_ts, output_ts }
            })
            .collect()
    }
}

/// The global registry built from the linked-in functions.
pub fn registry() -> crate::Result<&'static Registry> {
    static REGISTRY: OnceLock<Result<Registry, String>> = OnceLock::new();
    REGISTRY.get_or_init(|| Registry::build(&FUNCTIONS)).as_ref().map_err(|e| anyhow::anyhow!("{e}"))
}

/// The table definitions linked into the binary.
pub fn registered_tables() -> Vec<TableDef> {
    TABLES.iter().map(|f| f()).collect()
}

// ---------------------------------------------------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AppOptions {
    /// Show real error messages to clients instead of "Server Error", and send functions' log lines to clients.
    pub dev: bool,
    /// Number of read-only connections.
    pub readers: usize,
    /// Sign-in methods, token lifetimes, email and OAuth settings (`crate::auth`).
    pub auth: crate::auth::AuthConfig,
    /// A mutation still running after this long is rolled back with a [`MutationTimeout`] error. Default: 5s. Only
    /// checked at `.await` points: synchronous code is never preempted.
    pub mutation_timeout: Duration,
    /// Open even if existing documents may not match changed table types (see `db::schema_check`).
    pub skip_schema_check: bool,
    /// Retention of the execution log in `aux.db`. Default: 7 days or 200k rows.
    pub log_retention: LogRetention,
}

/// Default [`AppOptions::mutation_timeout`].
pub const DEFAULT_MUTATION_TIMEOUT: Duration = Duration::from_secs(5);

impl Default for AppOptions {
    fn default() -> Self {
        AppOptions {
            dev: false,
            readers: crate::db::DEFAULT_READERS,
            auth: Default::default(),
            mutation_timeout: DEFAULT_MUTATION_TIMEOUT,
            skip_schema_check: false,
            log_retention: LogRetention::default(),
        }
    }
}

/// The result of running a query: its value or error, what it read, the ts it ran at, and its log lines.
pub struct QueryRun {
    pub result: crate::Result<Value>,
    pub read_set: ReadSet,
    pub ts: u64,
    pub log_lines: Vec<String>,
    /// How much of the caller's identity the run read through `ctx.auth` (decides who may share the result).
    pub(crate) auth_deps: AuthDeps,
}

impl QueryRun {
    fn not_run(e: anyhow::Error, ts: u64) -> QueryRun {
        QueryRun { result: Err(e), read_set: ReadSet::new(), ts, log_lines: Vec::new(), auth_deps: AuthDeps::None }
    }
}

/// The result of running a mutation: `(value, commit ts)` or the error, and its log lines.
pub struct MutationRun {
    pub result: crate::Result<(Value, u64)>,
    pub log_lines: Vec<String>,
}

/// The result of running an action, and its log lines (including those of the functions it called).
pub struct ActionRun {
    pub result: crate::Result<Value>,
    pub log_lines: Vec<String>,
}

/// A running Ferrix application: database + function registry. Cheap to clone.
#[derive(Clone)]
pub struct App {
    inner: Arc<AppInner>,
}

struct AppInner {
    db: Database,
    registry: &'static Registry,
    options: AppOptions,
    auth: crate::auth::AuthService,
    logs: Logs,
    dir: PathBuf,
    metrics: Arc<Metrics>,
    started_at: f64,
    /// Started on first use (it needs a tokio runtime).
    subscriptions: OnceLock<SubscriptionManager>,
}

/// Live counters for the dashboard's overview (`App::metrics`). Sockets are counted by the WebSocket server; the
/// subscription counts are published by the subscription manager (`crate::sync::subscriptions`).
#[derive(Debug, Default)]
pub struct Metrics {
    sockets: AtomicU64,
    connections: AtomicU64,
    subscriptions: AtomicU64,
    shared_queries: AtomicU64,
    index_ranges: AtomicU64,
    rounds: AtomicU64,
    reruns: AtomicU64,
    last_round_reruns: AtomicU64,
}

impl Metrics {
    /// Open WebSocket connections.
    pub fn sockets(&self) -> u64 {
        self.sockets.load(Ordering::Relaxed)
    }

    /// Live query subscriptions across all connections (`(connection, queryId)` pairs).
    pub fn subscriptions(&self) -> u64 {
        self.subscriptions.load(Ordering::Relaxed)
    }

    /// Distinct shared queries: each runs once per invalidation, however many subscriptions it serves.
    pub fn shared_queries(&self) -> u64 {
        self.shared_queries.load(Ordering::Relaxed)
    }

    /// All subscription manager counters.
    pub fn subscription_stats(&self) -> SubscriptionStats {
        SubscriptionStats {
            connections: self.connections.load(Ordering::Relaxed),
            shared_queries: self.shared_queries(),
            subscriptions: self.subscriptions(),
            index_ranges: self.index_ranges.load(Ordering::Relaxed),
            rounds: self.rounds.load(Ordering::Relaxed),
            reruns: self.reruns.load(Ordering::Relaxed),
            last_round_reruns: self.last_round_reruns.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn socket_opened(&self) {
        self.sockets.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn socket_closed(&self) {
        self.sockets.fetch_sub(1, Ordering::Relaxed);
    }

    pub(crate) fn set_subscription_counts(&self, connections: u64, shared_queries: u64, subscriptions: u64, ranges: u64) {
        self.connections.store(connections, Ordering::Relaxed);
        self.shared_queries.store(shared_queries, Ordering::Relaxed);
        self.subscriptions.store(subscriptions, Ordering::Relaxed);
        self.index_ranges.store(ranges, Ordering::Relaxed);
    }

    pub(crate) fn record_round(&self, reruns: u64) {
        self.rounds.fetch_add(1, Ordering::Relaxed);
        self.reruns.fetch_add(reruns, Ordering::Relaxed);
        self.last_round_reruns.store(reruns, Ordering::Relaxed);
    }
}

/// A weak reference to an [`App`] (held by its background tasks so they don't keep it alive).
#[derive(Clone)]
pub(crate) struct WeakApp(std::sync::Weak<AppInner>);

impl WeakApp {
    pub(crate) fn upgrade(&self) -> Option<App> {
        self.0.upgrade().map(|inner| App { inner })
    }
}

impl App {
    /// Open the database in `dir` (`data.db`, plus `aux.db` for the execution log) and reconcile it with the tables
    /// linked into the binary. Fails if existing documents don't match changed table types (see
    /// [`AppOptions::skip_schema_check`]).
    pub async fn open(dir: impl AsRef<Path>, options: AppOptions) -> crate::Result<App> {
        crate::log::ensure_installed();
        let registry = registry()?;
        let tables = registered_tables();
        let dir = dir.as_ref().to_path_buf();
        let data_dir = dir.clone();
        let auth = crate::auth::AuthService::new(&dir, options.auth.clone(), options.dev)?;
        let open_options =
            crate::db::OpenOptions { readers: options.readers, skip_schema_check: options.skip_schema_check };
        let retention = options.log_retention.clone();
        let (db, logs) = tokio::task::spawn_blocking(move || -> crate::Result<_> {
            let db = Database::open_with(&dir, &tables, &open_options)?;
            let logs = Logs::open(&dir.join("aux.db"), retention)?;
            Ok((db, logs))
        })
        .await??;
        let started_at = crate::db::wall_clock_ms();
        let metrics = Arc::new(Metrics::default());
        let subscriptions = OnceLock::new();
        Ok(App {
            inner: Arc::new(AppInner { db, registry, options, auth, logs, dir: data_dir, metrics, started_at, subscriptions }),
        })
    }

    /// The data directory (`data.db`, `aux.db`, `auth_secret.key`).
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// Connection and subscription counters.
    pub fn metrics(&self) -> &Metrics {
        &self.inner.metrics
    }

    /// The shared subscription manager behind every sync connection (started on first use).
    pub fn subscriptions(&self) -> &SubscriptionManager {
        self.inner.subscriptions.get_or_init(|| SubscriptionManager::spawn(self, self.inner.metrics.clone()))
    }

    /// The subscription manager's counters: connections, shared queries, subscriptions, rounds and reruns.
    pub fn subscription_stats(&self) -> SubscriptionStats {
        self.inner.metrics.subscription_stats()
    }

    pub(crate) fn downgrade(&self) -> WeakApp {
        WeakApp(Arc::downgrade(&self.inner))
    }

    /// When this `App` was opened (ms since the epoch).
    pub fn started_at(&self) -> f64 {
        self.inner.started_at
    }

    pub fn db(&self) -> &Database {
        &self.inner.db
    }

    /// The execution log (`aux.db`): query, live tail and per-function stats.
    pub fn logs(&self) -> &Logs {
        &self.inner.logs
    }

    pub fn options(&self) -> &AppOptions {
        &self.inner.options
    }

    pub fn registry(&self) -> &'static Registry {
        self.inner.registry
    }

    /// Auth state and operations (token validation, sign-in, admins).
    pub fn auth(&self) -> &crate::auth::AuthService {
        &self.inner.auth
    }

    /// Look up a function of `kind`. Internal functions are only found when `allow_internal`.
    fn lookup(&self, path: &str, kind: FunctionKind, allow_internal: bool) -> crate::Result<&'static FunctionDef> {
        let def = self
            .inner
            .registry
            .get(path)
            .filter(|d| allow_internal || d.visibility == Visibility::Public)
            .ok_or_else(|| FunctionNotFound(format!("Could not find public function for '{path}'")))?;
        if def.handler.kind() != kind {
            return Err(FunctionNotFound(format!(
                "Trying to execute '{path}' as a {}, but it is defined as a {}",
                kind.as_str(),
                def.handler.kind().as_str()
            ))
            .into());
        }
        Ok(def)
    }

    /// Run a public query at the latest snapshot.
    pub async fn run_query(&self, path: &str, args: Value, identity: &Identity) -> QueryRun {
        self.call_query(path, args, identity, false).await
    }

    /// Run a public mutation; returns its result and commit ts.
    pub async fn run_mutation(&self, path: &str, args: Value, identity: &Identity) -> crate::Result<(Value, u64)> {
        self.call_mutation(path, args, identity, false, None).await.result
    }

    /// Run a public mutation, returning its log lines too. With an `idempotency_key`, a key seen in the last ~10
    /// minutes returns the recorded response without running the mutation again (see `db::dedup`).
    pub async fn execute_mutation(
        &self,
        path: &str,
        args: Value,
        identity: &Identity,
        idempotency_key: Option<&IdempotencyKey>,
    ) -> MutationRun {
        self.call_mutation(path, args, identity, false, idempotency_key).await
    }

    /// Run a public action.
    pub async fn run_action(&self, path: &str, args: Value, identity: &Identity) -> crate::Result<Value> {
        self.call_action(path, args, identity, false).await.result
    }

    /// Run a public action, returning its log lines too.
    pub async fn execute_action(&self, path: &str, args: Value, identity: &Identity) -> ActionRun {
        self.call_action(path, args, identity, false).await
    }

    /// Run a query by its typed reference (internal functions allowed): `app.query::<list>((x,))`.
    pub async fn query<F: QueryFunction>(&self, args: impl Into<F::Args>) -> crate::Result<F::Output> {
        self.typed_query::<F>(args.into(), &Identity::default()).await
    }

    /// Run a mutation by its typed reference (internal functions allowed).
    pub async fn mutation<F: MutationFunction>(&self, args: impl Into<F::Args>) -> crate::Result<F::Output> {
        Ok(self.typed_mutation::<F>(args.into(), &Identity::default()).await?.0)
    }

    /// Run an action by its typed reference (internal functions allowed).
    pub async fn action<F: ActionFunction>(&self, args: impl Into<F::Args>) -> crate::Result<F::Output> {
        self.typed_action::<F>(args.into(), &Identity::default()).await
    }

    pub(crate) async fn typed_query<F: QueryFunction>(&self, args: F::Args, identity: &Identity) -> crate::Result<F::Output> {
        let out = self.call_query(&F::path(), serde_json::to_value(args)?, identity, true).await.result?;
        Ok(serde_json::from_value(out)?)
    }

    pub(crate) async fn typed_mutation<F: MutationFunction>(
        &self,
        args: F::Args,
        identity: &Identity,
    ) -> crate::Result<(F::Output, u64)> {
        let (out, ts) = self.call_mutation(&F::path(), serde_json::to_value(args)?, identity, true, None).await.result?;
        Ok((serde_json::from_value(out)?, ts))
    }

    pub(crate) async fn typed_action<F: ActionFunction>(&self, args: F::Args, identity: &Identity) -> crate::Result<F::Output> {
        let out = self.call_action(&F::path(), serde_json::to_value(args)?, identity, true).await.result?;
        Ok(serde_json::from_value(out)?)
    }

    pub(crate) async fn call_query(&self, path: &str, args: Value, identity: &Identity, allow_internal: bool) -> QueryRun {
        let snapshot = match self.db().snapshot().await {
            Ok(s) => s,
            Err(e) => return QueryRun::not_run(e, 0),
        };
        self.query_in_snapshot(&snapshot, path, args, identity, allow_internal).await
    }

    /// Run a query in an existing snapshot. Several of these may run concurrently on the same snapshot.
    pub(crate) async fn query_in_snapshot(
        &self,
        snapshot: &Arc<Snapshot>,
        path: &str,
        args: Value,
        identity: &Identity,
        allow_internal: bool,
    ) -> QueryRun {
        let ts = snapshot.ts;
        let def = match self.lookup(path, FunctionKind::Query, allow_internal) {
            Ok(d) => d,
            Err(e) => return QueryRun::not_run(e, ts),
        };
        let Handler::Query(handler) = def.handler else { unreachable!("kind checked by lookup") };
        let db = QueryDb::new(Source::Snapshot(snapshot.clone()), self.db().catalog().clone());
        let read_set = db.read_set_handle();
        let (auth, auth_access) = Auth::tracked(identity.clone());
        let ctx = QueryCtx { db, auth, now: snapshot.time };
        let exec = Execution::start(path, FunctionKind::Query, identity, &args);
        let (result, log_lines) = crate::log::capture(catch_panics(handler(ctx, args))).await;
        exec.finish(self.logs(), &result, &log_lines);
        let read_set = std::mem::take(&mut *read_set.lock().unwrap_or_else(|e| e.into_inner()));
        QueryRun { result, read_set, ts, log_lines, auth_deps: auth_access.deps() }
    }

    pub(crate) async fn call_mutation(
        &self,
        path: &str,
        args: Value,
        identity: &Identity,
        allow_internal: bool,
        idempotency_key: Option<&IdempotencyKey>,
    ) -> MutationRun {
        let not_run = |e| MutationRun { result: Err(e), log_lines: Vec::new() };
        let def = match self.lookup(path, FunctionKind::Mutation, allow_internal) {
            Ok(d) => d,
            Err(e) => return not_run(e),
        };
        let Handler::Mutation(handler) = def.handler else { unreachable!("kind checked by lookup") };
        // Responses are stored per identity: a caller who learns (or guesses) another client's session id can't
        // replay that client's responses, nor can a socket that changed identity see its earlier identity's.
        let scoped_key = idempotency_key.map(|k| k.scoped_to(identity));
        let idempotency_key = scoped_key.as_ref();
        let txn = match self.db().begin_write().await {
            Ok(t) => t,
            Err(e) => return not_run(e),
        };
        // Checked under the writer lock, so a concurrent duplicate waits for the first and then finds its response.
        if let Some(key) = idempotency_key {
            match txn.with_state(|s| dedup::lookup(s.conn(), key)) {
                Ok(None) => {}
                Ok(Some(stored)) => {
                    txn.rollback();
                    tracing::debug!(path, session = %key.session_id, request_id = key.request_id, "duplicate mutation; replaying its response");
                    return MutationRun { result: stored.into_result(), log_lines: Vec::new() };
                }
                Err(e) => return not_run(e),
            }
        }
        let ctx = MutationCtx { db: MutationDb::new(txn.clone()), auth: Auth::new(identity.clone()), now: txn.now() };
        let timeout = self.options().mutation_timeout;
        let exec = Execution::start(path, FunctionKind::Mutation, identity, &args);
        let run = async {
            let result = match tokio::time::timeout(timeout, catch_panics(handler(ctx, args))).await {
                Ok(r) => r,
                Err(_elapsed) => Err(MutationTimeout { path: path.to_string(), timeout }.into()),
            };
            match result {
                Ok(value) => {
                    let ts = txn
                        .commit_with(|conn, ts| match idempotency_key {
                            Some(key) => dedup::record_success(conn, key, &value, ts),
                            None => Ok(()),
                        })
                        .context("commit failed")?;
                    Ok((value, ts))
                }
                Err(e) => {
                    match idempotency_key {
                        Some(key) => {
                            if let Err(record_err) = txn.rollback_then(|conn| dedup::record_error(conn, key, &e)) {
                                tracing::warn!(path, "could not record a failed mutation for deduplication: {record_err:#}");
                            }
                        }
                        None => txn.rollback(),
                    }
                    Err(e)
                }
            }
        };
        let (result, log_lines) = crate::log::capture(run).await;
        exec.finish(self.logs(), &result, &log_lines);
        MutationRun { result, log_lines }
    }

    pub(crate) async fn call_action(
        &self,
        path: &str,
        args: Value,
        identity: &Identity,
        allow_internal: bool,
    ) -> ActionRun {
        let def = match self.lookup(path, FunctionKind::Action, allow_internal) {
            Ok(d) => d,
            Err(e) => return ActionRun { result: Err(e), log_lines: Vec::new() },
        };
        let Handler::Action(handler) = def.handler else { unreachable!("kind checked by lookup") };
        let ctx = ActionCtx { auth: Auth::new(identity.clone()), app: self.clone(), identity: identity.clone() };
        let exec = Execution::start(path, FunctionKind::Action, identity, &args);
        let (result, log_lines) = crate::log::capture(catch_panics(handler(ctx, args))).await;
        exec.finish(self.logs(), &result, &log_lines);
        ActionRun { result, log_lines }
    }
}

/// An execution being timed for the execution log.
struct Execution {
    timestamp: i64,
    started: std::time::Instant,
    path: String,
    kind: FunctionKind,
    identity: Option<String>,
    args: (String, bool),
}

impl Execution {
    fn start(path: &str, kind: FunctionKind, identity: &Identity, args: &Value) -> Execution {
        Execution {
            timestamp: crate::db::wall_clock_ms() as i64,
            started: std::time::Instant::now(),
            path: path.to_string(),
            kind,
            identity: identity_summary(identity),
            args: crate::exec_log::truncate_args(args),
        }
    }

    fn finish<T>(self, logs: &Logs, result: &crate::Result<T>, log_lines: &[String]) {
        let (args, args_truncated) = self.args;
        logs.record(crate::exec_log::LogEntry {
            id: 0,
            timestamp: self.timestamp,
            path: self.path,
            kind: self.kind,
            identity: self.identity,
            duration_us: self.started.elapsed().as_micros() as u64,
            outcome: crate::exec_log::LogOutcome::of(result),
            error: result.as_ref().err().map(|e| format!("{e:#}")),
            log_lines: log_lines.to_vec(),
            args,
            args_truncated,
        });
    }
}

/// The execution log's summary of who called (`Identity::log_summary`: `user:<id>` / `admin:<id>`, never a token or
/// session id); the anonymous identity logs as `None`.
fn identity_summary(identity: &Identity) -> Option<String> {
    const MAX: usize = 256;
    let mut s = identity.log_summary()?;
    if s.len() > MAX {
        let mut cut = MAX;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
    Some(s)
}

/// Turn a panic inside user code into an error, so a buggy function can't take down a connection (and so that a
/// panicking mutation is rolled back like any other failure).
async fn catch_panics(fut: futures::future::BoxFuture<'static, crate::Result<Value>>) -> crate::Result<Value> {
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(r) => r,
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            Err(anyhow::anyhow!("function panicked: {msg}"))
        }
    }
}
