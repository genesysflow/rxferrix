//! Who is calling a function: [`Identity`] (set by the transport from a validated access token) and [`Auth`], its
//! view inside functions (`ctx.auth`).

use crate::UserError;
use crate::auth::tables::{Admin, Session, User};
use crate::db::query::QueryDb;
use crate::id::Id;
use crate::schema::Doc;

/// The caller's identity, derived from a validated access token (or anonymous). Part of every function call: the
/// sync worker keeps one per socket and passes it to queries, mutations and actions, and actions pass theirs on to
/// `ctx.run_*`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Identity {
    /// String form of the signed-in user's `Id<User>`.
    pub user_id: Option<String>,
    /// String form of the `Id<Admin>` for admin sessions (dashboard).
    pub admin_id: Option<String>,
    /// String form of the `Id<Session>` the access token belongs to.
    pub session_id: Option<String>,
}

impl Identity {
    /// No one is signed in.
    pub fn anonymous() -> Self {
        Identity::default()
    }

    /// A signed-in user.
    pub fn user(user: Id<User>, session: Option<Id<Session>>) -> Self {
        Identity { user_id: Some(user.to_string()), admin_id: None, session_id: session.map(|s| s.to_string()) }
    }

    /// A signed-in administrator.
    pub fn admin(admin: Id<Admin>, session: Option<Id<Session>>) -> Self {
        Identity { user_id: None, admin_id: Some(admin.to_string()), session_id: session.map(|s| s.to_string()) }
    }

    pub fn is_anonymous(&self) -> bool {
        self.user_id.is_none() && self.admin_id.is_none()
    }

    /// Who this is, for logs: `user:<id>`, `admin:<id>`, or `None` when anonymous. Identities never hold tokens;
    /// this also leaves out the session id.
    pub fn log_summary(&self) -> Option<String> {
        match (&self.user_id, &self.admin_id) {
            (Some(u), _) => Some(format!("user:{u}")),
            (None, Some(a)) => Some(format!("admin:{a}")),
            (None, None) => None,
        }
    }

    /// A key that separates callers for per-identity state such as mutation dedup: `"user:<id>"`,
    /// `"admin:<id>"` or `"anonymous"`.
    pub fn principal_key(&self) -> String {
        self.log_summary().unwrap_or_else(|| "anonymous".to_string())
    }
}

/// Authentication info available to functions as `ctx.auth`.
///
/// ```ignore
/// #[mutation]
/// pub async fn add(ctx: &MutationCtx, text: String) -> Result<Id<Todo>> {
///     let owner = ctx.auth.require_user()?;
///     ctx.db.insert(Todo { owner, text, done: false }).await
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct Auth {
    identity: Identity,
}

/// `UserError` code for calls that need a signed-in user (or admin) but have none.
pub const UNAUTHENTICATED: &str = "unauthenticated";
/// `UserError` code for calls that need an admin but come from a regular user.
pub const FORBIDDEN: &str = "forbidden";

impl Auth {
    pub(crate) fn new(identity: Identity) -> Self {
        Auth { identity }
    }

    /// The raw identity.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// The signed-in user's id, if any. `None` for anonymous callers and for admin sessions.
    pub fn user_id(&self) -> Option<Id<User>> {
        self.identity.user_id.as_deref().and_then(|s| s.parse().ok())
    }

    /// The signed-in user's id, or a `UserError` with code `"unauthenticated"`.
    pub fn require_user(&self) -> crate::Result<Id<User>> {
        self.user_id().ok_or_else(|| UserError::new(UNAUTHENTICATED, "You must be signed in").into())
    }

    /// Load the signed-in user's document through `db` (`ctx.db` of a query or mutation). The read is tracked like
    /// any other, so a query using it reruns when the profile changes. `None` when nobody is signed in (or the user
    /// was deleted).
    pub async fn user(&self, db: &QueryDb) -> crate::Result<Option<Doc<User>>> {
        match self.user_id() {
            Some(id) => db.get(id).await,
            None => Ok(None),
        }
    }

    /// The session id of the access token, if signed in.
    pub fn session_id(&self) -> Option<Id<Session>> {
        self.identity.session_id.as_deref().and_then(|s| s.parse().ok())
    }

    /// True for admin sessions (signed in through `/api/auth/admin/signin`).
    pub fn is_admin(&self) -> bool {
        self.admin_id().is_some()
    }

    /// The admin's id for admin sessions.
    pub fn admin_id(&self) -> Option<Id<Admin>> {
        self.identity.admin_id.as_deref().and_then(|s| s.parse().ok())
    }

    /// The admin's id, or a `UserError`: `"unauthenticated"` when nobody is signed in, `"forbidden"` otherwise.
    pub fn require_admin(&self) -> crate::Result<Id<Admin>> {
        match self.admin_id() {
            Some(id) => Ok(id),
            None if self.identity.is_anonymous() => {
                Err(UserError::new(UNAUTHENTICATED, "You must be signed in as an administrator").into())
            }
            None => Err(UserError::new(FORBIDDEN, "Only administrators may do this").into()),
        }
    }
}
