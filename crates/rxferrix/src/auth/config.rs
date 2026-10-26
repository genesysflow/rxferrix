//! Auth configuration: the [`AuthConfig`] builder passed to `RxFerrix::new().auth(...)` (or `AppOptions::auth`), OAuth
//! provider presets, SMTP and the redirect allowlist.
//!
//! ```ignore
//! RxFerrix::new()
//!     .auth(AuthConfig::password().magic_link().oauth(GitHub::from_env()).oauth(Google::from_env()))
//!     .run()
//! ```
//!
//! Anything set here can be extended from the command line or the environment (`serve --public-url`,
//! `--auth-redirect-url`, `--smtp-url`, `--smtp-from`, and `RXFERRIX_*` variables; see `rxferrix serve --help`).
//!
//! **Precedence over `_settings`.** Administrators can also configure OAuth providers, SMTP, the redirect allowlist,
//! sign-ups, token lifetimes and CORS origins at runtime from the dashboard (`crate::settings`). What is set here (in
//! code, on the command line or in the environment) wins over those settings, field by field: a builder call such as
//! `.signups(false)` or `.access_token_ttl(..)`, a non-empty redirect list, any SMTP config or custom mailer, and an
//! OAuth provider with credentials (for its id). Fields left at their defaults are taken from `_settings` when set
//! there. `_system/settings:get` reports which fields are locked this way.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use super::email::Mailer;
use super::service::{SignIn, SignUpAttempt};

/// What [`AuthConfig::on_sign_in`] runs.
pub(crate) type SignInHook =
    Arc<dyn Fn(crate::App, SignIn) -> futures::future::BoxFuture<'static, ()> + Send + Sync + 'static>;

/// What [`AuthConfig::before_sign_up`] runs.
pub(crate) type SignUpHook = Arc<
    dyn Fn(crate::App, SignUpAttempt) -> futures::future::BoxFuture<'static, crate::Result<()>> + Send + Sync + 'static,
>;

/// How users may sign in, plus token lifetimes, email and redirect settings.
///
/// The default (`AuthConfig::default()`) enables no user sign-in method: access tokens are still validated and admin
/// sign-in works, but `/api/auth/signup` and friends answer 404 until a method is enabled.
#[derive(Clone)]
pub struct AuthConfig {
    pub(crate) password: bool,
    pub(crate) magic_link: bool,
    pub(crate) signups: bool,
    pub(crate) require_email_verification: bool,
    pub(crate) oauth: Vec<OAuthProvider>,
    pub(crate) access_token_ttl: Duration,
    pub(crate) session_ttl: Duration,
    pub(crate) refresh_reuse_grace: Duration,
    pub(crate) public_url: Option<String>,
    pub(crate) redirect_urls: Vec<String>,
    pub(crate) smtp: Option<SmtpConfig>,
    pub(crate) mailer: Option<Arc<dyn Mailer>>,
    /// Replaces the built-in [`super::DevLogMailer`] in `--dev` when no other mailer is configured.
    pub(crate) dev_mailer: Option<Arc<dyn Mailer>>,
    pub(crate) rate_limits: Option<RateLimits>,
    pub(crate) trust_proxy: bool,
    /// External JWT issuers whose tokens are accepted (`super::jwt`).
    pub(crate) jwt: Vec<super::jwt::JwtProvider>,
    /// How long `_audit_log` entries are kept.
    pub(crate) audit_retention: Duration,
    /// Whether admins may mint impersonation tokens (`POST /api/admin/impersonate`).
    pub(crate) impersonation: bool,
    /// Runs after each successful user sign-in ([`AuthConfig::on_sign_in`]).
    pub(crate) on_sign_in: Option<SignInHook>,
    /// Decides whether a sign-up may create an account ([`AuthConfig::before_sign_up`]).
    pub(crate) before_sign_up: Option<SignUpHook>,
    /// Addresses ([`AuthConfig::allowed_emails`]) and domains ([`AuthConfig::allowed_domains`]) that may sign up;
    /// both empty: any. Normalized (lowercase; domains without `@`).
    pub(crate) allowed_emails: Vec<String>,
    pub(crate) allowed_domains: Vec<String>,
    /// The name authenticator apps show for this server's TOTP second factors ([`AuthConfig::totp_issuer`]).
    pub(crate) totp_issuer: Option<String>,
    /// Which defaults were set explicitly in code (they then win over `_settings`).
    pub(crate) explicit: Explicit,
}

/// Fields of [`AuthConfig`] whose builder method was called (their values take precedence over `_settings`).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Explicit {
    pub signups: bool,
    pub access_token_ttl: bool,
    pub session_ttl: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig {
            password: false,
            magic_link: false,
            signups: true,
            require_email_verification: false,
            oauth: Vec::new(),
            access_token_ttl: Duration::from_secs(15 * 60),
            session_ttl: Duration::from_secs(30 * 24 * 3600),
            refresh_reuse_grace: Duration::from_secs(10),
            public_url: None,
            redirect_urls: Vec::new(),
            smtp: None,
            mailer: None,
            dev_mailer: None,
            rate_limits: Some(RateLimits::default()),
            trust_proxy: false,
            jwt: Vec::new(),
            audit_retention: Duration::from_secs(90 * 24 * 3600),
            impersonation: true,
            on_sign_in: None,
            before_sign_up: None,
            allowed_emails: Vec::new(),
            allowed_domains: Vec::new(),
            totp_issuer: None,
            explicit: Explicit::default(),
        }
    }
}

impl fmt::Debug for AuthConfig {
    // Hand-written so that secrets (OAuth client secrets, SMTP credentials) never end up in logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthConfig")
            .field("password", &self.password)
            .field("magic_link", &self.magic_link)
            .field("signups", &self.signups)
            .field("require_email_verification", &self.require_email_verification)
            .field("oauth", &self.oauth.iter().map(|p| p.id.as_str()).collect::<Vec<_>>())
            .field("access_token_ttl", &self.access_token_ttl)
            .field("session_ttl", &self.session_ttl)
            .field("public_url", &self.public_url)
            .field("redirect_urls", &self.redirect_urls)
            .field("smtp", &self.smtp.as_ref().map(|_| "<configured>"))
            .field("mailer", &self.mailer.as_ref().map(|_| "<custom>"))
            .field("dev_mailer", &self.dev_mailer.as_ref().map(|_| "<custom>"))
            .field("rate_limits", &self.rate_limits)
            .field("trust_proxy", &self.trust_proxy)
            .field("jwt", &self.jwt)
            .field("audit_retention", &self.audit_retention)
            .field("impersonation", &self.impersonation)
            .field("on_sign_in", &self.on_sign_in.as_ref().map(|_| "<hook>"))
            .field("before_sign_up", &self.before_sign_up.as_ref().map(|_| "<hook>"))
            .field("allowed_emails", &self.allowed_emails)
            .field("allowed_domains", &self.allowed_domains)
            .field("totp_issuer", &self.totp_issuer)
            .finish()
    }
}

impl AuthConfig {
    /// Email + password sign-up and sign-in (with email verification and password reset).
    pub fn password() -> Self {
        AuthConfig { password: true, ..Default::default() }
    }

    /// Passwordless sign-in only: an email with a magic link and a one-time code.
    pub fn passwordless() -> Self {
        AuthConfig { magic_link: true, ..Default::default() }
    }

    /// Also allow sign-in with a magic link / one-time code sent by email.
    pub fn magic_link(mut self) -> Self {
        self.magic_link = true;
        self
    }

    /// Add an OAuth provider (see [`GitHub`], [`Google`], [`Oidc`]). Providers without credentials (e.g.
    /// `GitHub::from_env()` with the variables unset) are skipped with a warning at startup.
    pub fn oauth(mut self, provider: OAuthProvider) -> Self {
        self.oauth.push(provider);
        self
    }

    /// Whether new users may sign up (password sign-up, or first sign-in by magic link or OAuth). Default: true.
    pub fn signups(mut self, allowed: bool) -> Self {
        self.signups = allowed;
        self.explicit.signups = true;
        self
    }

    /// Refuse password sign-in until the email address is verified. Default: false.
    pub fn require_email_verification(mut self, required: bool) -> Self {
        self.require_email_verification = required;
        self
    }

    /// Lifetime of access tokens. Default: 15 minutes.
    pub fn access_token_ttl(mut self, ttl: Duration) -> Self {
        self.access_token_ttl = ttl;
        self.explicit.access_token_ttl = true;
        self
    }

    /// Sessions end after this long without a refresh. Default: 30 days.
    pub fn session_ttl(mut self, ttl: Duration) -> Self {
        self.session_ttl = ttl;
        self.explicit.session_ttl = true;
        self
    }

    /// Accept access tokens issued by an external identity provider (Clerk, Auth0, Supabase, any OIDC issuer),
    /// verified against its JWKS. Repeatable. See [`super::JwtProvider`].
    pub fn jwt(mut self, provider: super::jwt::JwtProvider) -> Self {
        self.jwt.push(provider);
        self
    }

    /// How long dashboard audit log entries (`_audit_log`) are kept. Default: 90 days.
    pub fn audit_retention(mut self, retention: Duration) -> Self {
        self.audit_retention = retention;
        self
    }

    /// Whether administrators may mint short-lived impersonation tokens to open the app as a user
    /// (`POST /api/admin/impersonate`). Default: true. The dashboard's "run as user" works either way.
    pub fn impersonation(mut self, allowed: bool) -> Self {
        self.impersonation = allowed;
        self
    }

    /// For this long after a refresh token was rotated, presenting it again is answered with
    /// `stale_refresh_token` (the client lost a race with another tab) instead of revoking the session. Default: 10s.
    pub fn refresh_reuse_grace(mut self, grace: Duration) -> Self {
        self.refresh_reuse_grace = grace;
        self
    }

    /// The public base URL of this server (e.g. `https://api.example.com`), used for OAuth callback URLs.
    /// Default: `http://<listen address>`.
    pub fn public_url(mut self, url: impl Into<String>) -> Self {
        self.public_url = Some(url.into());
        self
    }

    /// Allow redirects (after OAuth, and links in emails) to this app URL. An origin (`https://app.example.com`)
    /// allows every path on it; a URL with a path allows that path and everything below it. The first entry is the
    /// default target. In `--dev`, any `http://localhost` / `127.0.0.1` URL is allowed as well.
    pub fn redirect_url(mut self, url: impl Into<String>) -> Self {
        self.redirect_urls.push(url.into());
        self
    }

    /// Send email through this SMTP server.
    pub fn smtp(mut self, smtp: SmtpConfig) -> Self {
        self.smtp = Some(smtp);
        self
    }

    /// Send email through a custom [`Mailer`] (e.g. [`super::MemoryMailer`] in tests). Takes precedence over SMTP.
    pub fn mailer(mut self, mailer: Arc<dyn Mailer>) -> Self {
        self.mailer = Some(mailer);
        self
    }

    /// The mailer used in `--dev` when no email is configured (no [`AuthConfig::mailer`], no SMTP in code, on the
    /// command line or in the dashboard settings), in place of the built-in [`super::DevLogMailer`], which prints
    /// every email to the log. Outside `--dev` it is never used. To keep the log lines, forward to
    /// `DevLogMailer` from your mailer:
    ///
    /// ```ignore
    /// impl Mailer for DevInbox {
    ///     fn send(&self, email: Email) -> BoxFuture<'_, rxferrix::Result<()>> {
    ///         self.emails.lock().unwrap().push(email.clone());   // e.g. for an e2e test to read codes
    ///         DevLogMailer.send(email)
    ///     }
    /// }
    /// AuthConfig::password().dev_mailer(Arc::new(DevInbox::default()))
    /// ```
    pub fn dev_mailer(mut self, mailer: Arc<dyn Mailer>) -> Self {
        self.dev_mailer = Some(mailer);
        self
    }

    /// Run `hook` after each successful user sign-in that starts a session: password sign-in, a sign-up that signs
    /// in, a password reset, a magic link or code, and OAuth. Not for `AuthService::sign_in_as`, impersonation,
    /// admins, refreshes, or external JWTs (which start no session). The user is signed in when it runs: it can't
    /// refuse the sign-in, and it handles its own errors. The response waits for it, so keep it short (record the
    /// sign-in through a mutation; schedule anything slow):
    ///
    /// ```ignore
    /// AuthConfig::password().on_sign_in(|app, sign_in| async move {
    ///     if let Err(e) = app.mutation::<audit::record_sign_in>((sign_in.user, sign_in.method.to_string())).await {
    ///         tracing::warn!("recording a sign-in failed: {e:#}");
    ///     }
    /// })
    /// ```
    pub fn on_sign_in<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn(crate::App, SignIn) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.on_sign_in = Some(Arc::new(move |app, sign_in| Box::pin(hook(app, sign_in))));
        self
    }

    /// Decide whether a sign-up may create an account: password sign-up, and the first sign-in by magic link or code
    /// or by OAuth. `hook` runs before the account is created (after [`AuthConfig::allowed_emails`] /
    /// [`AuthConfig::allowed_domains`], when [`AuthConfig::signups`] allows sign-ups at all); an error refuses the
    /// sign-up and nothing is created. A [`crate::UserError`]'s code and message reach the client (status 403; for
    /// OAuth in the `rxferrix_error` of the redirect), so it can show "invite required"; any other error is a server
    /// error. Sign-ins to existing accounts don't run it, nor do accounts made by server code (`TestApp::create_user`)
    /// or by an external JWT issuer ([`AuthConfig::jwt`]), which decides who its users are.
    ///
    /// [`SignUpAttempt::redirect_to`] is the app URL the sign-up returns to, so an invite link can authorize it: the
    /// invite page passes its own URL as `redirectTo` (to `signUp`, `requestMagicLink` or the OAuth start), and the
    /// hook finds the invite token in it.
    ///
    /// ```ignore
    /// AuthConfig::password().magic_link().before_sign_up(|app, attempt| async move {
    ///     let token = attempt.redirect_to.as_deref().and_then(invite_token);
    ///     if !app.query::<invites::valid>((token, attempt.email)).await? {
    ///         return Err(UserError::new("invite_required", "You need an invite to sign up").into());
    ///     }
    ///     Ok(())
    /// })
    /// ```
    ///
    /// Keep it a check: the account can still fail to be created after it allowed the sign-up (the address was taken
    /// meanwhile), so record what the sign-up used (accept the invite) from the app once the user is signed in. For a
    /// magic link it runs when the link or code is used, not when it is requested (the address is proven then, and a
    /// refusal reveals nothing to someone who doesn't own the inbox). The response waits for it.
    pub fn before_sign_up<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn(crate::App, SignUpAttempt) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = crate::Result<()>> + Send + 'static,
    {
        self.before_sign_up = Some(Arc::new(move |app, attempt| Box::pin(hook(app, attempt))));
        self
    }

    /// Only these email addresses may sign up (with [`AuthConfig::allowed_domains`]: these, plus any address at those
    /// domains). Others are refused with `signup_not_allowed`, before [`AuthConfig::before_sign_up`] runs; existing
    /// accounts sign in as before. Repeatable; case doesn't matter.
    ///
    /// An address must be proven to pass: an OAuth sign-up needs a provider-verified one, and while a list is set,
    /// password accounts must verify their address before they can sign in (as with
    /// [`AuthConfig::require_email_verification`]), so typing someone else's address into the sign-up form gets nobody
    /// in. A magic link is only sent to an address without an account if it is allowed.
    pub fn allowed_emails(mut self, emails: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.allowed_emails.extend(emails.into_iter().map(|e| e.into().trim().to_lowercase()));
        self
    }

    /// Only addresses at these domains may sign up: `allowed_domains(["example.com"])` lets `ann@example.com` in, not
    /// `ann@mail.example.com` (list subdomains separately). See [`AuthConfig::allowed_emails`], which works the same
    /// way and adds to it.
    pub fn allowed_domains(mut self, domains: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.allowed_domains
            .extend(domains.into_iter().map(|d| d.into().trim().trim_start_matches('@').to_lowercase()));
        self
    }

    /// Whether [`AuthConfig::allowed_emails`] or [`AuthConfig::allowed_domains`] restrict sign-ups.
    pub(crate) fn has_signup_allowlist(&self) -> bool {
        !self.allowed_emails.is_empty() || !self.allowed_domains.is_empty()
    }

    /// Whether the allowlists let `email` (normalized) sign up. Always true without lists.
    pub(crate) fn allowlist_admits(&self, email: Option<&str>) -> bool {
        if !self.has_signup_allowlist() {
            return true;
        }
        let Some(email) = email else { return false };
        let domain = email.rsplit_once('@').map(|(_, d)| d).unwrap_or_default();
        self.allowed_emails.iter().any(|e| e == email) || self.allowed_domains.iter().any(|d| d == domain)
    }

    /// Whether password accounts must verify their address before signing in.
    pub(crate) fn verification_required(&self) -> bool {
        self.require_email_verification || self.has_signup_allowlist()
    }

    /// Rate limits for the auth endpoints; `None` disables rate limiting.
    pub fn rate_limits(mut self, limits: Option<RateLimits>) -> Self {
        self.rate_limits = limits;
        self
    }

    /// Take the client IP from the last `X-Forwarded-For` entry (set this only behind a reverse proxy). Without it,
    /// the `trustedProxy` setting (dashboard: Settings → IP proxy headers) decides which headers are trusted.
    pub fn trust_proxy(mut self, trust: bool) -> Self {
        self.trust_proxy = trust;
        self
    }

    /// The name shown in authenticator apps next to accounts enrolled in two-factor authentication here (the
    /// `issuer` of the `otpauth://` URI). Default: the host of the public URL, else `RxFerrix`.
    pub fn totp_issuer(mut self, name: impl Into<String>) -> Self {
        self.totp_issuer = Some(name.into());
        self
    }
}

/// SMTP settings.
#[derive(Clone)]
pub struct SmtpConfig {
    /// `smtp://user:pass@host:587` (STARTTLS), `smtps://user:pass@host:465` (TLS), or `smtp://localhost:1025` for a
    /// local catcher such as Mailpit. See lettre's `SmtpTransport::from_url`.
    pub url: String,
    /// The `From` address, e.g. `My App <noreply@example.com>`.
    pub from: String,
}

impl SmtpConfig {
    pub fn new(url: impl Into<String>, from: impl Into<String>) -> Self {
        SmtpConfig { url: url.into(), from: from.into() }
    }

    /// `RXFERRIX_SMTP_URL` and `RXFERRIX_SMTP_FROM`; `None` unless the URL is set.
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("RXFERRIX_SMTP_URL").ok().filter(|s| !s.is_empty())?;
        let from = std::env::var("RXFERRIX_SMTP_FROM").unwrap_or_else(|_| "rxferrix <noreply@localhost>".into());
        Some(SmtpConfig { url, from })
    }
}

/// Token bucket sizes for the auth endpoints (per client IP and endpoint group).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimits {
    /// Sign-in, sign-up, code verification and other credential checks: burst size.
    pub credential_burst: u32,
    /// ... refilled at this many per minute.
    pub credential_per_minute: u32,
    /// Endpoints that send email: burst per IP (and, separately, per email address).
    pub email_burst: u32,
    pub email_per_minute: u32,
    /// Token refresh.
    pub refresh_burst: u32,
    pub refresh_per_minute: u32,
}

impl Default for RateLimits {
    fn default() -> Self {
        RateLimits {
            credential_burst: 10,
            credential_per_minute: 10,
            email_burst: 5,
            email_per_minute: 2,
            refresh_burst: 30,
            refresh_per_minute: 60,
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// OAuth providers
// ---------------------------------------------------------------------------------------------------------------------

/// An OAuth 2.0 / OpenID Connect provider for the authorization code flow with PKCE. Build one with [`GitHub`],
/// [`Google`] or [`Oidc`].
#[derive(Clone)]
pub struct OAuthProvider {
    /// The id used in URLs (`/api/auth/oauth/<id>`) and stored in `_accounts.provider`.
    pub(crate) id: String,
    pub(crate) kind: ProviderKind,
    pub(crate) client_id: String,
    pub(crate) client_secret: String,
    pub(crate) scopes: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) enum ProviderKind {
    GitHub,
    /// Endpoints known up front (Google).
    Oidc(OidcEndpoints),
    /// Endpoints from `<issuer>/.well-known/openid-configuration`, fetched on first use.
    OidcDiscovery {
        discovery_url: String,
    },
    /// Discord: OAuth 2 with the `/users/@me` profile. `base` is `https://discord.com` (or a mock).
    Discord {
        base: String,
    },
    /// Sign in with Apple: a signed client secret, `form_post` callbacks, the ID token as the profile.
    Apple(Arc<super::apple::AppleConfig>),
}

impl ProviderKind {
    /// Whether the provider posts its callback (`response_mode=form_post`) instead of redirecting with a query.
    pub(crate) fn form_post(&self) -> bool {
        matches!(self, ProviderKind::Apple(_))
    }
}

#[derive(Clone, Debug, serde::Deserialize)]
pub(crate) struct OidcEndpoints {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub userinfo_endpoint: String,
}

impl OAuthProvider {
    /// Use a different id (e.g. two GitHub apps).
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// Replace the requested scopes.
    pub fn scopes(mut self, scopes: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.scopes = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// Point a Discord or Apple preset at another origin than `https://discord.com` / `https://appleid.apple.com`
    /// (a mock server in tests, or a proxy). For Apple this also becomes the expected issuer of ID tokens and the
    /// audience of the client secret. Other providers are unchanged (use [`Oidc`] for a custom issuer).
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        let url = url.into().trim_end_matches('/').to_string();
        self.kind = match self.kind {
            ProviderKind::Discord { .. } => ProviderKind::Discord { base: url },
            ProviderKind::Apple(a) => {
                ProviderKind::Apple(Arc::new(super::apple::AppleConfig::new(a.team_id.clone(), a.key_id.clone(), url)))
            }
            other => other,
        };
        self
    }

    pub(crate) fn is_configured(&self) -> bool {
        let apple_ids = match &self.kind {
            ProviderKind::Apple(a) => !a.team_id.is_empty() && !a.key_id.is_empty(),
            _ => true,
        };
        !self.client_id.is_empty() && !self.client_secret.is_empty() && apple_ids
    }

    /// Problems that make a configured provider unusable (an Apple private key that doesn't parse), for the
    /// startup log.
    pub(crate) fn problem(&self) -> Option<String> {
        match &self.kind {
            ProviderKind::Apple(_) if self.is_configured() => {
                super::apple::parse_private_key(&self.client_secret).err()
            }
            _ => None,
        }
    }
}

impl fmt::Debug for OAuthProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthProvider")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("scopes", &self.scopes)
            .finish()
    }
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

/// GitHub OAuth app preset (`/api/auth/oauth/github`). Requests `read:user user:email`; the user's primary
/// verified email is used.
pub struct GitHub;

// Presets are unit structs whose `new` builds the provider (reads like `GitHub::new(id, secret)`).
#[allow(clippy::new_ret_no_self)]
impl GitHub {
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> OAuthProvider {
        OAuthProvider {
            id: "github".into(),
            kind: ProviderKind::GitHub,
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            scopes: vec!["read:user".into(), "user:email".into()],
        }
    }

    /// `RXFERRIX_AUTH_GITHUB_CLIENT_ID` / `RXFERRIX_AUTH_GITHUB_CLIENT_SECRET`.
    pub fn from_env() -> OAuthProvider {
        Self::new(env("RXFERRIX_AUTH_GITHUB_CLIENT_ID"), env("RXFERRIX_AUTH_GITHUB_CLIENT_SECRET"))
    }
}

/// Google preset (`/api/auth/oauth/google`), OpenID Connect with `openid email profile`.
pub struct Google;

#[allow(clippy::new_ret_no_self)]
impl Google {
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> OAuthProvider {
        OAuthProvider {
            id: "google".into(),
            kind: ProviderKind::Oidc(OidcEndpoints {
                authorization_endpoint: "https://accounts.google.com/o/oauth2/v2/auth".into(),
                token_endpoint: "https://oauth2.googleapis.com/token".into(),
                userinfo_endpoint: "https://openidconnect.googleapis.com/v1/userinfo".into(),
            }),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            scopes: vec!["openid".into(), "email".into(), "profile".into()],
        }
    }

    /// `RXFERRIX_AUTH_GOOGLE_CLIENT_ID` / `RXFERRIX_AUTH_GOOGLE_CLIENT_SECRET`.
    pub fn from_env() -> OAuthProvider {
        Self::new(env("RXFERRIX_AUTH_GOOGLE_CLIENT_ID"), env("RXFERRIX_AUTH_GOOGLE_CLIENT_SECRET"))
    }
}

/// Discord preset (`/api/auth/oauth/discord`): OAuth 2 with `identify email`; the profile is `/users/@me`, and its
/// email is used only if Discord has verified it.
pub struct Discord;

#[allow(clippy::new_ret_no_self)]
impl Discord {
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> OAuthProvider {
        OAuthProvider {
            id: "discord".into(),
            kind: ProviderKind::Discord { base: "https://discord.com".into() },
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            scopes: vec!["identify".into(), "email".into()],
        }
    }

    /// `RXFERRIX_AUTH_DISCORD_CLIENT_ID` / `RXFERRIX_AUTH_DISCORD_CLIENT_SECRET`.
    pub fn from_env() -> OAuthProvider {
        Self::new(env("RXFERRIX_AUTH_DISCORD_CLIENT_ID"), env("RXFERRIX_AUTH_DISCORD_CLIENT_SECRET"))
    }
}

/// Sign in with Apple preset (`/api/auth/oauth/apple`), with `name email`.
///
/// - `client_id`: the Services ID (e.g. `com.example.web`), with this server's callback URL
///   (`<public url>/api/auth/oauth/apple/callback`, https) registered as a return URL;
/// - `team_id`: the Apple developer team id; `key_id`: the id of the Sign in with Apple key;
/// - `private_key`: that key's `.p8` file contents (PEM). It signs a short-lived client secret (ES256 JWT) that is
///   generated and rotated automatically; keep it out of source control (environment, a file, or the dashboard's
///   settings, where it is stored encrypted).
///
/// Apple posts the callback (`response_mode=form_post`), sends the user's name only on the first sign-in, and has no
/// userinfo endpoint: the profile is the ID token, verified against Apple's keys (the `auth::apple` module).
pub struct Apple;

#[allow(clippy::new_ret_no_self)]
impl Apple {
    pub fn new(
        client_id: impl Into<String>,
        team_id: impl Into<String>,
        key_id: impl Into<String>,
        private_key: impl Into<String>,
    ) -> OAuthProvider {
        let apple = super::apple::AppleConfig::new(team_id.into(), key_id.into(), super::apple::APPLE_ORIGIN.into());
        OAuthProvider {
            id: "apple".into(),
            kind: ProviderKind::Apple(Arc::new(apple)),
            client_id: client_id.into(),
            client_secret: private_key.into(),
            scopes: vec!["name".into(), "email".into()],
        }
    }

    /// `RXFERRIX_AUTH_APPLE_CLIENT_ID`, `RXFERRIX_AUTH_APPLE_TEAM_ID`, `RXFERRIX_AUTH_APPLE_KEY_ID`, and the private key
    /// from `RXFERRIX_AUTH_APPLE_PRIVATE_KEY` (the PEM; `\n` escapes allowed) or the file named by
    /// `RXFERRIX_AUTH_APPLE_PRIVATE_KEY_FILE`.
    pub fn from_env() -> OAuthProvider {
        let mut key = env("RXFERRIX_AUTH_APPLE_PRIVATE_KEY");
        let file = env("RXFERRIX_AUTH_APPLE_PRIVATE_KEY_FILE");
        if key.is_empty() && !file.is_empty() {
            key = std::fs::read_to_string(&file).unwrap_or_else(|e| {
                tracing::warn!("auth: can't read RXFERRIX_AUTH_APPLE_PRIVATE_KEY_FILE ({file}): {e}");
                String::new()
            });
        }
        Self::new(
            env("RXFERRIX_AUTH_APPLE_CLIENT_ID"),
            env("RXFERRIX_AUTH_APPLE_TEAM_ID"),
            env("RXFERRIX_AUTH_APPLE_KEY_ID"),
            key,
        )
    }
}

/// A generic OpenID Connect provider, configured from its discovery document.
pub struct Oidc;

#[allow(clippy::new_ret_no_self)]
impl Oidc {
    /// `id` is used in URLs; `issuer` is either the issuer URL or the full discovery URL
    /// (`.../.well-known/openid-configuration`).
    pub fn new(
        id: impl Into<String>,
        issuer: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> OAuthProvider {
        let issuer = issuer.into();
        let discovery_url = if issuer.contains("/.well-known/") {
            issuer
        } else {
            format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'))
        };
        OAuthProvider {
            id: id.into(),
            kind: ProviderKind::OidcDiscovery { discovery_url },
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            scopes: vec!["openid".into(), "email".into(), "profile".into()],
        }
    }

    /// `RXFERRIX_AUTH_<ID>_ISSUER`, `..._CLIENT_ID`, `..._CLIENT_SECRET` (ID uppercased).
    pub fn from_env(id: &str) -> OAuthProvider {
        let prefix = format!("RXFERRIX_AUTH_{}", id.to_uppercase().replace('-', "_"));
        Self::new(
            id,
            env(&format!("{prefix}_ISSUER")),
            env(&format!("{prefix}_CLIENT_ID")),
            env(&format!("{prefix}_CLIENT_SECRET")),
        )
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Redirect allowlist
// ---------------------------------------------------------------------------------------------------------------------

/// Validates app URLs that tokens or email links may be sent to.
#[derive(Debug, Clone)]
pub(crate) struct RedirectAllowlist {
    entries: Vec<url::Url>,
    allow_localhost: bool,
}

impl RedirectAllowlist {
    pub fn new(entries: &[String], allow_localhost: bool) -> crate::Result<Self> {
        let mut parsed = Vec::new();
        for e in entries.iter().map(|e| e.trim()).filter(|e| !e.is_empty()) {
            let url = url::Url::parse(e).map_err(|err| anyhow::anyhow!("invalid redirect URL `{e}`: {err}"))?;
            if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
                anyhow::bail!("redirect URL `{e}` must be an absolute http(s) URL");
            }
            parsed.push(url);
        }
        Ok(RedirectAllowlist { entries: parsed, allow_localhost })
    }

    /// The allowed entries, normalized.
    pub fn entries(&self) -> Vec<String> {
        self.entries.iter().map(|u| u.to_string()).collect()
    }

    /// The default redirect target: the first configured entry.
    pub fn default_target(&self) -> Option<String> {
        self.entries.first().map(|u| u.to_string())
    }

    /// Check `candidate` (or the default when `None`) and return it normalized, without a fragment.
    pub fn resolve(&self, candidate: Option<&str>) -> Option<String> {
        let Some(candidate) = candidate.filter(|c| !c.is_empty()) else { return self.default_target() };
        let url = url::Url::parse(candidate).ok()?;
        if !matches!(url.scheme(), "http" | "https") || !url.username().is_empty() || url.password().is_some() {
            return None;
        }
        let allowed =
            self.entries.iter().any(|e| Self::matches(e, &url)) || (self.allow_localhost && is_localhost(&url));
        if !allowed {
            return None;
        }
        let mut url = url;
        url.set_fragment(None);
        Some(url.to_string())
    }

    fn matches(entry: &url::Url, url: &url::Url) -> bool {
        if entry.origin() != url.origin() {
            return false;
        }
        let prefix = entry.path().trim_end_matches('/');
        if prefix.is_empty() {
            return true;
        }
        let path = url.path();
        path == prefix || path.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/'))
    }
}

fn is_localhost(url: &url::Url) -> bool {
    url.scheme() == "http"
        && match url.host() {
            Some(url::Host::Domain(d)) => d == "localhost",
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The presets against what the real providers publish today (network; `cargo test -p rxferrix --lib live_ --
    /// --ignored`): endpoints, scopes, PKCE, client authentication, signing keys. A full sign-in needs registered apps
    /// and a browser, so this is as close as an unattended test gets.
    #[tokio::test]
    #[ignore = "network: the real providers"]
    async fn live_presets_match_the_providers() {
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let get = async |url: &str| -> serde_json::Value { http.get(url).send().await.unwrap().json().await.unwrap() };
        let strs = |v: &serde_json::Value| -> Vec<String> {
            v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
        };
        let parses_keys = async |url: &str| {
            let body = http.get(url).send().await.unwrap().bytes().await.unwrap();
            assert!(super::super::jwt::usable_keys(&body).unwrap() > 0, "no usable key at {url}");
        };

        // Google: endpoints known up front.
        let google = get("https://accounts.google.com/.well-known/openid-configuration").await;
        let ProviderKind::Oidc(e) = Google::new("id", "secret").kind else { panic!() };
        assert_eq!(e.authorization_endpoint, google["authorization_endpoint"]);
        assert_eq!(e.token_endpoint, google["token_endpoint"]);
        assert_eq!(e.userinfo_endpoint, google["userinfo_endpoint"]);
        assert!(Google::new("id", "secret").scopes.iter().all(|s| strs(&google["scopes_supported"]).contains(s)));
        assert!(strs(&google["code_challenge_methods_supported"]).contains(&"S256".into()));
        assert!(strs(&google["token_endpoint_auth_methods_supported"]).contains(&"client_secret_post".into()));

        // Apple: the client secret is posted; ID tokens are RS256 from `<origin>/auth/keys`; callbacks are posted.
        let apple = get("https://appleid.apple.com/.well-known/openid-configuration").await;
        let config = super::super::apple::AppleConfig::new(
            "team".into(),
            "key".into(),
            super::super::apple::APPLE_ORIGIN.into(),
        );
        assert_eq!(apple["issuer"], super::super::apple::APPLE_ORIGIN);
        assert_eq!(config.authorization_endpoint(), apple["authorization_endpoint"]);
        assert_eq!(config.token_endpoint(), apple["token_endpoint"]);
        assert_eq!(apple["jwks_uri"], format!("{}/auth/keys", super::super::apple::APPLE_ORIGIN));
        assert!(strs(&apple["token_endpoint_auth_methods_supported"]).contains(&"client_secret_post".into()));
        assert!(strs(&apple["id_token_signing_alg_values_supported"]).contains(&"RS256".into()));
        assert!(strs(&apple["response_modes_supported"]).contains(&"form_post".into()));
        assert!(["name", "email"].iter().all(|s| strs(&apple["scopes_supported"]).contains(&s.to_string())));
        parses_keys("https://appleid.apple.com/auth/keys").await;

        // Discord: `/oauth2/authorize` (the page its docs link; discovery names `/api/oauth2/authorize`, which serves the
        // same) and `/api/oauth2/token`.
        let discord = get("https://discord.com/.well-known/openid-configuration").await;
        let ProviderKind::Discord { base } = Discord::new("id", "secret").kind else { panic!() };
        assert_eq!(format!("{base}/api/oauth2/token"), discord["token_endpoint"]);
        let page = http
            .get(format!("{base}/oauth2/authorize?client_id=1&response_type=code&scope=identify"))
            .send()
            .await
            .unwrap();
        assert!(page.status().is_success(), "{}", page.status());
        assert!(strs(&discord["code_challenge_methods_supported"]).contains(&"S256".into()));
        let unauthorized = http.get(format!("{base}/api/users/@me")).send().await.unwrap();
        assert_eq!(unauthorized.status(), 401, "the profile endpoint exists and wants a token");

        // GitHub (no discovery): the authorize page sends a signed-out user to sign in, the token endpoint answers JSON
        // errors with `Accept: application/json`, and the profile API wants a token.
        let authorize = http.get("https://github.com/login/oauth/authorize?client_id=x").send().await.unwrap();
        assert_eq!(authorize.status(), 302);
        let token: serde_json::Value = http
            .post("https://github.com/login/oauth/access_token")
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&[("client_id", "x"), ("client_secret", "y"), ("code", "z")])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(token.get("error").is_some() && token.get("access_token").is_none(), "{token}");
        let user = http
            .get("https://api.github.com/user")
            .header(reqwest::header::USER_AGENT, "rxferrix-test")
            .send()
            .await
            .unwrap();
        assert_eq!(user.status(), 401);
    }

    #[test]
    fn redirect_allowlist() {
        let list = RedirectAllowlist::new(
            &["https://app.example.com".into(), "https://other.example.com/auth/".into()],
            false,
        )
        .unwrap();
        assert_eq!(list.resolve(None).as_deref(), Some("https://app.example.com/"));
        assert_eq!(
            list.resolve(Some("https://app.example.com/x?y=1#frag")).as_deref(),
            Some("https://app.example.com/x?y=1")
        );
        assert!(list.resolve(Some("https://other.example.com/auth/cb")).is_some());
        assert!(list.resolve(Some("https://other.example.com/auth")).is_some());
        // Path prefix must end at a segment boundary; other hosts, schemes, ports and userinfo are rejected.
        assert!(list.resolve(Some("https://other.example.com/authx")).is_none());
        assert!(list.resolve(Some("https://other.example.com/")).is_none());
        assert!(list.resolve(Some("https://app.example.com.evil.com/")).is_none());
        assert!(list.resolve(Some("http://app.example.com/")).is_none());
        assert!(list.resolve(Some("https://app.example.com:444/")).is_none());
        assert!(list.resolve(Some("https://evil@app.example.com/")).is_none());
        assert!(list.resolve(Some("javascript:alert(1)")).is_none());
        assert!(list.resolve(Some("//evil.com")).is_none());
        assert!(list.resolve(Some("http://localhost:5173/")).is_none());
        let dev = RedirectAllowlist::new(&[], true).unwrap();
        assert_eq!(dev.resolve(None), None);
        assert!(dev.resolve(Some("http://localhost:5173/cb")).is_some());
        assert!(dev.resolve(Some("http://127.0.0.1:5173/cb")).is_some());
        assert!(dev.resolve(Some("https://localhost.evil.com/")).is_none());
    }
}
