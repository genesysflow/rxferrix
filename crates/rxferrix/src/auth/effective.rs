//! The auth configuration in effect: [`AuthConfig`] (code, command line, environment) merged with the runtime
//! settings administrators keep in `_settings` (`crate::settings`). Rebuilt and swapped atomically whenever the
//! settings change, so changes apply without a restart; requests in flight keep the snapshot they started with.
//!
//! Precedence, field by field: what [`AuthConfig`] sets explicitly wins (see its module docs), then `_settings`, then
//! the defaults.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderName;

use super::config::{AuthConfig, OAuthProvider, RedirectAllowlist, SmtpConfig};
use super::email::{DevLogMailer, Mailer, SmtpMailer};
use crate::settings::model::{EmailTemplates, OAuthKind, StoredSettings};
use crate::settings::rate_limits::RateLimitRules;

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
    /// Where the client IP comes from behind a reverse proxy.
    pub proxy: ProxyHeaders,
    /// Request rate limits (`rateLimits` setting); none when off.
    pub rate_limits: RateLimitRules,
    /// The auth emails' templates (`emailTemplates` setting); a template not set: the built-in email.
    pub email_templates: EmailTemplates,
    /// `{app_name}` in the templates: the setting, else the display name of the SMTP sender in use, else empty.
    pub app_name: String,
}

/// The request headers trusted to carry the client IP (`super::http::client_ip`): `AuthConfig::trust_proxy` (the
/// last `X-Forwarded-For` entry), else the `trustedProxy` setting. Empty: the peer address.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProxyHeaders {
    /// In order of preference: the first one present wins.
    pub headers: Vec<HeaderName>,
    /// Take the leftmost entry of a list rather than the rightmost.
    pub leftmost: bool,
    /// The proxy's addresses: the headers count only on requests from one of them. Empty: from any address.
    pub from: Vec<ipnet::IpNet>,
}

impl ProxyHeaders {
    /// Whether a request from `peer` may carry the client IP in the headers.
    pub(crate) fn trusts(&self, peer: Option<std::net::IpAddr>) -> bool {
        self.from.is_empty() || peer.is_some_and(|p| self.from.iter().any(|net| net.contains(&p.to_canonical())))
    }
}

/// A trusted proxy address: an IP (`10.0.0.2`, `2001:db8::1`) or a CIDR range (`10.0.0.0/8`, `2001:db8::/32`).
pub(crate) fn parse_proxy_address(s: &str) -> Option<ipnet::IpNet> {
    let s = s.trim();
    s.parse().ok().or_else(|| s.parse::<std::net::IpAddr>().ok().map(ipnet::IpNet::from)).map(|net| net.trunc())
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
            (None, None) if dev => Some(config.dev_mailer.clone().unwrap_or_else(|| Arc::new(DevLogMailer))),
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
                OAuthKind::Discord => super::Discord::new(p.client_id.clone(), secret),
                OAuthKind::Apple => {
                    let (Some(team), Some(key)) = (p.team_id.clone(), p.key_id.clone()) else { continue };
                    super::Apple::new(p.client_id.clone(), team, key, secret)
                }
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
        let mut config_from = Vec::new();
        for address in &config.trusted_proxies {
            let Some(net) = parse_proxy_address(address) else {
                anyhow::bail!("trusted proxy address `{address}` is not an IP address or CIDR range");
            };
            config_from.push(net);
        }
        let stored_from = |p: &crate::settings::model::TrustedProxy| -> Vec<ipnet::IpNet> {
            p.proxies.iter().flatten().filter_map(|a| parse_proxy_address(a)).collect()
        };
        let mut proxy = match &stored.trusted_proxy {
            _ if config.trust_proxy => ProxyHeaders {
                headers: vec![HeaderName::from_static("x-forwarded-for")],
                leftmost: false,
                from: Vec::new(),
            },
            Some(p) => ProxyHeaders {
                headers: p.headers.iter().filter_map(|h| HeaderName::from_bytes(h.as_bytes()).ok()).collect(),
                leftmost: p.use_leftmost_ip,
                from: stored_from(p),
            },
            None => ProxyHeaders::default(),
        };
        // Addresses the configuration gives apply to whichever headers are in effect.
        if !config_from.is_empty() {
            proxy.from = config_from;
        }
        let rate_limits = RateLimitRules::build(stored.rate_limits.as_ref());
        let email_templates = stored.email_templates.clone().unwrap_or_default();
        // The sender of the SMTP mailer in use (a custom mailer has none we know of).
        let from = match (&config.mailer, &config.smtp, &stored.smtp) {
            (Some(_), _, _) => None,
            (None, Some(smtp), _) => Some(smtp.from.as_str()),
            (None, None, Some(smtp)) => Some(smtp.from.as_str()),
            (None, None, None) => None,
        };
        let sender = from.and_then(|f| f.parse::<lettre::message::Mailbox>().ok()).and_then(|m| m.name);
        let app_name = email_templates.app_name.clone().or(sender).unwrap_or_default();
        Ok(Effective {
            ts,
            signups,
            access_token_ttl,
            session_ttl,
            redirects,
            mailer,
            oauth,
            cors_origins,
            proxy,
            rate_limits,
            email_templates,
            app_name,
        })
    }
}

/// The settings fields that `config` fixes (its values win over `_settings`), as reported by `_system/settings:get`:
/// `signups`, `accessTokenTtlSecs`, `sessionTtlSecs`, `redirectUrls`, `smtp`, `trustedProxy`, and `oauth.<id>` per
/// provider.
pub(crate) fn locked_fields(config: &AuthConfig) -> Vec<String> {
    let mut out = Vec::new();
    let e = config.explicit;
    for (set, name) in
        [(e.signups, "signups"), (e.access_token_ttl, "accessTokenTtlSecs"), (e.session_ttl, "sessionTtlSecs")]
    {
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
    if config.trust_proxy {
        out.push("trustedProxy".into());
    }
    if !config.trusted_proxies.is_empty() {
        out.push("trustedProxy.proxies".into());
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
