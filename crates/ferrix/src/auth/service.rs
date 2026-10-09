//! [`AuthService`]: the auth state an [`App`] carries (keys, config, mailer, rate limiter, OAuth flows, the admin
//! setup token), and the operations behind the HTTP endpoints, the sync worker and the CLI.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use super::config::{AuthConfig, RedirectAllowlist};
use super::email::{Email, LogMailer, Mailer, Purpose, SmtpMailer};
use super::identity::Identity;
use super::oauth::OAuthFlows;
use super::rate_limit::{RateLimiter, Rule};
use super::revocation::{Revocations, SessionWatch};
use super::store::{self, ClientInfo, Principal, RefreshOutcome};
use super::tables::{Admin, Session, User};
use super::tokens::{AccessClaims, Keys, TokenError, ct_eq, random_token};
use super::password;
use crate::UserError;
use crate::id::Id;
use crate::runtime::App;

/// Tokens returned by every successful sign-in and refresh.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Seconds until the access token expires.
    pub expires_in: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admin_id: Option<String>,
}

/// An access token that passed validation.
#[derive(Debug, Clone)]
pub struct Authenticated {
    pub identity: Identity,
    /// When the access token expires.
    pub expires_at: SystemTime,
}

/// A failed auth operation: an HTTP status plus a `UserError` code and message for the client.
#[derive(Debug, Clone)]
pub struct AuthFailure {
    pub status: u16,
    pub error: UserError,
    pub retry_after: Option<Duration>,
}

impl AuthFailure {
    pub fn new(status: u16, code: &str, message: impl Into<String>) -> Self {
        AuthFailure { status, error: UserError::new(code, message), retry_after: None }
    }

    pub fn bad_request(e: UserError) -> Self {
        AuthFailure { status: 400, error: e, retry_after: None }
    }

    pub fn not_found(what: &str) -> Self {
        AuthFailure::new(404, "not_enabled", format!("{what} is not enabled on this server"))
    }

    fn invalid_credentials() -> Self {
        AuthFailure::new(401, "invalid_credentials", "Invalid email or password")
    }

    fn invalid_code() -> Self {
        AuthFailure::new(400, "invalid_code", "The code or link is invalid or has expired")
    }
}

impl From<anyhow::Error> for AuthFailure {
    fn from(e: anyhow::Error) -> Self {
        if let Some(u) = e.downcast_ref::<UserError>() {
            return AuthFailure::bad_request(u.clone());
        }
        tracing::error!("auth: internal error: {e:#}");
        AuthFailure::new(500, "server_error", "Server Error")
    }
}

impl From<UserError> for AuthFailure {
    fn from(e: UserError) -> Self {
        AuthFailure::bad_request(e)
    }
}

pub type AuthResult<T> = Result<T, AuthFailure>;

/// Endpoint groups for rate limiting.
#[derive(Debug, Clone, Copy)]
pub enum Limit {
    Credentials,
    Email,
    Refresh,
}

/// Auth state of an [`App`]. Get it with `app.auth()`.
pub struct AuthService {
    pub(crate) config: AuthConfig,
    pub(crate) keys: Keys,
    pub(crate) redirects: RedirectAllowlist,
    mailer: Option<Arc<dyn Mailer>>,
    limiter: RateLimiter,
    pub(crate) oauth: OAuthFlows,
    setup_token_hash: Mutex<Option<String>>,
    revocations: Arc<Revocations>,
}

pub(crate) fn now_ms() -> f64 {
    crate::db::wall_clock_ms()
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl AuthService {
    /// Load the server secret from `dir` and set up mail, OAuth and limits from `config`.
    pub fn new(dir: &Path, config: AuthConfig, dev: bool) -> crate::Result<AuthService> {
        let keys = Keys::load_or_create(dir)?;
        let redirects = RedirectAllowlist::new(&config.redirect_urls, dev)?;
        let mailer: Option<Arc<dyn Mailer>> = match (&config.mailer, &config.smtp) {
            (Some(m), _) => Some(m.clone()),
            (None, Some(smtp)) => Some(Arc::new(SmtpMailer::new(smtp)?)),
            (None, None) if dev => Some(Arc::new(LogMailer)),
            (None, None) => None,
        };
        if mailer.is_none() && (config.password || config.magic_link) {
            tracing::warn!(
                "auth: no SMTP configured (--smtp-url / FERRIX_SMTP_URL); verification, reset and magic-link emails \
                 can't be sent"
            );
        }
        let oauth = OAuthFlows::new(&config.oauth);
        Ok(AuthService { config, keys, redirects, mailer, limiter: RateLimiter::default(), oauth, setup_token_hash: Mutex::new(None), revocations: Arc::default() })
    }

    pub fn config(&self) -> &AuthConfig {
        &self.config
    }

    fn access_ttl_s(&self) -> u64 {
        self.config.access_token_ttl.as_secs().max(1)
    }

    fn session_ttl_ms(&self) -> f64 {
        self.config.session_ttl.as_secs_f64() * 1000.0
    }

    /// Take a rate-limit token for `(group, key)`.
    pub(crate) fn limit(&self, group: Limit, key: &str) -> AuthResult<()> {
        let Some(limits) = self.config.rate_limits else { return Ok(()) };
        let (name, rule) = match group {
            Limit::Credentials => ("credentials", Rule { burst: limits.credential_burst, per_minute: limits.credential_per_minute }),
            Limit::Email => ("email", Rule { burst: limits.email_burst, per_minute: limits.email_per_minute }),
            Limit::Refresh => ("refresh", Rule { burst: limits.refresh_burst, per_minute: limits.refresh_per_minute }),
        };
        self.limiter.check(name, key, rule).map_err(|retry| AuthFailure {
            status: 429,
            error: UserError::new("rate_limited", "Too many requests; try again later"),
            retry_after: Some(retry),
        })
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Access tokens

    /// Validate an access token: signature, expiry, and that its session is still active (so sign-out and
    /// revocation take effect for every new connection and request immediately).
    pub async fn authenticate(&self, app: &App, token: &str) -> Result<Authenticated, TokenError> {
        self.authenticate_inner(app, token, false).await.map(|(a, _)| a)
    }

    /// [`AuthService::authenticate`] for long-lived connections: also returns a [`SessionWatch`] that fires as soon
    /// as the token's session ends (sign-out, revocation, user deletion, ...; see [`super::revocation`]). The watch is
    /// registered before the session check, so no revocation can slip in between.
    pub async fn authenticate_watched(
        &self,
        app: &App,
        token: &str,
    ) -> Result<(Authenticated, Option<SessionWatch>), TokenError> {
        self.authenticate_inner(app, token, true).await
    }

    /// The registry of connections watching their sessions (live revocation).
    pub fn revocations(&self) -> &Arc<Revocations> {
        &self.revocations
    }

    async fn authenticate_inner(
        &self,
        app: &App,
        token: &str,
        watch: bool,
    ) -> Result<(Authenticated, Option<SessionWatch>), TokenError> {
        let claims = self.keys.verify(token, now_s())?;
        let session: Id<Session> = claims.sid.parse().map_err(|_| TokenError::Malformed)?;
        let (principal, identity) = if claims.adm {
            let admin: Id<Admin> = claims.sub.parse().map_err(|_| TokenError::Malformed)?;
            (Principal::Admin(admin), Identity::admin(admin, Some(session)))
        } else {
            let user: Id<User> = claims.sub.parse().map_err(|_| TokenError::Malformed)?;
            (Principal::User(user), Identity::user(user, Some(session)))
        };
        let watch = watch.then(|| self.revocations.watch(app, session.to_string()));
        match store::session_active(app, session, principal, now_ms()).await {
            Ok(true) => Ok((Authenticated { identity, expires_at: UNIX_EPOCH + Duration::from_secs(claims.exp) }, watch)),
            Ok(false) => Err(TokenError::SessionEnded),
            Err(e) => {
                tracing::error!("auth: session lookup failed: {e:#}");
                Err(TokenError::SessionEnded)
            }
        }
    }

    fn issue(&self, session: Id<Session>, principal: Principal, refresh: String) -> AuthTokens {
        let ttl = self.access_ttl_s();
        let (sub, adm, user_id, admin_id) = match principal {
            Principal::User(u) => (u.to_string(), false, Some(u.to_string()), None),
            Principal::Admin(a) => (a.to_string(), true, None, Some(a.to_string())),
        };
        let access_token = self.keys.sign(&AccessClaims::new(sub, session.to_string(), adm, now_s(), ttl));
        AuthTokens { access_token, refresh_token: refresh, expires_in: ttl, user_id, admin_id }
    }

    async fn start_session(&self, app: &App, principal: Principal, client: ClientInfo) -> AuthResult<AuthTokens> {
        let (session, refresh) =
            store::create_session(app, &self.keys, principal, client, now_ms(), self.session_ttl_ms()).await?;
        Ok(self.issue(session, principal, refresh))
    }

    /// Mint tokens for `user` without credentials, e.g. from a test or an admin tool.
    pub async fn sign_in_as(&self, app: &App, user: Id<User>) -> crate::Result<AuthTokens> {
        self.start_session(app, Principal::User(user), ClientInfo::default()).await.map_err(|f| anyhow::anyhow!(f.error))
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Password

    fn require_password(&self) -> AuthResult<()> {
        if self.config.password { Ok(()) } else { Err(AuthFailure::not_found("Password sign-in")) }
    }

    pub async fn sign_up(
        &self,
        app: &App,
        email: &str,
        password: String,
        name: Option<String>,
        client: ClientInfo,
    ) -> AuthResult<SignUpResult> {
        self.require_password()?;
        if !self.config.signups {
            return Err(AuthFailure::new(403, "signups_disabled", "New accounts can't be created"));
        }
        let email = store::normalize_email(email)?;
        password::check_password_policy(&password)?;
        let name = name.map(|n| n.trim().chars().take(200).collect::<String>()).filter(|n| !n.is_empty());
        let hash = password::hash(password).await?;
        let user = store::create_password_user(app, email.clone(), hash, name).await??;
        if self.mailer.is_some() {
            let redirect = self.redirects.default_target();
            self.send_code(app, email, Purpose::VerifyEmail, Some(user), redirect).await?;
        }
        if self.config.require_email_verification {
            return Ok(SignUpResult::VerificationRequired);
        }
        Ok(SignUpResult::SignedIn(self.start_session(app, Principal::User(user), client).await?))
    }

    pub async fn sign_in(&self, app: &App, email: &str, password: String, client: ClientInfo) -> AuthResult<AuthTokens> {
        self.require_password()?;
        // An invalid email gets the same answer as a wrong password (no enumeration through validation either).
        let email = store::normalize_email(email).map_err(|_| AuthFailure::invalid_credentials())?;
        let found = store::password_login(app, email).await?;
        let (user, stored) = match found {
            Some((u, h)) => (Some(u), Some(h)),
            None => (None, None),
        };
        // Always runs argon2 (against a dummy hash for unknown accounts), so timing reveals nothing.
        if !password::verify(password, stored).await {
            return Err(AuthFailure::invalid_credentials());
        }
        let user = user.expect("verified against a real hash");
        if self.config.require_email_verification && !user.email_verified {
            return Err(AuthFailure::new(403, "email_not_verified", "Verify your email address before signing in"));
        }
        self.start_session(app, Principal::User(user.id), client).await
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Refresh and sign-out

    pub async fn refresh(&self, app: &App, refresh_token: &str, client: ClientInfo) -> AuthResult<AuthTokens> {
        let grace = self.config.refresh_reuse_grace.as_secs_f64() * 1000.0;
        let outcome = store::rotate_refresh_token(
            app,
            &self.keys,
            refresh_token,
            client,
            now_ms(),
            self.session_ttl_ms(),
            grace,
        )
        .await?;
        match outcome {
            RefreshOutcome::Rotated { session, principal, refresh } => Ok(self.issue(session, principal, refresh)),
            RefreshOutcome::Invalid => Err(AuthFailure::new(401, "invalid_refresh_token", "Invalid refresh token")),
            RefreshOutcome::Expired => Err(AuthFailure::new(401, "session_expired", "The session has expired")),
            RefreshOutcome::Stale => Err(AuthFailure::new(
                401,
                "stale_refresh_token",
                "This refresh token was just rotated; use the newer one",
            )),
            RefreshOutcome::Reused => {
                tracing::warn!("auth: a rotated refresh token was reused; the session has been revoked");
                Err(AuthFailure::new(401, "refresh_token_reused", "Refresh token reuse detected; the session was revoked"))
            }
        }
    }

    /// End the session of `refresh_token`, or else of the (valid) `access_token`. Always succeeds.
    pub async fn sign_out(&self, app: &App, refresh_token: Option<&str>, access_token: Option<&str>) -> AuthResult<()> {
        if let Some(r) = refresh_token
            && store::delete_session_by_refresh(app, &self.keys, r).await?
        {
            return Ok(());
        }
        if let Some(a) = access_token
            && let Ok(claims) = self.keys.verify(a, now_s())
            && let Ok(session) = claims.sid.parse::<Id<Session>>()
        {
            store::delete_session(app, session).await?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Email codes

    fn require_mailer(&self) -> AuthResult<Arc<dyn Mailer>> {
        self.mailer.clone().ok_or_else(|| AuthFailure::new(503, "email_not_configured", "Email is not configured on this server"))
    }

    /// Resolve an app URL for links/redirects. An explicit URL that isn't allowed is an error (never silently
    /// replaced); no URL means the default target, if any.
    pub(crate) fn resolve_redirect(&self, redirect_to: Option<&str>) -> AuthResult<Option<String>> {
        match redirect_to.filter(|r| !r.is_empty()) {
            Some(r) => match self.redirects.resolve(Some(r)) {
                Some(url) => Ok(Some(url)),
                None => Err(AuthFailure::new(400, "invalid_redirect", "This redirect URL is not allowed")),
            },
            None => Ok(self.redirects.default_target()),
        }
    }

    /// Store a fresh code and send it (the sending happens in the background, so the response time doesn't depend
    /// on whether an email went out).
    async fn send_code(
        &self,
        app: &App,
        email: String,
        purpose: Purpose,
        user: Option<Id<User>>,
        redirect: Option<String>,
    ) -> AuthResult<()> {
        let mailer = self.require_mailer()?;
        let expires_at = now_ms() + purpose.ttl_ms();
        let (code, token) = store::create_code(app, &self.keys, email.clone(), purpose.as_str(), user, expires_at).await?;
        let link = redirect.map(|r| format!("{r}#{}={token}", purpose.fragment_key()));
        let (subject, body) = purpose.render(&code, link.as_deref());
        let message = Email { to: email, subject, body };
        tokio::spawn(async move {
            if let Err(e) = mailer.send(message).await {
                tracing::warn!("auth: sending email failed: {e:#}");
            }
        });
        Ok(())
    }

    /// Consume a code given either as a link token or as `(email, code)`.
    async fn consume(&self, app: &App, purpose: Purpose, proof: CodeProof) -> AuthResult<super::tables::VerificationCode> {
        let now = now_ms();
        let row = match proof {
            CodeProof::Token(t) => store::consume_token(app, &self.keys, purpose.as_str(), &t, now).await?,
            CodeProof::Code { email, code } => {
                let email = store::normalize_email(&email).map_err(|_| AuthFailure::invalid_code())?;
                store::consume_code(app, &self.keys, purpose.as_str(), email, &code, now).await?
            }
        };
        row.ok_or_else(AuthFailure::invalid_code)
    }

    /// Send an email verification code to `email` if it belongs to an unverified user. Always succeeds (no
    /// enumeration).
    pub async fn request_email_verification(&self, app: &App, email: &str, redirect_to: Option<&str>) -> AuthResult<()> {
        self.require_password()?;
        self.require_mailer()?;
        let redirect = self.resolve_redirect(redirect_to)?;
        let Ok(email) = store::normalize_email(email) else { return Ok(()) };
        self.limit(Limit::Email, &email)?;
        let user = store::read(app, |db| {
            let email = email.clone();
            async move { store::user_by_email(&db, &email).await }
        })
        .await?;
        if let Some(user) = user.filter(|u| !u.email_verified) {
            self.send_code(app, email, Purpose::VerifyEmail, Some(user.id), redirect).await?;
        }
        Ok(())
    }

    pub async fn confirm_email_verification(&self, app: &App, proof: CodeProof) -> AuthResult<()> {
        self.require_password()?;
        let row = self.consume(app, Purpose::VerifyEmail, proof).await?;
        let user = row.user.ok_or_else(AuthFailure::invalid_code)?;
        store::mark_email_verified(app, user).await?;
        Ok(())
    }

    /// Send a password reset code if `email` belongs to a user. Always succeeds (no enumeration).
    pub async fn request_password_reset(&self, app: &App, email: &str, redirect_to: Option<&str>) -> AuthResult<()> {
        self.require_password()?;
        self.require_mailer()?;
        let redirect = self.resolve_redirect(redirect_to)?;
        let Ok(email) = store::normalize_email(email) else { return Ok(()) };
        self.limit(Limit::Email, &email)?;
        let user = store::read(app, |db| {
            let email = email.clone();
            async move { store::user_by_email(&db, &email).await }
        })
        .await?;
        if let Some(user) = user {
            self.send_code(app, email, Purpose::ResetPassword, Some(user.id), redirect).await?;
        }
        Ok(())
    }

    /// Set a new password with a reset code. Ends every other session, marks the email verified (the user proved
    /// they read it) and signs in.
    pub async fn confirm_password_reset(
        &self,
        app: &App,
        proof: CodeProof,
        new_password: String,
        client: ClientInfo,
    ) -> AuthResult<AuthTokens> {
        self.require_password()?;
        password::check_password_policy(&new_password)?;
        let row = self.consume(app, Purpose::ResetPassword, proof).await?;
        let user = row.user.ok_or_else(AuthFailure::invalid_code)?;
        let hash = password::hash(new_password).await?;
        store::set_password(app, user, hash).await?;
        store::mark_email_verified(app, user).await?;
        self.start_session(app, Principal::User(user), client).await
    }

    /// Send a magic link + one-time code. Always succeeds (no enumeration): unknown emails get one too when
    /// sign-ups are allowed, and nothing otherwise.
    pub async fn request_magic_link(&self, app: &App, email: &str, redirect_to: Option<&str>) -> AuthResult<()> {
        if !self.config.magic_link {
            return Err(AuthFailure::not_found("Magic link sign-in"));
        }
        self.require_mailer()?;
        let redirect = self.resolve_redirect(redirect_to)?;
        let email = store::normalize_email(email)?;
        self.limit(Limit::Email, &email)?;
        if !self.config.signups {
            let exists = store::read(app, |db| {
                let email = email.clone();
                async move { store::user_by_email(&db, &email).await }
            })
            .await?
            .is_some();
            if !exists {
                return Ok(());
            }
        }
        self.send_code(app, email, Purpose::SignIn, None, redirect).await
    }

    pub async fn verify_magic_link(&self, app: &App, proof: CodeProof, client: ClientInfo) -> AuthResult<AuthTokens> {
        if !self.config.magic_link {
            return Err(AuthFailure::not_found("Magic link sign-in"));
        }
        let row = self.consume(app, Purpose::SignIn, proof).await?;
        let user = store::passwordless_user(app, row.email, self.config.signups).await??;
        self.start_session(app, Principal::User(user), client).await
    }

    /// Finish an OAuth sign-in for a provider profile.
    pub(crate) async fn oauth_sign_in(
        &self,
        app: &App,
        provider: &str,
        profile: store::ExternalProfile,
        client: ClientInfo,
    ) -> AuthResult<AuthTokens> {
        let user = store::oauth_user(app, provider.to_string(), profile, self.config.signups).await??;
        self.start_session(app, Principal::User(user), client).await
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Admins

    pub async fn admin_sign_in(&self, app: &App, email: &str, password: String, client: ClientInfo) -> AuthResult<AuthTokens> {
        let email = store::normalize_email(email).map_err(|_| AuthFailure::invalid_credentials())?;
        let admin = store::admin_by_email(app, email).await?;
        let stored = admin.as_ref().map(|a| a.password_hash.clone());
        if !password::verify(password, stored).await {
            return Err(AuthFailure::invalid_credentials());
        }
        let admin = admin.expect("verified against a real hash");
        self.start_session(app, Principal::Admin(admin.id), client).await
    }

    /// Create an admin (CLI `admin create`).
    pub async fn create_admin(&self, app: &App, email: &str, password: String) -> crate::Result<Id<Admin>> {
        let email = store::normalize_email(email)?;
        password::check_password_policy(&password)?;
        let hash = password::hash(password).await?;
        Ok(store::create_admin(app, email, hash, false).await??)
    }

    /// Set an admin's password (CLI `admin reset-password`). Ends the admin's sessions.
    pub async fn reset_admin_password(&self, app: &App, email: &str, password: String) -> crate::Result<()> {
        let email = store::normalize_email(email)?;
        password::check_password_policy(&password)?;
        let hash = password::hash(password).await?;
        if !store::set_admin_password(app, email.clone(), hash).await? {
            anyhow::bail!("no administrator with email {email}");
        }
        Ok(())
    }

    pub async fn needs_setup(&self, app: &App) -> crate::Result<bool> {
        Ok(!store::any_admin(app).await?)
    }

    /// When no admin exists, create a one-time setup token (replacing any earlier one) and return it; it is kept
    /// only in memory, hashed. `serve` prints it as `<public url>/_/setup#token=<token>`.
    pub async fn create_setup_token(&self, app: &App) -> crate::Result<Option<String>> {
        if !self.needs_setup(app).await? {
            return Ok(None);
        }
        let token = random_token();
        *self.setup_token_hash.lock().unwrap_or_else(|e| e.into_inner()) = Some(self.keys.hash_token(&token));
        Ok(Some(token))
    }

    /// Create the first admin with the setup token, and sign them in.
    pub async fn complete_setup(
        &self,
        app: &App,
        token: &str,
        email: &str,
        password: String,
        client: ClientInfo,
    ) -> AuthResult<AuthTokens> {
        let expected = self.setup_token_hash.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let valid = expected.is_some_and(|h| ct_eq(&h, &self.keys.hash_token(token)));
        if !valid {
            return Err(AuthFailure::new(401, "invalid_setup_token", "Invalid or used setup token"));
        }
        let email = store::normalize_email(email)?;
        password::check_password_policy(&password)?;
        let hash = password::hash(password).await?;
        let admin = store::create_admin(app, email, hash, true).await??;
        *self.setup_token_hash.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.start_session(app, Principal::Admin(admin), client).await
    }
}

/// Result of a sign-up.
#[derive(Debug)]
pub enum SignUpResult {
    SignedIn(AuthTokens),
    /// `require_email_verification` is on: a verification email was sent; sign in after verifying.
    VerificationRequired,
}

/// Proof of email ownership: the token from a link, or the numeric code plus the email it was sent to.
#[derive(Debug, Clone)]
pub enum CodeProof {
    Token(String),
    Code { email: String, code: String },
}
