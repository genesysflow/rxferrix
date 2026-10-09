//! Authentication: users, sessions, tokens, email codes, OAuth and admins (PLAN.md §7.1, docs/PROTOCOL.md).
//!
//! - [`tables`]: the system tables (`_users`, `_credentials`, `_accounts`, `_sessions`, `_used_refresh_tokens`,
//!   `_verification_codes`, `_admins`).
//! - [`identity`]: [`Identity`] (who is calling) and [`Auth`] (`ctx.auth` in functions).
//! - [`tokens`]: the server secret, HS256 access tokens, opaque tokens and keyed hashes.
//! - [`config`]: [`AuthConfig`] and the OAuth presets [`GitHub`], [`Google`], [`Oidc`].
//! - [`service`]: [`AuthService`], the auth state of an `App` (`app.auth()`), and the operations behind the
//!   endpoints. [`http`]: the `/api/auth/*` routes. [`oauth`]: the authorization code flow with PKCE.
//! - [`email`]: [`Mailer`] (SMTP, dev log, [`MemoryMailer`] for tests).
//!
//! The functions in this module are the client-facing system functions `_system/auth:me` and
//! `_system/auth:update_profile` (hand-written references in `@ferrix/client`, since codegen skips system functions).

pub mod config;
pub mod email;
pub mod http;
pub mod identity;
pub mod oauth;
mod password;
pub mod rate_limit;
pub mod revocation;
pub mod service;
pub(crate) mod store;
pub mod tables;
pub mod tokens;


pub use config::{AuthConfig, GitHub, Google, OAuthProvider, Oidc, RateLimits, SmtpConfig};
pub use email::{Email, Mailer, MemoryMailer};
pub use identity::{Auth, FORBIDDEN, Identity, UNAUTHENTICATED};
pub use revocation::{Revocations, SessionWatch};
pub use service::{AuthFailure, AuthService, AuthTokens, Authenticated, CodeProof, SignUpResult};
pub use store::{ClientInfo, normalize_email};
pub use tables::{Account, Admin, Session, User, VerificationCode};

use crate::prelude::*;

/// The signed-in user's document, or `null` when signed out (or signed in as an admin). Reactive: it reruns when
/// the identity or the user's profile changes. Contains no secrets (passwords live in `_credentials`).
#[query]
pub async fn me(ctx: &QueryCtx) -> Result<Option<Doc<User>>> {
    ctx.auth.user(&ctx.db).await
}

/// Update the signed-in user's display name and/or image URL. A missing argument leaves the field unchanged; an
/// empty string clears it.
#[mutation]
pub async fn update_profile(ctx: &MutationCtx, name: Option<String>, image: Option<String>) -> Result<()> {
    let id = ctx.auth.require_user()?;
    let Some(mut user) = ctx.db.get(id).await? else {
        return Err(UserError::new(UNAUTHENTICATED, "This account no longer exists").into());
    };
    if let Some(name) = name {
        let name = name.trim();
        if name.chars().count() > 200 {
            return Err(UserError::new("invalid_name", "Name must be at most 200 characters").into());
        }
        user.value.name = (!name.is_empty()).then(|| name.to_string());
    }
    if let Some(image) = image {
        let image = image.trim();
        if !image.is_empty() && !(image.starts_with("https://") || image.starts_with("http://")) {
            return Err(UserError::new("invalid_image", "Image must be an http(s) URL").into());
        }
        if image.len() > 2048 {
            return Err(UserError::new("invalid_image", "Image URL is too long").into());
        }
        user.value.image = (!image.is_empty()).then(|| image.to_string());
    }
    ctx.db.replace(id, user.value).await
}
