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
//! - [`sync`]: WebSocket protocol messages and the per-connection sync worker.
//! - [`auth`]: users, sessions, access tokens, email codes, OAuth, admins, and `ctx.auth`.
//! - [`dashboard`]: the admin dashboard's system functions, `/api/admin/*` endpoints and embedded SPA (`/_/`).
//! - [`server`], [`cli`]: axum server and command line. [`testing`]: `TestApp`.
//! - [`codegen`]: TypeScript bindings (`api.ts`, `dataModel.ts`) for the client packages.

// The macros emit `::ferrix::...` paths; this lets ferrix define its own system tables and functions with them.
extern crate self as ferrix;

pub mod auth;
pub mod cli;
pub mod codegen;
pub mod dashboard;
pub mod db;
pub mod error;
pub mod exec_log;
pub mod id;
pub mod keys;
pub mod log;
pub mod runtime;
pub mod schema;
pub mod server;
pub mod sync;
pub mod testing;
pub mod writelog;

mod private;

/// Items used by `ferrix-macros` output only (docs/CONTRACT.md §2).
#[doc(hidden)]
pub mod __private {
    pub use crate::private::*;
}

pub use cli::Ferrix;
pub use ferrix_macros::{action, internal_action, internal_mutation, internal_query, mutation, query, table};
pub use db::pagination::{PaginationOpts, PaginationResult};
pub use db::dedup::IdempotencyKey;
pub use db::query::{IndexRange, MutationDb, Query, QueryDb, RawIndexRange, RawQuery};
pub use error::{Error, MutationTimeout, Result, UserError};
pub use exec_log::{FunctionStats, LogEntry, LogFilter, LogOutcome, LogRetention, Logs};
pub use private::FunctionKind;
pub use id::Id;
pub use runtime::{ActionCtx, App, AppOptions, Auth, FunctionInfo, Identity, Metrics, MutationCtx, QueryCtx, Registry, registry};
pub use schema::{Doc, Field, Index, IndexDef, Order, Table, TableDef};
pub use server::{router, serve, serve_with_shutdown};
pub use serde_json::json;

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

/// `use ferrix::prelude::*;`
///
/// Logging is not glob-exported (a glob-imported `log` module or `info!` macro would be ambiguous with the `log`
/// crate): add `use ferrix::log;` and write `log::info!(...)`. See [`log`](crate::log).
pub mod prelude {
    pub use crate::{
        ActionCtx, Doc, Error, Ferrix, Field, Id, Index, MutationCtx, Order, QueryCtx, Result, UserError, action,
        internal_action, internal_mutation, internal_query, json, mutation, query, table,
    };
    pub use crate::{PaginationOpts, PaginationResult};
}
