//! Password hashing with argon2id (the `argon2` crate's defaults: m = 19 MiB, t = 2, p = 1, per OWASP). Hashing runs
//! on the blocking pool, never inside a mutation (which would hold the writer lock).

use std::sync::LazyLock;

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};

use crate::UserError;

pub const MIN_PASSWORD_LEN: usize = 8;
/// Upper bound so a huge "password" can't be used to burn CPU.
pub const MAX_PASSWORD_LEN: usize = 256;

/// Reject passwords that are too short or too long (by characters).
pub fn check_password_policy(password: &str) -> Result<(), UserError> {
    let len = password.chars().count();
    if len < MIN_PASSWORD_LEN {
        return Err(UserError::new(
            "weak_password",
            format!("Password must be at least {MIN_PASSWORD_LEN} characters"),
        ));
    }
    if len > MAX_PASSWORD_LEN {
        return Err(UserError::new(
            "invalid_password",
            format!("Password must be at most {MAX_PASSWORD_LEN} characters"),
        ));
    }
    Ok(())
}

/// Hash a password into an argon2id PHC string.
pub async fn hash(password: String) -> crate::Result<String> {
    tokio::task::spawn_blocking(move || hash_blocking(&password)).await?
}

pub fn hash_blocking(password: &str) -> crate::Result<String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("password hashing failed: {e}"))
}

/// Verify `password` against a stored PHC string. With `stored = None` (unknown account), a dummy hash is verified
/// instead, so the response time doesn't reveal whether the account exists.
pub async fn verify(password: String, stored: Option<String>) -> bool {
    tokio::task::spawn_blocking(move || {
        let (phc, real) = match &stored {
            Some(s) => (s.as_str(), true),
            None => (DUMMY_HASH.as_str(), false),
        };
        let ok = match PasswordHash::new(phc) {
            Ok(parsed) => Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok(),
            Err(_) => false,
        };
        ok && real
    })
    .await
    .unwrap_or(false)
}

/// A real argon2id hash (of a random password) with the default parameters, so verifying against it costs the same
/// as verifying a real one.
static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    hash_blocking(&super::tokens::random_token()).expect("hashing a random password works")
});

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hash_and_verify() {
        let h = hash("correct horse".into()).await.unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify("correct horse".into(), Some(h.clone())).await);
        assert!(!verify("wrong horse".into(), Some(h)).await);
        assert!(!verify("anything".into(), None).await);
        assert!(check_password_policy("short").is_err());
        assert!(check_password_policy("long enough").is_ok());
    }
}
