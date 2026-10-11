//! Runtime settings, editable from the dashboard without a restart: OAuth providers, SMTP, the redirect allowlist,
//! sign-ups, token lifetimes and CORS origins. Stored in the `_settings` system table ([`model`]); secrets (OAuth
//! client secrets, the SMTP password) are encrypted at rest (`crate::auth::secretbox`) and never returned in clear.
//!
//! **Precedence.** What the code, the command line or the environment configures (`AuthConfig`) wins over
//! `_settings`, field by field (see `crate::auth::config`); `get` lists those fields in `locked`. Settings fill in what
//! the configuration leaves at its defaults.
//!
//! **Applying changes.** `update` validates the whole result, writes the rows and an `_audit_log` entry in one
//! transaction, then rebuilds the effective auth configuration (`crate::auth::effective`) and swaps it in before
//! returning: the next sign-in, OAuth redirect, email or CORS check uses the new values. Sessions and tokens that
//! already exist are not affected (a shorter access token lifetime applies to tokens issued from then on).
//!
//! **Secrets in transit and logs.** Secrets are only ever accepted (in `update` and `send_test_email`), never
//! returned: `get` shows `"••••"` for a stored secret, and sending `"••••"` (or omitting the field) back keeps it. The
//! execution log records the arguments of system functions with every `password`, `secret` and `token` field masked
//! ([`redact_args`]), so they don't end up in `aux.db` either.
//!
//! | Path `_system/settings:…` | Kind | |
//! |---|---|---|
//! | `get` | action (admin) | `{}` → [`SettingsView`] |
//! | `update` | action (admin) | partial update → [`SettingsView`] |
//! | `send_test_email` | action (admin) | `{ to, smtp? }` → `null`; sends with the current (or the given, unsaved) SMTP settings |

pub mod model;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::audit::{self, Record};
use crate::auth::effective::{ACCESS_TTL_RANGE, Effective, SESSION_TTL_RANGE};
use crate::auth::email::Email;
use crate::dashboard::secrets::MASK;
use crate::prelude::*;
use crate::runtime::App;
use model::{OAuthKind, Setting, StoredOAuth, StoredSettings, StoredSmtp};

const MAX_LIST: usize = 50;
const MAX_PROVIDERS: usize = 20;

fn invalid(message: impl Into<String>) -> Error {
    UserError::new("invalid_settings", message).into()
}

// ---------------------------------------------------------------------------------------------------------------------
// Views and inputs
// ---------------------------------------------------------------------------------------------------------------------

/// An OAuth provider as `get` returns it.
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct OAuthProviderView {
    pub id: String,
    pub kind: OAuthKind,
    pub issuer: Option<String>,
    pub client_id: String,
    /// `"••••"` when a secret is stored, else null.
    pub client_secret: Option<String>,
    pub enabled: bool,
    pub scopes: Option<Vec<String>>,
    /// Apple: the developer team id and the private key's id (`clientSecret` is then the masked private key).
    pub team_id: Option<String>,
    pub key_id: Option<String>,
    /// A provider with this id is configured in code (it wins; this entry is ignored).
    pub locked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct SmtpView {
    /// Without the password.
    pub url: String,
    pub from: String,
    /// `"••••"` when a password is stored, else null.
    pub password: Option<String>,
}

/// What is in effect after merging the configuration with the settings.
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct EffectiveView {
    pub signups: bool,
    pub access_token_ttl_secs: u64,
    pub session_ttl_secs: u64,
    pub redirect_urls: Vec<String>,
    pub oauth_providers: Vec<String>,
    pub email_configured: bool,
    /// null: any origin.
    pub cors_origins: Option<Vec<String>>,
}

/// The stored settings (secrets masked; null = not set), the fields the configuration locks, and the result.
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct SettingsView {
    pub oauth: Vec<OAuthProviderView>,
    pub smtp: Option<SmtpView>,
    pub redirect_urls: Option<Vec<String>>,
    pub signups: Option<bool>,
    pub access_token_ttl_secs: Option<u64>,
    pub session_ttl_secs: Option<u64>,
    pub cors_origins: Option<Vec<String>>,
    /// Fields fixed by the configuration: `signups`, `accessTokenTtlSecs`, `sessionTtlSecs`, `redirectUrls`, `smtp`,
    /// `oauth.<id>`.
    pub locked: Vec<String>,
    pub effective: EffectiveView,
}

/// An OAuth provider for `update`.
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase", optional_fields = nullable)]
pub struct OAuthProviderInput {
    pub id: String,
    pub kind: OAuthKind,
    pub issuer: Option<String>,
    pub client_id: String,
    /// Omitted, null or `"••••"`: keep the stored secret of this id. For Apple: the `.p8` private key (PEM).
    pub client_secret: Option<String>,
    pub enabled: bool,
    pub scopes: Option<Vec<String>>,
    /// Apple only (required there): the developer team id and the key id.
    pub team_id: Option<String>,
    pub key_id: Option<String>,
}

/// SMTP for `update` and `send_test_email`.
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase", optional_fields = nullable)]
pub struct SmtpInput {
    /// `smtp://user@host:587` (STARTTLS) or `smtps://user@host:465`. A password in the URL is moved to `password`.
    pub url: String,
    /// e.g. `My App <noreply@example.com>`.
    pub from: String,
    /// Omitted, null or `"••••"`: keep the stored password; `""`: none.
    pub password: Option<String>,
}

// ---------------------------------------------------------------------------------------------------------------------
// Functions
// ---------------------------------------------------------------------------------------------------------------------

/// The stored settings (secrets masked), what the configuration locks, and the effective result.
#[action]
pub async fn get(ctx: &ActionCtx) -> Result<SettingsView> {
    ctx.auth.require_admin()?;
    let stored = load(&ctx.app).await?;
    Ok(view(&ctx.app, &stored))
}

/// Change settings. Every argument is optional: omitted ones are unchanged, and `clear` lists keys to reset
/// (`oauth`, `smtp`, `redirectUrls`, `signups`, `accessTokenTtlSecs`, `sessionTtlSecs`, `corsOrigins`). `oauth`
/// replaces the list of providers configured here. Everything is validated before anything is written; the change
/// is audited and applies immediately.
#[action]
#[allow(clippy::too_many_arguments)]
pub async fn update(
    ctx: &ActionCtx,
    oauth: Option<Vec<OAuthProviderInput>>,
    smtp: Option<SmtpInput>,
    redirect_urls: Option<Vec<String>>,
    signups: Option<bool>,
    access_token_ttl_secs: Option<u64>,
    session_ttl_secs: Option<u64>,
    cors_origins: Option<Vec<String>>,
    clear: Option<Vec<String>>,
) -> Result<SettingsView> {
    let admin = ctx.auth.require_admin()?;
    let app = ctx.app.clone();
    let changes =
        Changes { oauth, smtp, redirect_urls, signups, access_token_ttl_secs, session_ttl_secs, cors_origins, clear };
    let auth = app.auth();
    let sbox = auth.keys.secret_box();
    let dev = app.options().dev;
    let app2 = app.clone();
    let stored = crate::auth::store::write(&app, |db| async move {
        let before = StoredSettings::load(&db, &sbox).await?;
        let after = changes.apply(&before, dev)?;
        // Build the effective config now: anything that can't be used (an SMTP URL lettre rejects) fails here.
        Effective::build(&app2.auth().config, dev, app2.auth().config_mailer.clone(), &after, 0)
            .map_err(|e| invalid(format!("{e:#}")))?;
        let now = db.txn().now();
        let old_rows: BTreeMap<_, _> = before.rows().into_iter().collect();
        let mut changed = Vec::new();
        for (key, row) in after.rows() {
            let old = old_rows.get(key).cloned().flatten();
            if old == row {
                continue;
            }
            changed.push(key);
            let existing = db
                .query::<Setting>()
                .with_index(Setting::by_key, |q| q.eq(Setting::key, key.to_string()))
                .first()
                .await?;
            match (existing, row) {
                (Some(doc), None) => db.delete(doc.id).await?,
                (existing, Some((value, secret))) => {
                    let encrypted = secret.map(|s| sbox.seal(key, &s));
                    let setting = Setting {
                        key: key.to_string(),
                        value,
                        encrypted,
                        updated_at: now,
                        updated_by: Some(admin.to_string()),
                    };
                    match existing {
                        Some(doc) => db.replace(doc.id, setting).await?,
                        None => {
                            db.insert(setting).await?;
                        }
                    }
                }
                (None, None) => {}
            }
        }
        if !changed.is_empty() {
            let (b, a) = (masked_json(&before), masked_json(&after));
            let (b, a) = audit::diff(&b, &a);
            let rec =
                Record { before: Some(b), after: Some(a), ..Record::new("settings_update").target("_settings", None) }
                    .details(serde_json::json!({ "keys": changed }));
            audit::record(&db, Some(admin), rec).await?;
        }
        Ok(after)
    })
    .await?;
    auth.reload_settings(&app).await?;
    Ok(view(&app, &stored))
}

/// Send a test email to `to` through the SMTP settings in effect, or through `smtp` (not saved; a missing password
/// means the stored one). Fails with `email_failed` and the transport's error.
#[action]
pub async fn send_test_email(ctx: &ActionCtx, to: String, smtp: Option<SmtpInput>) -> Result<()> {
    ctx.auth.require_admin()?;
    let app = &ctx.app;
    let to = crate::auth::normalize_email(&to)?;
    let mut secret = None;
    let mailer = match smtp {
        Some(input) => {
            let stored = load(app).await?;
            let smtp = smtp_from_input(input, stored.smtp.as_ref())?;
            secret = smtp.password.clone();
            let config = smtp.config().map_err(|e| invalid(format!("{e:#}")))?;
            crate::auth::effective::smtp_mailer(&config).map_err(|e| invalid(format!("{e:#}")))?
        }
        None => app.auth().current().mailer.clone().ok_or_else(|| {
            Error::from(UserError::new("email_not_configured", "Email is not configured on this server"))
        })?,
    };
    let email = Email {
        to,
        subject: "Ferrix test email".into(),
        body: "This is a test email from your Ferrix server. Email delivery works.".into(),
    };
    match tokio::time::timeout(Duration::from_secs(30), mailer.send(email)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            let mut message = format!("{e:#}");
            if let Some(s) = secret.filter(|s| !s.is_empty()) {
                message = message.replace(&s, MASK);
            }
            Err(UserError::new("email_failed", format!("Sending failed: {message}")).into())
        }
        Err(_) => Err(UserError::new("email_failed", "Sending timed out after 30s").into()),
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------------------------------------------------

/// Read and decrypt the stored settings.
pub(crate) async fn load(app: &App) -> Result<StoredSettings> {
    let sbox = app.auth().keys.secret_box();
    crate::auth::store::read(app, |db| async move { StoredSettings::load(&db, &sbox).await }).await
}

fn mask_secret(s: &Option<String>) -> Option<String> {
    s.as_ref().filter(|s| !s.is_empty()).map(|_| MASK.to_string())
}

fn view(app: &App, stored: &StoredSettings) -> SettingsView {
    let auth = app.auth();
    let locked = crate::auth::effective::locked_fields(&auth.config);
    let e = auth.current();
    SettingsView {
        oauth: stored
            .oauth
            .iter()
            .map(|p| OAuthProviderView {
                id: p.id.clone(),
                kind: p.kind,
                issuer: p.issuer.clone(),
                client_id: p.client_id.clone(),
                client_secret: mask_secret(&p.client_secret),
                enabled: p.enabled,
                scopes: p.scopes.clone(),
                team_id: p.team_id.clone(),
                key_id: p.key_id.clone(),
                locked: locked.contains(&format!("oauth.{}", p.id)),
            })
            .collect(),
        smtp: stored.smtp.as_ref().map(|s| SmtpView {
            url: s.url.clone(),
            from: s.from.clone(),
            password: mask_secret(&s.password),
        }),
        redirect_urls: stored.redirect_urls.clone(),
        signups: stored.signups,
        access_token_ttl_secs: stored.access_token_ttl_secs,
        session_ttl_secs: stored.session_ttl_secs,
        cors_origins: stored.cors_origins.clone(),
        locked,
        effective: EffectiveView {
            signups: e.signups,
            access_token_ttl_secs: e.access_token_ttl.as_secs(),
            session_ttl_secs: e.session_ttl.as_secs(),
            redirect_urls: e.redirects.entries(),
            oauth_providers: e.oauth.keys().cloned().collect(),
            email_configured: e.mailer.is_some(),
            cors_origins: e.cors_origins.clone(),
        },
    }
}

/// The settings as JSON with secrets masked (for the audit log).
fn masked_json(s: &StoredSettings) -> Value {
    serde_json::json!({
        "oauth": s.oauth.iter().map(|p| {
            let mut v = serde_json::to_value(p).unwrap_or_default();
            v["clientSecret"] = serde_json::to_value(mask_secret(&p.client_secret)).unwrap_or_default();
            v
        }).collect::<Vec<_>>(),
        "smtp": s.smtp.as_ref().map(|smtp| {
            let mut v = serde_json::to_value(smtp).unwrap_or_default();
            v["password"] = serde_json::to_value(mask_secret(&smtp.password)).unwrap_or_default();
            v
        }),
        "redirectUrls": s.redirect_urls,
        "signups": s.signups,
        "accessTokenTtlSecs": s.access_token_ttl_secs,
        "sessionTtlSecs": s.session_ttl_secs,
        "corsOrigins": s.cors_origins,
    })
}

/// A secret field from an input: `None` (omitted, null or the mask) keeps `stored`.
fn secret_from_input(given: Option<String>, stored: Option<&String>) -> Option<String> {
    match given {
        None => stored.cloned(),
        Some(s) if s == MASK => stored.cloned(),
        Some(s) if s.is_empty() => None,
        Some(s) => Some(s),
    }
}

fn smtp_from_input(input: SmtpInput, stored: Option<&StoredSmtp>) -> Result<StoredSmtp> {
    let mut url = url::Url::parse(input.url.trim()).map_err(|e| invalid(format!("SMTP URL: {e}")))?;
    if !matches!(url.scheme(), "smtp" | "smtps") || url.host_str().is_none_or(str::is_empty) {
        return Err(invalid("SMTP URL must look like smtp://user@host:587 or smtps://user@host:465"));
    }
    let inline = url.password().map(|p| {
        url::form_urlencoded::parse(format!("p={p}").as_bytes()).next().map(|(_, v)| v.into_owned()).unwrap_or_default()
    });
    let _ = url.set_password(None);
    let password = match (input.password, inline) {
        (given @ Some(_), _) => secret_from_input(given, stored.and_then(|s| s.password.as_ref())),
        (None, Some(inline)) => Some(inline),
        (None, None) => stored.and_then(|s| s.password.clone()),
    };
    let from = input.from.trim().to_string();
    from.parse::<lettre::message::Mailbox>().map_err(|e| invalid(format!("SMTP from address: {e}")))?;
    if password.as_ref().is_some_and(|p| p.len() > 1024) {
        return Err(invalid("SMTP password is too long"));
    }
    Ok(StoredSmtp { url: url.to_string(), from, password })
}

fn check_http_url(what: &str, s: &str, dev: bool) -> Result<url::Url> {
    let url = url::Url::parse(s).map_err(|e| invalid(format!("{what} `{s}`: {e}")))?;
    let loopback = matches!(url.host(), Some(url::Host::Domain("localhost")))
        || matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback());
    let ok = url.scheme() == "https" || (url.scheme() == "http" && (dev || loopback));
    if !ok || url.host().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(invalid(format!("{what} `{s}` must be an https URL")));
    }
    Ok(url)
}

/// The changes `update` was asked to make.
struct Changes {
    oauth: Option<Vec<OAuthProviderInput>>,
    smtp: Option<SmtpInput>,
    redirect_urls: Option<Vec<String>>,
    signups: Option<bool>,
    access_token_ttl_secs: Option<u64>,
    session_ttl_secs: Option<u64>,
    cors_origins: Option<Vec<String>>,
    clear: Option<Vec<String>>,
}

impl Changes {
    fn apply(self, before: &StoredSettings, dev: bool) -> Result<StoredSettings> {
        let mut s = before.clone();
        for key in self.clear.unwrap_or_default() {
            match key.as_str() {
                model::OAUTH => s.oauth.clear(),
                model::SMTP => s.smtp = None,
                model::REDIRECT_URLS => s.redirect_urls = None,
                model::SIGNUPS => s.signups = None,
                model::ACCESS_TTL => s.access_token_ttl_secs = None,
                model::SESSION_TTL => s.session_ttl_secs = None,
                model::CORS_ORIGINS => s.cors_origins = None,
                other => return Err(invalid(format!("unknown setting `{other}` in clear"))),
            }
        }
        if let Some(providers) = self.oauth {
            if providers.len() > MAX_PROVIDERS {
                return Err(invalid(format!("at most {MAX_PROVIDERS} OAuth providers")));
            }
            let mut out: Vec<StoredOAuth> = Vec::new();
            for p in providers {
                let id = p.id.trim().to_string();
                let valid_id = !id.is_empty()
                    && id.len() <= 40
                    && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
                if !valid_id {
                    return Err(invalid(format!("OAuth provider id `{id}`: use 1-40 of a-z, 0-9, - and _")));
                }
                if out.iter().any(|o| o.id == id) {
                    return Err(invalid(format!("OAuth provider `{id}` is listed twice")));
                }
                let client_id = p.client_id.trim().to_string();
                if client_id.is_empty() || client_id.len() > 512 {
                    return Err(invalid(format!("OAuth provider `{id}`: a client id is required")));
                }
                let issuer = match (p.kind, p.issuer.map(|i| i.trim().to_string()).filter(|i| !i.is_empty())) {
                    (OAuthKind::Oidc, None) => {
                        return Err(invalid(format!("OAuth provider `{id}`: OIDC needs an issuer")));
                    }
                    (OAuthKind::Oidc, Some(i)) => {
                        Some(check_http_url("issuer", &i, dev)?.to_string().trim_end_matches('/').to_string())
                    }
                    (_, _) => None,
                };
                let stored_secret = before.oauth.iter().find(|o| o.id == id).and_then(|o| o.client_secret.as_ref());
                let client_secret = secret_from_input(p.client_secret, stored_secret);
                let max_secret = if p.kind == OAuthKind::Apple { 4096 } else { 1024 };
                if client_secret.as_ref().is_some_and(|s| s.len() > max_secret) {
                    return Err(invalid(format!("OAuth provider `{id}`: client secret is too long")));
                }
                if p.enabled && client_secret.is_none() {
                    return Err(invalid(format!("OAuth provider `{id}`: a client secret is required to enable it")));
                }
                // Apple: the team and key ids (10 characters, letters and digits), and a private key that loads.
                let apple_id = |what: &str, v: Option<String>| -> Result<Option<String>> {
                    match v.map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
                        Some(v) if v.len() <= 64 && v.chars().all(|c| c.is_ascii_alphanumeric()) => Ok(Some(v)),
                        Some(v) => Err(invalid(format!("OAuth provider `{id}`: invalid {what} `{v}`"))),
                        None => Err(invalid(format!("OAuth provider `{id}`: Sign in with Apple needs the {what}"))),
                    }
                };
                let (team_id, key_id) = if p.kind == OAuthKind::Apple {
                    if let Some(key) = &client_secret {
                        crate::auth::apple::parse_private_key(key)
                            .map_err(|e| invalid(format!("OAuth provider `{id}`: {e}")))?;
                    }
                    (apple_id("team id", p.team_id)?, apple_id("key id", p.key_id)?)
                } else {
                    (None, None)
                };
                let scopes = match p.scopes {
                    Some(scopes)
                        if scopes.len() > 20
                            || scopes
                                .iter()
                                .any(|s| s.is_empty() || s.len() > 100 || s.contains(char::is_whitespace)) =>
                    {
                        return Err(invalid(format!("OAuth provider `{id}`: invalid scopes")));
                    }
                    other => other.filter(|s| !s.is_empty()),
                };
                out.push(StoredOAuth {
                    id,
                    kind: p.kind,
                    issuer,
                    client_id,
                    enabled: p.enabled,
                    scopes,
                    team_id,
                    key_id,
                    client_secret,
                });
            }
            s.oauth = out;
        }
        if let Some(smtp) = self.smtp {
            s.smtp = Some(smtp_from_input(smtp, before.smtp.as_ref())?);
        }
        if let Some(urls) = self.redirect_urls {
            let urls: Vec<String> = urls.into_iter().map(|u| u.trim().to_string()).filter(|u| !u.is_empty()).collect();
            if urls.len() > MAX_LIST {
                return Err(invalid(format!("at most {MAX_LIST} redirect URLs")));
            }
            crate::auth::config::RedirectAllowlist::new(&urls, false).map_err(|e| invalid(format!("{e:#}")))?;
            s.redirect_urls = Some(urls);
        }
        if let Some(v) = self.signups {
            s.signups = Some(v);
        }
        let range = |what: &str, v: u64, (min, max): (u64, u64)| {
            if (min..=max).contains(&v) {
                Ok(v)
            } else {
                Err(invalid(format!("{what} must be between {min} and {max} seconds")))
            }
        };
        if let Some(v) = self.access_token_ttl_secs {
            s.access_token_ttl_secs = Some(range("accessTokenTtlSecs", v, ACCESS_TTL_RANGE)?);
        }
        if let Some(v) = self.session_ttl_secs {
            s.session_ttl_secs = Some(range("sessionTtlSecs", v, SESSION_TTL_RANGE)?);
        }
        if let Some(origins) = self.cors_origins {
            let mut out = Vec::new();
            for o in origins.into_iter().map(|o| o.trim().trim_end_matches('/').to_string()).filter(|o| !o.is_empty()) {
                if o != "*" {
                    let url = url::Url::parse(&o).map_err(|e| invalid(format!("CORS origin `{o}`: {e}")))?;
                    if !matches!(url.scheme(), "http" | "https") || url.origin().ascii_serialization() != o {
                        return Err(invalid(format!(
                            "CORS origin `{o}` must be an origin like https://app.example.com"
                        )));
                    }
                }
                out.push(o);
            }
            if out.len() > MAX_LIST {
                return Err(invalid(format!("at most {MAX_LIST} CORS origins")));
            }
            s.cors_origins = Some(out);
        }
        Ok(s)
    }
}

/// The execution log's copy of a system function's arguments: every object field whose name contains `password`,
/// `secret` or `token` (case-insensitive) is replaced by `"••••"`, and so is the password of any URL with credentials
/// (`smtp://user:pass@host`), recursively (so `run_function`'s nested `args` are covered too). App functions'
/// arguments are logged as given.
pub fn redact_args<'a>(path: &str, args: &'a Value) -> Cow<'a, Value> {
    /// A URL with credentials (`smtp://user:pass@host`), with the password masked.
    fn url_without_password(s: &str) -> Option<String> {
        if !s.contains("://") || !s.contains('@') {
            return None;
        }
        let mut url = url::Url::parse(s).ok()?;
        url.password()?;
        url.set_password(Some(MASK)).ok()?;
        Some(url.to_string())
    }
    fn sensitive(v: &Value) -> bool {
        match v {
            Value::Object(m) => m.iter().any(|(k, v)| is_secret_key(k) || sensitive(v)),
            Value::Array(a) => a.iter().any(sensitive),
            Value::String(s) => url_without_password(s).is_some(),
            _ => false,
        }
    }
    fn redact(v: &mut Value) {
        match v {
            Value::Object(m) => {
                for (k, v) in m.iter_mut() {
                    if is_secret_key(k) && !v.is_null() {
                        *v = Value::String(MASK.into());
                    } else {
                        redact(v);
                    }
                }
            }
            Value::Array(a) => a.iter_mut().for_each(redact),
            Value::String(s) => {
                if let Some(masked) = url_without_password(s) {
                    *s = masked;
                }
            }
            _ => {}
        }
    }
    if !crate::private::is_system_path(path) || !sensitive(args) {
        return Cow::Borrowed(args);
    }
    let mut v = args.clone();
    redact(&mut v);
    Cow::Owned(v)
}

fn is_secret_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    k.contains("password") || k.contains("secret") || k.contains("token")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_secrets_of_system_functions_only() {
        let args = json!({ "oauth": [{ "id": "gh", "clientSecret": "s" }], "smtp": { "password": "p" }, "x": 1 });
        let r = redact_args("_system/settings:update", &args);
        assert_eq!(r["oauth"][0]["clientSecret"], MASK);
        assert_eq!(r["smtp"]["password"], MASK);
        assert_eq!(r["x"], 1);
        assert_eq!(redact_args("app:fn", &args).into_owned(), args);
        let nested = json!({ "path": "_system/settings:update", "args": { "smtp": { "password": "p" } } });
        assert_eq!(redact_args("_system/dashboard:run_function", &nested)["args"]["smtp"]["password"], MASK);
        assert!(matches!(redact_args("_system/x:y", &json!({ "a": 1 })), Cow::Borrowed(_)));
    }

    #[test]
    fn smtp_input_moves_inline_passwords_out() {
        let input =
            SmtpInput { url: "smtps://me:p%40ss@mail.test:465".into(), from: "App <a@b.co>".into(), password: None };
        let s = smtp_from_input(input, None).unwrap();
        assert_eq!(s.url, "smtps://me@mail.test:465");
        assert_eq!(s.password.as_deref(), Some("p@ss"));
        assert!(s.config().unwrap().url.contains("me:p%40ss@"));
        let keep =
            SmtpInput { url: "smtps://me@mail.test:465".into(), from: "a@b.co".into(), password: Some(MASK.into()) };
        assert_eq!(smtp_from_input(keep, Some(&s)).unwrap().password.as_deref(), Some("p@ss"));
        let bad = SmtpInput { url: "http://mail.test".into(), from: "a@b.co".into(), password: None };
        assert!(smtp_from_input(bad, None).is_err());
    }
}
