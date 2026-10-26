//! [`Users`]: deleting users and ending their sessions from app code (`ctx.users` in mutations), in the mutation's
//! transaction. The dashboard's Users page (`delete_user`, `revoke_session`) does the same through these.
//!
//! ```ignore
//! #[internal_mutation]
//! pub async fn reject_request(ctx: &MutationCtx, request: Id<AccessRequest>) -> Result<()> {
//!     let Some(r) = ctx.db.get(request).await? else { return Ok(()) };
//!     ctx.db.delete(request).await?;
//!     ctx.users.delete(r.user).await?;    // gone, with their sessions: their sockets get `AuthError` at once
//!     Ok(())
//! }
//! ```
//!
//! Ending a session works like signing out: its refresh token stops working, new requests and connections with its
//! access tokens are refused, and sockets authenticated with it are told at once (live revocation). It doesn't keep
//! anyone from signing in again; to keep someone out, delete the user and refuse their sign-up
//! ([`super::AuthConfig::before_sign_up`]), or record the ban in the app's data and check it in the guard
//! ([`crate::guard`]) or the functions.
//!
//! These are capabilities, not checks: a public mutation that calls them decides itself who may (an administrator of
//! the app, the user themself).

use serde::{Deserialize, Serialize};

use super::store::{self, Principal};
use super::tables::{Account, Credential, Session, User, VerificationCode};
use crate::db::query::MutationDb;
use crate::id::Id;

/// What deleting a user removed besides the user.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct DeletedUser {
    pub sessions: u32,
    pub accounts: u32,
    pub credentials: u32,
    pub verification_codes: u32,
    /// Second factors (0 or 1).
    #[serde(default)]
    pub mfa_factors: u32,
}

/// Users and sessions, in a mutation's transaction: `ctx.users`. See the [module docs](self).
pub struct Users {
    db: MutationDb,
}

impl Users {
    pub(crate) fn new(db: MutationDb) -> Users {
        Users { db }
    }

    /// Delete `user` together with their sessions (and rotated refresh tokens), linked OAuth and external accounts,
    /// password, second factor and pending email codes. Documents of app tables that reference the user are left
    /// alone. `None` if there is no such user.
    pub async fn delete(&self, user: Id<User>) -> crate::Result<Option<DeletedUser>> {
        delete_user_in(&self.db, user).await
    }

    /// End every session of `user` (signed out everywhere). Returns how many there were.
    pub async fn revoke_sessions(&self, user: Id<User>) -> crate::Result<u32> {
        let db = &self.db;
        let sessions =
            db.query::<Session>().with_index(Session::by_user, |q| q.eq(Session::user, Some(user))).collect().await?;
        for s in &sessions {
            store::delete_session_in(db, s.id).await?;
        }
        Ok(sessions.len() as u32)
    }

    /// End one session (a user's or an administrator's). `false` if it had already ended.
    pub async fn revoke_session(&self, session: Id<Session>) -> crate::Result<bool> {
        if self.db.get(session).await?.is_none() {
            return Ok(false);
        }
        store::delete_session_in(&self.db, session).await?;
        Ok(true)
    }
}

/// [`Users::delete`].
pub(crate) async fn delete_user_in(db: &MutationDb, id: Id<User>) -> crate::Result<Option<DeletedUser>> {
    let Some(doc) = db.get(id).await? else { return Ok(None) };
    let mut out = DeletedUser::default();
    for s in db.query::<Session>().with_index(Session::by_user, |q| q.eq(Session::user, Some(id))).collect().await? {
        store::delete_session_in(db, s.id).await?;
        out.sessions += 1;
    }
    for a in db.query::<Account>().with_index(Account::by_user, |q| q.eq(Account::user, id)).collect().await? {
        db.delete(a.id).await?;
        out.accounts += 1;
    }
    for c in db.query::<Credential>().with_index(Credential::by_user, |q| q.eq(Credential::user, id)).collect().await? {
        db.delete(c.id).await?;
        out.credentials += 1;
    }
    if super::mfa::remove_factor_in(db, Principal::User(id)).await? {
        out.mfa_factors += 1;
    }
    if let Some(email) = &doc.email {
        let codes = db
            .query::<VerificationCode>()
            .with_index(VerificationCode::by_email_purpose, |q| q.eq(VerificationCode::email, email.clone()))
            .collect()
            .await?;
        for c in codes {
            db.delete(c.id).await?;
            out.verification_codes += 1;
        }
    }
    db.delete(id).await?;
    Ok(Some(out))
}
