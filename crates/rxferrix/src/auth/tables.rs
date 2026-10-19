//! The auth system tables. Defined with the normal `#[table]` macro (docs/CONTRACT.md §4); their names start with `_`
//! so codegen leaves them out of the app's `DataModel` (except `_users`, which app tables may reference).
//!
//! Secrets never live in `_users`: password hashes are in `_credentials` / `_admins`, and every token or code is
//! stored only as a keyed hash (see [`super::tokens::TokenHasher`]). `Doc<User>` is therefore safe to return to
//! clients.

use crate::prelude::*;

/// A user account (`_users`). Apps keep their own profile data in their own tables, referencing `Id<User>`.
/// The creation time is the document's `_creationTime`. `email` is normalized (trimmed, lowercase), and `None` for
/// OAuth accounts whose provider shared no email. (No field doc comments here: ts-rs would inline them into the
/// generated `SystemDataModel`.)
#[table(name = "_users")]
#[index(by_email = [email])]
pub struct User {
    pub email: Option<String>,
    pub email_verified: bool,
    pub name: Option<String>,
    pub image: Option<String>,
}

/// A user's password (`_credentials`). Kept apart from `_users` so a user document can never leak the hash.
#[table(name = "_credentials")]
#[index(by_user = [user])]
pub struct Credential {
    pub user: Id<User>,
    /// argon2id PHC string; an imported bcrypt hash until the user's next sign-in (`auth::password`).
    pub password_hash: String,
}

/// A linked OAuth / OIDC account (`_accounts`).
#[table(name = "_accounts")]
#[index(by_provider_account = [provider, provider_account_id])]
#[index(by_user = [user])]
pub struct Account {
    pub user: Id<User>,
    /// Provider id as configured, e.g. `"github"`, `"google"` or a custom OIDC id.
    pub provider: String,
    /// The provider's stable subject id (`sub`, or GitHub's numeric id).
    pub provider_account_id: String,
}

/// A dashboard administrator (`_admins`). Separate from `_users`: admins sign in at `/api/auth/admin/signin`.
#[table(name = "_admins")]
#[index(by_email = [email])]
pub struct Admin {
    pub email: String,
    /// argon2id PHC string.
    pub password_hash: String,
}

/// A sign-in session (`_sessions`): one refresh token chain. Exactly one of `user` / `admin` is set.
#[table(name = "_sessions")]
#[index(by_refresh_token_hash = [refresh_token_hash])]
#[index(by_user = [user])]
#[index(by_admin = [admin])]
pub struct Session {
    pub user: Option<Id<User>>,
    pub admin: Option<Id<Admin>>,
    /// Keyed hash of the current refresh token.
    pub refresh_token_hash: String,
    /// Sliding expiry (ms since the epoch), pushed forward on every refresh.
    pub expires_at: f64,
    pub user_agent: Option<String>,
    pub ip: Option<String>,
    pub last_used_at: f64,
    /// Set on sessions an administrator minted to act as the user (impersonation): such sessions have a fixed,
    /// short expiry that refreshing doesn't extend. Apps can refuse sensitive operations when it is set
    /// (`ctx.db.get(ctx.auth.session_id()?)`).
    pub impersonated_by: Option<Id<Admin>>,
}

/// A refresh token that has already been rotated (`_used_refresh_tokens`). Presenting one again means the token
/// leaked, so the whole session is revoked (reuse detection). Rows live as long as their session.
#[table(name = "_used_refresh_tokens")]
#[index(by_token_hash = [token_hash])]
#[index(by_session = [session])]
pub struct UsedRefreshToken {
    pub session: Id<Session>,
    pub token_hash: String,
    pub rotated_at: f64,
}

/// A one-time code for email verification, password reset or passwordless sign-in (`_verification_codes`).
/// Each row carries both a short numeric code (typed by the user, attempt-limited) and a long link token.
#[table(name = "_verification_codes")]
#[index(by_token_hash = [token_hash])]
#[index(by_email_purpose = [email, purpose])]
pub struct VerificationCode {
    pub email: String,
    /// `"verify_email"`, `"reset_password"` or `"sign_in"`.
    pub purpose: String,
    pub user: Option<Id<User>>,
    /// Keyed hash of the numeric code.
    pub code_hash: String,
    /// Keyed hash of the link token.
    pub token_hash: String,
    pub expires_at: f64,
    /// Failed attempts at the numeric code.
    pub attempts: u32,
}

/// A second factor of a user or an administrator (`_mfa_factors`): a TOTP secret, sealed (`auth::secretbox`) with
/// the owner's id as associated data, plus the keyed hashes of the unused recovery codes. Exactly one of `user` /
/// `admin` is set, and at most one row exists per owner. Unconfirmed rows are enrollments in progress: the
/// factor counts only once its first code was entered (`confirmed`).
#[table(name = "_mfa_factors")]
#[index(by_user = [user])]
#[index(by_admin = [admin])]
pub struct MfaFactor {
    pub user: Option<Id<User>>,
    pub admin: Option<Id<Admin>>,
    /// `"totp"`.
    pub kind: String,
    pub secret_encrypted: String,
    pub confirmed: bool,
    /// Keyed hashes of the recovery codes not used yet.
    pub recovery_code_hashes: Vec<String>,
    /// The TOTP step of the last accepted code: that step and earlier ones are refused (a code works once).
    pub last_used_step: Option<i64>,
    /// Wrong codes in a row; `MAX_MFA_ATTEMPTS` of them lock the factor for a while.
    pub failed_attempts: u32,
    /// While set and in the future, codes are refused (ms since the epoch).
    pub locked_until: Option<f64>,
}
