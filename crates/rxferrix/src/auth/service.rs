//! [`AuthService`]: the auth state an [`App`] carries (keys, config, mailer, rate limiter, OAuth flows, the admin
//! setup token), and the operations behind the HTTP endpoints, the sync worker and the CLI.

use std::net::IpAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use super::config::{AuthConfig, OAuthProvider};
use super::effective::Effective;
use super::email::{Email, Mailer, Purpose};
use super::identity::Identity;
use super::jwt::JwtVerifier;
use super::oauth::OAuthFlows;
use super::password;
use super::rate_limit::{RateLimiter, Rule};
use super::revocation::{Revocations, SessionWatch};
use super::store::{self, ClientInfo, Principal, RefreshOutcome};
use super::tables::{Admin, Session, User};
use super::tokens::{AccessClaims, Keys, TokenError, ct_eq, random_token};
use crate::UserError;
use crate::id::Id;
use crate::runtime::App;
use crate::settings::model::StoredSettings;
use crate::settings::rate_limits::TokenCaller;

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

/// A successful user sign-in, as [`super::AuthConfig::on_sign_in`] sees it.
#[derive(Debug, Clone)]
pub struct SignIn {
    pub user: Id<User>,
    /// The session it started.
    pub session: Id<Session>,
    pub method: SignInMethod,
    /// User agent and IP address of the client.
    pub client: ClientInfo,
}

/// How a user signed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInMethod {
    Password,
    /// A sign-up that signed the new user in.
    SignUp,
    /// Setting a new password with a reset code signs the user in.
    PasswordReset,
    /// A magic link or a one-time code.
    MagicLink,
    /// The OAuth provider's id (`google`, `github`, ...).
    OAuth(String),
}

impl std::fmt::Display for SignInMethod {
    /// `password`, `sign_up`, `password_reset`, `magic_link`, `oauth:<provider>`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignInMethod::Password => f.write_str("password"),
            SignInMethod::SignUp => f.write_str("sign_up"),
            SignInMethod::PasswordReset => f.write_str("password_reset"),
            SignInMethod::MagicLink => f.write_str("magic_link"),
            SignInMethod::OAuth(provider) => write!(f, "oauth:{provider}"),
        }
    }
}

impl SignInMethod {
    /// The inverse of `Display` (for the method carried by a second-factor token).
    pub(crate) fn parse(s: &str) -> Option<SignInMethod> {
        Some(match s {
            "password" => SignInMethod::Password,
            "sign_up" => SignInMethod::SignUp,
            "password_reset" => SignInMethod::PasswordReset,
            "magic_link" => SignInMethod::MagicLink,
            _ => SignInMethod::OAuth(s.strip_prefix("oauth:")?.to_string()),
        })
    }
}

/// How a sign-in ended: with a session, or, for an account with two-factor authentication, with a short-lived
/// token to present together with a code (`POST /api/auth/mfa/verify`), which then starts the session.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum SignInOutcome {
    Tokens(AuthTokens),
    #[serde(rename_all = "camelCase")]
    MfaRequired {
        /// Always `true` (so clients can tell the two apart).
        mfa_required: bool,
        mfa_token: String,
        /// The factors that can complete it (`"totp"`; a recovery code is accepted wherever a code is).
        methods: Vec<&'static str>,
    },
}

impl SignInOutcome {
    /// The tokens, if the sign-in started a session (tests and callers that don't support a second factor).
    pub fn tokens(self) -> Option<AuthTokens> {
        match self {
            SignInOutcome::Tokens(t) => Some(t),
            SignInOutcome::MfaRequired { .. } => None,
        }
    }
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
    /// Credential checks, per client IP.
    Credentials,
    Email,
    Refresh,
    /// Password checks per account (keyed by the email), so a distributed attacker gets no steady guess rate
    /// against one account either. Same budget as `Credentials`.
    Account,
}

/// Auth state of an [`App`]. Get it with `app.auth()`.
pub struct AuthService {
    pub(crate) config: AuthConfig,
    pub(crate) keys: Keys,
    dev: bool,
    /// The mailer configured in code or on the command line (wins over SMTP from `_settings`).
    pub(crate) config_mailer: Option<Arc<dyn Mailer>>,
    /// `config` merged with `_settings`; swapped when the settings change.
    effective: RwLock<Arc<Effective>>,
    limiter: RateLimiter,
    /// Buckets of the `rateLimits` setting's rules, by (rule, client IP) (`crate::settings::rate_limits`).
    pub(crate) request_limiter: RateLimiter<(u64, IpAddr)>,
    /// Whether the effective configuration has path / function rate limit rules: read on every request, so a server
    /// without rules pays one atomic load instead of a lock and a reference count.
    path_limits_on: AtomicBool,
    function_limits_on: AtomicBool,
    pub(crate) oauth: OAuthFlows,
    pub(crate) jwt: JwtVerifier,
    setup_token_hash: Mutex<Option<String>>,
    revocations: Arc<Revocations>,
}

/// The longest an impersonation session may last.
pub const MAX_IMPERSONATION_TTL: Duration = Duration::from_secs(3600);
/// The default lifetime of an impersonation session.
pub const DEFAULT_IMPERSONATION_TTL: Duration = Duration::from_secs(15 * 60);

pub(crate) fn now_ms() -> f64 {
    crate::db::wall_clock_ms()
}

pub(crate) fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl AuthService {
    /// Load the server secret from `dir` and set up mail, OAuth and limits from `config`.
    pub fn new(dir: &Path, config: AuthConfig, dev: bool) -> crate::Result<AuthService> {
        let keys = Keys::load_or_create(dir)?;
        for p in config.oauth.iter().filter(|p| !p.is_configured()) {
            tracing::warn!("auth: OAuth provider `{}` has no client id/secret configured; it is disabled", p.id);
        }
        for p in &config.oauth {
            if let Some(problem) = p.problem() {
                tracing::error!("auth: OAuth provider `{}` can't sign anyone in: {problem}", p.id);
            }
        }
        let config_mailer = super::effective::config_mailer(&config)?;
        // Settings are loaded once the database is open (`start`); until then the configuration alone applies.
        let effective = Effective::build(&config, dev, config_mailer.clone(), &StoredSettings::default(), 0)?;
        let jwt = JwtVerifier::new(&config.jwt)?;
        Ok(AuthService {
            config,
            keys,
            dev,
            config_mailer,
            effective: RwLock::new(Arc::new(effective)),
            limiter: RateLimiter::default(),
            request_limiter: RateLimiter::default(),
            path_limits_on: AtomicBool::new(false),
            function_limits_on: AtomicBool::new(false),
            oauth: OAuthFlows::new(),
            jwt,
            setup_token_hash: Mutex::new(None),
            revocations: Arc::default(),
        })
    }

    /// The configuration given in code and on the command line (without `_settings`; the merged result is `current()`).
    pub fn config(&self) -> &AuthConfig {
        &self.config
    }

    /// The configuration in effect: [`AuthService::config`] merged with the runtime settings in `_settings`.
    pub(crate) fn current(&self) -> Arc<Effective> {
        self.effective.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Re-read `_settings` and swap in the resulting configuration (a newer one is never replaced by an older one).
    pub(crate) async fn reload_settings(&self, app: &App) -> crate::Result<()> {
        let snapshot = app.db().snapshot().await?;
        let ts = snapshot.ts;
        let db =
            crate::db::query::QueryDb::new(crate::db::query::Source::Snapshot(snapshot), app.db().catalog().clone());
        let stored = StoredSettings::load(&db, &self.keys.secret_box()).await?;
        drop(db);
        let next = match Effective::build(&self.config, self.dev, self.config_mailer.clone(), &stored, ts) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("settings: the stored settings can't be applied; keeping the previous ones: {e:#}");
                return Ok(());
            }
        };
        let mut current = self.effective.write().unwrap_or_else(|e| e.into_inner());
        if next.ts >= current.ts {
            self.path_limits_on.store(next.rate_limits.has_paths(), Ordering::Relaxed);
            self.function_limits_on.store(next.rate_limits.has_functions(), Ordering::Relaxed);
            *current = Arc::new(next);
        }
        Ok(())
    }

    /// Whether any path rate limit rule is in effect.
    pub(crate) fn path_limits_on(&self) -> bool {
        self.path_limits_on.load(Ordering::Relaxed)
    }

    /// Whether any function rate limit rule is in effect.
    pub(crate) fn function_limits_on(&self) -> bool {
        self.function_limits_on.load(Ordering::Relaxed)
    }

    /// Who `token` says the caller is, as far as its signature and expiry tell (no session lookup for RxFerrix's own
    /// tokens): how path rate limit rules tell signed-in users from guests on every request without a database read.
    pub(crate) async fn token_caller(&self, app: &App, token: &str) -> TokenCaller {
        if !self.jwt.is_empty() && super::tokens::header_alg(token).is_some_and(|alg| alg != "HS256") {
            return match self.authenticate(app, token).await {
                Ok(a) if a.identity.admin_id.is_some() => TokenCaller::Admin,
                Ok(_) => TokenCaller::User,
                Err(_) => TokenCaller::Guest,
            };
        }
        match self.keys.verify(token, now_s()) {
            Ok(claims) if claims.adm => TokenCaller::Admin,
            Ok(_) => TokenCaller::User,
            Err(_) => TokenCaller::Guest,
        }
    }

    /// The OAuth provider `id`, if enabled (in code or in `_settings`).
    pub(crate) fn oauth_provider(&self, id: &str) -> Option<OAuthProvider> {
        self.current().oauth.get(id).cloned()
    }

    /// The ids of the enabled OAuth providers.
    pub fn oauth_provider_ids(&self) -> Vec<String> {
        self.current().oauth.keys().cloned().collect()
    }

    /// Whether sign-ups are allowed (configuration, then `_settings`).
    pub fn signups_allowed(&self) -> bool {
        self.current().signups
    }

    /// Whether a browser origin may call the API (CORS); every origin unless `_settings` restricts them.
    pub fn cors_allows(&self, origin: &str) -> bool {
        match &self.current().cors_origins {
            None => true,
            Some(list) => list.iter().any(|o| o == origin),
        }
    }

    fn access_ttl_s(&self) -> u64 {
        self.current().access_token_ttl.as_secs().max(1)
    }

    fn session_ttl_ms(&self) -> f64 {
        self.current().session_ttl.as_secs_f64() * 1000.0
    }

    /// Take a rate-limit token for `(group, key)`.
    pub(crate) fn limit(&self, group: Limit, key: &str) -> AuthResult<()> {
        let Some(limits) = self.config.rate_limits else { return Ok(()) };
        let (name, rule) = match group {
            Limit::Credentials => {
                ("credentials", Rule { burst: limits.credential_burst, per_minute: limits.credential_per_minute })
            }
            Limit::Email => ("email", Rule { burst: limits.email_burst, per_minute: limits.email_per_minute }),
            Limit::Refresh => ("refresh", Rule { burst: limits.refresh_burst, per_minute: limits.refresh_per_minute }),
            Limit::Account => {
                ("account", Rule { burst: limits.credential_burst, per_minute: limits.credential_per_minute })
            }
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
        if !self.jwt.is_empty() && super::tokens::header_alg(token).is_some_and(|alg| alg != "HS256") {
            return self.authenticate_external(app, token, watch).await;
        }
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
            Ok(true) => {
                Ok((Authenticated { identity, expires_at: UNIX_EPOCH + Duration::from_secs(claims.exp) }, watch))
            }
            Ok(false) => Err(TokenError::SessionEnded),
            Err(e) => {
                tracing::error!("auth: session lookup failed: {e:#}");
                Err(TokenError::SessionEnded)
            }
        }
    }

    /// An external JWT (`super::jwt`): verify it, find or provision its user, and (for connections) watch the user
    /// document, since such tokens have no `_sessions` row: deleting the user ends their connections at once.
    async fn authenticate_external(
        &self,
        app: &App,
        token: &str,
        watch: bool,
    ) -> Result<(Authenticated, Option<SessionWatch>), TokenError> {
        let claims = self.jwt.verify(token, now_s()).await?;
        let user = store::external_user(app, &claims).await.map_err(|e| {
            tracing::error!("auth: provisioning the user of an external token failed: {e:#}");
            TokenError::SessionEnded
        })?;
        let identity = Identity::user(user, None);
        let authenticated = Authenticated { identity, expires_at: UNIX_EPOCH + Duration::from_secs(claims.expires_at) };
        if !watch {
            return Ok((authenticated, None));
        }
        // Registered before this existence check, like sessions (see `super::revocation`).
        let w = self.revocations.watch(app, user.to_string());
        match store::exists(app, user.to_string()).await {
            Ok(true) => Ok((authenticated, Some(w))),
            _ => Err(TokenError::SessionEnded),
        }
    }

    /// Sign tokens for a session. The access token never outlives the session (`session_expires_at`, ms).
    fn issue(
        &self,
        session: Id<Session>,
        principal: Principal,
        refresh: String,
        session_expires_at: f64,
    ) -> AuthTokens {
        let left_s = ((session_expires_at - now_ms()) / 1000.0).ceil().max(1.0) as u64;
        let ttl = self.access_ttl_s().min(left_s);
        let (sub, adm, user_id, admin_id) = match principal {
            Principal::User(u) => (u.to_string(), false, Some(u.to_string()), None),
            Principal::Admin(a) => (a.to_string(), true, None, Some(a.to_string())),
        };
        let access_token = self.keys.sign(&AccessClaims::new(sub, session.to_string(), adm, now_s(), ttl));
        AuthTokens { access_token, refresh_token: refresh, expires_in: ttl, user_id, admin_id }
    }

    /// Start a session. A user sign-in (`method`) then runs the `on_sign_in` hook.
    async fn start_session(
        &self,
        app: &App,
        principal: Principal,
        client: ClientInfo,
        method: Option<SignInMethod>,
    ) -> AuthResult<AuthTokens> {
        let (now, ttl) = (now_ms(), self.session_ttl_ms());
        let (session, refresh) = store::create_session(app, &self.keys, principal, client.clone(), now, ttl).await?;
        let tokens = self.issue(session, principal, refresh, now + ttl);
        if let (Some(method), Principal::User(user), Some(hook)) = (method, principal, &self.config.on_sign_in) {
            hook(app.clone(), SignIn { user, session, method, client }).await;
        }
        Ok(tokens)
    }

    /// The second step of a sign-in: a session right away, unless the account has a confirmed second factor, in
    /// which case a short-lived second-factor token is issued and [`AuthService::mfa_verify`] starts the session.
    async fn finish_sign_in(
        &self,
        app: &App,
        principal: Principal,
        client: ClientInfo,
        method: Option<SignInMethod>,
    ) -> AuthResult<SignInOutcome> {
        if super::mfa::has_confirmed_factor(app, principal).await? {
            let (sub, adm) = match principal {
                Principal::User(u) => (u.to_string(), false),
                Principal::Admin(a) => (a.to_string(), true),
            };
            let claims = super::tokens::MfaClaims::new(
                sub,
                adm,
                method.map(|m| m.to_string()),
                now_s(),
                super::mfa::MFA_TOKEN_TTL.as_secs(),
            );
            return Ok(SignInOutcome::MfaRequired {
                mfa_required: true,
                mfa_token: self.keys.sign_mfa(&claims),
                methods: vec!["totp"],
            });
        }
        Ok(SignInOutcome::Tokens(self.start_session(app, principal, client, method).await?))
    }

    /// Start the session a second-factor token stands for, once its code was accepted (`super::mfa`).
    pub(crate) async fn start_session_after_mfa(
        &self,
        app: &App,
        principal: Principal,
        client: ClientInfo,
        method: Option<SignInMethod>,
    ) -> AuthResult<AuthTokens> {
        self.start_session(app, principal, client, method).await
    }

    /// Mint tokens for `user` without credentials, e.g. from a test or an admin tool.
    pub async fn sign_in_as(&self, app: &App, user: Id<User>) -> crate::Result<AuthTokens> {
        self.start_session(app, Principal::User(user), ClientInfo::default(), None)
            .await
            .map_err(|f| anyhow::anyhow!(f.error))
    }

    /// Impersonation: a real session of `user` minted by `admin`, marked `impersonated_by`, expiring after `ttl`
    /// (capped at [`MAX_IMPERSONATION_TTL`]; refreshing never extends it), and audited (`impersonate`) in the same
    /// transaction. `purpose` goes into the audit entry's details. `None` if the user doesn't exist.
    pub(crate) async fn impersonate(
        &self,
        app: &App,
        admin: Id<Admin>,
        user: Id<User>,
        ttl: Duration,
        purpose: serde_json::Value,
        client: ClientInfo,
    ) -> crate::Result<Option<(AuthTokens, Id<Session>, f64)>> {
        let ttl_ms = ttl.min(MAX_IMPERSONATION_TTL).max(Duration::from_secs(1)).as_secs_f64() * 1000.0;
        let created =
            store::create_impersonation_session(app, &self.keys, admin, user, client, now_ms(), ttl_ms, purpose)
                .await?;
        Ok(created.map(|(session, refresh, expires_at)| {
            (self.issue(session, Principal::User(user), refresh, expires_at), session, expires_at)
        }))
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
        if !self.current().signups {
            return Err(AuthFailure::new(403, "signups_disabled", "New accounts can't be created"));
        }
        let email = store::normalize_email(email)?;
        password::check_password_policy(&password)?;
        let name = name.map(|n| n.trim().chars().take(200).collect::<String>()).filter(|n| !n.is_empty());
        let hash = password::hash(password).await?;
        let user = store::create_password_user(app, email.clone(), hash, name).await??;
        if self.current().mailer.is_some() {
            let redirect = self.current().redirects.default_target();
            self.send_code(app, email, Purpose::VerifyEmail, Some(user), redirect).await?;
        }
        if self.config.require_email_verification {
            return Ok(SignUpResult::VerificationRequired);
        }
        Ok(SignUpResult::SignedIn(
            self.start_session(app, Principal::User(user), client, Some(SignInMethod::SignUp)).await?,
        ))
    }

    /// Password sign-in. The outcome is a session, or a second-factor token when the user has two-factor
    /// authentication.
    pub async fn sign_in(
        &self,
        app: &App,
        email: &str,
        password: String,
        client: ClientInfo,
    ) -> AuthResult<SignInOutcome> {
        self.require_password()?;
        // An invalid email gets the same answer as a wrong password (no enumeration through validation either).
        let email = store::normalize_email(email).map_err(|_| AuthFailure::invalid_credentials())?;
        self.limit(Limit::Account, &email)?;
        let found = store::password_login(app, email).await?;
        let (user, stored) = match found {
            Some((u, h)) => (Some(u), Some(h)),
            None => (None, None),
        };
        // An imported (bcrypt) hash becomes argon2id once the password is known to be right.
        let rehash = stored.clone().filter(|h| password::needs_rehash(h)).map(|h| (h, password.clone()));
        // Always runs argon2 (against a dummy hash for unknown accounts), so timing reveals nothing.
        if !password::verify(password, stored).await {
            return Err(AuthFailure::invalid_credentials());
        }
        let user = user.expect("verified against a real hash");
        if let Some((old_hash, password)) = rehash {
            let upgraded =
                async { store::upgrade_password_hash(app, user.id, old_hash, password::hash(password).await?).await };
            if let Err(e) = upgraded.await {
                tracing::warn!(user = %user.id, "could not upgrade an imported password hash: {e:#}");
            }
        }
        if self.config.require_email_verification && !user.email_verified {
            return Err(AuthFailure::new(403, "email_not_verified", "Verify your email address before signing in"));
        }
        self.finish_sign_in(app, Principal::User(user.id), client, Some(SignInMethod::Password)).await
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Refresh and sign-out

    pub async fn refresh(&self, app: &App, refresh_token: &str, client: ClientInfo) -> AuthResult<AuthTokens> {
        let grace = self.config.refresh_reuse_grace.as_secs_f64() * 1000.0;
        let outcome =
            store::rotate_refresh_token(app, &self.keys, refresh_token, client, now_ms(), self.session_ttl_ms(), grace)
                .await?;
        match outcome {
            RefreshOutcome::Rotated { session, principal, refresh, expires_at } => {
                Ok(self.issue(session, principal, refresh, expires_at))
            }
            RefreshOutcome::Invalid => Err(AuthFailure::new(401, "invalid_refresh_token", "Invalid refresh token")),
            RefreshOutcome::Expired => Err(AuthFailure::new(401, "session_expired", "The session has expired")),
            RefreshOutcome::Stale => Err(AuthFailure::new(
                401,
                "stale_refresh_token",
                "This refresh token was just rotated; use the newer one",
            )),
            RefreshOutcome::Reused => {
                tracing::warn!("auth: a rotated refresh token was reused; the session has been revoked");
                Err(AuthFailure::new(
                    401,
                    "refresh_token_reused",
                    "Refresh token reuse detected; the session was revoked",
                ))
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

    /// The mailer auth emails go through (code, `--smtp-url`, or SMTP from the dashboard's settings; the dev log
    /// mailer in `--dev`), if any. Apps send their own email with it through `ActionCtx::send_email`.
    pub fn mailer(&self) -> Option<Arc<dyn Mailer>> {
        self.current().mailer.clone()
    }

    fn require_mailer(&self) -> AuthResult<Arc<dyn Mailer>> {
        self.current()
            .mailer
            .clone()
            .ok_or_else(|| AuthFailure::new(503, "email_not_configured", "Email is not configured on this server"))
    }

    /// Resolve an app URL for links/redirects. An explicit URL that isn't allowed is an error (never silently
    /// replaced); no URL means the default target, if any.
    pub(crate) fn resolve_redirect(&self, redirect_to: Option<&str>) -> AuthResult<Option<String>> {
        match redirect_to.filter(|r| !r.is_empty()) {
            Some(r) => match self.current().redirects.resolve(Some(r)) {
                Some(url) => Ok(Some(url)),
                None => Err(AuthFailure::new(400, "invalid_redirect", "This redirect URL is not allowed")),
            },
            None => Ok(self.current().redirects.default_target()),
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
        let (code, token) =
            store::create_code(app, &self.keys, email.clone(), purpose.as_str(), user, expires_at).await?;
        let link = redirect.map(|r| format!("{r}#{}={token}", purpose.fragment_key()));
        let (subject, body) = purpose.render(&code, link.as_deref());
        let message = Email::new(email, subject, body);
        tokio::spawn(async move {
            if let Err(e) = mailer.send(message).await {
                tracing::warn!("auth: sending email failed: {e:#}");
            }
        });
        Ok(())
    }

    /// Consume a code given either as a link token or as `(email, code)`.
    async fn consume(
        &self,
        app: &App,
        purpose: Purpose,
        proof: CodeProof,
    ) -> AuthResult<super::tables::VerificationCode> {
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
    pub async fn request_email_verification(
        &self,
        app: &App,
        email: &str,
        redirect_to: Option<&str>,
    ) -> AuthResult<()> {
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
    /// they read it) and signs in (through the second factor, if the user has one: a reset email doesn't bypass it).
    pub async fn confirm_password_reset(
        &self,
        app: &App,
        proof: CodeProof,
        new_password: String,
        client: ClientInfo,
    ) -> AuthResult<SignInOutcome> {
        self.require_password()?;
        password::check_password_policy(&new_password)?;
        let row = self.consume(app, Purpose::ResetPassword, proof).await?;
        let user = row.user.ok_or_else(AuthFailure::invalid_code)?;
        let hash = password::hash(new_password).await?;
        store::set_password(app, user, hash).await?;
        store::mark_email_verified(app, user).await?;
        self.finish_sign_in(app, Principal::User(user), client, Some(SignInMethod::PasswordReset)).await
    }

    /// Change the password of the user signed in with `access_token`, who proves it's them with `current`. The user's
    /// other sessions end; the one making the change stays signed in. A user without a password (OAuth only) sets one
    /// through a password reset instead.
    pub async fn change_password(
        &self,
        app: &App,
        access_token: &str,
        current: String,
        new_password: String,
    ) -> AuthResult<()> {
        self.require_password()?;
        let identity = self.authenticate(app, access_token).await.map_err(super::http::token_failure)?.identity;
        let user = identity.user_id.as_deref().and_then(|u| u.parse::<Id<User>>().ok());
        let Some(user) = user else {
            return Err(AuthFailure::new(403, "forbidden", "Only a signed-in user can change their password here"));
        };
        let session = identity.session_id.as_deref().and_then(|s| s.parse::<Id<Session>>().ok());
        password::check_password_policy(&new_password)?;
        // Always runs the hash check (against a dummy for users without a password), like sign-in.
        let stored = store::password_hash_of(app, user).await?;
        if !password::verify(current, stored).await {
            return Err(AuthFailure::new(400, "invalid_current_password", "The current password is not correct"));
        }
        let hash = password::hash(new_password).await?;
        store::set_password_keeping(app, user, hash, session).await?;
        Ok(())
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
        if !self.current().signups {
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

    pub async fn verify_magic_link(
        &self,
        app: &App,
        proof: CodeProof,
        client: ClientInfo,
    ) -> AuthResult<SignInOutcome> {
        if !self.config.magic_link {
            return Err(AuthFailure::not_found("Magic link sign-in"));
        }
        let row = self.consume(app, Purpose::SignIn, proof).await?;
        let user = store::passwordless_user(app, row.email, self.current().signups).await??;
        self.finish_sign_in(app, Principal::User(user), client, Some(SignInMethod::MagicLink)).await
    }

    /// Finish an OAuth sign-in for a provider profile.
    pub(crate) async fn oauth_sign_in(
        &self,
        app: &App,
        provider: &str,
        profile: store::ExternalProfile,
        client: ClientInfo,
    ) -> AuthResult<SignInOutcome> {
        let user = store::oauth_user(app, provider.to_string(), profile, self.current().signups).await??;
        self.finish_sign_in(app, Principal::User(user), client, Some(SignInMethod::OAuth(provider.to_string()))).await
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Admins

    /// Administrator sign-in: a session, or a second-factor token when the admin has two-factor authentication.
    pub async fn admin_sign_in(
        &self,
        app: &App,
        email: &str,
        password: String,
        client: ClientInfo,
    ) -> AuthResult<SignInOutcome> {
        let email = store::normalize_email(email).map_err(|_| AuthFailure::invalid_credentials())?;
        self.limit(Limit::Account, &email)?;
        let admin = store::admin_by_email(app, email).await?;
        let stored = admin.as_ref().map(|a| a.password_hash.clone());
        if !password::verify(password, stored).await {
            return Err(AuthFailure::invalid_credentials());
        }
        let admin = admin.expect("verified against a real hash");
        self.finish_sign_in(app, Principal::Admin(admin.id), client, None).await
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

    /// Remove an admin's second factor (CLI `admin disable-2fa`): for an administrator who lost their
    /// authenticator and recovery codes. `false` if there is no such admin.
    pub async fn disable_admin_mfa(&self, app: &App, email: &str) -> crate::Result<bool> {
        let email = store::normalize_email(email)?;
        let Some(admin) = store::admin_by_email(app, email).await? else { return Ok(false) };
        super::mfa::remove_factor(app, Principal::Admin(admin.id)).await?;
        Ok(true)
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
        self.start_session(app, Principal::Admin(admin), client, None).await
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
