//! Sign in with Apple: the client secret and the ID token.
//!
//! **Client secret.** Apple doesn't issue a static client secret. The app's private key (a `.p8` file: P-256, PKCS#8
//! PEM, created in the Apple developer account with a key id) signs a short ES256 JWT instead:
//! `{ iss: <team id>, sub: <client id (the Services ID)>, aud: "https://appleid.apple.com", iat, exp }` with
//! `{ alg: "ES256", kid: <key id> }`. It is generated on first use and cached; it lives [`CLIENT_SECRET_TTL`] (Apple
//! accepts up to 6 months, a short life limits what a leaked one is worth) and is replaced [`ROTATE_BEFORE`] before
//! it expires. The private key itself never leaves the server: it is only used to sign, it is redacted from `Debug`
//! output, and when configured from the dashboard it is stored encrypted in `_settings` like every other secret.
//!
//! **ID token.** Apple's userinfo is the ID token returned by the token endpoint (there is no userinfo endpoint). It is
//! verified with the JWKS code of external JWTs (`super::jwt`): RS256 only, against `https://appleid.apple.com/auth/keys`
//! (cached, refetched on an unknown `kid`), `iss` = `https://appleid.apple.com`, `aud` = the client id, `exp`, and the
//! `nonce` sent with the authorization request (it binds the token to the browser flow that started it). The ID token
//! that Apple also posts to the callback is ignored: only the one fetched over TLS with the code, the verifier and the
//! client secret is used.

use std::sync::{Mutex, OnceLock};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
use data_encoding::{BASE64, BASE64URL_NOPAD};
use serde_json::{Value, json};

use super::jwt::{ExternalClaims, JwtAlgorithm, JwtProvider, JwtVerifier};

/// Apple's origin: the issuer of its ID tokens, and the base of its endpoints.
pub const APPLE_ORIGIN: &str = "https://appleid.apple.com";
/// How long a generated client secret is valid.
pub const CLIENT_SECRET_TTL: u64 = 3600;
/// A cached client secret is replaced when it has less than this left.
pub const ROTATE_BEFORE: u64 = 600;

/// Parse an Apple private key: the `.p8` file's PEM (`-----BEGIN PRIVATE KEY-----`, PKCS#8, P-256). Literal `\n`
/// sequences (a key pasted into an environment variable on one line) are accepted.
pub(crate) fn parse_private_key(pem: &str) -> Result<EcdsaKeyPair, String> {
    // The armor (`-----BEGIN PRIVATE KEY-----`) is dropped wherever it is: a key pasted into a one-line field loses
    // its line breaks.
    let mut text = pem.replace("\\n", "\n");
    while let Some(start) = text.find("-----") {
        let Some(len) = text[start + 5..].find("-----") else { break };
        text.replace_range(start..start + 5 + len + 5, " ");
    }
    let body: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if body.is_empty() {
        return Err("the Apple private key is empty (paste the whole .p8 file)".into());
    }
    let der = BASE64.decode(body.as_bytes()).map_err(|_| "the Apple private key is not a PEM file (.p8)".to_string())?;
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &der)
        .map_err(|_| "the Apple private key is not a P-256 PKCS#8 key (the .p8 file from Apple)".to_string())
}

/// Apple's settings besides the client id and the private key (which are the provider's `client_id` and
/// `client_secret`), the client secret cache and the ID token verifier. Shared by clones of the provider.
pub(crate) struct AppleConfig {
    pub team_id: String,
    pub key_id: String,
    /// `https://appleid.apple.com`, or a mock in tests (`OAuthProvider::base_url`).
    pub base: String,
    /// `(client secret, its exp)`.
    secret: Mutex<Option<(String, u64)>>,
    verifier: OnceLock<Result<JwtVerifier, String>>,
}

impl std::fmt::Debug for AppleConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Apple").field("team_id", &self.team_id).field("key_id", &self.key_id).field("base", &self.base).finish()
    }
}

impl AppleConfig {
    pub fn new(team_id: String, key_id: String, base: String) -> AppleConfig {
        AppleConfig { team_id, key_id, base, secret: Mutex::new(None), verifier: OnceLock::new() }
    }

    pub fn authorization_endpoint(&self) -> String {
        format!("{}/auth/authorize", self.base.trim_end_matches('/'))
    }

    pub fn token_endpoint(&self) -> String {
        format!("{}/auth/token", self.base.trim_end_matches('/'))
    }

    /// The client secret JWT for `client_id`, signed with `private_key` (cached; see the module docs).
    pub fn client_secret(&self, client_id: &str, private_key: &str, now_s: u64) -> Result<String, String> {
        let mut cached = self.secret.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((secret, exp)) = cached.as_ref()
            && *exp > now_s + ROTATE_BEFORE
        {
            return Ok(secret.clone());
        }
        let key = parse_private_key(private_key)?;
        let exp = now_s + CLIENT_SECRET_TTL;
        let header = json!({ "alg": "ES256", "kid": self.key_id });
        let claims = json!({
            "iss": self.team_id,
            "iat": now_s,
            "exp": exp,
            "aud": self.base.trim_end_matches('/'),
            "sub": client_id,
        });
        let input = format!(
            "{}.{}",
            BASE64URL_NOPAD.encode(header.to_string().as_bytes()),
            BASE64URL_NOPAD.encode(claims.to_string().as_bytes())
        );
        let sig = key.sign(&SystemRandom::new(), input.as_bytes()).map_err(|_| "signing the client secret failed".to_string())?;
        let secret = format!("{input}.{}", BASE64URL_NOPAD.encode(sig.as_ref()));
        *cached = Some((secret.clone(), exp));
        tracing::debug!(exp, "auth: generated a Sign in with Apple client secret");
        Ok(secret)
    }

    /// Verify an ID token from the token endpoint: signature (Apple's JWKS, RS256), `iss`, `aud` = `client_id`,
    /// `exp`, and `nonce`. Returns the claims and the raw claim set.
    pub async fn verify_id_token(
        &self,
        client_id: &str,
        token: &str,
        nonce: &str,
        now_s: u64,
    ) -> Result<(ExternalClaims, Value), String> {
        let verifier = self
            .verifier
            .get_or_init(|| {
                let base = self.base.trim_end_matches('/').to_string();
                let provider = JwtProvider::new(base.clone())
                    .jwks_url(format!("{base}/auth/keys"))
                    .audience(client_id)
                    .algorithms([JwtAlgorithm::Rs256]);
                JwtVerifier::new(&[provider]).map_err(|e| format!("{e:#}"))
            })
            .as_ref()
            .map_err(Clone::clone)?;
        let (claims, raw) = verifier.verify_with_claims(token, now_s).await.map_err(|e| format!("the ID token is invalid: {e}"))?;
        // Constant-time: the nonce is a secret of the flow until it completes.
        let given = raw.get("nonce").and_then(Value::as_str).unwrap_or_default();
        if !super::tokens::ct_eq(given, nonce) {
            return Err("the ID token's nonce doesn't match this sign-in".into());
        }
        Ok((claims, raw))
    }
}

/// The name in the `user` field Apple posts on the first sign-in only: `{"name":{"firstName","lastName"},"email"}`.
/// The email there is ignored (the verified one comes from the ID token).
pub(crate) fn name_from_user_field(user: Option<&str>) -> Option<String> {
    let v: Value = serde_json::from_str(user?).ok()?;
    let name = v.get("name")?;
    let part = |k: &str| name.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let full = match (part("firstName"), part("lastName")) {
        (Some(f), Some(l)) => format!("{f} {l}"),
        (Some(n), None) | (None, Some(n)) => n.to_string(),
        (None, None) => return None,
    };
    Some(full.chars().filter(|c| !c.is_control()).take(200).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::signature::{KeyPair, UnparsedPublicKey};

    fn pem(key_der: &[u8]) -> String {
        let b64 = BASE64.encode(key_der);
        let lines: Vec<&str> = b64.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap()).collect();
        format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", lines.join("\n"))
    }

    #[test]
    fn client_secret_is_an_es256_jwt_signed_with_the_key_and_cached() {
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new()).unwrap();
        let key_pem = pem(pkcs8.as_ref());
        let public = parse_private_key(&key_pem).unwrap().public_key().as_ref().to_vec();
        let apple = AppleConfig::new("TEAM123456".into(), "KEY1234567".into(), APPLE_ORIGIN.into());
        let secret = apple.client_secret("com.example.web", &key_pem, 1_000_000).unwrap();
        let parts: Vec<&str> = secret.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: Value = serde_json::from_slice(&BASE64URL_NOPAD.decode(parts[0].as_bytes()).unwrap()).unwrap();
        let claims: Value = serde_json::from_slice(&BASE64URL_NOPAD.decode(parts[1].as_bytes()).unwrap()).unwrap();
        assert_eq!(header, json!({ "alg": "ES256", "kid": "KEY1234567" }));
        assert_eq!(
            claims,
            json!({ "iss": "TEAM123456", "iat": 1_000_000, "exp": 1_000_000 + CLIENT_SECRET_TTL, "aud": APPLE_ORIGIN, "sub": "com.example.web" })
        );
        let sig = BASE64URL_NOPAD.decode(parts[2].as_bytes()).unwrap();
        UnparsedPublicKey::new(&aws_lc_rs::signature::ECDSA_P256_SHA256_FIXED, &public)
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
            .expect("signed with the private key");
        // Cached while it has more than ROTATE_BEFORE left; then a new one.
        assert_eq!(apple.client_secret("com.example.web", &key_pem, 1_000_000 + 100).unwrap(), secret);
        let later = 1_000_000 + CLIENT_SECRET_TTL - ROTATE_BEFORE;
        assert_ne!(apple.client_secret("com.example.web", &key_pem, later).unwrap(), secret);
        // One-line keys with literal \n work; anything else is refused with a readable message.
        assert!(parse_private_key(&key_pem.replace('\n', "\\n")).is_ok());
        assert!(parse_private_key(&key_pem.replace('\n', "")).is_ok(), "pasted into a one-line field");
        assert!(parse_private_key("").unwrap_err().contains("empty"));
        assert!(parse_private_key("-----BEGIN PRIVATE KEY-----\n!!!\n-----END PRIVATE KEY-----").unwrap_err().contains("PEM"));
        assert!(parse_private_key(&pem(b"not a key")).unwrap_err().contains("P-256"));
    }

    #[test]
    fn names_from_the_user_field() {
        let user = r#"{"name":{"firstName":"Ann","lastName":"Lee"},"email":"x@privaterelay.appleid.com"}"#;
        assert_eq!(name_from_user_field(Some(user)).as_deref(), Some("Ann Lee"));
        assert_eq!(name_from_user_field(Some(r#"{"name":{"firstName":" Bo "}}"#)).as_deref(), Some("Bo"));
        assert_eq!(name_from_user_field(Some(r#"{"email":"a@b.c"}"#)), None);
        assert_eq!(name_from_user_field(Some("not json")), None);
        assert_eq!(name_from_user_field(None), None);
        let long = format!(r#"{{"name":{{"firstName":"{}"}}}}"#, "x".repeat(500));
        assert_eq!(name_from_user_field(Some(&long)).unwrap().len(), 200);
    }
}
