//! The `_settings` table and its decrypted in-memory form, [`StoredSettings`].
//!
//! One row per setting key. `value` holds the non-secret part as JSON; `encrypted` holds the secret part (OAuth
//! client secrets, the SMTP password) sealed with `crate::auth::secretbox`, bound to the row's key. Secrets never
//! leave the server in clear: `_system/settings:get` returns `"••••"` for a stored secret, the dashboard's data
//! browser masks the `encrypted` field, and execution logs never see them (the settings functions take them as
//! arguments only on `update`, whose arguments are not logged; see `super`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::config::SmtpConfig;
use crate::auth::secretbox::SecretBox;
use crate::db::query::QueryDb;
use crate::prelude::*;

/// A runtime setting (`_settings`): `key` is one of [`KEYS`].
#[table(name = "_settings")]
#[index(by_key = [key])]
pub struct Setting {
    pub key: String,
    #[ts(type = "any")]
    pub value: Value,
    // The sealed secret part (`v1.…`), if the setting has one.
    pub encrypted: Option<String>,
    pub updated_at: f64,
    // `Id<Admin>` of the last editor.
    pub updated_by: Option<String>,
}

pub const OAUTH: &str = "oauth";
pub const SMTP: &str = "smtp";
pub const REDIRECT_URLS: &str = "redirectUrls";
pub const SIGNUPS: &str = "signups";
pub const ACCESS_TTL: &str = "accessTokenTtlSecs";
pub const SESSION_TTL: &str = "sessionTtlSecs";
pub const CORS_ORIGINS: &str = "corsOrigins";
pub const BACKUPS: &str = "backups";
pub const TRUSTED_PROXY: &str = "trustedProxy";
pub const RATE_LIMITS: &str = "rateLimits";
pub const EMAIL_TEMPLATES: &str = "emailTemplates";
/// Every setting key.
pub const KEYS: &[&str] = &[
    OAUTH,
    SMTP,
    REDIRECT_URLS,
    SIGNUPS,
    ACCESS_TTL,
    SESSION_TTL,
    CORS_ORIGINS,
    BACKUPS,
    TRUSTED_PROXY,
    RATE_LIMITS,
    EMAIL_TEMPLATES,
];

/// The kind of an OAuth provider configured at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "lowercase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "lowercase")]
pub enum OAuthKind {
    Github,
    Google,
    Discord,
    /// Sign in with Apple; needs `teamId` and `keyId`, and its secret is the `.p8` private key.
    Apple,
    /// Generic OpenID Connect; needs `issuer` (discovery).
    Oidc,
}

/// An OAuth provider as stored (`value` of the `oauth` row is a list of these; secrets are separate).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredOAuth {
    pub id: String,
    pub kind: OAuthKind,
    #[serde(default)]
    pub issuer: Option<String>,
    pub client_id: String,
    pub enabled: bool,
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
    /// Apple: the developer team id and the key id of the private key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    /// Decrypted; never serialized into the row's `value`. For Apple, the private key (PEM).
    #[serde(skip)]
    pub client_secret: Option<String>,
}

/// SMTP as stored: the URL without its password (the password is the row's secret).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredSmtp {
    /// `smtp://user@host:587` or `smtps://user@host:465`, no password.
    pub url: String,
    pub from: String,
    #[serde(skip)]
    pub password: Option<String>,
}

impl StoredSmtp {
    /// The full SMTP config, password included.
    pub(crate) fn config(&self) -> crate::Result<SmtpConfig> {
        let mut url = url::Url::parse(&self.url).map_err(|e| anyhow::anyhow!("invalid SMTP URL: {e}"))?;
        if let Some(p) = self.password.as_deref().filter(|p| !p.is_empty()) {
            url.set_password(Some(p)).map_err(|_| anyhow::anyhow!("invalid SMTP URL: it can't carry a password"))?;
        }
        Ok(SmtpConfig::new(url.to_string(), self.from.clone()))
    }
}

/// The request headers a reverse proxy puts the client's IP in, trusted in this order (`crate::auth::http::Caller`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct TrustedProxy {
    /// Header names, e.g. `CF-Connecting-IP`, `X-Forwarded-For`, `Fly-Client-IP`; the first one present wins.
    pub headers: Vec<String>,
    /// Take the leftmost IP of a list (`client, proxy1, proxy2`) instead of the rightmost. The leftmost is the
    /// original client but anyone can forge it; the rightmost is the one the last proxy added.
    #[serde(default)]
    pub use_leftmost_ip: bool,
}

/// Request rate limits per client IP (`super::rate_limits`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct RateLimitSettings {
    /// Off: the rules are kept but none applies.
    pub enabled: bool,
    pub rules: Vec<RateLimitRule>,
}

/// One rate limit: at most `max_requests` per `interval_secs` from one client IP, as a token bucket (bursts up to
/// `max_requests`, refilled evenly over the interval).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct RateLimitRule {
    /// What it limits: an HTTP path prefix (`/api/`, `/api/storage/upload`), or app functions called by clients
    /// (`messages:send`, `messages:*`, `*:create`, `*`).
    pub label: String,
    pub max_requests: u32,
    pub interval_secs: u32,
    #[serde(default)]
    pub audience: RateLimitAudience,
}

/// Which callers a rate limit rule applies to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub enum RateLimitAudience {
    #[default]
    All,
    /// Requests without a valid access token.
    Guest,
    /// Requests from signed-in users.
    SignedIn,
}

/// The subject and HTML body of an auth email, with placeholders (`crate::auth::email::PLACEHOLDERS`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct EmailTemplate {
    pub subject: String,
    pub html: String,
}

/// The auth emails' templates and the `{app_name}` they use. A template not set: the built-in plain-text email.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase", optional_fields = nullable)]
pub struct EmailTemplates {
    /// `{app_name}`; not set: the display name of the SMTP sender (`My App <noreply@…>`), if any.
    #[serde(default)]
    pub app_name: Option<String>,
    #[serde(default)]
    pub verify_email: Option<EmailTemplate>,
    #[serde(default)]
    pub reset_password: Option<EmailTemplate>,
    #[serde(default)]
    pub sign_in: Option<EmailTemplate>,
}

impl EmailTemplates {
    /// The template for one kind of auth email, if set.
    pub fn get(&self, purpose: crate::auth::email::Purpose) -> Option<&EmailTemplate> {
        use crate::auth::email::Purpose;
        match purpose {
            Purpose::VerifyEmail => self.verify_email.as_ref(),
            Purpose::ResetPassword => self.reset_password.as_ref(),
            Purpose::SignIn => self.sign_in.as_ref(),
        }
    }
}

/// A stored row: the JSON value and the plaintext of its secret part.
pub(crate) type Row = (Value, Option<Vec<u8>>);

/// Every setting, decrypted. `None` = not set (the config or the default applies).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StoredSettings {
    pub oauth: Vec<StoredOAuth>,
    pub smtp: Option<StoredSmtp>,
    pub redirect_urls: Option<Vec<String>>,
    pub signups: Option<bool>,
    pub access_token_ttl_secs: Option<u64>,
    pub session_ttl_secs: Option<u64>,
    pub cors_origins: Option<Vec<String>>,
    /// Scheduled backups and their destination (`super::backups`).
    pub backups: Option<super::backups::StoredBackups>,
    /// Where the client IP comes from behind a reverse proxy.
    pub trusted_proxy: Option<TrustedProxy>,
    /// Request rate limits per client IP.
    pub rate_limits: Option<RateLimitSettings>,
    /// Templates of the verification, password reset and sign-in emails.
    pub email_templates: Option<EmailTemplates>,
}

impl StoredSettings {
    /// Read every `_settings` row. A row that can't be parsed or decrypted is skipped with an error in the log (an
    /// unreadable secret leaves its provider without one, i.e. disabled), so a bad row can't stop the server.
    pub(crate) async fn load(db: &QueryDb, sbox: &SecretBox) -> crate::Result<StoredSettings> {
        let rows = db.query::<Setting>().collect().await?;
        let mut out = StoredSettings::default();
        for row in rows {
            if let Err(e) = out.apply_row(&row.value, sbox) {
                tracing::error!("settings: ignoring `{}`: {e:#}", row.key);
            }
        }
        Ok(out)
    }

    fn apply_row(&mut self, row: &Setting, sbox: &SecretBox) -> crate::Result<()> {
        let value = row.value.clone();
        let secret = match &row.encrypted {
            Some(sealed) => Some(sbox.open(&row.key, sealed)?),
            None => None,
        };
        match row.key.as_str() {
            OAUTH => {
                let mut providers: Vec<StoredOAuth> = serde_json::from_value(value)?;
                let secrets: std::collections::BTreeMap<String, String> = match secret {
                    Some(s) => serde_json::from_slice(&s)?,
                    None => Default::default(),
                };
                for p in &mut providers {
                    p.client_secret = secrets.get(&p.id).cloned();
                }
                self.oauth = providers;
            }
            SMTP => {
                let mut smtp: StoredSmtp = serde_json::from_value(value)?;
                smtp.password = secret.map(String::from_utf8).transpose()?;
                self.smtp = Some(smtp);
            }
            REDIRECT_URLS => self.redirect_urls = Some(serde_json::from_value(value)?),
            SIGNUPS => self.signups = Some(serde_json::from_value(value)?),
            ACCESS_TTL => self.access_token_ttl_secs = Some(serde_json::from_value(value)?),
            SESSION_TTL => self.session_ttl_secs = Some(serde_json::from_value(value)?),
            CORS_ORIGINS => self.cors_origins = Some(serde_json::from_value(value)?),
            BACKUPS => {
                let mut backups: super::backups::StoredBackups = serde_json::from_value(value)?;
                backups.secret_access_key = secret.map(String::from_utf8).transpose()?;
                self.backups = Some(backups);
            }
            TRUSTED_PROXY => self.trusted_proxy = Some(serde_json::from_value(value)?),
            RATE_LIMITS => self.rate_limits = Some(serde_json::from_value(value)?),
            EMAIL_TEMPLATES => self.email_templates = Some(serde_json::from_value(value)?),
            other => anyhow::bail!("unknown setting `{other}`"),
        }
        Ok(())
    }

    /// The rows these settings are stored as: `(key, value, secret plaintext)`, `None` for unset keys.
    pub(crate) fn rows(&self) -> Vec<(&'static str, Option<Row>)> {
        let json = |v: Value| Some((v, None));
        let secrets: std::collections::BTreeMap<&str, &str> = self
            .oauth
            .iter()
            .filter_map(|p| p.client_secret.as_deref().filter(|s| !s.is_empty()).map(|s| (p.id.as_str(), s)))
            .collect();
        vec![
            (
                OAUTH,
                (!self.oauth.is_empty()).then(|| {
                    let secret = (!secrets.is_empty()).then(|| serde_json::to_vec(&secrets).expect("map serializes"));
                    (serde_json::to_value(&self.oauth).expect("serializes"), secret)
                }),
            ),
            (
                SMTP,
                self.smtp.as_ref().map(|s| {
                    let secret = s.password.as_ref().filter(|p| !p.is_empty()).map(|p| p.as_bytes().to_vec());
                    (serde_json::to_value(s).expect("serializes"), secret)
                }),
            ),
            (REDIRECT_URLS, self.redirect_urls.clone().and_then(|v| json(v.into()))),
            (SIGNUPS, self.signups.and_then(|v| json(v.into()))),
            (ACCESS_TTL, self.access_token_ttl_secs.and_then(|v| json(v.into()))),
            (SESSION_TTL, self.session_ttl_secs.and_then(|v| json(v.into()))),
            (CORS_ORIGINS, self.cors_origins.clone().and_then(|v| json(v.into()))),
            (
                BACKUPS,
                self.backups.as_ref().map(|b| {
                    let secret = b.secret_access_key.as_ref().filter(|s| !s.is_empty()).map(|s| s.as_bytes().to_vec());
                    (serde_json::to_value(b).expect("serializes"), secret)
                }),
            ),
            (
                TRUSTED_PROXY,
                self.trusted_proxy.as_ref().and_then(|p| json(serde_json::to_value(p).expect("serializes"))),
            ),
            (RATE_LIMITS, self.rate_limits.as_ref().and_then(|r| json(serde_json::to_value(r).expect("serializes")))),
            (
                EMAIL_TEMPLATES,
                self.email_templates.as_ref().and_then(|t| json(serde_json::to_value(t).expect("serializes"))),
            ),
        ]
    }
}
