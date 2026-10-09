//! The function runtime: the registry of functions, the three contexts, and [`App`], which runs functions against
//! the database.

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use anyhow::Context;
use futures::FutureExt;
use serde_json::Value;

use crate::db::query::{MutationDb, QueryDb, Source};
use crate::db::readset::ReadSet;
use crate::db::{Database, Snapshot};
use crate::error::FunctionNotFound;
use crate::private::{FUNCTIONS, FunctionDef, FunctionKind, Handler, TABLES, Visibility};
use crate::{ActionFunction, MutationFunction, QueryFunction, TableDef};

// ---------------------------------------------------------------------------------------------------------------------
// Identity and contexts
// ---------------------------------------------------------------------------------------------------------------------

/// Who is calling. Phase 0: always anonymous.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    pub user_id: Option<String>,
}

/// Authentication info available to functions.
#[derive(Debug, Clone, Default)]
pub struct Auth {
    identity: Identity,
}

impl Auth {
    pub(crate) fn new(identity: Identity) -> Self {
        Auth { identity }
    }

    /// The authenticated user's id, if any (phase 0: always `None`).
    pub fn user_id(&self) -> Option<&str> {
        self.identity.user_id.as_deref()
    }
}

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
    app: App,
    identity: Identity,
}

impl ActionCtx {
    /// Run a query (public or internal) in its own snapshot: `ctx.run_query::<list>((x,))`.
    pub async fn run_query<F: QueryFunction>(&self, args: impl Into<F::Args>) -> crate::Result<F::Output> {
        self.app.typed_query::<F>(args.into(), &self.identity).await
    }

    /// Run a mutation (public or internal) in its own transaction.
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
    /// Show real error messages to clients instead of "Server Error".
    pub dev: bool,
    /// Number of read-only connections.
    pub readers: usize,
}

impl Default for AppOptions {
    fn default() -> Self {
        AppOptions { dev: false, readers: crate::db::DEFAULT_READERS }
    }
}

/// The result of running a query: its value or error, what it read, and the ts it ran at.
pub struct QueryRun {
    pub result: crate::Result<Value>,
    pub read_set: ReadSet,
    pub ts: u64,
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
}

impl App {
    /// Open the database in `dir` and reconcile it with the tables linked into the binary.
    pub async fn open(dir: impl AsRef<Path>, options: AppOptions) -> crate::Result<App> {
        let registry = registry()?;
        let tables = registered_tables();
        let dir = dir.as_ref().to_path_buf();
        let readers = options.readers;
        let db = tokio::task::spawn_blocking(move || Database::open(&dir, &tables, readers)).await??;
        Ok(App { inner: Arc::new(AppInner { db, registry, options }) })
    }

    pub fn db(&self) -> &Database {
        &self.inner.db
    }

    pub fn options(&self) -> &AppOptions {
        &self.inner.options
    }

    pub fn registry(&self) -> &'static Registry {
        self.inner.registry
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
        self.call_mutation(path, args, identity, false).await
    }

    /// Run a public action.
    pub async fn run_action(&self, path: &str, args: Value, identity: &Identity) -> crate::Result<Value> {
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
        let (out, ts) = self.call_mutation(&F::path(), serde_json::to_value(args)?, identity, true).await?;
        Ok((serde_json::from_value(out)?, ts))
    }

    pub(crate) async fn typed_action<F: ActionFunction>(&self, args: F::Args, identity: &Identity) -> crate::Result<F::Output> {
        let out = self.call_action(&F::path(), serde_json::to_value(args)?, identity, true).await?;
        Ok(serde_json::from_value(out)?)
    }

    pub(crate) async fn call_query(&self, path: &str, args: Value, identity: &Identity, allow_internal: bool) -> QueryRun {
        let snapshot = match self.db().snapshot().await {
            Ok(s) => s,
            Err(e) => return QueryRun { result: Err(e), read_set: ReadSet::new(), ts: 0 },
        };
        let ts = snapshot.ts;
        let (result, read_set) = self.query_in_snapshot(&snapshot, path, args, identity, allow_internal).await;
        QueryRun { result, read_set, ts }
    }

    /// Run a query in an existing snapshot. Several of these may run concurrently on the same snapshot.
    pub(crate) async fn query_in_snapshot(
        &self,
        snapshot: &Arc<Snapshot>,
        path: &str,
        args: Value,
        identity: &Identity,
        allow_internal: bool,
    ) -> (crate::Result<Value>, ReadSet) {
        let def = match self.lookup(path, FunctionKind::Query, allow_internal) {
            Ok(d) => d,
            Err(e) => return (Err(e), ReadSet::new()),
        };
        let Handler::Query(handler) = def.handler else { unreachable!("kind checked by lookup") };
        let db = QueryDb::new(Source::Snapshot(snapshot.clone()), self.db().catalog().clone());
        let read_set = db.read_set_handle();
        let ctx = QueryCtx { db, auth: Auth::new(identity.clone()), now: snapshot.time };
        let result = catch_panics(handler(ctx, args)).await;
        let read_set = std::mem::take(&mut *read_set.lock().unwrap_or_else(|e| e.into_inner()));
        (result, read_set)
    }

    pub(crate) async fn call_mutation(
        &self,
        path: &str,
        args: Value,
        identity: &Identity,
        allow_internal: bool,
    ) -> crate::Result<(Value, u64)> {
        let def = self.lookup(path, FunctionKind::Mutation, allow_internal)?;
        let Handler::Mutation(handler) = def.handler else { unreachable!("kind checked by lookup") };
        let txn = self.db().begin_write().await?;
        let ctx = MutationCtx { db: MutationDb::new(txn.clone()), auth: Auth::new(identity.clone()), now: txn.now() };
        match catch_panics(handler(ctx, args)).await {
            Ok(value) => {
                let ts = txn.commit().context("commit failed")?;
                Ok((value, ts))
            }
            Err(e) => {
                txn.rollback();
                Err(e)
            }
        }
    }

    pub(crate) async fn call_action(
        &self,
        path: &str,
        args: Value,
        identity: &Identity,
        allow_internal: bool,
    ) -> crate::Result<Value> {
        let def = self.lookup(path, FunctionKind::Action, allow_internal)?;
        let Handler::Action(handler) = def.handler else { unreachable!("kind checked by lookup") };
        let ctx = ActionCtx { auth: Auth::new(identity.clone()), app: self.clone(), identity: identity.clone() };
        catch_panics(handler(ctx, args)).await
    }
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
