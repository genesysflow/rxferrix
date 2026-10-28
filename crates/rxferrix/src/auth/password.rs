//! Password hashing with argon2id (the `argon2` crate's defaults: m = 19 MiB, t = 2, p = 1, per OWASP). Hashing runs
//! on the blocking pool, never inside a mutation (which would hold the writer lock).
//!
//! **Imported hashes.** Verification also accepts bcrypt hashes (`$2a$`, `$2b$`, `$2y$`: Pocketbase, Laravel, Devise, Supabase,
//! ...), so users imported from another system keep their passwords. A successful user sign-in with one replaces it
//! with an argon2id hash ([`needs_rehash`]). Until then, a bcrypt account answers in bcrypt's time instead of argon2's,
//! so timing tells it apart from an unknown email.

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

/// How many password hashes (or verifications) run at once: one per core. Each takes about 19 MiB and a core for
/// tens of milliseconds, so a flood of sign-ins queues here instead of filling the blocking pool (512 threads) and
/// gigabytes of memory.
static HASHING: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(std::thread::available_parallelism().map_or(2, |n| n.get())));

/// Hash a password into an argon2id PHC string.
pub async fn hash(password: String) -> crate::Result<String> {
    let _permit = HASHING.acquire().await?;
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
    let Ok(_permit) = HASHING.acquire().await else { return false };
    tokio::task::spawn_blocking(move || {
        let (phc, real) = match &stored {
            Some(s) => (s.as_str(), true),
            None => (DUMMY_HASH.as_str(), false),
        };
        let ok = if is_bcrypt(phc) {
            bcrypt::verify(password.as_bytes(), phc).unwrap_or(false)
        } else {
            match PasswordHash::new(phc) {
                Ok(parsed) => Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok(),
                Err(_) => false,
            }
        };
        ok && real
    })
    .await
    .unwrap_or(false)
}

/// Whether a stored hash should be replaced by a fresh argon2id hash after a successful sign-in: true for imported
/// (bcrypt) hashes.
pub fn needs_rehash(stored: &str) -> bool {
    !stored.starts_with("$argon2id$")
}

fn is_bcrypt(stored: &str) -> bool {
    ["$2a$", "$2b$", "$2y$"].iter().any(|p| stored.starts_with(p))
}

/// A real argon2id hash (of a random password) with the default parameters, so verifying against it costs the same
/// as verifying a real one.
static DUMMY_HASH: LazyLock<String> =
    LazyLock::new(|| hash_blocking(&super::tokens::random_token()).expect("hashing a random password works"));

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
        assert!(!needs_rehash(&hash("x".repeat(8)).await.unwrap()));
    }

    #[tokio::test]
    async fn imported_bcrypt_hashes() {
        for prefix in ["$2a$", "$2b$", "$2y$"] {
            let h = bcrypt::hash("correct horse", 4).unwrap().replacen("$2b$", prefix, 1);
            assert!(verify("correct horse".into(), Some(h.clone())).await, "{prefix}");
            assert!(!verify("wrong horse".into(), Some(h.clone())).await, "{prefix}");
            assert!(needs_rehash(&h));
        }
        assert!(!verify("anything".into(), Some("$2b$garbage".into())).await);
        assert!(check_password_policy("long enough").is_ok());
    }
}
