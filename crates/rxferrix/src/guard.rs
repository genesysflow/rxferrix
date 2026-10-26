//! The function guard: app code that runs before every call a client makes to a public function, and can refuse it
//! ([`crate::RxFerrix::guard`], [`crate::AppOptions::guard`]). One place for an access policy that would otherwise be
//! a check at the top of each function, where a new function that forgets it is a hole:
//!
//! ```ignore
//! RxFerrix::new().guard(|ctx, function| async move {
//!     // Reachable before access is granted.
//!     if function.is::<access::status>() || function.is::<invites::accept>() {
//!         return Ok(());
//!     }
//!     let user = ctx.auth.require_user()?;
//!     if !access::granted(&ctx.db, user).await? {
//!         return Err(UserError::new("access_required", "Ask for an invite first").into());
//!     }
//!     Ok(())
//! })
//! ```
//!
//! **What it covers.** Calls from clients (WebSocket and HTTP) of the app's public queries, mutations and actions,
//! including every rerun of a subscribed query. It does not run for internal functions (clients can't call them),
//! for system functions (`_system/…`: `_system/auth:me`, the dashboard's), nor for calls made by server code, which is
//! trusted: `ctx.run_query` & co. from an action that already passed the guard, `app.query` / `app.mutation`, scheduled
//! jobs and crons, migrations, and the dashboard's function runner (an administrator, also when running as a user).
//! `TestApp`'s calls as a user (`as_user`, `query_as`, ...) go through it like a client's.
//!
//! **Where it runs.** Inside the call, as its first step: for a query in the query's snapshot, and what it reads
//! through `ctx.db` and `ctx.auth` counts as the query's reads (a subscribed query reruns, and its guard with it, when
//! the access data changes; results are only shared between callers the guard can't tell apart); for a mutation in
//! the mutation's transaction; for an action in a fresh snapshot. An error refuses the call: it reaches the client
//! like an error of the function itself (a [`crate::UserError`] with its code), and the execution log records the
//! call as failed. It counts toward the function's timeout.

use std::future::Future;
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::db::query::QueryDb;
use crate::private::FunctionKind;
use crate::runtime::Auth;

/// What a guard can see of the call: the caller and the data, read-only.
pub struct GuardCtx {
    /// The query's snapshot, the mutation's transaction, or (actions) a fresh snapshot.
    pub db: QueryDb,
    /// The caller, as the function will see it.
    pub auth: Auth,
    /// Environment variables, read in `db`.
    pub env: crate::env::Env,
}

/// The function being called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardedFunction {
    path: String,
    kind: FunctionKind,
}

impl GuardedFunction {
    pub(crate) fn new(path: &str, kind: FunctionKind) -> GuardedFunction {
        GuardedFunction { path: path.to_string(), kind }
    }

    /// The function's path, e.g. `"access:status"`.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Query, mutation or action.
    pub fn kind(&self) -> FunctionKind {
        self.kind
    }

    /// Whether this is `F`: `function.is::<access::status>()`. Checked by the compiler, unlike a path.
    pub fn is<F: crate::FunctionRef>(&self) -> bool {
        self.path == F::path()
    }
}

/// A guard: see the [module docs](self). Build one with [`Guard::new`], or pass the closure to
/// [`crate::RxFerrix::guard`].
#[derive(Clone)]
pub struct Guard(Arc<dyn Fn(GuardCtx, GuardedFunction) -> BoxFuture<'static, crate::Result<()>> + Send + Sync>);

impl Guard {
    pub fn new<F, Fut>(guard: F) -> Guard
    where
        F: Fn(GuardCtx, GuardedFunction) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = crate::Result<()>> + Send + 'static,
    {
        Guard(Arc::new(move |ctx, function| Box::pin(guard(ctx, function))))
    }

    pub(crate) fn check(&self, ctx: GuardCtx, function: GuardedFunction) -> BoxFuture<'static, crate::Result<()>> {
        (self.0)(ctx, function)
    }
}

impl std::fmt::Debug for Guard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Guard(<fn>)")
    }
}

/// Who is calling a function, which decides what it may call and whether the guard runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// A client (WebSocket, HTTP API): public functions only, through the guard.
    Client,
    /// Server code (`ctx.run_*`, `app.*`, the scheduler, migrations, the dashboard's runner): internal functions too,
    /// no guard.
    Server,
    /// `TestApp`'s calls as an identity: internal functions too, and public ones through the guard, like a client's.
    Test,
}

impl Origin {
    pub(crate) fn allows_internal(self) -> bool {
        self != Origin::Client
    }

    /// Whether a call of a function with `visibility` at `path` from here goes through the guard.
    pub(crate) fn guarded(self, path: &str, visibility: crate::private::Visibility) -> bool {
        self != Origin::Server
            && visibility == crate::private::Visibility::Public
            && !crate::private::is_system_path(path)
    }
}
