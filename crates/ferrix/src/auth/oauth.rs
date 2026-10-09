//! OAuth 2.0 authorization code flow with PKCE (S256) and `state`, for GitHub, Google and generic OIDC providers.
//!
//! 1. `GET /api/auth/oauth/<provider>?redirect_to=<app url>`: the app URL is checked against the allowlist; a random
//!    `state` and PKCE verifier are kept in memory (10 minutes, one use), `state` is also set as an HttpOnly
//!    SameSite=Lax cookie, and the browser is redirected to the provider.
//! 2. `GET /api/auth/oauth/<provider>/callback?code&state`: `state` must match a pending flow for this provider AND the
//!    cookie (so a callback URL from someone else's flow can't sign the victim into the attacker's account). The code
//!    is exchanged with the verifier, the profile is fetched (OIDC `userinfo`, or GitHub's user and emails APIs), the
//!    user is found or created, and the browser is redirected to the app URL with the tokens in the fragment:
//!    `#ferrix_access_token=…&ferrix_refresh_token=…&ferrix_expires_in=…`. Fragments are never sent to servers, so
//!    the tokens stay out of access logs and `Referer` headers.
//!
//! ID tokens are not used: the profile comes from the userinfo endpoint over TLS with the access token, which avoids
//! JWKS handling while relying on the same trust (the provider's TLS endpoint).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use super::config::{OAuthProvider, OidcEndpoints, ProviderKind};
use super::service::{AuthFailure, AuthResult};
use super::store::ExternalProfile;
use super::tokens::{pkce_challenge, random_token};

/// Name of the cookie binding a flow to the browser that started it.
pub const STATE_COOKIE: &str = "ferrix_oauth_state";
/// How long a started flow stays valid.
pub const STATE_TTL: Duration = Duration::from_secs(600);
const MAX_PENDING: usize = 10_000;

pub(crate) struct Pending {
    pub provider: String,
    pub verifier: String,
    pub redirect_to: String,
    created: Instant,
}

/// Pending flows and the HTTP client. The providers themselves come from the configuration in effect
/// (`AuthService::oauth_provider`), which administrators can change at runtime.
pub(crate) struct OAuthFlows {
    pending: Mutex<HashMap<String, Pending>>,
    /// Discovered endpoints by discovery URL.
    discovered: tokio::sync::Mutex<HashMap<String, OidcEndpoints>>,
    http: reqwest::Client,
}

impl OAuthFlows {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("ferrix/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("building the HTTP client");
        OAuthFlows { pending: Mutex::new(HashMap::new()), discovered: Default::default(), http }
    }

    async fn endpoints(&self, provider: &OAuthProvider) -> AuthResult<Option<OidcEndpoints>> {
        match &provider.kind {
            ProviderKind::GitHub => Ok(None),
            ProviderKind::Oidc(e) => Ok(Some(e.clone())),
            ProviderKind::OidcDiscovery { discovery_url } => {
                let mut cache = self.discovered.lock().await;
                if let Some(e) = cache.get(discovery_url) {
                    return Ok(Some(e.clone()));
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
                Ok(Some(endpoints))
            }
        }
    }

    /// Start a flow; returns the provider's authorization URL and the `state`.
    pub async fn start(&self, provider: &OAuthProvider, redirect_to: String, callback_url: &str) -> AuthResult<(String, String)> {
        let authorize = match self.endpoints(provider).await? {
            Some(e) => e.authorization_endpoint,
            None => "https://github.com/login/oauth/authorize".to_string(),
        };
        let mut url = url::Url::parse(&authorize)
            .map_err(|e| provider_error(&provider.id, "invalid authorization endpoint", e))?;
        let state = random_token();
        let verifier = random_token();
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &provider.client_id)
            .append_pair("redirect_uri", callback_url)
            .append_pair("scope", &provider.scopes.join(" "))
            .append_pair("state", &state)
            .append_pair("code_challenge", &pkce_challenge(&verifier))
            .append_pair("code_challenge_method", "S256");
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if pending.len() >= MAX_PENDING {
            pending.retain(|_, p| p.created.elapsed() < STATE_TTL);
            if pending.len() >= MAX_PENDING {
                return Err(AuthFailure::new(503, "busy", "Too many sign-ins in progress; try again later"));
            }
        }
        pending.insert(
            state.clone(),
            Pending { provider: provider.id.clone(), verifier, redirect_to, created: Instant::now() },
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
        verifier: &str,
        callback_url: &str,
    ) -> AuthResult<ExternalProfile> {
        let endpoints = self.endpoints(provider).await?;
        let token_url = match &endpoints {
            Some(e) => e.token_endpoint.as_str(),
            None => "https://github.com/login/oauth/access_token",
        };
        let response: Value = self
            .http
            .post(token_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", callback_url),
                ("client_id", provider.client_id.as_str()),
                ("client_secret", provider.client_secret.as_str()),
                ("code_verifier", verifier),
            ])
            .send()
            .await
            .map_err(|e| provider_error(&provider.id, "token request failed", e))?
            .json()
            .await
            .map_err(|e| provider_error(&provider.id, "invalid token response", e))?;
        let Some(access_token) = response.get("access_token").and_then(Value::as_str) else {
            let error = response.get("error").and_then(Value::as_str).unwrap_or("no access_token");
            tracing::warn!("auth: OAuth provider `{}` rejected the code: {error}", provider.id);
            return Err(AuthFailure::new(400, "oauth_failed", "The sign-in provider rejected the request"));
        };
        match endpoints {
            Some(e) => self.oidc_profile(provider, &e.userinfo_endpoint, access_token).await,
            None => self.github_profile(provider, access_token).await,
        }
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, provider: &OAuthProvider, url: &str, token: &str) -> AuthResult<T> {
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
        Ok(ExternalProfile { account_id: info.sub, email: info.email, email_verified, name: info.name, image: info.picture })
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
