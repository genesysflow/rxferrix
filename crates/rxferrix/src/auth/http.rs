//! HTTP endpoints under `/api/auth/*` (see docs/PROTOCOL.md, "Auth endpoints") and bearer token parsing for the HTTP
//! function API. The admin setup page (`/_/setup#token=…`) is part of the dashboard SPA; builds without the
//! `dashboard` feature get a minimal stand-alone page ([`setup_page_routes`]).
//!
//! Bodies are JSON with camelCase keys. Success responses are the JSON object itself; errors use the same shape as
//! the function API: `{ "status": "error", "errorMessage", "errorData": { "code", "data" } }`. Every response
//! carries `Cache-Control: no-store`.

use std::net::{IpAddr, SocketAddr};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
#[cfg(not(feature = "dashboard"))]
use axum::response::Html;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use super::effective::ProxyHeaders;
use super::identity::Identity;
use super::oauth::{CallbackExtras, STATE_COOKIE, STATE_TTL, with_fragment};
use super::service::{AuthFailure, AuthResult, CodeProof, Limit, SignInOutcome, SignUpResult};
use super::store::ClientInfo;
use super::tokens::{TokenError, ct_eq};
use crate::runtime::App;

/// The auth routes, merged into the main router by `server::router`.
pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/auth/providers", get(providers))
        .route("/api/auth/signup", post(sign_up))
        .route("/api/auth/signin", post(sign_in))
        .route("/api/auth/refresh", post(refresh))
        .route("/api/auth/signout", post(sign_out))
        .route("/api/auth/verify-email/request", post(verify_email_request))
        .route("/api/auth/verify-email/confirm", post(verify_email_confirm))
        .route("/api/auth/reset/request", post(reset_request))
        .route("/api/auth/reset/confirm", post(reset_confirm))
        .route("/api/auth/password/change", post(change_password))
        .route("/api/auth/magic/request", post(magic_request))
        .route("/api/auth/magic/verify", post(magic_verify))
        .route("/api/auth/oauth/{provider}", get(oauth_start))
        .route("/api/auth/oauth/{provider}/callback", get(oauth_callback).post(oauth_callback_post))
        .route("/api/auth/admin/signin", post(admin_sign_in))
        .route("/api/auth/admin/setup", get(admin_setup_status).post(admin_setup))
        .route("/api/auth/mfa", get(mfa_status))
        .route("/api/auth/mfa/verify", post(mfa_verify))
        .route("/api/auth/mfa/totp/setup", post(mfa_totp_setup))
        .route("/api/auth/mfa/totp/confirm", post(mfa_totp_confirm))
        .route("/api/auth/mfa/disable", post(mfa_disable))
        .route("/api/auth/mfa/recovery-codes", post(mfa_recovery_codes))
}

/// `/_/setup` for builds without the dashboard SPA.
#[cfg(not(feature = "dashboard"))]
pub(crate) fn setup_page_routes() -> Router<App> {
    Router::new().route("/_/setup", get(setup_page))
}

// ---------------------------------------------------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------------------------------------------------

impl IntoResponse for AuthFailure {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = json!({
            "status": "error",
            "errorMessage": self.error.message,
            "errorData": { "code": self.error.code, "data": self.error.data },
        });
        let mut response = (status, no_store(), axum::Json(body)).into_response();
        if let Some(retry) = self.retry_after
            && let Ok(v) = HeaderValue::from_str(&retry.as_secs().max(1).to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, v);
        }
        response
    }
}

fn no_store() -> [(header::HeaderName, &'static str); 2] {
    [(header::CACHE_CONTROL, "no-store"), (header::REFERRER_POLICY, "no-referrer")]
}

fn ok(value: impl serde::Serialize) -> Response {
    (no_store(), axum::Json(value)).into_response()
}

fn reply<T: serde::Serialize>(result: AuthResult<T>) -> Response {
    match result {
        Ok(v) => ok(v),
        Err(e) => e.into_response(),
    }
}

fn parse<T: DeserializeOwned>(body: &[u8]) -> AuthResult<T> {
    serde_json::from_slice(body)
        .map_err(|e| AuthFailure::new(400, "invalid_request", format!("Invalid request body: {e}")))
}

/// The caller's IP address and user agent, as RxFerrix records them on sessions: taken from the first trusted proxy
/// header present ([`client_ip`]: the `trustedProxy` setting, or the last `X-Forwarded-For` entry with
/// `AuthConfig::trust_proxy` / `--trust-proxy`), else the peer address. An axum extractor for the app's own routes
/// (`RxFerrix::app_routes`), so an allowlist or a rate limit sees the same address RxFerrix does:
///
/// ```ignore
/// async fn webhook(State(app): State<App>, caller: Caller, body: Bytes) -> StatusCode { /* caller.ip */ }
/// ```
#[derive(Debug, Clone)]
pub struct Caller {
    /// `unknown` when the connection's address isn't available.
    pub ip: String,
    pub user_agent: Option<String>,
}

impl Caller {
    /// As recorded on a session.
    pub fn client(&self) -> ClientInfo {
        ClientInfo { user_agent: self.user_agent.clone(), ip: Some(self.ip.clone()) }
    }
}

impl FromRequestParts<App> for Caller {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        let peer = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
        let ip = client_ip(&app.auth().current().proxy, &parts.headers, peer).0;
        let ip = ip.map_or_else(|| "unknown".into(), |ip| ip.to_string());
        let user_agent = parts.headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(str::to_string);
        Ok(Caller { ip, user_agent })
    }
}

/// The client IP of a request and the trusted header it came from: the first of `proxy.headers` that holds an IP
/// address (the leftmost or rightmost entry of a comma-separated list, over all lines of the header), else `peer`.
pub(crate) fn client_ip<'a>(
    proxy: &'a ProxyHeaders,
    headers: &HeaderMap,
    peer: Option<IpAddr>,
) -> (Option<IpAddr>, Option<&'a HeaderName>) {
    for name in &proxy.headers {
        let mut entries = headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|e| !e.is_empty());
        let entry = if proxy.leftmost { entries.next() } else { entries.next_back() };
        if let Some(ip) = entry.and_then(parse_ip) {
            return (Some(ip), Some(name));
        }
    }
    (peer, None)
}

/// An address as proxies write it: `203.0.113.7`, `2001:db8::1`, or with a port (`203.0.113.7:4711`, `[2001:db8::1]:4711`).
fn parse_ip(s: &str) -> Option<IpAddr> {
    s.parse().ok().or_else(|| s.parse::<SocketAddr>().ok().map(|a| a.ip()))
}

/// The token of an `Authorization: Bearer <token>` header.
pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim()).filter(|t| !t.is_empty())
}

/// The identity of an HTTP API call: anonymous without an `Authorization` header, an error (HTTP 401) when the
/// bearer token is invalid, expired or its session has ended.
pub async fn identity_from_headers(app: &App, headers: &HeaderMap) -> Result<Identity, AuthFailure> {
    match bearer_token(headers) {
        None => Ok(Identity::default()),
        Some(token) => app.auth().authenticate(app, token).await.map(|a| a.identity).map_err(token_failure),
    }
}

pub(crate) fn token_failure(e: TokenError) -> AuthFailure {
    AuthFailure::new(401, "invalid_token", e.to_string())
}

/// The bearer token an endpoint that acts on the signed-in account requires.
fn require_bearer(headers: &HeaderMap) -> AuthResult<&str> {
    bearer_token(headers).ok_or_else(|| AuthFailure::new(401, "unauthenticated", "Sign in first"))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodeBody {
    token: Option<String>,
    email: Option<String>,
    code: Option<String>,
}

impl CodeBody {
    fn proof(self) -> AuthResult<CodeProof> {
        match (self.token, self.email, self.code) {
            (Some(t), _, _) if !t.is_empty() => Ok(CodeProof::Token(t)),
            (_, Some(email), Some(code)) => Ok(CodeProof::Code { email, code }),
            _ => Err(AuthFailure::new(400, "invalid_request", "Provide `token`, or `email` and `code`")),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EmailBody {
    email: String,
    redirect_to: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Credentials {
    email: String,
    password: String,
}

// ---------------------------------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------------------------------

async fn providers(State(app): State<App>) -> Response {
    let auth = app.auth();
    let c = auth.config();
    ok(json!({
        "password": c.password,
        "magicLink": c.magic_link,
        "signups": auth.signups_allowed(),
        "oauth": auth.oauth_provider_ids(),
    }))
}

async fn sign_up(State(app): State<App>, caller: Caller, body: Bytes) -> Response {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Body {
        email: String,
        password: String,
        name: Option<String>,
    }
    let result = async {
        app.auth().limit(Limit::Credentials, &caller.ip)?;
        let b: Body = parse(&body)?;
        match app.auth().sign_up(&app, &b.email, b.password, b.name, caller.client()).await? {
            SignUpResult::SignedIn(tokens) => Ok(serde_json::to_value(tokens).unwrap_or_default()),
            SignUpResult::VerificationRequired => Ok(json!({ "verificationRequired": true })),
        }
    }
    .await;
    reply(result)
}

async fn sign_in(State(app): State<App>, caller: Caller, body: Bytes) -> Response {
    let result = async {
        app.auth().limit(Limit::Credentials, &caller.ip)?;
        let b: Credentials = parse(&body)?;
        app.auth().sign_in(&app, &b.email, b.password, caller.client()).await
    }
    .await;
    reply(result)
}

async fn refresh(State(app): State<App>, caller: Caller, body: Bytes) -> Response {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Body {
        refresh_token: String,
    }
    let result = async {
        app.auth().limit(Limit::Refresh, &caller.ip)?;
        let b: Body = parse(&body)?;
        app.auth().refresh(&app, &b.refresh_token, caller.client()).await
    }
    .await;
    reply(result)
}

async fn sign_out(State(app): State<App>, caller: Caller, headers: HeaderMap, body: Bytes) -> Response {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase")]
    struct Body {
        refresh_token: Option<String>,
    }
    let result = async {
        // Unauthenticated and it can end in a write: budgeted like refreshes.
        app.auth().limit(Limit::Refresh, &caller.ip)?;
        let b: Body = if body.is_empty() { Body::default() } else { parse(&body)? };
        app.auth().sign_out(&app, b.refresh_token.as_deref(), bearer_token(&headers)).await?;
        Ok(json!({}))
    }
    .await;
    reply(result)
}

async fn verify_email_request(State(app): State<App>, caller: Caller, body: Bytes) -> Response {
    let result = async {
        app.auth().limit(Limit::Email, &caller.ip)?;
        let b: EmailBody = parse(&body)?;
        app.auth().request_email_verification(&app, &b.email, b.redirect_to.as_deref()).await?;
        Ok(json!({}))
    }
    .await;
    reply(result)
}

async fn verify_email_confirm(State(app): State<App>, caller: Caller, body: Bytes) -> Response {
    let result = async {
        app.auth().limit(Limit::Credentials, &caller.ip)?;
        let b: CodeBody = parse(&body)?;
        app.auth().confirm_email_verification(&app, b.proof()?).await?;
        Ok(json!({}))
    }
    .await;
    reply(result)
}

async fn change_password(State(app): State<App>, caller: Caller, headers: HeaderMap, body: Bytes) -> Response {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Body {
        current_password: String,
        new_password: String,
    }
    let result = async {
        app.auth().limit(Limit::Credentials, &caller.ip)?;
        let b: Body = parse(&body)?;
        let token = require_bearer(&headers)?;
        app.auth().change_password(&app, token, b.current_password, b.new_password).await?;
        Ok(json!({}))
    }
    .await;
    reply(result)
}

async fn reset_request(State(app): State<App>, caller: Caller, body: Bytes) -> Response {
    let result = async {
        app.auth().limit(Limit::Email, &caller.ip)?;
        let b: EmailBody = parse(&body)?;
        app.auth().request_password_reset(&app, &b.email, b.redirect_to.as_deref()).await?;
        Ok(json!({}))
    }
    .await;
    reply(result)
}

async fn reset_confirm(State(app): State<App>, caller: Caller, body: Bytes) -> Response {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Body {
        #[serde(flatten)]
        proof: CodeBody,
        password: String,
    }
    let result = async {
        app.auth().limit(Limit::Credentials, &caller.ip)?;
        let b: Body = parse(&body)?;
        app.auth().confirm_password_reset(&app, b.proof.proof()?, b.password, caller.client()).await
    }
    .await;
    reply(result)
}

async fn magic_request(State(app): State<App>, caller: Caller, body: Bytes) -> Response {
    let result = async {
        app.auth().limit(Limit::Email, &caller.ip)?;
        let b: EmailBody = parse(&body)?;
        app.auth().request_magic_link(&app, &b.email, b.redirect_to.as_deref()).await?;
        Ok(json!({}))
    }
    .await;
    reply(result)
}

async fn magic_verify(State(app): State<App>, caller: Caller, body: Bytes) -> Response {
    let result = async {
        app.auth().limit(Limit::Credentials, &caller.ip)?;
        let b: CodeBody = parse(&body)?;
        app.auth().verify_magic_link(&app, b.proof()?, caller.client()).await
    }
    .await;
    reply(result)
}

async fn admin_sign_in(State(app): State<App>, caller: Caller, body: Bytes) -> Response {
    let result = async {
        app.auth().limit(Limit::Credentials, &caller.ip)?;
        let b: Credentials = parse(&body)?;
        app.auth().admin_sign_in(&app, &b.email, b.password, caller.client()).await
    }
    .await;
    reply(result)
}

async fn admin_setup_status(State(app): State<App>) -> Response {
    reply(app.auth().needs_setup(&app).await.map(|n| json!({ "needsSetup": n })).map_err(AuthFailure::from))
}

async fn admin_setup(State(app): State<App>, caller: Caller, body: Bytes) -> Response {
    #[derive(Deserialize)]
    struct Body {
        token: String,
        email: String,
        password: String,
    }
    let result = async {
        app.auth().limit(Limit::Credentials, &caller.ip)?;
        let b: Body = parse(&body)?;
        app.auth().complete_setup(&app, &b.token, &b.email, b.password, caller.client()).await
    }
    .await;
    reply(result)
}

// ---------------------------------------------------------------------------------------------------------------------
// Two-factor authentication (`super::mfa`)
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct MfaCode {
    code: String,
}

async fn mfa_status(State(app): State<App>, headers: HeaderMap) -> Response {
    let result = async { app.auth().mfa_status(&app, require_bearer(&headers)?).await }.await;
    reply(result)
}

async fn mfa_verify(State(app): State<App>, caller: Caller, body: Bytes) -> Response {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Body {
        mfa_token: String,
        code: String,
    }
    let result = async {
        app.auth().limit(Limit::Credentials, &caller.ip)?;
        let b: Body = parse(&body)?;
        app.auth().mfa_verify(&app, &b.mfa_token, &b.code, caller.client()).await
    }
    .await;
    reply(result)
}

async fn mfa_totp_setup(State(app): State<App>, caller: Caller, headers: HeaderMap, body: Bytes) -> Response {
    #[derive(Deserialize, Default)]
    struct Body {
        password: Option<String>,
    }
    let result = async {
        app.auth().limit(Limit::Credentials, &caller.ip)?;
        let b: Body = if body.is_empty() { Body::default() } else { parse(&body)? };
        app.auth().mfa_setup(&app, require_bearer(&headers)?, b.password).await
    }
    .await;
    reply(result)
}

async fn mfa_totp_confirm(State(app): State<App>, caller: Caller, headers: HeaderMap, body: Bytes) -> Response {
    let result = async {
        app.auth().limit(Limit::Credentials, &caller.ip)?;
        let b: MfaCode = parse(&body)?;
        app.auth().mfa_confirm(&app, require_bearer(&headers)?, &b.code).await
    }
    .await;
    reply(result)
}

async fn mfa_disable(State(app): State<App>, caller: Caller, headers: HeaderMap, body: Bytes) -> Response {
    #[derive(Deserialize, Default)]
    struct Body {
        code: Option<String>,
    }
    let result = async {
        app.auth().limit(Limit::Credentials, &caller.ip)?;
        let b: Body = if body.is_empty() { Body::default() } else { parse(&body)? };
        app.auth().mfa_disable(&app, require_bearer(&headers)?, b.code.as_deref()).await?;
        Ok(json!({}))
    }
    .await;
    reply(result)
}

async fn mfa_recovery_codes(State(app): State<App>, caller: Caller, headers: HeaderMap, body: Bytes) -> Response {
    let result = async {
        app.auth().limit(Limit::Credentials, &caller.ip)?;
        let b: MfaCode = parse(&body)?;
        app.auth().mfa_regenerate_recovery_codes(&app, require_bearer(&headers)?, &b.code).await
    }
    .await;
    reply(result)
}

// ---------------------------------------------------------------------------------------------------------------------
// OAuth
// ---------------------------------------------------------------------------------------------------------------------

/// `<public url>/api/auth/oauth/<provider>/callback`. The public URL comes from the config (`--public-url`); without
/// it (tests, embedded use) the request's Host header is used.
fn callback_url(app: &App, headers: &HeaderMap, provider: &str) -> String {
    let base = match &app.auth().config().public_url {
        Some(u) => u.trim_end_matches('/').to_string(),
        None => {
            let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost");
            format!("http://{host}")
        }
    };
    format!("{base}/api/auth/oauth/{provider}/callback")
}

#[derive(Deserialize)]
struct StartQuery {
    redirect_to: Option<String>,
}

async fn oauth_start(
    State(app): State<App>,
    caller: Caller,
    headers: HeaderMap,
    Path(provider_id): Path<String>,
    Query(q): Query<StartQuery>,
) -> Response {
    let result: AuthResult<(String, String)> = async {
        app.auth().limit(Limit::Credentials, &caller.ip)?;
        let auth = app.auth();
        let provider =
            auth.oauth_provider(&provider_id).ok_or_else(|| AuthFailure::not_found("This OAuth provider"))?;
        let redirect = auth
            .resolve_redirect(q.redirect_to.as_deref())?
            .ok_or_else(|| AuthFailure::new(400, "invalid_redirect", "No redirect URL given and none configured"))?;
        let callback = callback_url(&app, &headers, &provider_id);
        let (url, state) = auth.oauth.start(&provider, redirect, &callback).await?;
        let https = callback.starts_with("https://");
        // A provider that posts the callback cross-site needs `SameSite=None` (https only); see `super::oauth`.
        let same_site = if provider.kind.form_post() && https { "None" } else { "Lax" };
        let secure = if https { "; Secure" } else { "" };
        let cookie = format!(
            "{STATE_COOKIE}={state}; Path=/api/auth/oauth; HttpOnly; SameSite={same_site}; Max-Age={}{secure}",
            STATE_TTL.as_secs()
        );
        Ok((url, cookie))
    }
    .await;
    match result {
        Ok((url, cookie)) => redirect_response(&url, Some(&cookie)),
        Err(e) => e.into_response(),
    }
}

fn redirect_response(location: &str, cookie: Option<&str>) -> Response {
    let mut response = (StatusCode::FOUND, no_store()).into_response();
    let headers = response.headers_mut();
    if let Ok(v) = HeaderValue::from_str(location) {
        headers.insert(header::LOCATION, v);
    }
    if let Some(c) = cookie.and_then(|c| HeaderValue::from_str(c).ok()) {
        headers.insert(header::SET_COOKIE, c);
    }
    response
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

/// The callback's parameters: the query of a `GET`, or the form of a `POST` (`response_mode=form_post`).
#[derive(Deserialize, Default)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    /// Apple: `{"name":{"firstName","lastName"},"email"}`, on the first sign-in only.
    user: Option<String>,
}

async fn oauth_callback(
    State(app): State<App>,
    caller: Caller,
    headers: HeaderMap,
    Path(provider_id): Path<String>,
    Query(q): Query<CallbackParams>,
) -> Response {
    // Only form_post providers send a `user` field, and only in the form.
    let q = CallbackParams { user: None, ..q };
    oauth_complete(app, caller, headers, provider_id, q, false).await
}

/// `POST` callbacks, for providers that use `response_mode=form_post` (Apple) only.
async fn oauth_callback_post(
    State(app): State<App>,
    caller: Caller,
    headers: HeaderMap,
    Path(provider_id): Path<String>,
    form: Result<axum::Form<CallbackParams>, axum::extract::rejection::FormRejection>,
) -> Response {
    if !app.auth().oauth_provider(&provider_id).is_some_and(|p| p.kind.form_post()) {
        return (StatusCode::METHOD_NOT_ALLOWED, no_store(), "This provider's callback is a GET").into_response();
    }
    let Ok(axum::Form(params)) = form else {
        return (StatusCode::BAD_REQUEST, no_store(), "Sign-in failed: the callback form is malformed").into_response();
    };
    oauth_complete(app, caller, headers, provider_id, params, true).await
}

async fn oauth_complete(
    app: App,
    caller: Caller,
    headers: HeaderMap,
    provider_id: String,
    q: CallbackParams,
    posted: bool,
) -> Response {
    let auth = app.auth();
    // Clear the cookie with the attributes it was set with (`oauth_start`).
    let https = callback_url(&app, &headers, &provider_id).starts_with("https://");
    let same_site = if posted && https { "None; Secure" } else { "Lax" };
    let clear_cookie = format!("{STATE_COOKIE}=; Path=/api/auth/oauth; HttpOnly; SameSite={same_site}; Max-Age=0");
    // After a POST, the redirect must turn into a GET of the app URL.
    let redirect = |location: &str| {
        let mut r = redirect_response(location, Some(&clear_cookie));
        if posted {
            *r.status_mut() = StatusCode::SEE_OTHER;
        }
        r
    };
    // Without a valid pending flow there is no trusted app URL to go back to: answer with a plain error.
    let state = q.state.as_deref().unwrap_or_default();
    let Some(redirect_to) = auth.oauth.redirect_of(state) else {
        let body = "Sign-in failed: this sign-in link is invalid or has expired. Please start again.";
        return (StatusCode::BAD_REQUEST, no_store(), body).into_response();
    };
    let fail = |code: &str, message: &str| {
        redirect(&with_fragment(&redirect_to, &[("rxferrix_error", code), ("rxferrix_error_description", message)]))
    };
    // The cookie must name this flow, checked before the flow is consumed: a callback replayed into another browser
    // (no cookie, or another flow's) doesn't burn the pending sign-in of the browser that started it.
    if !cookie_value(&headers, STATE_COOKIE).is_some_and(|c| ct_eq(c, state)) {
        return fail("state_mismatch", "The sign-in was started in another browser or has expired");
    }
    let Some(pending) = auth.oauth.take(state).filter(|p| p.provider == provider_id) else {
        return fail("oauth_failed", "This sign-in link is invalid or has expired");
    };
    if let Some(error) = q.error.as_deref() {
        let code = if matches!(error, "access_denied" | "user_cancelled_authorize") {
            "access_denied"
        } else {
            "oauth_failed"
        };
        return fail(code, "The sign-in provider did not grant access");
    }
    let Some(code) = q.code.as_deref() else { return fail("oauth_failed", "The provider returned no code") };
    let Some(provider) = auth.oauth_provider(&provider_id) else { return fail("oauth_failed", "Unknown provider") };
    let callback = callback_url(&app, &headers, &provider_id);
    let extras = CallbackExtras { apple_user: q.user.clone() };
    let result = async {
        let profile = auth.oauth.finish(&provider, code, &pending, &callback, &extras).await?;
        auth.oauth_sign_in(&app, &provider_id, profile, caller.client()).await
    }
    .await;
    match result {
        // The account has two-factor authentication: the app gets the second-factor token to finish with a code.
        Ok(SignInOutcome::MfaRequired { mfa_token, .. }) => {
            redirect(&with_fragment(&pending.redirect_to, &[("rxferrix_mfa_token", mfa_token.as_str())]))
        }
        Ok(SignInOutcome::Tokens(tokens)) => {
            let expires_in = tokens.expires_in.to_string();
            let mut pairs = vec![
                ("rxferrix_access_token", tokens.access_token.as_str()),
                ("rxferrix_refresh_token", tokens.refresh_token.as_str()),
                ("rxferrix_expires_in", expires_in.as_str()),
            ];
            if let Some(u) = tokens.user_id.as_deref() {
                pairs.push(("rxferrix_user_id", u));
            }
            redirect(&with_fragment(&pending.redirect_to, &pairs))
        }
        Err(e) => fail(&e.error.code, &e.error.message),
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Admin setup page
// ---------------------------------------------------------------------------------------------------------------------

/// A minimal page for `/_/setup#token=…` in builds without the dashboard. The token stays in the fragment (never sent
/// to the server except in the POST body).
#[cfg(not(feature = "dashboard"))]
async fn setup_page() -> Response {
    (no_store(), Html(SETUP_PAGE)).into_response()
}

#[cfg(not(feature = "dashboard"))]
const SETUP_PAGE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="referrer" content="no-referrer"><title>RxFerrix setup</title>
<style>body{font-family:system-ui,sans-serif;max-width:24rem;margin:4rem auto;padding:0 1rem}
label{display:block;margin:.75rem 0 .25rem}input{width:100%;padding:.5rem;box-sizing:border-box}
button{margin-top:1rem;padding:.5rem 1rem}#msg{margin-top:1rem}</style></head>
<body><h1>Create the first administrator</h1>
<form id="f"><label>Email<input id="email" type="email" required autocomplete="username"></label>
<label>Password (8+ characters)<input id="password" type="password" minlength="8" required autocomplete="new-password"></label>
<button>Create administrator</button></form><p id="msg"></p>
<script>
const token = new URLSearchParams(location.hash.slice(1)).get("token") || "";
history.replaceState(null, "", location.pathname);
const msg = document.getElementById("msg");
document.getElementById("f").addEventListener("submit", async (e) => {
  e.preventDefault();
  const res = await fetch("/api/auth/admin/setup", { method: "POST", headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ token, email: document.getElementById("email").value, password: document.getElementById("password").value }) });
  const body = await res.json().catch(() => ({}));
  msg.textContent = res.ok ? "Administrator created. You can close this page." : (body.errorMessage || "Setup failed");
  if (res.ok) document.getElementById("f").remove();
});
</script></body></html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_static(v));
        }
        h
    }

    fn proxy(names: &[&'static str], leftmost: bool) -> ProxyHeaders {
        ProxyHeaders { headers: names.iter().map(|n| HeaderName::from_static(n)).collect(), leftmost }
    }

    #[test]
    fn client_ip_takes_the_first_trusted_header_present() {
        let peer = Some("10.0.0.1".parse().unwrap());
        let h = headers(&[
            ("x-forwarded-for", "198.51.100.9, 203.0.113.1"),
            ("x-forwarded-for", "203.0.113.2"),
            ("cf-connecting-ip", "2001:db8::7"),
        ]);
        let ip = |p: &ProxyHeaders| client_ip(p, &h, peer).0.unwrap().to_string();
        assert_eq!(ip(&proxy(&[], false)), "10.0.0.1");
        assert_eq!(ip(&proxy(&["x-forwarded-for"], false)), "203.0.113.2");
        assert_eq!(ip(&proxy(&["x-forwarded-for"], true)), "198.51.100.9");
        assert_eq!(ip(&proxy(&["fly-client-ip", "cf-connecting-ip", "x-forwarded-for"], false)), "2001:db8::7");
        let p = proxy(&["cf-connecting-ip"], false);
        assert_eq!(client_ip(&p, &h, peer).1.map(HeaderName::as_str), Some("cf-connecting-ip"));
        assert_eq!(client_ip(&proxy(&["x-real-ip"], false), &h, peer).1, None);
    }

    #[test]
    fn client_ip_skips_values_that_are_not_addresses() {
        let peer = Some("10.0.0.1".parse().unwrap());
        let h = headers(&[("x-real-ip", "unknown"), ("x-forwarded-for", "[2001:db8::1]:4711"), ("fly-client-ip", "")]);
        let p = proxy(&["fly-client-ip", "x-real-ip", "x-forwarded-for"], false);
        assert_eq!(client_ip(&p, &h, peer).0.unwrap().to_string(), "2001:db8::1");
        assert_eq!(client_ip(&proxy(&["x-real-ip"], false), &h, peer).0, peer);
        assert_eq!(parse_ip("203.0.113.7:80").unwrap().to_string(), "203.0.113.7");
    }
}
