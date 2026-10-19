//! Two-factor authentication (2FA): a TOTP second factor ([`super::totp`]) with recovery codes, for users and
//! administrators alike. Opt-in per account.
//!
//! **Enrolling** (with the account's access token): `mfa_setup` creates an unconfirmed factor and returns the secret
//! (as an `otpauth://` URI and its QR code) for the authenticator app; `mfa_confirm` takes the first code, which
//! proves the app was set up, and returns the recovery codes (shown once; each works once). Accounts with a password
//! confirm it to start a setup, so a leaked access token alone can't take over the second factor.
//!
//! **Signing in**: once a factor is confirmed, every sign-in method (password, magic link, OAuth, password reset,
//! admin sign-in) answers with a short-lived second-factor token instead of a session
//! ([`super::SignInOutcome::MfaRequired`]); `mfa_verify` takes it with a TOTP or recovery code and starts the
//! session. Impersonation, `sign_in_as` and external JWTs never go through the second factor.
//!
//! **Brute force**: codes are checked in constant time; a TOTP code works once (the step of the last accepted code
//! is kept); [`MAX_MFA_ATTEMPTS`] wrong codes in a row lock the factor for [`MFA_LOCKOUT`]; the endpoints are rate
//! limited per IP like the credential endpoints. The secret is sealed with the server's settings key
//! (`super::secretbox`) and the owner's id as associated data, and recovery codes are stored as keyed hashes, so
//! the database alone reveals neither.
//!
//! **Lost device**: a recovery code disables the factor or signs the user in; without one, an administrator removes
//! the factor from the dashboard (`_system/dashboard:reset_mfa`), and `admin disable-2fa <email>` does it for an
//! administrator.

use std::time::Duration;

use serde::Serialize;

use super::identity::{FORBIDDEN, Identity, UNAUTHENTICATED};
use super::password;
use super::service::{AuthFailure, AuthResult, AuthService, AuthTokens, SignInMethod, now_ms, now_s};
use super::store::{self, ClientInfo, Principal};
use super::tables::{MfaFactor, Session};
use super::tokens::ct_eq;
use super::totp;
use crate::db::query::{MutationDb, QueryDb};
use crate::id::Id;
use crate::runtime::App;
use crate::schema::Doc;

/// How long the second step of a sign-in may take.
pub const MFA_TOKEN_TTL: Duration = Duration::from_secs(5 * 60);
/// Wrong codes in a row that lock a factor.
pub const MAX_MFA_ATTEMPTS: u32 = 5;
/// How long a factor stays locked after too many wrong codes.
pub const MFA_LOCKOUT: Duration = Duration::from_secs(5 * 60);

const KIND_TOTP: &str = "totp";

/// Whether (and how) an account has two-factor authentication.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MfaStatus {
    /// A confirmed factor exists: sign-ins need a code.
    pub enabled: bool,
    /// A setup was started but its first code hasn't been entered yet.
    pub pending: bool,
    /// Unused recovery codes.
    pub recovery_codes_left: u32,
    /// The enabled factors (`"totp"`).
    pub methods: Vec<&'static str>,
}

/// A started TOTP enrollment: what the authenticator app needs.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MfaSetup {
    /// The secret in base32, for apps that take it by hand.
    pub secret: String,
    /// The `otpauth://totp/...` URI.
    pub uri: String,
    /// The URI as a QR code, an `<svg>` element to put in a page.
    pub qr_svg: String,
}

/// Fresh recovery codes (`xxxxx-xxxxx`); shown once, each works once.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryCodes {
    pub recovery_codes: Vec<String>,
}

// ---------------------------------------------------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------------------------------------------------

/// The associated data the secret of `principal` is sealed with: a ciphertext copied to another row won't open.
fn aad(principal: Principal) -> String {
    match principal {
        Principal::User(u) => format!("mfa:user:{u}"),
        Principal::Admin(a) => format!("mfa:admin:{a}"),
    }
}

/// The factor row of `principal`, confirmed or not.
pub(crate) async fn factor_in(db: &QueryDb, principal: Principal) -> crate::Result<Option<Doc<MfaFactor>>> {
    match principal {
        Principal::User(u) => {
            db.query::<MfaFactor>().with_index(MfaFactor::by_user, |q| q.eq(MfaFactor::user, Some(u))).first().await
        }
        Principal::Admin(a) => {
            db.query::<MfaFactor>().with_index(MfaFactor::by_admin, |q| q.eq(MfaFactor::admin, Some(a))).first().await
        }
    }
}

/// Whether `principal` has a confirmed second factor (so a sign-in needs a code).
pub(crate) async fn has_confirmed_factor(app: &App, principal: Principal) -> crate::Result<bool> {
    store::read(app, |db| async move { Ok(factor_in(&db, principal).await?.is_some_and(|f| f.confirmed)) }).await
}

/// Delete the factor of `principal` (confirmed or pending). Returns whether there was one.
pub(crate) async fn remove_factor_in(db: &MutationDb, principal: Principal) -> crate::Result<bool> {
    match factor_in(db, principal).await? {
        Some(f) => {
            db.delete(f.id).await?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// [`remove_factor_in`] in its own transaction.
pub(crate) async fn remove_factor(app: &App, principal: Principal) -> crate::Result<bool> {
    store::write(app, |db| async move { remove_factor_in(&db, principal).await }).await
}

/// Administrators' own changes are audited (users' aren't: the audit log records administrator actions).
async fn audit_admin(db: &MutationDb, principal: Principal, action: &'static str) -> crate::Result<()> {
    if let Principal::Admin(admin) = principal {
        let rec = crate::audit::Record::new(action).target("_admins", Some(admin.to_string()));
        crate::audit::record(db, Some(admin), rec).await?;
    }
    Ok(())
}

/// The email of the account, as the authenticator app labels it.
async fn account_label(db: &QueryDb, principal: Principal) -> crate::Result<String> {
    Ok(match principal {
        Principal::User(u) => db.get(u).await?.and_then(|d| d.value.email).unwrap_or_else(|| u.to_string()),
        Principal::Admin(a) => db.get(a).await?.map(|d| d.value.email).unwrap_or_else(|| a.to_string()),
    })
}

pub(crate) fn status_of(factor: Option<&MfaFactor>) -> MfaStatus {
    match factor {
        Some(f) if f.confirmed => MfaStatus {
            enabled: true,
            pending: false,
            recovery_codes_left: f.recovery_code_hashes.len() as u32,
            methods: vec![KIND_TOTP],
        },
        Some(_) => MfaStatus { enabled: false, pending: true, recovery_codes_left: 0, methods: Vec::new() },
        None => MfaStatus { enabled: false, pending: false, recovery_codes_left: 0, methods: Vec::new() },
    }
}

fn invalid_code() -> AuthFailure {
    AuthFailure::new(400, "invalid_mfa_code", "The code is not valid")
}

fn invalid_mfa_token() -> AuthFailure {
    AuthFailure::new(401, "invalid_mfa_token", "This sign-in has expired; sign in again")
}

/// The principal of an identity, if it is a user or an admin.
fn principal_of(identity: &Identity) -> Option<Principal> {
    match (&identity.user_id, &identity.admin_id) {
        (Some(u), _) => u.parse().ok().map(Principal::User),
        (None, Some(a)) => a.parse().ok().map(Principal::Admin),
        (None, None) => None,
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------------------------------------------------

impl AuthService {
    /// The account behind an access token, for managing its second factor: a user or admin signed in through this
    /// server (external JWTs have no second factor here) and not an impersonation session (an administrator acting
    /// as the user must not be able to enroll or remove the user's factor).
    async fn mfa_principal(&self, app: &App, access_token: &str) -> AuthResult<Principal> {
        let identity = self.authenticate(app, access_token).await.map_err(super::http::token_failure)?.identity;
        let principal =
            principal_of(&identity).ok_or_else(|| AuthFailure::new(401, UNAUTHENTICATED, "Sign in first"))?;
        let Some(session) = identity.session_id.as_deref().and_then(|s| s.parse::<Id<Session>>().ok()) else {
            return Err(AuthFailure::new(
                403,
                FORBIDDEN,
                "Two-factor authentication is managed by accounts signed in through this server",
            ));
        };
        let impersonated =
            store::read(
                app,
                |db| async move { Ok(db.get(session).await?.is_some_and(|s| s.impersonated_by.is_some())) },
            )
            .await?;
        if impersonated {
            return Err(AuthFailure::new(
                403,
                FORBIDDEN,
                "An impersonation session can't change two-factor authentication",
            ));
        }
        Ok(principal)
    }

    /// Accounts with a password must confirm it before their second factor is set up (a leaked access token must
    /// not be enough). Accounts without one (OAuth or magic link only) have nothing to confirm.
    async fn confirm_password(&self, app: &App, principal: Principal, password: Option<String>) -> AuthResult<()> {
        let stored = match principal {
            Principal::User(u) => store::password_hash_of(app, u).await?,
            Principal::Admin(a) => {
                store::read(app, |db| async move { Ok(db.get(a).await?.map(|d| d.value.password_hash)) }).await?
            }
        };
        let Some(stored) = stored else { return Ok(()) };
        let Some(password) = password.filter(|p| !p.is_empty()) else {
            return Err(AuthFailure::new(
                400,
                "password_required",
                "Enter your password to set up two-factor authentication",
            ));
        };
        if !password::verify(password, Some(stored)).await {
            return Err(AuthFailure::new(400, "invalid_current_password", "The password is not correct"));
        }
        Ok(())
    }

    /// The `issuer` of the `otpauth://` URI: [`super::AuthConfig::totp_issuer`], else the public URL's host, else
    /// `RxFerrix`.
    fn totp_issuer(&self) -> String {
        if let Some(issuer) = &self.config.totp_issuer {
            return issuer.clone();
        }
        self.config
            .public_url
            .as_deref()
            .and_then(|u| url::Url::parse(u).ok())
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| "RxFerrix".to_string())
    }

    /// Whether the account of `access_token` has two-factor authentication (`GET /api/auth/mfa`).
    pub async fn mfa_status(&self, app: &App, access_token: &str) -> AuthResult<MfaStatus> {
        let principal = self.mfa_principal(app, access_token).await?;
        let factor = store::read(app, |db| async move { factor_in(&db, principal).await }).await?;
        Ok(status_of(factor.as_ref().map(|f| &f.value)))
    }

    /// Start a TOTP enrollment for the account of `access_token`: a fresh secret, stored unconfirmed (replacing a
    /// pending one) until [`AuthService::mfa_confirm`] gets its first code. `mfa_already_enabled` if a confirmed
    /// factor exists (disable it first). `password` is required for accounts that have one.
    pub async fn mfa_setup(&self, app: &App, access_token: &str, password: Option<String>) -> AuthResult<MfaSetup> {
        let principal = self.mfa_principal(app, access_token).await?;
        self.confirm_password(app, principal, password).await?;
        let secret = totp::new_secret();
        let sealed = self.keys.secret_box().seal(&aad(principal), &secret);
        let (user, admin) = match principal {
            Principal::User(u) => (Some(u), None),
            Principal::Admin(a) => (None, Some(a)),
        };
        let account = store::write(app, |db| async move {
            if factor_in(&db, principal).await?.is_some_and(|f| f.confirmed) {
                return Ok(Err(AuthFailure::new(
                    400,
                    "mfa_already_enabled",
                    "Two-factor authentication is already enabled; disable it before setting it up again",
                )));
            }
            remove_factor_in(&db, principal).await?;
            db.insert(MfaFactor {
                user,
                admin,
                kind: KIND_TOTP.to_string(),
                secret_encrypted: sealed,
                confirmed: false,
                recovery_code_hashes: Vec::new(),
                last_used_step: None,
                failed_attempts: 0,
                locked_until: None,
            })
            .await?;
            Ok(Ok(account_label(&db, principal).await?))
        })
        .await??;
        let uri = totp::otpauth_uri(&self.totp_issuer(), &account, &secret);
        let qr_svg = totp::qr_svg(&uri)?;
        Ok(MfaSetup { secret: totp::encode_secret(&secret), uri, qr_svg })
    }

    /// Finish the enrollment with the authenticator's first code: the factor is confirmed and sign-ins need a code
    /// from now on. Returns the recovery codes (the only time they are shown). `invalid_mfa_code`, `mfa_not_pending`.
    pub async fn mfa_confirm(&self, app: &App, access_token: &str, code: &str) -> AuthResult<RecoveryCodes> {
        let principal = self.mfa_principal(app, access_token).await?;
        let codes = totp::recovery_codes(totp::RECOVERY_CODES);
        let hashes = self.recovery_hashes(&codes);
        let secret_box = self.keys.secret_box();
        let (code, now_s) = (code.to_string(), now_s());
        store::write(app, |db| async move {
            let Some(f) = factor_in(&db, principal).await? else {
                return Ok(Err(AuthFailure::new(400, "mfa_not_pending", "Start the setup first")));
            };
            if f.confirmed {
                return Ok(Err(AuthFailure::new(
                    400,
                    "mfa_already_enabled",
                    "Two-factor authentication is already enabled",
                )));
            }
            let secret = secret_box.open(&aad(principal), &f.secret_encrypted)?;
            let Some(step) = totp::matching_step(&secret, &code, now_s) else { return Ok(Err(invalid_code())) };
            let mut factor = f.value;
            factor.confirmed = true;
            factor.last_used_step = Some(step as i64);
            factor.recovery_code_hashes = hashes;
            factor.failed_attempts = 0;
            factor.locked_until = None;
            db.replace(f.id, factor).await?;
            audit_admin(&db, principal, "mfa_enable").await?;
            Ok(Ok(()))
        })
        .await??;
        Ok(RecoveryCodes { recovery_codes: codes })
    }

    fn recovery_hashes(&self, codes: &[String]) -> Vec<String> {
        codes.iter().map(|c| self.keys.hash_token(&c.replace('-', ""))).collect()
    }

    /// Check a TOTP or recovery code against the confirmed factor of `principal`, in one transaction: a wrong code
    /// counts an attempt (and locks the factor after [`MAX_MFA_ATTEMPTS`]), a right one resets the count, marks its
    /// step used or removes the recovery code.
    async fn check_code(&self, app: &App, principal: Principal, code: &str) -> AuthResult<()> {
        let secret_box = self.keys.secret_box();
        let recovery_hash = totp::normalize_recovery_code(code).map(|c| self.keys.hash_token(&c));
        let (code, now_s, now_ms) = (code.to_string(), now_s(), now_ms());
        store::write(app, |db| async move {
            let Some(f) = factor_in(&db, principal).await?.filter(|f| f.confirmed) else {
                return Ok(Err(AuthFailure::new(400, "mfa_not_enabled", "Two-factor authentication is not enabled")));
            };
            if let Some(until) = f.locked_until
                && until > now_ms
            {
                return Ok(Err(AuthFailure {
                    status: 429,
                    error: crate::UserError::new(
                        "mfa_locked",
                        "Too many wrong codes; wait a few minutes before trying again",
                    ),
                    retry_after: Some(Duration::from_millis((until - now_ms).max(1000.0) as u64)),
                }));
            }
            let mut factor = f.value;
            let secret = secret_box.open(&aad(principal), &factor.secret_encrypted)?;
            // A TOTP code works once: only steps after the last accepted one count.
            let totp_step = totp::matching_step(&secret, &code, now_s)
                .filter(|step| factor.last_used_step.is_none_or(|last| (*step as i64) > last));
            let recovery_at = recovery_hash
                .as_deref()
                .and_then(|h| factor.recovery_code_hashes.iter().position(|stored| ct_eq(stored, h)));
            if totp_step.is_none() && recovery_at.is_none() {
                factor.failed_attempts += 1;
                if factor.failed_attempts >= MAX_MFA_ATTEMPTS {
                    factor.failed_attempts = 0;
                    factor.locked_until = Some(now_ms + MFA_LOCKOUT.as_secs_f64() * 1000.0);
                }
                db.replace(f.id, factor).await?;
                return Ok(Err(invalid_code()));
            }
            if let Some(step) = totp_step {
                factor.last_used_step = Some(step as i64);
            }
            if let Some(at) = recovery_at {
                factor.recovery_code_hashes.remove(at);
            }
            factor.failed_attempts = 0;
            factor.locked_until = None;
            db.replace(f.id, factor).await?;
            Ok(Ok(()))
        })
        .await?
    }

    /// The second step of a sign-in: the second-factor token of [`super::SignInOutcome::MfaRequired`] plus a TOTP or
    /// recovery code start the session. `invalid_mfa_token` (401), `invalid_mfa_code` (400), `mfa_locked` (429).
    pub async fn mfa_verify(
        &self,
        app: &App,
        mfa_token: &str,
        code: &str,
        client: ClientInfo,
    ) -> AuthResult<AuthTokens> {
        let claims = self.keys.verify_mfa(mfa_token, now_s()).map_err(|_| invalid_mfa_token())?;
        let principal = if claims.adm {
            Principal::Admin(claims.sub.parse().map_err(|_| invalid_mfa_token())?)
        } else {
            Principal::User(claims.sub.parse().map_err(|_| invalid_mfa_token())?)
        };
        // The factor is gone (disabled by an admin, or the account was deleted) since the token was issued: a
        // sign-in has to start over rather than skip the step that the account had then.
        self.check_code(app, principal, code).await.map_err(|e| match e.error.code.as_str() {
            "mfa_not_enabled" => invalid_mfa_token(),
            _ => e,
        })?;
        let method = claims.mth.as_deref().and_then(SignInMethod::parse);
        self.start_session_after_mfa(app, principal, client, method).await
    }

    /// Turn two-factor authentication off for the account of `access_token`. A confirmed factor needs a TOTP or
    /// recovery code; a pending setup is just dropped. `mfa_not_enabled` if there is none.
    pub async fn mfa_disable(&self, app: &App, access_token: &str, code: Option<&str>) -> AuthResult<()> {
        let principal = self.mfa_principal(app, access_token).await?;
        let factor = store::read(app, |db| async move { factor_in(&db, principal).await }).await?;
        let Some(factor) = factor else {
            return Err(AuthFailure::new(400, "mfa_not_enabled", "Two-factor authentication is not enabled"));
        };
        if factor.confirmed {
            self.check_code(app, principal, code.unwrap_or_default()).await?;
        }
        store::write(app, |db| async move {
            let removed = remove_factor_in(&db, principal).await?;
            if removed && factor.confirmed {
                audit_admin(&db, principal, "mfa_disable").await?;
            }
            Ok(())
        })
        .await?;
        Ok(())
    }

    /// Replace the recovery codes of the account of `access_token` (a TOTP or recovery code proves it's them). The
    /// old ones stop working; the new ones are returned once.
    pub async fn mfa_regenerate_recovery_codes(
        &self,
        app: &App,
        access_token: &str,
        code: &str,
    ) -> AuthResult<RecoveryCodes> {
        let principal = self.mfa_principal(app, access_token).await?;
        self.check_code(app, principal, code).await?;
        let codes = totp::recovery_codes(totp::RECOVERY_CODES);
        let hashes = self.recovery_hashes(&codes);
        store::write(app, |db| async move {
            let Some(f) = factor_in(&db, principal).await?.filter(|f| f.confirmed) else {
                anyhow::bail!("the factor disappeared while its codes were being replaced");
            };
            let mut factor = f.value;
            factor.recovery_code_hashes = hashes;
            db.replace(f.id, factor).await
        })
        .await?;
        Ok(RecoveryCodes { recovery_codes: codes })
    }
}
