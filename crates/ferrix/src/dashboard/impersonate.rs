//! `POST /api/admin/impersonate`: mint a short-lived session of a user, to open the app as them.
//!
//! Body: `{ user, ttlSeconds?, redirectTo? }` (default 15 minutes, at most 1 hour). Response: `{ accessToken,
//! refreshToken, expiresIn, userId, sessionId, expiresAt, url? }`; with `redirectTo` (checked against the redirect
//! allowlist like OAuth redirects), `url` is that app URL with the tokens in the fragment
//! (`#ferrix_access_token=…&ferrix_refresh_token=…&ferrix_expires_in=…&ferrix_user_id=…`), which the TS client's
//! `handleRedirectCallback()` picks up: open it in a private window.
//!
//! **Security.** Admin bearer token required (like every `/api/admin/*` endpoint) and the feature can be turned off
//! with `AuthConfig::impersonation(false)` (403 `impersonation_disabled`). The session is a real one: marked
//! `impersonatedBy` (apps can refuse sensitive operations), listed with the user's sessions, revocable, ended by
//! `delete_user`, with a fixed expiry that refreshing never extends; every access token it yields expires with it.
//! Minting is audited (`impersonate`, in the session's transaction). The tokens grant everything the user can do,
//! so they are returned only to the admin, never logged, and travel only in the response body or a URL fragment.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::auth::AuthFailure;
use crate::auth::service::{DEFAULT_IMPERSONATION_TTL, MAX_IMPERSONATION_TTL};
use crate::auth::tables::{Admin, User};
use crate::id::Id;
use crate::runtime::App;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Body {
    user: String,
    ttl_seconds: Option<u64>,
    redirect_to: Option<String>,
}

pub(crate) async fn impersonate(
    State(app): State<App>,
    caller: crate::auth::http::Caller,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let result: Result<serde_json::Value, AuthFailure> = async {
        let auth = super::http::require_admin(&app, &headers).await.map_err(|f| *f)?;
        if !app.auth().config().impersonation {
            return Err(AuthFailure::new(403, "impersonation_disabled", "Impersonation is disabled on this server"));
        }
        let admin: Id<Admin> = auth.identity.admin_id.as_deref().and_then(|a| a.parse().ok()).ok_or_else(|| {
            AuthFailure::new(403, crate::auth::FORBIDDEN, "Only administrators may do this")
        })?;
        let body: Body = serde_json::from_slice(&body)
            .map_err(|e| AuthFailure::new(400, "invalid_request", format!("Invalid request body: {e}")))?;
        let user: Id<User> =
            body.user.parse().map_err(|_| AuthFailure::new(400, "invalid_id", format!("`{}` is not a user id", body.user)))?;
        let ttl = body.ttl_seconds.map(std::time::Duration::from_secs).unwrap_or(DEFAULT_IMPERSONATION_TTL);
        if ttl.is_zero() || ttl > MAX_IMPERSONATION_TTL {
            return Err(AuthFailure::new(400, "invalid_request", "ttlSeconds must be between 1 and 3600"));
        }
        let redirect = match body.redirect_to.as_deref().filter(|r| !r.is_empty()) {
            Some(r) => app.auth().resolve_redirect(Some(r))?,
            None => None,
        };
        let purpose = json!({ "purpose": "open_app", "ip": caller.ip });
        let client = crate::auth::ClientInfo { user_agent: caller.user_agent.clone(), ip: Some(caller.ip.clone()) };
        let (tokens, session, expires_at) = app
            .auth()
            .impersonate(&app, admin, user, ttl, purpose, client)
            .await?
            .ok_or_else(|| AuthFailure::new(404, "not_found", format!("User {user} does not exist")))?;
        let url = redirect.map(|r| {
            let expires_in = tokens.expires_in.to_string();
            let user_id = user.to_string();
            crate::auth::oauth::with_fragment(
                &r,
                &[
                    ("ferrix_access_token", tokens.access_token.as_str()),
                    ("ferrix_refresh_token", tokens.refresh_token.as_str()),
                    ("ferrix_expires_in", expires_in.as_str()),
                    ("ferrix_user_id", user_id.as_str()),
                ],
            )
        });
        Ok(json!({
            "accessToken": tokens.access_token,
            "refreshToken": tokens.refresh_token,
            "expiresIn": tokens.expires_in,
            "userId": user.to_string(),
            "sessionId": session.to_string(),
            "expiresAt": expires_at,
            "url": url,
        }))
    }
    .await;
    match result {
        Ok(v) => ([(axum::http::header::CACHE_CONTROL, "no-store")], axum::Json(v)).into_response(),
        Err(e) => e.into_response(),
    }
}
