//! Secrets and tokens: the server secret, HS256 access tokens, random opaque tokens and keyed hashes.
//!
//! **The server secret** is 64 random bytes in `<data dir>/auth_secret.key`, created on first start with mode 0600.
//! It lives in a file rather than in the database so that copies of `data.db` (backups, exports, the dashboard's
//! data browser) never contain it. Two independent keys are derived from it with BLAKE3's KDF mode:
//! - the HS256 key that signs access tokens;
//! - the key of the keyed hash ("pepper") applied to refresh tokens, link tokens and numeric codes before they are
//!   stored, so a leaked database alone does not allow brute-forcing 6-digit codes or matching tokens;
//! - the HMAC key that signs file upload URLs (`crate::storage`).
//!
//! Deleting the file signs everyone out: every access token and every stored token hash becomes invalid.
//!
//! **Access tokens** are compact JWTs with header `{"alg":"HS256","typ":"JWT"}` and claims
//! `{ iss: "ferrix", typ: "access", sub, sid, adm?, iat, exp }`. Verification accepts exactly that header (no
//! algorithm negotiation, so no `alg: none` / key-confusion attacks) and compares the MAC in constant time.

use std::path::Path;

use anyhow::{Context, bail};
use data_encoding::BASE64URL_NOPAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// File name of the server secret inside the data directory.
pub const SECRET_FILE: &str = "auth_secret.key";

const ISSUER: &str = "ferrix";
const ACCESS_TYPE: &str = "access";
const JWT_HEADER: &str = r#"{"alg":"HS256","typ":"JWT"}"#;

/// Keys derived from the server secret.
pub struct Keys {
    jwt: [u8; 32],
    hash: [u8; 32],
    storage: [u8; 32],
}

impl Keys {
    /// Load `<dir>/auth_secret.key`, creating it (0600) if missing.
    pub fn load_or_create(dir: &Path) -> crate::Result<Keys> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating data dir {}", dir.display()))?;
        let path = dir.join(SECRET_FILE);
        let secret = match std::fs::read(&path) {
            Ok(bytes) => {
                check_permissions(&path);
                let decoded = data_encoding::HEXLOWER
                    .decode(String::from_utf8_lossy(&bytes).trim().as_bytes())
                    .with_context(|| format!("{} is not valid hex", path.display()))?;
                if decoded.len() < 32 {
                    bail!("{} must hold at least 32 bytes of secret", path.display());
                }
                decoded
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let secret: [u8; 64] = rand::random();
                write_new_secret(&path, &data_encoding::HEXLOWER.encode(&secret))?;
                tracing::info!("auth: created a new server secret at {}", path.display());
                secret.to_vec()
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Keys::from_secret(&secret))
    }

    pub fn from_secret(secret: &[u8]) -> Keys {
        Keys {
            jwt: blake3::derive_key("ferrix auth v1 access token HS256 key", secret),
            hash: blake3::derive_key("ferrix auth v1 stored token hash key", secret),
            storage: blake3::derive_key("ferrix storage v1 upload url HMAC key", secret),
        }
    }

    /// The HMAC-SHA256 key that signs file upload URLs (`crate::storage`).
    pub(crate) fn storage_key(&self) -> &[u8; 32] {
        &self.storage
    }

    /// The keyed hash under which a token or code is stored (hex).
    pub fn hash_token(&self, token: &str) -> String {
        blake3::keyed_hash(&self.hash, token.as_bytes()).to_hex().to_string()
    }

    /// Sign access token claims.
    pub fn sign(&self, claims: &AccessClaims) -> String {
        let header = BASE64URL_NOPAD.encode(JWT_HEADER.as_bytes());
        let payload = BASE64URL_NOPAD.encode(&serde_json::to_vec(claims).expect("claims serialize"));
        let signing_input = format!("{header}.{payload}");
        let sig = self.mac(signing_input.as_bytes()).finalize().into_bytes();
        format!("{signing_input}.{}", BASE64URL_NOPAD.encode(&sig))
    }

    /// Verify an access token's header, signature, issuer, type and expiry (`now_s`: seconds since the epoch).
    pub fn verify(&self, token: &str, now_s: u64) -> Result<AccessClaims, TokenError> {
        let mut parts = token.split('.');
        let (Some(header), Some(payload), Some(sig), None) = (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(TokenError::Malformed);
        };
        let header_json = BASE64URL_NOPAD.decode(header.as_bytes()).map_err(|_| TokenError::Malformed)?;
        let header_value: serde_json::Value =
            serde_json::from_slice(&header_json).map_err(|_| TokenError::Malformed)?;
        if header_value.get("alg").and_then(|a| a.as_str()) != Some("HS256") {
            return Err(TokenError::Malformed);
        }
        let sig = BASE64URL_NOPAD.decode(sig.as_bytes()).map_err(|_| TokenError::Malformed)?;
        let signing_input_len = header.len() + 1 + payload.len();
        // `verify_slice` compares in constant time.
        self.mac(&token.as_bytes()[..signing_input_len]).verify_slice(&sig).map_err(|_| TokenError::BadSignature)?;
        let payload = BASE64URL_NOPAD.decode(payload.as_bytes()).map_err(|_| TokenError::Malformed)?;
        let claims: AccessClaims = serde_json::from_slice(&payload).map_err(|_| TokenError::Malformed)?;
        if claims.iss != ISSUER || claims.typ != ACCESS_TYPE {
            return Err(TokenError::Malformed);
        }
        if claims.exp <= now_s {
            return Err(TokenError::Expired);
        }
        Ok(claims)
    }

    fn mac(&self, data: &[u8]) -> Hmac<Sha256> {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.jwt).expect("HMAC takes any key length");
        mac.update(data);
        mac
    }
}

/// Claims of an access token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessClaims {
    pub iss: String,
    pub typ: String,
    /// `Id<User>`, or `Id<Admin>` when `adm` is true.
    pub sub: String,
    /// `Id<Session>`.
    pub sid: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub adm: bool,
    pub iat: u64,
    pub exp: u64,
}

impl AccessClaims {
    pub fn new(sub: String, sid: String, adm: bool, now_s: u64, ttl_s: u64) -> Self {
        AccessClaims { iss: ISSUER.into(), typ: ACCESS_TYPE.into(), sub, sid, adm, iat: now_s, exp: now_s + ttl_s }
    }
}

/// Why an access token was rejected. The messages are safe to show to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    Malformed,
    BadSignature,
    Expired,
    /// Signed and unexpired, but its session has ended (signed out, revoked or expired).
    SessionEnded,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TokenError::Malformed => "malformed access token",
            TokenError::BadSignature => "invalid access token signature",
            TokenError::Expired => "access token expired",
            TokenError::SessionEnded => "session has ended",
        })
    }
}

impl std::error::Error for TokenError {}

/// A random URL-safe token with 256 bits of entropy (43 characters).
pub fn random_token() -> String {
    let bytes: [u8; 32] = rand::random();
    BASE64URL_NOPAD.encode(&bytes)
}

/// A uniformly random numeric code of `digits` digits (leading zeros kept).
pub fn random_code(digits: u32) -> String {
    let modulus = 10u64.pow(digits);
    // Rejection sampling: no modulo bias.
    let zone = u64::MAX - (u64::MAX % modulus);
    loop {
        let n: u64 = rand::random();
        if n < zone {
            return format!("{:0width$}", n % modulus, width = digits as usize);
        }
    }
}

/// Constant-time string equality (for secrets of public length).
pub fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

/// SHA-256 PKCE challenge (`S256`) of a verifier.
pub fn pkce_challenge(verifier: &str) -> String {
    use sha2::Digest;
    BASE64URL_NOPAD.encode(&Sha256::digest(verifier.as_bytes()))
}

#[cfg(unix)]
fn write_new_secret(path: &Path, contents: &str) -> crate::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_new_secret(path: &Path, contents: &str) -> crate::Result<()> {
    std::fs::write(path, contents).with_context(|| format!("creating {}", path.display()))
}

#[cfg(unix)]
fn check_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        tracing::warn!("auth: {} is readable by other users; run `chmod 600` on it", path.display());
    }
}

#[cfg(not(unix))]
fn check_permissions(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let keys = Keys::from_secret(&[7u8; 64]);
        let claims = AccessClaims::new("u".into(), "s".into(), false, 1000, 900);
        let token = keys.sign(&claims);
        assert_eq!(keys.verify(&token, 1000).unwrap(), claims);
        assert_eq!(keys.verify(&token, 1900), Err(TokenError::Expired));
        // Another key, a tampered payload, a truncated token and alg=none are all rejected.
        assert_eq!(Keys::from_secret(&[8u8; 64]).verify(&token, 1000), Err(TokenError::BadSignature));
        let parts: Vec<&str> = token.split('.').collect();
        let forged_claims = AccessClaims::new("admin".into(), "s".into(), true, 1000, 900);
        let forged = BASE64URL_NOPAD.encode(&serde_json::to_vec(&forged_claims).unwrap());
        assert_eq!(keys.verify(&format!("{}.{forged}.{}", parts[0], parts[2]), 1000), Err(TokenError::BadSignature));
        assert_eq!(keys.verify(&format!("{}.{}", parts[0], parts[1]), 1000), Err(TokenError::Malformed));
        let none = BASE64URL_NOPAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        assert_eq!(keys.verify(&format!("{none}.{}.", parts[1]), 1000), Err(TokenError::Malformed));
    }

    #[test]
    fn codes_and_tokens() {
        for _ in 0..100 {
            let c = random_code(6);
            assert_eq!(c.len(), 6);
            assert!(c.chars().all(|ch| ch.is_ascii_digit()));
        }
        assert_eq!(random_token().len(), 43);
        assert_ne!(random_token(), random_token());
        let keys = Keys::from_secret(&[1u8; 64]);
        assert_eq!(keys.hash_token("a"), keys.hash_token("a"));
        assert_ne!(keys.hash_token("a"), Keys::from_secret(&[2u8; 64]).hash_token("a"));
        assert!(ct_eq("abc", "abc") && !ct_eq("abc", "abd") && !ct_eq("abc", "ab"));
        // BASE64URL(SHA256(verifier)), cross-checked with Python's hashlib.
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r7wW1gFWFOEjXk"), "bwWFMyPfdG9qreDhH2lmftFx_dFeLDalzcT1gb_j68g");
    }

    #[test]
    fn secret_file_is_created_once_with_0600() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("not-yet-created");
        let a = Keys::load_or_create(&dir).unwrap();
        let b = Keys::load_or_create(&dir).unwrap();
        assert_eq!(a.hash_token("x"), b.hash_token("x"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(SECRET_FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
