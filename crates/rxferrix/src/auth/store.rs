//! Reads and writes of the auth tables.
//!
//! These run directly against the database (a snapshot, or one write transaction per operation) instead of through
//! registered internal functions: they take password and token hashes as inputs, and keeping them out of the
//! function registry means they can't be invoked from the dashboard's function runner and their arguments never
//! reach execution logs. Every write still goes through `MutationDb`, so indexes, the write log and reactivity work
//! as for any mutation (a query reading `_users` reruns when a profile changes).

use std::future::Future;

use crate::UserError;
use crate::auth::tables::*;
use crate::auth::tokens::{Keys, ct_eq, random_code, random_token};
use crate::db::query::{MutationDb, QueryDb, Source};
use crate::id::Id;
use crate::runtime::App;
use crate::schema::Doc;

/// Failed attempts allowed per numeric code before it is burned.
pub const MAX_CODE_ATTEMPTS: u32 = 5;
/// Failed attempts allowed at the numeric codes of one email and purpose per [`CODE_FAILURE_WINDOW_MS`], across
/// re-issued codes. Past it, codes for that email can only be used through their link until the window ends.
pub const MAX_CODE_FAILURES: u32 = 10;
/// The window [`MAX_CODE_FAILURES`] counts over.
pub const CODE_FAILURE_WINDOW_MS: f64 = 3600.0 * 1000.0;
/// Digits of numeric codes.
pub const CODE_DIGITS: u32 = 6;

/// Run `f` against a fresh snapshot.
pub(crate) async fn read<R, F, Fut>(app: &App, f: F) -> crate::Result<R>
where
    F: FnOnce(QueryDb) -> Fut,
    Fut: Future<Output = crate::Result<R>>,
{
    let snapshot = app.db().snapshot().await?;
    f(QueryDb::new(Source::Snapshot(snapshot), app.db().catalog().clone())).await
}

/// Run `f` in one write transaction; commit when it returns `Ok`. Outcomes that must still persist writes (a failed
/// code attempt, a revoked session) are returned as `Ok` values, not errors.
pub(crate) async fn write<R, F, Fut>(app: &App, f: F) -> crate::Result<R>
where
    F: FnOnce(MutationDb) -> Fut,
    Fut: Future<Output = crate::Result<R>>,
{
    let txn = app.db().begin_write().await?;
    match f(MutationDb::new(txn.clone())).await {
        Ok(v) => {
            txn.commit_with(|_, _| Ok(())).await?;
            Ok(v)
        }
        Err(e) => {
            txn.rollback();
            Err(e)
        }
    }
}

/// Trim and lowercase; reject things that are obviously not an address.
pub fn normalize_email(email: &str) -> Result<String, UserError> {
    let email = email.trim().to_lowercase();
    let valid = email.len() <= 254
        && email.parse::<lettre::Address>().is_ok()
        && !email.chars().any(|c| c.is_whitespace() || c.is_control());
    if valid { Ok(email) } else { Err(UserError::new("invalid_email", "Invalid email address")) }
}

pub(crate) async fn user_by_email(db: &QueryDb, email: &str) -> crate::Result<Option<Doc<User>>> {
    db.query::<User>().with_index(User::by_email, |q| q.eq(User::email, Some(email.to_string()))).first().await
}

async fn credential_of(db: &QueryDb, user: Id<User>) -> crate::Result<Option<Doc<Credential>>> {
    db.query::<Credential>().with_index(Credential::by_user, |q| q.eq(Credential::user, user)).first().await
}

/// For password sign-in: the user and their password hash, if both exist.
pub(crate) async fn password_login(app: &App, email: String) -> crate::Result<Option<(Doc<User>, String)>> {
    read(app, |db| async move {
        let Some(user) = user_by_email(&db, &email).await? else { return Ok(None) };
        Ok(credential_of(&db, user.id).await?.map(|c| (user, c.value.password_hash)))
    })
    .await
}

pub(crate) async fn create_password_user(
    app: &App,
    email: String,
    password_hash: String,
    name: Option<String>,
) -> crate::Result<Result<Id<User>, UserError>> {
    write(app, |db| async move {
        if user_by_email(&db, &email).await?.is_some() {
            return Ok(Err(email_in_use()));
        }
        let user = db.insert(User { email: Some(email), email_verified: false, name, image: None }).await?;
        db.insert(Credential { user, password_hash }).await?;
        Ok(Ok(user))
    })
    .await
}

/// Replace a user's password hash with `new_hash` (same password, newer algorithm) if it is still `old_hash`. Sessions
/// are kept: nothing changed for the user. Returns whether it was replaced.
pub(crate) async fn upgrade_password_hash(
    app: &App,
    user: Id<User>,
    old_hash: String,
    new_hash: String,
) -> crate::Result<bool> {
    write(app, |db| async move {
        match credential_of(&db, user).await? {
            Some(c) if c.value.password_hash == old_hash => {
                db.replace(c.id, Credential { user, password_hash: new_hash }).await?;
                Ok(true)
            }
            _ => Ok(false),
        }
    })
    .await
}

/// Set (or add) a user's password and end all their sessions.
pub(crate) async fn set_password(app: &App, user: Id<User>, password_hash: String) -> crate::Result<()> {
    set_password_keeping(app, user, password_hash, None).await
}

/// Set (or add) a user's password and end their sessions, except `keep` (the session making the change).
pub(crate) async fn set_password_keeping(
    app: &App,
    user: Id<User>,
    password_hash: String,
    keep: Option<Id<Session>>,
) -> crate::Result<()> {
    write(app, |db| async move {
        match credential_of(&db, user).await? {
            Some(c) => db.replace(c.id, Credential { user, password_hash }).await?,
            None => {
                db.insert(Credential { user, password_hash }).await?;
            }
        }
        let sessions =
            db.query::<Session>().with_index(Session::by_user, |q| q.eq(Session::user, Some(user))).collect().await?;
        for s in sessions.into_iter().filter(|s| Some(s.id) != keep) {
            delete_session_in(&db, s.id).await?;
        }
        Ok(())
    })
    .await
}

/// A user's password hash, if they have a password.
pub(crate) async fn password_hash_of(app: &App, user: Id<User>) -> crate::Result<Option<String>> {
    read(app, |db| async move { Ok(credential_of(&db, user).await?.map(|c| c.value.password_hash)) }).await
}

/// Mark a user's email verified because they proved they own the inbox (a verification or reset code). Any OAuth
/// account linked while the address was unverified was linked by someone who never proved it (a provider that
/// shared an unverified address): those links are dropped, so the earlier holder can't sign in through them.
pub(crate) async fn mark_email_verified(app: &App, user: Id<User>) -> crate::Result<()> {
    write(app, |db| async move {
        if let Some(mut u) = db.get(user).await?
            && !u.email_verified
        {
            u.value.email_verified = true;
            db.replace(user, u.value).await?;
            unlink_accounts(&db, user).await?;
        }
        Ok(())
    })
    .await
}

/// Delete every OAuth / external account link of `user`.
async fn unlink_accounts(db: &MutationDb, user: Id<User>) -> crate::Result<()> {
    for a in db.query::<Account>().with_index(Account::by_user, |q| q.eq(Account::user, user)).collect().await? {
        db.delete(a.id).await?;
    }
    Ok(())
}

/// Passwordless sign-in for a verified email: find the user or create one.
pub(crate) async fn passwordless_user(
    app: &App,
    email: String,
    allow_signup: bool,
) -> crate::Result<Result<Id<User>, UserError>> {
    write(app, |db| async move {
        if let Some(mut u) = user_by_email(&db, &email).await? {
            if !u.email_verified {
                // Whoever set up this unverified account (maybe not the inbox owner) loses its password.
                strip_unverified_account(&db, u.id).await?;
                u.value.email_verified = true;
                db.replace(u.id, u.value).await?;
            }
            return Ok(Ok(u.id));
        }
        if !allow_signup {
            return Ok(Err(signups_disabled()));
        }
        Ok(Ok(db.insert(User { email: Some(email), email_verified: true, name: None, image: None }).await?))
    })
    .await
}

/// The answer to a sign-up with an address that has an account.
pub(crate) fn email_in_use() -> UserError {
    UserError::new("email_in_use", "An account with this email already exists")
}

/// A user profile from an OAuth / OIDC provider.
#[derive(Debug, Clone, Default)]
pub(crate) struct ExternalProfile {
    pub account_id: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub name: Option<String>,
    pub image: Option<String>,
}

/// Find the user linked to this provider account, link it to the user with the same (provider-verified) email, or
/// create a new user.
pub(crate) async fn oauth_user(
    app: &App,
    provider: String,
    profile: ExternalProfile,
    allow_signup: bool,
) -> crate::Result<Result<Id<User>, UserError>> {
    write(app, |db| async move {
        let account_id = profile.account_id.clone();
        let existing = db
            .query::<Account>()
            .with_index(Account::by_provider_account, |q| {
                q.eq(Account::provider, provider.clone()).eq(Account::provider_account_id, account_id.clone())
            })
            .first()
            .await?;
        if let Some(account) = existing {
            return Ok(Ok(account.user));
        }
        let email = profile.email.as_deref().and_then(|e| normalize_email(e).ok());
        if let Some(email) = &email
            && let Some(mut user) = user_by_email(&db, email).await?
        {
            if !profile.email_verified {
                return Ok(Err(UserError::new(
                    "email_in_use",
                    "An account with this email already exists; sign in to it first",
                )));
            }
            if !user.email_verified {
                // Pre-account-takeover defense: an unverified account with this email may have been created by
                // someone else. The provider has verified the address, so its owner takes over the account and any
                // password set by the previous holder is removed.
                strip_unverified_account(&db, user.id).await?;
                user.value.email_verified = true;
                if user.name.is_none() {
                    user.value.name = profile.name.clone();
                }
                if user.image.is_none() {
                    user.value.image = profile.image.clone();
                }
                db.replace(user.id, user.value.clone()).await?;
            }
            db.insert(Account { user: user.id, provider, provider_account_id: account_id }).await?;
            return Ok(Ok(user.id));
        }
        if !allow_signup {
            return Ok(Err(signups_disabled()));
        }
        // An address the provider didn't verify is not kept: it could be anyone's, and an unverified user is
        // taken over by whoever proves the inbox (above, and `passwordless_user`), which would hand that person
        // this account's data. Whatever the provider, such users start without an email.
        let email = email.filter(|_| profile.email_verified);
        let user = db
            .insert(User { email_verified: email.is_some(), email, name: profile.name, image: profile.image })
            .await?;
        db.insert(Account { user, provider, provider_account_id: account_id }).await?;
        Ok(Ok(user))
    })
    .await
}

/// Whether [`oauth_user`] would create a user for this profile (as of a snapshot): no account links it, and no user
/// has its address.
pub(crate) async fn oauth_would_create(app: &App, provider: &str, profile: &ExternalProfile) -> crate::Result<bool> {
    let (provider, account_id) = (provider.to_string(), profile.account_id.clone());
    let email = profile.email.as_deref().and_then(|e| normalize_email(e).ok());
    read(app, |db| async move {
        let linked = db
            .query::<Account>()
            .with_index(Account::by_provider_account, |q| {
                q.eq(Account::provider, provider).eq(Account::provider_account_id, account_id)
            })
            .first()
            .await?
            .is_some();
        if linked {
            return Ok(false);
        }
        match email {
            Some(email) => Ok(user_by_email(&db, &email).await?.is_none()),
            None => Ok(true),
        }
    })
    .await
}

/// The user behind an external JWT subject (`super::jwt`): the `_accounts` row `(provider = issuer,
/// providerAccountId = subject)` links it, and is created together with a fresh `_users` row the first time the
/// subject is seen.
///
/// External subjects are never linked to an existing user by email (unlike OAuth): the issuer is trusted to identify
/// its own users, not to vouch for addresses that another sign-in method may already own. The new user gets the
/// token's `email` only when the issuer marks it verified and no other user has it yet (so password sign-up and
/// magic links can't be used to take it over either), plus `name` and `picture`. The profile is not updated on later
/// sign-ins (apps can do that from the claims they receive through their own functions).
///
/// Returns the user id; looks up first in a snapshot so that known subjects cost no write.
pub(crate) async fn external_user(app: &App, claims: &super::jwt::ExternalClaims) -> crate::Result<Id<User>> {
    let (provider, subject) = (claims.issuer.clone(), claims.subject.clone());
    let lookup = |db: QueryDb, provider: String, subject: String| async move {
        let account = db
            .query::<Account>()
            .with_index(Account::by_provider_account, |q| {
                q.eq(Account::provider, provider).eq(Account::provider_account_id, subject)
            })
            .first()
            .await?;
        Ok::<_, crate::Error>(match account {
            Some(a) if db.get(a.user).await?.is_some() => Some(Ok(a.user)),
            Some(a) => Some(Err(a.id)),
            None => None,
        })
    };
    let (p, s) = (provider.clone(), subject.clone());
    if let Some(Ok(user)) = read(app, |db| lookup(db, p, s)).await? {
        return Ok(user);
    }
    let claims = claims.clone();
    write(app, |db| async move {
        match lookup(db.share(), provider.clone(), subject.clone()).await? {
            Some(Ok(user)) => return Ok(user),
            // An account whose user is gone (shouldn't happen: delete_user removes accounts): replace it.
            Some(Err(stale)) => db.delete(stale).await?,
            None => {}
        }
        let mut email = claims.email.as_deref().filter(|_| claims.email_verified).and_then(|e| normalize_email(e).ok());
        if let Some(e) = &email
            && user_by_email(&db, e).await?.is_some()
        {
            email = None;
        }
        let image = claims.picture.filter(|p| p.starts_with("https://") && p.len() <= 2048);
        let name = claims.name.map(|n| n.chars().take(200).collect());
        let user = db.insert(User { email_verified: email.is_some(), email, name, image }).await?;
        db.insert(Account { user, provider, provider_account_id: subject }).await?;
        Ok(user)
    })
    .await
}

/// Whether a document (a user, a session) still exists.
pub(crate) async fn exists(app: &App, id: String) -> crate::Result<bool> {
    read(app, |db| async move { Ok(db.get_raw(&id).await?.is_some()) }).await
}

fn signups_disabled() -> UserError {
    UserError::new("signups_disabled", "New accounts can't be created")
}

/// Remove the password, sessions and linked accounts of an account whose email was never verified: whoever set it
/// up (maybe not the inbox owner) loses every way back in, including the OAuth account that created it.
async fn strip_unverified_account(db: &MutationDb, user: Id<User>) -> crate::Result<()> {
    if let Some(c) = credential_of(db, user).await? {
        db.delete(c.id).await?;
    }
    let sessions =
        db.query::<Session>().with_index(Session::by_user, |q| q.eq(Session::user, Some(user))).collect().await?;
    for s in sessions {
        delete_session_in(db, s.id).await?;
    }
    unlink_accounts(db, user).await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------------------------------------------------

/// Who a session belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Principal {
    User(Id<User>),
    Admin(Id<Admin>),
}

/// Client details recorded on a session.
#[derive(Debug, Clone, Default)]
pub struct ClientInfo {
    pub user_agent: Option<String>,
    pub ip: Option<String>,
}

/// Create a session; returns its id and the (plaintext) refresh token.
pub(crate) async fn create_session(
    app: &App,
    keys: &Keys,
    principal: Principal,
    client: ClientInfo,
    now: f64,
    ttl_ms: f64,
) -> crate::Result<(Id<Session>, String)> {
    let refresh = random_token();
    let refresh_token_hash = keys.hash_token(&refresh);
    let (user, admin) = match principal {
        Principal::User(u) => (Some(u), None),
        Principal::Admin(a) => (None, Some(a)),
    };
    let session = Session {
        user,
        admin,
        refresh_token_hash,
        expires_at: now + ttl_ms,
        user_agent: client.user_agent.map(|s| s.chars().take(512).collect()),
        ip: client.ip,
        last_used_at: now,
        impersonated_by: None,
    };
    let id = write(app, |db| async move { db.insert(session).await }).await?;
    Ok((id, refresh))
}

/// Create a session of `user` on behalf of `admin` (impersonation), with a fixed expiry, and audit it in the same
/// transaction. Returns its id, refresh token and expiry. `None` if the user doesn't exist.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn create_impersonation_session(
    app: &App,
    keys: &Keys,
    admin: Id<Admin>,
    user: Id<User>,
    client: ClientInfo,
    now: f64,
    ttl_ms: f64,
    purpose: serde_json::Value,
) -> crate::Result<Option<(Id<Session>, String, f64)>> {
    let refresh = random_token();
    let refresh_token_hash = keys.hash_token(&refresh);
    let expires_at = now + ttl_ms;
    let found = write(app, |db| async move {
        // An id of another table is "no such user", not an error.
        if db.get(user).await.ok().flatten().is_none() {
            return Ok(None);
        }
        let admin_email = db.get(admin).await?.map(|a| a.value.email).unwrap_or_default();
        let session = Session {
            user: Some(user),
            admin: None,
            refresh_token_hash,
            expires_at,
            user_agent: Some(format!("impersonation by {admin_email}").chars().take(512).collect()),
            ip: client.ip,
            last_used_at: now,
            impersonated_by: Some(admin),
        };
        let id = db.insert(session).await?;
        let mut details = purpose;
        details["sessionId"] = id.to_string().into();
        details["expiresAt"] = expires_at.into();
        let rec = crate::audit::Record::new("impersonate").target("_users", Some(user.to_string())).details(details);
        crate::audit::record(&db, Some(admin), rec).await?;
        Ok(Some(id))
    })
    .await?;
    Ok(found.map(|id| (id, refresh, expires_at)))
}

pub(crate) async fn delete_session_in(db: &MutationDb, session: Id<Session>) -> crate::Result<()> {
    let used = db
        .query::<UsedRefreshToken>()
        .with_index(UsedRefreshToken::by_session, |q| q.eq(UsedRefreshToken::session, session))
        .collect()
        .await?;
    for u in used {
        db.delete(u.id).await?;
    }
    if db.get(session).await?.is_some() {
        db.delete(session).await?;
    }
    Ok(())
}

pub(crate) async fn delete_session(app: &App, session: Id<Session>) -> crate::Result<()> {
    write(app, |db| async move { delete_session_in(&db, session).await }).await
}

/// Delete the session whose current refresh token is `refresh` (sign-out with a refresh token). Checked in a
/// snapshot first: an unknown token (the endpoint is unauthenticated) never takes the write lock.
pub(crate) async fn delete_session_by_refresh(app: &App, keys: &Keys, refresh: &str) -> crate::Result<bool> {
    let hash = keys.hash_token(refresh);
    let known = read(app, |db| {
        let hash = hash.clone();
        async move {
            Ok(db
                .query::<Session>()
                .with_index(Session::by_refresh_token_hash, |q| q.eq(Session::refresh_token_hash, hash))
                .first()
                .await?
                .is_some())
        }
    })
    .await?;
    if !known {
        return Ok(false);
    }
    write(app, |db| async move {
        let session = db
            .query::<Session>()
            .with_index(Session::by_refresh_token_hash, |q| q.eq(Session::refresh_token_hash, hash))
            .first()
            .await?;
        match session {
            Some(s) => {
                delete_session_in(&db, s.id).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    })
    .await
}

/// Whether `session` exists, is unexpired and belongs to `principal`.
pub(crate) async fn session_active(
    app: &App,
    session: Id<Session>,
    principal: Principal,
    now: f64,
) -> crate::Result<bool> {
    read(app, |db| async move {
        let Some(s) = db.get(session).await? else { return Ok(false) };
        let owner = match principal {
            Principal::User(u) => s.user == Some(u),
            Principal::Admin(a) => s.admin == Some(a),
        };
        Ok(owner && s.expires_at > now)
    })
    .await
}

/// What happened to a refresh token.
#[derive(Debug)]
pub(crate) enum RefreshOutcome {
    Rotated {
        session: Id<Session>,
        principal: Principal,
        refresh: String,
        expires_at: f64,
    },
    /// Unknown token.
    Invalid,
    /// The session expired (and was deleted).
    Expired,
    /// Rotated moments ago (within the grace window): probably a concurrent refresh from another tab.
    Stale,
    /// An already-rotated token was presented again: the session has been revoked.
    Reused,
}

pub(crate) async fn rotate_refresh_token(
    app: &App,
    keys: &Keys,
    refresh: &str,
    client: ClientInfo,
    now: f64,
    ttl_ms: f64,
    grace_ms: f64,
) -> crate::Result<RefreshOutcome> {
    let hash = keys.hash_token(refresh);
    let new_refresh = random_token();
    let new_hash = keys.hash_token(&new_refresh);
    write(app, |db| async move {
        let current = db
            .query::<Session>()
            .with_index(Session::by_refresh_token_hash, |q| q.eq(Session::refresh_token_hash, hash.clone()))
            .first()
            .await?;
        if let Some(s) = current {
            // The index lookup matched the keyed hash; compare again in constant time for good measure.
            if !ct_eq(&s.refresh_token_hash, &hash) {
                return Ok(RefreshOutcome::Invalid);
            }
            if s.expires_at <= now {
                delete_session_in(&db, s.id).await?;
                return Ok(RefreshOutcome::Expired);
            }
            let principal = match (s.user, s.admin) {
                (Some(u), _) => Principal::User(u),
                (None, Some(a)) => Principal::Admin(a),
                (None, None) => anyhow::bail!("session {} has no owner", s.id),
            };
            db.insert(UsedRefreshToken { session: s.id, token_hash: hash, rotated_at: now }).await?;
            let mut session = s.value;
            session.refresh_token_hash = new_hash;
            // Impersonation sessions keep their fixed expiry; others slide.
            if session.impersonated_by.is_none() {
                session.expires_at = now + ttl_ms;
            }
            session.last_used_at = now;
            if client.ip.is_some() {
                session.ip = client.ip;
            }
            let expires_at = session.expires_at;
            db.replace(s.id, session).await?;
            return Ok(RefreshOutcome::Rotated { session: s.id, principal, refresh: new_refresh, expires_at });
        }
        let used = db
            .query::<UsedRefreshToken>()
            .with_index(UsedRefreshToken::by_token_hash, |q| q.eq(UsedRefreshToken::token_hash, hash))
            .first()
            .await?;
        match used {
            Some(u) if now - u.rotated_at < grace_ms => Ok(RefreshOutcome::Stale),
            Some(u) => {
                delete_session_in(&db, u.session).await?;
                Ok(RefreshOutcome::Reused)
            }
            None => Ok(RefreshOutcome::Invalid),
        }
    })
    .await
}

// ---------------------------------------------------------------------------------------------------------------------
// Verification codes
// ---------------------------------------------------------------------------------------------------------------------

/// Create a code for `(email, purpose)`, replacing earlier ones. Returns `(numeric code, link token)`. The failure
/// window of the code it replaces carries over ([`MAX_CODE_FAILURES`]).
pub(crate) async fn create_code(
    app: &App,
    keys: &Keys,
    email: String,
    purpose: &'static str,
    user: Option<Id<User>>,
    redirect_to: Option<String>,
    expires_at: f64,
) -> crate::Result<(String, String)> {
    let code = random_code(CODE_DIGITS);
    let token = random_token();
    let now = crate::auth::service::now_ms();
    let mut row = VerificationCode {
        email: email.clone(),
        purpose: purpose.to_string(),
        user,
        redirect_to,
        code_hash: keys.hash_token(&code),
        token_hash: keys.hash_token(&token),
        expires_at,
        attempts: 0,
        recent_failures: None,
        failures_since: None,
    };
    write(app, |db| async move {
        for old in codes_for(&db, &email, purpose).await? {
            if let Some(since) = old.failures_since.filter(|since| now - since < CODE_FAILURE_WINDOW_MS) {
                let failures = old.recent_failures.unwrap_or(0);
                if failures >= row.recent_failures.unwrap_or(0) {
                    (row.recent_failures, row.failures_since) = (Some(failures), Some(since));
                }
            }
            db.delete(old.id).await?;
        }
        db.insert(row).await
    })
    .await?;
    Ok((code, token))
}

async fn codes_for(db: &QueryDb, email: &str, purpose: &str) -> crate::Result<Vec<Doc<VerificationCode>>> {
    db.query::<VerificationCode>()
        .with_index(VerificationCode::by_email_purpose, |q| {
            q.eq(VerificationCode::email, email.to_string()).eq(VerificationCode::purpose, purpose.to_string())
        })
        .collect()
        .await
}

/// Consume a link token. Returns the code's row if it was valid (it is deleted either way once found).
pub(crate) async fn consume_token(
    app: &App,
    keys: &Keys,
    purpose: &'static str,
    token: &str,
    now: f64,
) -> crate::Result<Option<VerificationCode>> {
    let hash = keys.hash_token(token);
    write(app, |db| async move {
        let row = db
            .query::<VerificationCode>()
            .with_index(VerificationCode::by_token_hash, |q| q.eq(VerificationCode::token_hash, hash.clone()))
            .first()
            .await?;
        let Some(row) = row else { return Ok(None) };
        if row.purpose != purpose || !ct_eq(&row.token_hash, &hash) || row.attempts >= MAX_CODE_ATTEMPTS {
            return Ok(None);
        }
        db.delete(row.id).await?;
        Ok((row.expires_at > now).then_some(row.value))
    })
    .await
}

/// Check a numeric code for `(email, purpose)`. A wrong code counts an attempt; after [`MAX_CODE_ATTEMPTS`] the code
/// is burned, and after [`MAX_CODE_FAILURES`] in the window no code for the email is checked until the window ends.
/// Returns the row when the code matched (and deletes it). Burned and expired rows stay until a new code replaces
/// them, so their failure window isn't lost.
pub(crate) async fn consume_code(
    app: &App,
    keys: &Keys,
    purpose: &'static str,
    email: String,
    code: &str,
    now: f64,
) -> crate::Result<Option<VerificationCode>> {
    let hash = keys.hash_token(code.trim());
    write(app, |db| async move {
        let Some(row) = codes_for(&db, &email, purpose).await?.into_iter().next() else { return Ok(None) };
        let window_open = row.failures_since.is_some_and(|since| now - since < CODE_FAILURE_WINDOW_MS);
        let recent = if window_open { row.recent_failures.unwrap_or(0) } else { 0 };
        if row.expires_at <= now || row.attempts >= MAX_CODE_ATTEMPTS || recent >= MAX_CODE_FAILURES {
            return Ok(None);
        }
        if ct_eq(&row.code_hash, &hash) {
            db.delete(row.id).await?;
            return Ok(Some(row.value));
        }
        let mut updated = row.value;
        updated.attempts += 1;
        updated.recent_failures = Some(recent + 1);
        if !window_open {
            updated.failures_since = Some(now);
        }
        db.replace(row.id, updated).await?;
        Ok(None)
    })
    .await
}

// ---------------------------------------------------------------------------------------------------------------------
// Admins
// ---------------------------------------------------------------------------------------------------------------------

pub(crate) async fn any_admin(app: &App) -> crate::Result<bool> {
    read(app, |db| async move { Ok(db.query::<Admin>().first().await?.is_some()) }).await
}

pub(crate) async fn admin_by_email(app: &App, email: String) -> crate::Result<Option<Doc<Admin>>> {
    read(app, |db| async move {
        db.query::<Admin>().with_index(Admin::by_email, |q| q.eq(Admin::email, email)).first().await
    })
    .await
}

/// Create an admin. With `only_if_none`, fails with `setup_done` if any admin exists (checked in the same
/// transaction, so two concurrent setups can't both succeed).
pub(crate) async fn create_admin(
    app: &App,
    email: String,
    password_hash: String,
    only_if_none: bool,
) -> crate::Result<Result<Id<Admin>, UserError>> {
    write(app, |db| async move {
        if only_if_none && db.query::<Admin>().first().await?.is_some() {
            return Ok(Err(UserError::new("setup_done", "An administrator already exists")));
        }
        let existing =
            db.query::<Admin>().with_index(Admin::by_email, |q| q.eq(Admin::email, email.clone())).first().await?;
        if existing.is_some() {
            return Ok(Err(UserError::new("email_in_use", "An administrator with this email already exists")));
        }
        Ok(Ok(db.insert(Admin { email, password_hash }).await?))
    })
    .await
}

/// Set an admin's password and end their sessions. `false` if there is no such admin.
pub(crate) async fn set_admin_password(app: &App, email: String, password_hash: String) -> crate::Result<bool> {
    write(app, |db| async move {
        let Some(admin) =
            db.query::<Admin>().with_index(Admin::by_email, |q| q.eq(Admin::email, email.clone())).first().await?
        else {
            return Ok(false);
        };
        db.replace(admin.id, Admin { email, password_hash }).await?;
        let sessions = db
            .query::<Session>()
            .with_index(Session::by_admin, |q| q.eq(Session::admin, Some(admin.id)))
            .collect()
            .await?;
        for s in sessions {
            delete_session_in(&db, s.id).await?;
        }
        Ok(true)
    })
    .await
}
