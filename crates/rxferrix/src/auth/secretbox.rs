//! Authenticated encryption of secrets at rest (OAuth client secrets and the SMTP password in `_settings`).
//!
//! XChaCha20-Poly1305 with a key derived from `<data dir>/auth_secret.key` (see [`super::tokens::Keys`]), a fresh
//! random 192-bit nonce per value (random nonces are safe at this size), and the setting's key as associated data,
//! so a ciphertext copied into another setting's row fails to decrypt instead of being used there. The sealed form
//! is `v1.<base64url(nonce || ciphertext || tag)>`.
//!
//! The database alone therefore never reveals a secret: a backup or an export of `data.db` without
//! `auth_secret.key` holds ciphertext only (and losing the key file makes the stored secrets unrecoverable; they
//! must then be entered again).

use anyhow::{Context, bail};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use data_encoding::BASE64URL_NOPAD;

const PREFIX: &str = "v1.";
const NONCE_LEN: usize = 24;

/// Seals and opens secrets with the server's settings key.
pub(crate) struct SecretBox {
    cipher: XChaCha20Poly1305,
}

impl SecretBox {
    pub fn new(key: [u8; 32]) -> SecretBox {
        SecretBox { cipher: XChaCha20Poly1305::new(&key.into()) }
    }

    /// Encrypt `plaintext`, bound to `aad` (the setting's key).
    pub fn seal(&self, aad: &str, plaintext: &[u8]) -> String {
        let nonce_bytes: [u8; NONCE_LEN] = rand::random();
        let nonce = XNonce::from(nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(&nonce, Payload { msg: plaintext, aad: aad.as_bytes() })
            .expect("XChaCha20-Poly1305 encryption cannot fail for in-memory buffers");
        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        format!("{PREFIX}{}", BASE64URL_NOPAD.encode(&out))
    }

    /// Decrypt a value sealed for `aad`. Fails for another key (a replaced `auth_secret.key`), another `aad`, or any
    /// tampering.
    pub fn open(&self, aad: &str, sealed: &str) -> crate::Result<Vec<u8>> {
        let Some(body) = sealed.strip_prefix(PREFIX) else { bail!("unknown secret format") };
        let raw = BASE64URL_NOPAD.decode(body.as_bytes()).context("secret is not valid base64")?;
        if raw.len() < NONCE_LEN + 16 {
            bail!("secret is truncated");
        }
        let (nonce, ciphertext) = raw.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().expect("split at NONCE_LEN");
        self.cipher
            .decrypt(&XNonce::from(nonce), Payload { msg: ciphertext, aad: aad.as_bytes() })
            .map_err(|_| anyhow::anyhow!("secret could not be decrypted (wrong auth_secret.key, or tampered)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_and_tamper() {
        let b = SecretBox::new([3u8; 32]);
        let sealed = b.seal("oauth", b"s3cret");
        assert!(sealed.starts_with("v1.") && !sealed.contains("s3cret"));
        assert_ne!(sealed, b.seal("oauth", b"s3cret"), "fresh nonce every time");
        assert_eq!(b.open("oauth", &sealed).unwrap(), b"s3cret");
        assert!(b.open("smtp", &sealed).is_err(), "bound to its setting");
        assert!(SecretBox::new([4u8; 32]).open("oauth", &sealed).is_err(), "other key");
        let mut raw = BASE64URL_NOPAD.decode(&sealed.as_bytes()[3..]).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 1;
        assert!(b.open("oauth", &format!("v1.{}", BASE64URL_NOPAD.encode(&raw))).is_err(), "tampered");
        assert!(b.open("oauth", "v1.AAAA").is_err() && b.open("oauth", "plain").is_err());
    }
}
