//! The auth configuration in effect: [`AuthConfig`] (code, command line, environment) merged with the runtime
//! settings administrators keep in `_settings` (`crate::settings`). Rebuilt and swapped atomically whenever the
//! settings change, so changes apply without a restart; requests in flight keep the snapshot they started with.
//!
//! Precedence, field by field: what [`AuthConfig`] sets explicitly wins (see its module docs), then `_settings`, then
//! the defaults.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use super::config::{AuthConfig, OAuthProvider, RedirectAllowlist, SmtpConfig};
use super::email::{LogMailer, Mailer, SmtpMailer};
use crate::settings::model::{OAuthKind, StoredSettings};

/// Bounds for token lifetimes set at runtime.
pub(crate) const ACCESS_TTL_RANGE: (u64, u64) = (30, 24 * 3600);
pub(crate) const SESSION_TTL_RANGE: (u64, u64) = (60, 400 * 24 * 3600);

/// One consistent view of the auth configuration.
pub(crate) struct Effective {
    /// The commit ts of the `_settings` it was built from (a newer build never gets replaced by an older one).
    pub ts: u64,
    pub signups: bool,
    pub access_token_ttl: Duration,
    pub session_ttl: Duration,
    pub redirects: RedirectAllowlist,
    pub mailer: Option<Arc<dyn Mailer>>,
    /// OAuth providers by id: configured in code, plus the enabled ones from `_settings` with other ids.
    pub oauth: BTreeMap<String, OAuthProvider>,
    /// Allowed CORS origins; `None` allows any origin (the default: auth uses bearer tokens, not cookies).
    pub cors_origins: Option<Vec<String>>,
}

impl Effective {
    /// Merge `config` with `stored`. Fails when a stored value can't be used (e.g. an SMTP URL lettre rejects); the
    /// settings functions validate before writing, so that only happens for hand-edited databases.
    pub fn build(
        config: &AuthConfig,
        dev: bool,
        config_mailer: Option<Arc<dyn Mailer>>,
        stored: &StoredSettings,
        ts: u64,
    ) -> crate::Result<Effective> {
        let explicit = config.explicit;
        let signups = if explicit.signups { config.signups } else { stored.signups.unwrap_or(config.signups) };
        let ttl = |explicit: bool, configured: Duration, stored: Option<u64>| match (explicit, stored) {
            (false, Some(secs)) => Duration::from_secs(secs),
            _ => configured,
        };
        let access_token_ttl = ttl(explicit.access_token_ttl, config.access_token_ttl, stored.access_token_ttl_secs);
        let session_ttl = ttl(explicit.session_ttl, config.session_ttl, stored.session_ttl_secs);
        let redirect_urls = match (&stored.redirect_urls, config.redirect_urls.is_empty()) {
            (Some(urls), true) => urls.clone(),
            _ => config.redirect_urls.clone(),
        };
        let redirects = RedirectAllowlist::new(&redirect_urls, dev)?;
        let mailer: Option<Arc<dyn Mailer>> = match (config_mailer, &stored.smtp) {
            (Some(m), _) => Some(m),
            (None, Some(smtp)) => Some(Arc::new(SmtpMailer::new(&smtp.config()?)?)),
            (None, None) if dev => Some(Arc::new(LogMailer)),
            (None, None) => None,
        };
        let mut oauth: BTreeMap<String, OAuthProvider> =
            config.oauth.iter().filter(|p| p.is_configured()).map(|p| (p.id.clone(), p.clone())).collect();
        for p in &stored.oauth {
            if !p.enabled || oauth.contains_key(&p.id) {
                continue;
            }
            let Some(secret) = p.client_secret.clone().filter(|s| !s.is_empty()) else { continue };
            let mut provider = match p.kind {
                OAuthKind::Github => super::GitHub::new(p.client_id.clone(), secret),
                OAuthKind::Google => super::Google::new(p.client_id.clone(), secret),
                OAuthKind::Oidc => {
                    let Some(issuer) = p.issuer.clone() else { continue };
                    super::Oidc::new(p.id.clone(), issuer, p.client_id.clone(), secret)
                }
            }
            .id(p.id.clone());
            if let Some(scopes) = p.scopes.clone().filter(|s| !s.is_empty()) {
                provider = provider.scopes(scopes);
            }
            oauth.insert(p.id.clone(), provider);
        }
        let cors_origins = stored.cors_origins.clone().filter(|o| !o.is_empty() && !o.iter().any(|x| x == "*"));
        Ok(Effective { ts, signups, access_token_ttl, session_ttl, redirects, mailer, oauth, cors_origins })
    }
}

/// The settings fields that `config` fixes (its values win over `_settings`), as reported by `_system/settings:get`:
/// `signups`, `accessTokenTtlSecs`, `sessionTtlSecs`, `redirectUrls`, `smtp`, and `oauth.<id>` per provider.
pub(crate) fn locked_fields(config: &AuthConfig) -> Vec<String> {
    let mut out = Vec::new();
    let e = config.explicit;
    for (set, name) in [(e.signups, "signups"), (e.access_token_ttl, "accessTokenTtlSecs"), (e.session_ttl, "sessionTtlSecs")] {
        if set {
            out.push(name.to_string());
        }
    }
    if !config.redirect_urls.is_empty() {
        out.push("redirectUrls".into());
    }
    if config.mailer.is_some() || config.smtp.is_some() {
        out.push("smtp".into());
    }
    out.extend(config.oauth.iter().filter(|p| p.is_configured()).map(|p| format!("oauth.{}", p.id)));
    out
}

/// The mailer the code configured explicitly (a custom mailer, or SMTP from the builder / `--smtp-url`).
pub(crate) fn config_mailer(config: &AuthConfig) -> crate::Result<Option<Arc<dyn Mailer>>> {
    Ok(match (&config.mailer, &config.smtp) {
        (Some(m), _) => Some(m.clone()),
        (None, Some(smtp)) => Some(Arc::new(SmtpMailer::new(smtp)?)),
        (None, None) => None,
    })
}

/// Build an SMTP mailer for a test email from an SMTP config.
pub(crate) fn smtp_mailer(smtp: &SmtpConfig) -> crate::Result<Arc<dyn Mailer>> {
    Ok(Arc::new(SmtpMailer::new(smtp)?))
}
