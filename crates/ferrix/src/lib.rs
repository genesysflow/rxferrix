//! Ferrix: a single-binary reactive backend. See PLAN.md, docs/CONTRACT.md and docs/PROTOCOL.md.
//!
//! Module map:
//! - [`id`], [`schema`], [`error`]: user-facing types (`Id`, `Doc`, `Table`, `Field`, `Index`, `UserError`, ...).
//! - [`keys`]: memcomparable index key encoding.
//! - [`db`]: SQLite storage, catalog, snapshots, the write transaction, the query engine and read sets.
//! - [`writelog`]: recent commits, used to invalidate subscriptions.
//! - [`runtime`]: function registry, contexts and [`App`].
//! - [`log`]: `log::info!` & co. for functions, and capture of their log lines. [`exec_log`]: the execution log in
//!   `aux.db` ([`App::logs`]).
//! - [`sync`]: WebSocket protocol messages, the per-connection sync worker, and the shared subscription manager.
//! - [`scheduler`]: `ctx.scheduler` (`run_after`, `run_at`, `cancel`, workpools), the job runner, crons and
//!   `_scheduled_jobs`. [`workflow`]: durable `#[workflow]`s (`ctx.workflows`, `_workflows`, `_workflow_steps`).
//! - [`migrations`]: `#[migration]` data migrations in batches, `_migrations`, and the schema check's lenient mode.
//! - [`auth`]: users, sessions, access tokens, email codes, OAuth, external JWTs, admins, and `ctx.auth`; live
//!   revocation. [`settings`]: runtime settings (`_settings`). [`audit`]: the admin audit log (`_audit_log`).
//! - [`storage`]: file storage (`_storage`, `ctx.storage`, upload and serving endpoints).
//! - [`dashboard`]: the admin dashboard's system functions, `/api/admin/*` endpoints and embedded SPA (`/_/`).
//! - [`server`], [`cli`]: axum server and command line. [`testing`]: `TestApp`.
//! - [`ops`]: backups and restore, export and import, `/metrics`, the app's static frontend, TLS.
//! - [`codegen`]: TypeScript bindings (`api.ts`, `dataModel.ts`) for the client packages.

// The macros emit `::ferrix::...` paths; this lets ferrix define its own system tables and functions with them.
extern crate self as ferrix;

pub mod audit;
pub mod auth;
pub mod bytes;
pub mod cli;
pub mod codegen;
pub mod dashboard;
pub mod db;
mod determinism;
pub mod error;
pub mod exec_log;
pub mod id;
pub mod keys;
pub mod log;
pub mod migrations;
pub mod olap;
pub mod ops;
pub mod runtime;
pub mod scheduler;
pub mod schema;
pub mod server;
pub mod settings;
pub mod shape;
pub mod storage;
pub mod sync;
pub mod testing;
pub mod workflow;
pub mod writelog;

mod private;

/// Items used by `ferrix-macros` output only (docs/CONTRACT.md §2).
#[doc(hidden)]
pub mod __private {
    pub use crate::private::*;
}

pub use bytes::Bytes;
pub use cli::Ferrix;
pub use db::aggregate::{AggregateGroup, AggregateQuery, AggregateValue, Sum as AggregateSum};
pub use db::dedup::IdempotencyKey;
pub use db::pagination::{PaginationOpts, PaginationResult};
pub use db::query::{IndexRange, MutationDb, Query, QueryDb, RawIndexRange, RawQuery};
pub use db::search::{RawSearchQuery, SearchFilter, SearchQuery, VectorFilter, VectorSearchOpts};
pub use error::{Error, MutationTimeout, QueryTimeout, Result, UserError};
pub use exec_log::{FunctionStats, LogEntry, LogFilter, LogOutcome, LogRetention, Logs, TimePoint, TimeseriesSpec};
pub use ferrix_macros::{
    action, internal_action, internal_mutation, internal_query, migration, mutation, object, query, table, workflow,
};
pub use id::Id;
pub use private::FunctionKind;
pub use runtime::{
    ActionCtx, App, AppOptions, Auth, FunctionInfo, Identity, Metrics, MutationCtx, QueryCacheOptions, QueryCtx,
    Registry, registry,
};
/// For `Ferrix::embed_static`: `#[derive(ferrix::rust_embed::Embed)] #[crate_path = "ferrix::rust_embed"]`.
pub use rust_embed;
pub use scheduler::{CronSpec, ScheduledJob, Scheduler, SchedulerOptions};
pub use schema::{
    Aggregate, AggregateDef, Doc, Field, HnswParams, Index, IndexDef, Order, SearchIndex, SearchIndexDef, Table,
    TableDef, Tokenizer, TokenizerKind, VectorIndex, VectorIndexDef, VectorIndexKind,
};
pub use serde_json::json;
pub use server::{router, serve, serve_with_shutdown};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// A typed reference to a registered function. Implemented by the macros on a type-namespace-only struct named like
/// the function (never instantiated); call through the runtime with turbofish, e.g. `ctx.run_query::<list>(args)`.
pub trait FunctionRef: Send + Sync + 'static {
    type Args: Serialize + DeserializeOwned + Send + 'static;
    type Output: Serialize + DeserializeOwned + Send + 'static;
    /// Function path as in docs/PROTOCOL.md, e.g. `"todos:list"`.
    fn path() -> String;
}
pub trait QueryFunction: FunctionRef {}
pub trait MutationFunction: FunctionRef {}
pub trait ActionFunction: FunctionRef {}
/// A function that `ctx.scheduler` and crons can run: every mutation and action (internal ones included). The
/// macros implement it next to `MutationFunction` / `ActionFunction`; queries don't have it, so scheduling one is a
/// compile error:
///
/// ```compile_fail
/// use ferrix::prelude::*;
/// #[query]
/// pub async fn list(_ctx: &QueryCtx) -> Result<u32> { Ok(0) }
/// #[mutation]
/// pub async fn later(ctx: &MutationCtx) -> Result<()> {
///     ctx.scheduler.run_after::<list>(std::time::Duration::ZERO, ()).await?;   // error: `list: SchedulableFunction`
///     Ok(())
/// }
/// # fn main() {}
/// ```
pub trait SchedulableFunction: FunctionRef {}

/// `use ferrix::prelude::*;`
///
/// Logging is not glob-exported (a glob-imported `log` module or `info!` macro would be ambiguous with the `log`
/// crate): add `use ferrix::log;` and write `log::info!(...)`. See [`log`](crate::log).
pub mod prelude {
    pub use crate::migrations::DocChange;
    pub use crate::workflow::WorkflowCtx;
    pub use crate::{
        ActionCtx, Bytes, Doc, Error, Ferrix, Field, Id, Index, MutationCtx, Order, QueryCtx, Result, UserError,
        action, internal_action, internal_mutation, internal_query, json, migration, mutation, object, query, table,
        workflow,
    };
    pub use crate::{Aggregate, SearchIndex, VectorFilter, VectorIndex, VectorSearchOpts};
    pub use crate::{PaginationOpts, PaginationResult};
}
