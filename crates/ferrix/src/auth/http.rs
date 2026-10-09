//! HTTP endpoints under `/api/auth/*` (see docs/PROTOCOL.md, "Auth endpoints"), the admin setup page at `/_/setup`,
//! and bearer token parsing for the HTTP function API.
//!
//! Bodies are JSON with camelCase keys. Success responses are the JSON object itself; errors use the same shape as
//! the function API: `{ "status": "error", "errorMessage", "errorData": { "code", "data" } }`. Every response
//! carries `Cache-Control: no-store`.

use std::net::SocketAddr;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use super::identity::Identity;
use super::oauth::{STATE_COOKIE, STATE_TTL, with_fragment};
use super::service::{AuthFailure, AuthResult, CodeProof, Limit, SignUpResult};
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
        .route("/api/auth/magic/request", post(magic_request))
        .route("/api/auth/magic/verify", post(magic_verify))
        .route("/api/auth/oauth/{provider}", get(oauth_start))
        .route("/api/auth/oauth/{provider}/callback", get(oauth_callback))
        .route("/api/auth/admin/signin", post(admin_sign_in))
        .route("/api/auth/admin/setup", get(admin_setup_status).post(admin_setup))
        .route("/_/setup", get(setup_page))
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

/// The caller's IP and user agent.
pub(crate) struct Caller {
    pub ip: String,
    pub user_agent: Option<String>,
}

impl Caller {
    fn client(&self) -> ClientInfo {
        ClientInfo { user_agent: self.user_agent.clone(), ip: Some(self.ip.clone()) }
    }
}

impl FromRequestParts<App> for Caller {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        let peer = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip().to_string());
        let forwarded = app.auth().config().trust_proxy.then(|| {
            parts
                .headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit(',').next())
                .map(|ip| ip.trim().to_string())
                .filter(|ip| !ip.is_empty())
        });
        let ip = forwarded.flatten().or(peer).unwrap_or_else(|| "unknown".into());
        let user_agent = parts.headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(str::to_string);
        Ok(Caller { ip, user_agent })
    }
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
        "signups": c.signups,
        "oauth": auth.oauth.provider_ids(),
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

async fn sign_out(State(app): State<App>, headers: HeaderMap, body: Bytes) -> Response {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase")]
    struct Body {
        refresh_token: Option<String>,
    }
    let result = async {
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
        let provider = auth.oauth.provider(&provider_id).ok_or_else(|| AuthFailure::not_found("This OAuth provider"))?;
        let redirect = auth
            .resolve_redirect(q.redirect_to.as_deref())?
            .ok_or_else(|| AuthFailure::new(400, "invalid_redirect", "No redirect URL given and none configured"))?;
        let callback = callback_url(&app, &headers, &provider_id);
        let (url, state) = auth.oauth.start(provider, redirect, &callback).await?;
        let secure = if callback.starts_with("https://") { "; Secure" } else { "" };
        let cookie = format!(
            "{STATE_COOKIE}={state}; Path=/api/auth/oauth; HttpOnly; SameSite=Lax; Max-Age={}{secure}",
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

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn oauth_callback(
    State(app): State<App>,
    caller: Caller,
    headers: HeaderMap,
    Path(provider_id): Path<String>,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let clear_cookie = format!("{STATE_COOKIE}=; Path=/api/auth/oauth; HttpOnly; SameSite=Lax; Max-Age=0");
    let auth = app.auth();
    // Without a valid pending flow there is no trusted app URL to go back to: answer with a plain error.
    let Some(pending) = q.state.as_deref().and_then(|s| auth.oauth.take(s)).filter(|p| p.provider == provider_id) else {
        let body = "Sign-in failed: this sign-in link is invalid or has expired. Please start again.";
        return (StatusCode::BAD_REQUEST, no_store(), body).into_response();
    };
    let fail = |code: &str, message: &str| {
        let location = with_fragment(&pending.redirect_to, &[("ferrix_error", code), ("ferrix_error_description", message)]);
        redirect_response(&location, Some(&clear_cookie))
    };
    let state = q.state.as_deref().unwrap_or_default();
    if !cookie_value(&headers, STATE_COOKIE).is_some_and(|c| ct_eq(c, state)) {
        return fail("state_mismatch", "The sign-in was started in another browser or has expired");
    }
    if let Some(error) = q.error.as_deref() {
        let code = if error == "access_denied" { "access_denied" } else { "oauth_failed" };
        return fail(code, "The sign-in provider did not grant access");
    }
    let Some(code) = q.code.as_deref() else { return fail("oauth_failed", "The provider returned no code") };
    let Some(provider) = auth.oauth.provider(&provider_id) else { return fail("oauth_failed", "Unknown provider") };
    let callback = callback_url(&app, &headers, &provider_id);
    let result = async {
        let profile = auth.oauth.finish(provider, code, &pending.verifier, &callback).await?;
        auth.oauth_sign_in(&app, &provider_id, profile, caller.client()).await
    }
    .await;
    match result {
        Ok(tokens) => {
            let expires_in = tokens.expires_in.to_string();
            let mut pairs = vec![
                ("ferrix_access_token", tokens.access_token.as_str()),
                ("ferrix_refresh_token", tokens.refresh_token.as_str()),
                ("ferrix_expires_in", expires_in.as_str()),
            ];
            if let Some(u) = tokens.user_id.as_deref() {
                pairs.push(("ferrix_user_id", u));
            }
            redirect_response(&with_fragment(&pending.redirect_to, &pairs), Some(&clear_cookie))
        }
        Err(e) => fail(&e.error.code, &e.error.message),
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Admin setup page
// ---------------------------------------------------------------------------------------------------------------------

/// A minimal page for `/_/setup#token=…` until the dashboard exists. The token stays in the fragment (never sent to
/// the server except in the POST body).
async fn setup_page() -> Response {
    (no_store(), Html(SETUP_PAGE)).into_response()
}

const SETUP_PAGE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="referrer" content="no-referrer"><title>Ferrix setup</title>
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
