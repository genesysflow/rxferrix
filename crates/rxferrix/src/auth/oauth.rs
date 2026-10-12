//! OAuth 2.0 authorization code flow with PKCE (S256) and `state`, for GitHub, Google, Discord, Apple and generic
//! OIDC providers.
//!
//! 1. `GET /api/auth/oauth/<provider>?redirect_to=<app url>`: the app URL is checked against the allowlist; a random
//!    `state` and PKCE verifier (and, for Apple, a `nonce`) are kept in memory (10 minutes, one use), `state` is also
//!    set as an HttpOnly cookie, and the browser is redirected to the provider.
//! 2. `GET /api/auth/oauth/<provider>/callback?code&state` (or, for Apple, a `POST` of the same fields as a form:
//!    `response_mode=form_post`): `state` must match a pending flow for this provider AND the cookie (so a callback
//!    URL from someone else's flow can't sign the victim into the attacker's account). The code is exchanged with the
//!    verifier, the profile is fetched (OIDC `userinfo`, GitHub's user and emails APIs, Discord's `/users/@me`, or
//!    Apple's ID token), the user is found or created, and the browser is redirected to the app URL with the tokens in
//!    the fragment: `#rxferrix_access_token=…&rxferrix_refresh_token=…&rxferrix_expires_in=…`. Fragments are never sent to
//!    servers, so the tokens stay out of access logs and `Referer` headers.
//!
//! For GitHub, Google, Discord and OIDC, ID tokens are not used: the profile comes from the userinfo endpoint over TLS
//! with the access token, which avoids JWKS handling while relying on the same trust (the provider's TLS endpoint).
//! Apple has no userinfo endpoint, so its ID token is verified against Apple's JWKS (`super::apple`).
//!
//! **The state cookie and `form_post`.** The cookie is `SameSite=Lax` for providers that redirect back with a `GET`.
//! Apple posts the callback from `appleid.apple.com`, a cross-site `POST`, and browsers don't send `Lax` cookies with
//! those, so for Apple the cookie is `SameSite=None; Secure` (only when the callback URL is https, which Apple requires;
//! a plain-http callback keeps `Lax` and the browser leaves the cookie out). This doesn't weaken the check: the cookie is
//! not a credential but the flow's binding, and the callback requires the posted `state` to equal it (constant-time)
//! and to name a pending flow of this provider, used once. A third-party page can make the browser post to the
//! callback with the cookie attached, but it doesn't know the cookie's value (HttpOnly, random, only in this browser),
//! so it can at most spend the victim's flow (they start again), never complete a sign-in with its own code. The
//! cookie is limited to `Path=/api/auth/oauth`, lives 10 minutes and is cleared by the callback.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;

use super::config::{OAuthProvider, OidcEndpoints, ProviderKind};
use super::service::{AuthFailure, AuthResult};
use super::store::ExternalProfile;
use super::tokens::{pkce_challenge, random_token};

/// Name of the cookie binding a flow to the browser that started it.
pub const STATE_COOKIE: &str = "rxferrix_oauth_state";
/// How long a started flow stays valid.
pub const STATE_TTL: Duration = Duration::from_secs(600);
const MAX_PENDING: usize = 10_000;

pub(crate) struct Pending {
    pub provider: String,
    pub verifier: String,
    pub redirect_to: String,
    /// Sent with the authorization request and required in the ID token (Apple).
    pub nonce: Option<String>,
    created: Instant,
}

/// What the callback received besides `code` and `state`.
#[derive(Debug, Default)]
pub(crate) struct CallbackExtras {
    /// Apple's `user` form field (JSON with the name), posted on the first sign-in only.
    pub apple_user: Option<String>,
}

/// Pending flows and the HTTP client. The providers themselves come from the configuration in effect
/// (`AuthService::oauth_provider`), which administrators can change at runtime.
pub(crate) struct OAuthFlows {
    pending: Mutex<HashMap<String, Pending>>,
    /// Discovered endpoints by discovery URL.
    discovered: tokio::sync::Mutex<HashMap<String, OidcEndpoints>>,
    http: reqwest::Client,
}

/// Where a flow goes: the authorization and token endpoints, and how the profile is read.
enum Endpoints {
    GitHub,
    Oidc(OidcEndpoints),
    Discord { base: String },
    Apple(std::sync::Arc<super::apple::AppleConfig>),
}

impl Endpoints {
    fn authorization(&self) -> String {
        match self {
            Endpoints::GitHub => "https://github.com/login/oauth/authorize".into(),
            Endpoints::Oidc(e) => e.authorization_endpoint.clone(),
            Endpoints::Discord { base } => format!("{base}/oauth2/authorize"),
            Endpoints::Apple(a) => a.authorization_endpoint(),
        }
    }

    fn token(&self) -> String {
        match self {
            Endpoints::GitHub => "https://github.com/login/oauth/access_token".into(),
            Endpoints::Oidc(e) => e.token_endpoint.clone(),
            Endpoints::Discord { base } => format!("{base}/api/oauth2/token"),
            Endpoints::Apple(a) => a.token_endpoint(),
        }
    }
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl OAuthFlows {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("rxferrix/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("building the HTTP client");
        OAuthFlows { pending: Mutex::new(HashMap::new()), discovered: Default::default(), http }
    }

    async fn endpoints(&self, provider: &OAuthProvider) -> AuthResult<Endpoints> {
        match &provider.kind {
            ProviderKind::GitHub => Ok(Endpoints::GitHub),
            ProviderKind::Oidc(e) => Ok(Endpoints::Oidc(e.clone())),
            ProviderKind::Discord { base } => Ok(Endpoints::Discord { base: base.trim_end_matches('/').to_string() }),
            ProviderKind::Apple(a) => Ok(Endpoints::Apple(a.clone())),
            ProviderKind::OidcDiscovery { discovery_url } => {
                let mut cache = self.discovered.lock().await;
                if let Some(e) = cache.get(discovery_url) {
                    return Ok(Endpoints::Oidc(e.clone()));
                }
                let endpoints: OidcEndpoints = self
                    .http
                    .get(discovery_url)
                    .send()
                    .await
                    .and_then(|r| r.error_for_status())
                    .map_err(|e| provider_error(&provider.id, "discovery request failed", e))?
                    .json()
                    .await
                    .map_err(|e| provider_error(&provider.id, "invalid discovery document", e))?;
                cache.insert(discovery_url.clone(), endpoints.clone());
                Ok(Endpoints::Oidc(endpoints))
            }
        }
    }

    /// Start a flow; returns the provider's authorization URL and the `state`.
    pub async fn start(
        &self,
        provider: &OAuthProvider,
        redirect_to: String,
        callback_url: &str,
    ) -> AuthResult<(String, String)> {
        let endpoints = self.endpoints(provider).await?;
        let mut url = url::Url::parse(&endpoints.authorization())
            .map_err(|e| provider_error(&provider.id, "invalid authorization endpoint", e))?;
        let state = random_token();
        let verifier = random_token();
        let nonce = matches!(endpoints, Endpoints::Apple(_)).then(random_token);
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("response_type", "code")
                .append_pair("client_id", &provider.client_id)
                .append_pair("redirect_uri", callback_url)
                .append_pair("scope", &provider.scopes.join(" "))
                .append_pair("state", &state)
                .append_pair("code_challenge", &pkce_challenge(&verifier))
                .append_pair("code_challenge_method", "S256");
            if let Some(nonce) = &nonce {
                // Apple requires form_post when the name or email is requested.
                q.append_pair("response_mode", "form_post").append_pair("nonce", nonce);
            }
        }
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if pending.len() >= MAX_PENDING {
            pending.retain(|_, p| p.created.elapsed() < STATE_TTL);
            if pending.len() >= MAX_PENDING {
                return Err(AuthFailure::new(503, "busy", "Too many sign-ins in progress; try again later"));
            }
        }
        pending.insert(
            state.clone(),
            Pending { provider: provider.id.clone(), verifier, redirect_to, nonce, created: Instant::now() },
        );
        Ok((url.to_string(), state))
    }

    /// Remove and return the pending flow for `state` (each state is usable once).
    pub fn take(&self, state: &str) -> Option<Pending> {
        let p = self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(state)?;
        (p.created.elapsed() < STATE_TTL).then_some(p)
    }

    /// Exchange the code and fetch the user's profile.
    pub async fn finish(
        &self,
        provider: &OAuthProvider,
        code: &str,
        pending: &Pending,
        callback_url: &str,
        extras: &CallbackExtras,
    ) -> AuthResult<ExternalProfile> {
        let endpoints = self.endpoints(provider).await?;
        let client_secret = match &endpoints {
            Endpoints::Apple(apple) => apple
                .client_secret(&provider.client_id, &provider.client_secret, now_s())
                .map_err(|e| provider_error(&provider.id, "can't make the client secret", e))?,
            _ => provider.client_secret.clone(),
        };
        let response: Value = self
            .http
            .post(endpoints.token())
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", callback_url),
                ("client_id", provider.client_id.as_str()),
                ("client_secret", client_secret.as_str()),
                ("code_verifier", pending.verifier.as_str()),
            ])
            .send()
            .await
            .map_err(|e| provider_error(&provider.id, "token request failed", e))?
            .json()
            .await
            .map_err(|e| provider_error(&provider.id, "invalid token response", e))?;
        let rejected = |what: &str| {
            let error = response.get("error").and_then(Value::as_str).unwrap_or(what);
            tracing::warn!("auth: OAuth provider `{}` rejected the code: {error}", provider.id);
            AuthFailure::new(400, "oauth_failed", "The sign-in provider rejected the request")
        };
        if let Endpoints::Apple(apple) = &endpoints {
            let Some(id_token) = response.get("id_token").and_then(Value::as_str) else {
                return Err(rejected("no id_token"));
            };
            let nonce = pending.nonce.as_deref().unwrap_or_default();
            let (claims, _) = apple
                .verify_id_token(&provider.client_id, id_token, nonce, now_s())
                .await
                .map_err(|e| provider_error(&provider.id, "ID token refused", e))?;
            return Ok(ExternalProfile {
                account_id: claims.subject,
                email: claims.email,
                email_verified: claims.email_verified,
                name: super::apple::name_from_user_field(extras.apple_user.as_deref()),
                image: None,
            });
        }
        let Some(access_token) = response.get("access_token").and_then(Value::as_str) else {
            return Err(rejected("no access_token"));
        };
        match endpoints {
            Endpoints::Oidc(e) => self.oidc_profile(provider, &e.userinfo_endpoint, access_token).await,
            Endpoints::GitHub => self.github_profile(provider, access_token).await,
            Endpoints::Discord { base } => self.discord_profile(provider, &base, access_token).await,
            Endpoints::Apple(_) => unreachable!("handled above"),
        }
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        provider: &OAuthProvider,
        url: &str,
        token: &str,
    ) -> AuthResult<T> {
        self.http
            .get(url)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| provider_error(&provider.id, "profile request failed", e))?
            .json()
            .await
            .map_err(|e| provider_error(&provider.id, "invalid profile response", e))
    }

    async fn oidc_profile(&self, provider: &OAuthProvider, userinfo: &str, token: &str) -> AuthResult<ExternalProfile> {
        #[derive(Deserialize)]
        struct UserInfo {
            sub: String,
            email: Option<String>,
            email_verified: Option<Value>,
            name: Option<String>,
            picture: Option<String>,
        }
        let info: UserInfo = self.get_json(provider, userinfo, token).await?;
        if info.sub.is_empty() {
            return Err(AuthFailure::new(400, "oauth_failed", "The sign-in provider returned no user id"));
        }
        // Some providers send `"true"` as a string.
        let email_verified = match info.email_verified {
            Some(Value::Bool(b)) => b,
            Some(Value::String(s)) => s == "true",
            _ => false,
        };
        Ok(ExternalProfile {
            account_id: info.sub,
            email: info.email,
            email_verified,
            name: info.name,
            image: info.picture,
        })
    }

    async fn github_profile(&self, provider: &OAuthProvider, token: &str) -> AuthResult<ExternalProfile> {
        #[derive(Deserialize)]
        struct GhUser {
            id: u64,
            login: String,
            name: Option<String>,
            avatar_url: Option<String>,
        }
        #[derive(Deserialize)]
        struct GhEmail {
            email: String,
            primary: bool,
            verified: bool,
        }
        let user: GhUser = self.get_json(provider, "https://api.github.com/user", token).await?;
        let emails: Vec<GhEmail> =
            self.get_json(provider, "https://api.github.com/user/emails", token).await.unwrap_or_default();
        let chosen = emails
            .iter()
            .find(|e| e.primary && e.verified)
            .or_else(|| emails.iter().find(|e| e.verified))
            .or_else(|| emails.iter().find(|e| e.primary));
        Ok(ExternalProfile {
            account_id: user.id.to_string(),
            email: chosen.map(|e| e.email.clone()),
            email_verified: chosen.is_some_and(|e| e.verified),
            name: user.name.or(Some(user.login)),
            image: user.avatar_url,
        })
    }

    /// Discord's `/users/@me`. The email is used only if Discord verified it (`verified`); an unverified address is
    /// dropped rather than passed on as unverified, so it can neither claim nor be attached to an account.
    async fn discord_profile(&self, provider: &OAuthProvider, base: &str, token: &str) -> AuthResult<ExternalProfile> {
        #[derive(Deserialize)]
        struct DiscordUser {
            id: String,
            username: String,
            global_name: Option<String>,
            avatar: Option<String>,
            email: Option<String>,
            #[serde(default)]
            verified: bool,
        }
        let user: DiscordUser = self.get_json(provider, &format!("{base}/api/users/@me"), token).await?;
        if user.id.is_empty() || !user.id.chars().all(|c| c.is_ascii_digit()) {
            return Err(AuthFailure::new(400, "oauth_failed", "The sign-in provider returned no user id"));
        }
        // Avatar hashes are hex (`a_` prefixed when animated); anything else is not put into a URL.
        let image = user
            .avatar
            .filter(|a| !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
            .map(|a| format!("https://cdn.discordapp.com/avatars/{}/{a}.png", user.id));
        let email = user.email.filter(|_| user.verified);
        Ok(ExternalProfile {
            email_verified: email.is_some(),
            email,
            name: user.global_name.filter(|n| !n.is_empty()).or(Some(user.username)),
            image,
            account_id: user.id,
        })
    }
}

fn provider_error(provider: &str, what: &str, e: impl std::fmt::Display) -> AuthFailure {
    tracing::warn!("auth: OAuth provider `{provider}`: {what}: {e}");
    AuthFailure::new(502, "oauth_failed", "Could not complete sign-in with the provider")
}

/// `<url>#k=v&...` with form encoding.
pub(crate) fn with_fragment(url: &str, pairs: &[(&str, &str)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    format!("{url}#{}", s.finish())
}
