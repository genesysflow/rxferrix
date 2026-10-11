//! External JWTs: access tokens issued by another identity provider (Clerk, Auth0, Supabase, Firebase, any OIDC
//! issuer), verified against the provider's JWKS and mapped to a ferrix user.
//!
//! ```ignore
//! AuthConfig::default()
//!     .jwt(JwtProvider::new("https://clerk.example.com").any_audience())          // JWKS via OIDC discovery
//!     .jwt(JwtProvider::new("https://tenant.auth0.com/").audience("https://api.example.com"))
//!     .jwt(JwtProvider::new("https://<project>.supabase.co/auth/v1")
//!         .jwks_url("https://<project>.supabase.co/auth/v1/.well-known/jwks.json")
//!         .audience("authenticated"))
//! ```
//!
//! **Verification.** A token whose header says `HS256` is one of ferrix's own (`super::tokens`); anything else is
//! external. Its (unverified) `iss` selects the provider by exact match, so tokens of unknown issuers are refused
//! without any network traffic. The header's `alg` must be one the provider allows (RS256, ES256 and EdDSA by
//! default; never `none` or HMAC, so there is no algorithm confusion) and must match the key's type and curve. The
//! key is chosen by `kid` (or, without one, every key of the right type is tried). Then `exp` (required) and `nbf`
//! (if present) are checked with a leeway (60s by default), `aud` must contain a configured audience (unless the
//! provider was set to [`JwtProvider::any_audience`]), and the user id claim (`sub` by default) must be a non-empty
//! string of at most 255 characters.
//!
//! **JWKS cache.** Keys are fetched on first use (from `jwks_url`, or from the `jwks_uri` of the issuer's
//! `/.well-known/openid-configuration`, whose `issuer` must match) and cached for an hour. A token with an unknown
//! `kid` triggers a refetch (key rotation), at most once per [`JwtProvider::jwks_refetch_interval`] (30s by default)
//! per provider, so a flood of forged `kid`s can't make the server hammer the provider. Fetches time out after 5s,
//! follow no redirects, and refuse bodies over 1 MiB. When a refresh fails, the cached keys stay in use.
//!
//! **Users.** See `store::external_user`: the first time an `(issuer, subject)` pair is seen, a `_users` row and an
//! `_accounts` row (`provider` = the issuer, `providerAccountId` = the subject) are created, so `ctx.auth.user_id()`
//! is an ordinary `Id<User>` for every sign-in method and app tables can reference it.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use aws_lc_rs::signature::{self, RsaPublicKeyComponents, UnparsedPublicKey};
use data_encoding::BASE64URL_NOPAD;
use serde::Deserialize;
use serde_json::Value;

use super::tokens::TokenError;

const MAX_JWKS_BYTES: usize = 1 << 20;
const MAX_SUBJECT_LEN: usize = 255;

/// A JWS algorithm accepted for external tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JwtAlgorithm {
    /// RSASSA-PKCS1-v1_5 with SHA-256 (RSA keys of 2048 to 8192 bits).
    Rs256,
    /// ECDSA with P-256 and SHA-256.
    Es256,
    /// Ed25519.
    EdDsa,
}

impl JwtAlgorithm {
    fn from_header(alg: &str) -> Option<Self> {
        match alg {
            "RS256" => Some(Self::Rs256),
            "ES256" => Some(Self::Es256),
            "EdDSA" => Some(Self::EdDsa),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Rs256 => "RS256",
            Self::Es256 => "ES256",
            Self::EdDsa => "EdDSA",
        }
    }
}

/// Where a provider's keys come from.
#[derive(Debug, Clone)]
enum JwksSource {
    Url(String),
    /// `<issuer>/.well-known/openid-configuration` → `jwks_uri`.
    Discovery,
}

/// An external token issuer. Build with [`JwtProvider::new`] and add with `AuthConfig::jwt`.
#[derive(Debug, Clone)]
pub struct JwtProvider {
    pub(crate) issuer: String,
    jwks: JwksSource,
    audiences: Vec<String>,
    any_audience: bool,
    user_id_claim: String,
    leeway: Duration,
    algorithms: Vec<JwtAlgorithm>,
    refetch_interval: Duration,
    cache_ttl: Duration,
}

impl JwtProvider {
    /// Tokens whose `iss` is exactly `issuer` (e.g. `https://tenant.auth0.com/`, trailing slash included if the
    /// provider puts one there). Keys come from OIDC discovery unless [`JwtProvider::jwks_url`] is set. An audience
    /// must be configured ([`JwtProvider::audience`]) or explicitly waived ([`JwtProvider::any_audience`]).
    pub fn new(issuer: impl Into<String>) -> Self {
        JwtProvider {
            issuer: issuer.into(),
            jwks: JwksSource::Discovery,
            audiences: Vec::new(),
            any_audience: false,
            user_id_claim: "sub".into(),
            leeway: Duration::from_secs(60),
            algorithms: vec![JwtAlgorithm::Rs256, JwtAlgorithm::Es256, JwtAlgorithm::EdDsa],
            refetch_interval: Duration::from_secs(30),
            cache_ttl: Duration::from_secs(3600),
        }
    }

    /// Fetch keys from this JWKS URL instead of discovering it.
    pub fn jwks_url(mut self, url: impl Into<String>) -> Self {
        self.jwks = JwksSource::Url(url.into());
        self
    }

    /// Accept tokens whose `aud` (a string or an array) contains `audience`. Repeatable.
    pub fn audience(mut self, audience: impl Into<String>) -> Self {
        self.audiences.push(audience.into());
        self
    }

    /// Don't check `aud`. Only for providers whose tokens carry none (e.g. Clerk session tokens): any token this
    /// issuer signs, for any of its applications, is then accepted.
    pub fn any_audience(mut self) -> Self {
        self.any_audience = true;
        self
    }

    /// The claim holding the stable user id. Default: `sub`.
    pub fn user_id_claim(mut self, claim: impl Into<String>) -> Self {
        self.user_id_claim = claim.into();
        self
    }

    /// Clock skew tolerated for `exp` and `nbf`. Default: 60s.
    pub fn leeway(mut self, leeway: Duration) -> Self {
        self.leeway = leeway;
        self
    }

    /// Restrict the accepted algorithms. Default: RS256, ES256 and EdDSA.
    pub fn algorithms(mut self, algorithms: impl IntoIterator<Item = JwtAlgorithm>) -> Self {
        self.algorithms = algorithms.into_iter().collect();
        self
    }

    /// Minimum time between JWKS refetches triggered by an unknown `kid`. Default: 30s.
    pub fn jwks_refetch_interval(mut self, interval: Duration) -> Self {
        self.refetch_interval = interval;
        self
    }

    /// How long fetched keys are used before they are refreshed. Default: 1 hour.
    pub fn jwks_cache_ttl(mut self, ttl: Duration) -> Self {
        self.cache_ttl = ttl;
        self
    }

    pub(crate) fn validate(&self) -> crate::Result<()> {
        let issuer = url::Url::parse(&self.issuer).map_err(|e| anyhow::anyhow!("JWT issuer `{}`: {e}", self.issuer))?;
        if !matches!(issuer.scheme(), "https" | "http") {
            anyhow::bail!("JWT issuer `{}` must be an http(s) URL", self.issuer);
        }
        if self.audiences.is_empty() && !self.any_audience {
            anyhow::bail!(
                "JWT provider `{}`: set the expected audience with `.audience(..)`, or call `.any_audience()` if its \
                 tokens carry none",
                self.issuer
            );
        }
        if self.algorithms.is_empty() {
            anyhow::bail!("JWT provider `{}` allows no algorithm", self.issuer);
        }
        if let JwksSource::Url(u) = &self.jwks {
            url::Url::parse(u).map_err(|e| anyhow::anyhow!("JWKS URL `{u}`: {e}"))?;
        }
        Ok(())
    }
}

/// What an external token says about its user.
#[derive(Debug, Clone)]
pub(crate) struct ExternalClaims {
    pub issuer: String,
    pub subject: String,
    /// Seconds since the epoch (plus the leeway): when the connection's identity ends.
    pub expires_at: u64,
    pub email: Option<String>,
    pub email_verified: bool,
    pub name: Option<String>,
    pub picture: Option<String>,
}

// ---------------------------------------------------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum PublicKey {
    Rsa { n: Vec<u8>, e: Vec<u8> },
    /// Uncompressed SEC1 point (0x04 || x || y).
    P256(Vec<u8>),
    Ed25519(Vec<u8>),
}

impl PublicKey {
    fn fits(&self, alg: JwtAlgorithm) -> bool {
        matches!(
            (self, alg),
            (PublicKey::Rsa { .. }, JwtAlgorithm::Rs256)
                | (PublicKey::P256(_), JwtAlgorithm::Es256)
                | (PublicKey::Ed25519(_), JwtAlgorithm::EdDsa)
        )
    }

    fn verify(&self, msg: &[u8], sig: &[u8]) -> bool {
        match self {
            PublicKey::Rsa { n, e } => RsaPublicKeyComponents { n: n.as_slice(), e: e.as_slice() }
                .verify(&signature::RSA_PKCS1_2048_8192_SHA256, msg, sig)
                .is_ok(),
            PublicKey::P256(point) => {
                UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, point).verify(msg, sig).is_ok()
            }
            PublicKey::Ed25519(x) => UnparsedPublicKey::new(&signature::ED25519, x).verify(msg, sig).is_ok(),
        }
    }
}

#[derive(Debug, Clone)]
struct Key {
    kid: Option<String>,
    /// The JWK's `alg`, if it pins one.
    alg: Option<String>,
    key: PublicKey,
}

#[derive(Deserialize)]
struct Jwk {
    kty: String,
    kid: Option<String>,
    alg: Option<String>,
    #[serde(rename = "use")]
    use_: Option<String>,
    crv: Option<String>,
    n: Option<String>,
    e: Option<String>,
    x: Option<String>,
    y: Option<String>,
}

fn b64(s: &Option<String>) -> Option<Vec<u8>> {
    BASE64URL_NOPAD.decode(s.as_deref()?.trim_end_matches('=').as_bytes()).ok()
}

fn parse_jwk(jwk: Jwk) -> Option<Key> {
    if jwk.use_.as_deref().is_some_and(|u| u != "sig") {
        return None;
    }
    let key = match (jwk.kty.as_str(), jwk.crv.as_deref()) {
        ("RSA", _) => PublicKey::Rsa { n: b64(&jwk.n)?, e: b64(&jwk.e)? },
        ("EC", Some("P-256")) => {
            let (x, y) = (b64(&jwk.x)?, b64(&jwk.y)?);
            if x.len() != 32 || y.len() != 32 {
                return None;
            }
            let mut point = vec![0x04];
            point.extend_from_slice(&x);
            point.extend_from_slice(&y);
            PublicKey::P256(point)
        }
        ("OKP", Some("Ed25519")) => {
            let x = b64(&jwk.x)?;
            if x.len() != 32 {
                return None;
            }
            PublicKey::Ed25519(x)
        }
        _ => return None,
    };
    Some(Key { kid: jwk.kid, alg: jwk.alg, key })
}

fn parse_jwks(body: &[u8]) -> crate::Result<Vec<Key>> {
    #[derive(Deserialize)]
    struct Jwks {
        keys: Vec<Value>,
    }
    let jwks: Jwks = serde_json::from_slice(body).map_err(|e| anyhow::anyhow!("invalid JWKS: {e}"))?;
    // Keys we can't use (other types, encryption keys, malformed entries) are skipped, not fatal.
    Ok(jwks.keys.into_iter().filter_map(|k| serde_json::from_value(k).ok()).filter_map(parse_jwk).collect())
}

/// How many keys of a JWKS body ferrix can verify with (the live preset test in `config`).
#[cfg(test)]
pub(super) fn usable_keys(body: &[u8]) -> crate::Result<usize> {
    Ok(parse_jwks(body)?.len())
}

// ---------------------------------------------------------------------------------------------------------------------
// Verifier
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Default)]
struct KeyCache {
    keys: Vec<Key>,
    fetched_at: Option<Instant>,
    /// Last fetch attempt (successful or not), for rate limiting.
    attempted_at: Option<Instant>,
    /// The discovered JWKS URL.
    jwks_uri: Option<String>,
}

struct ProviderState {
    config: JwtProvider,
    cache: RwLock<Arc<KeyCache>>,
    /// Serializes fetches (single flight).
    fetching: tokio::sync::Mutex<()>,
}

/// The configured external issuers and their key caches. Part of `AuthService`.
pub(crate) struct JwtVerifier {
    providers: HashMap<String, ProviderState>,
    http: reqwest::Client,
}

impl JwtVerifier {
    pub fn new(providers: &[JwtProvider]) -> crate::Result<JwtVerifier> {
        let mut map = HashMap::new();
        for p in providers {
            p.validate()?;
            let state = ProviderState { config: p.clone(), cache: Default::default(), fetching: Default::default() };
            if map.insert(p.issuer.clone(), state).is_some() {
                anyhow::bail!("JWT issuer `{}` is configured twice", p.issuer);
            }
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("ferrix/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("building the HTTP client");
        Ok(JwtVerifier { providers: map, http })
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Verify an external token (`now_s`: seconds since the epoch).
    pub async fn verify(&self, token: &str, now_s: u64) -> Result<ExternalClaims, TokenError> {
        self.verify_with_claims(token, now_s).await.map(|(c, _)| c)
    }

    /// [`Self::verify`], also returning the whole verified claim set (e.g. for a `nonce`, `super::apple`).
    pub async fn verify_with_claims(&self, token: &str, now_s: u64) -> Result<(ExternalClaims, Value), TokenError> {
        let mut parts = token.split('.');
        let (Some(header), Some(payload), Some(sig), None) = (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(TokenError::Malformed);
        };
        let header_json: Value = decode_json(header)?;
        let claims: Value = decode_json(payload)?;
        let signature = BASE64URL_NOPAD.decode(sig.as_bytes()).map_err(|_| TokenError::Malformed)?;
        let alg = header_json.get("alg").and_then(Value::as_str).and_then(JwtAlgorithm::from_header);
        let Some(alg) = alg else { return Err(TokenError::Malformed) };
        let issuer = claims.get("iss").and_then(Value::as_str).ok_or(TokenError::UnknownIssuer)?;
        let provider = self.providers.get(issuer).ok_or(TokenError::UnknownIssuer)?;
        let cfg = &provider.config;
        if !cfg.algorithms.contains(&alg) {
            return Err(TokenError::Malformed);
        }
        let kid = header_json.get("kid").and_then(Value::as_str);
        let signing_input = &token.as_bytes()[..header.len() + 1 + payload.len()];

        // Keys from the cache; an unknown kid triggers one (rate-limited) refetch.
        let mut cache = self.keys(provider, false).await;
        if kid.is_some_and(|k| !cache.keys.iter().any(|key| key.kid.as_deref() == Some(k))) {
            cache = self.keys(provider, true).await;
        }
        let candidates: Vec<&Key> = cache
            .keys
            .iter()
            .filter(|k| kid.is_none() || k.kid.as_deref() == kid)
            .filter(|k| k.key.fits(alg) && k.alg.as_deref().is_none_or(|a| a == alg.name()))
            .collect();
        if candidates.is_empty() {
            return Err(TokenError::UnknownKey);
        }
        if !candidates.iter().any(|k| k.key.verify(signing_input, &signature)) {
            return Err(TokenError::BadSignature);
        }
        let checked = check_claims(cfg, &claims, now_s)?;
        Ok((checked, claims))
    }

    /// The provider's keys: cached, refreshed after the TTL, or refetched now (`unknown_kid`, rate limited).
    async fn keys(&self, provider: &ProviderState, unknown_kid: bool) -> Arc<KeyCache> {
        let current = provider.cache.read().unwrap_or_else(|e| e.into_inner()).clone();
        if !needs_fetch(&current, &provider.config, unknown_kid) {
            return current;
        }
        let _guard = provider.fetching.lock().await;
        // Another request may have fetched while we waited.
        let current = provider.cache.read().unwrap_or_else(|e| e.into_inner()).clone();
        if !needs_fetch(&current, &provider.config, unknown_kid) {
            return current;
        }
        let mut next = KeyCache {
            keys: current.keys.clone(),
            fetched_at: current.fetched_at,
            attempted_at: Some(Instant::now()),
            jwks_uri: current.jwks_uri.clone(),
        };
        match self.fetch(&provider.config, &mut next.jwks_uri).await {
            Ok(keys) => {
                tracing::debug!(issuer = provider.config.issuer, keys = keys.len(), "auth: fetched JWKS");
                next.keys = keys;
                next.fetched_at = Some(Instant::now());
            }
            Err(e) => tracing::warn!("auth: fetching the JWKS of `{}` failed: {e:#}", provider.config.issuer),
        }
        let next = Arc::new(next);
        *provider.cache.write().unwrap_or_else(|e| e.into_inner()) = next.clone();
        next
    }

    async fn fetch(&self, cfg: &JwtProvider, jwks_uri: &mut Option<String>) -> crate::Result<Vec<Key>> {
        let url = match (&cfg.jwks, jwks_uri.as_ref()) {
            (JwksSource::Url(u), _) => u.clone(),
            (JwksSource::Discovery, Some(u)) => u.clone(),
            (JwksSource::Discovery, None) => {
                #[derive(Deserialize)]
                struct Discovery {
                    issuer: String,
                    jwks_uri: String,
                }
                let url = format!("{}/.well-known/openid-configuration", cfg.issuer.trim_end_matches('/'));
                let doc: Discovery = serde_json::from_slice(&self.get(&url).await?)
                    .map_err(|e| anyhow::anyhow!("invalid discovery document: {e}"))?;
                if doc.issuer != cfg.issuer {
                    anyhow::bail!("discovery document names issuer `{}`, expected `{}`", doc.issuer, cfg.issuer);
                }
                *jwks_uri = Some(doc.jwks_uri.clone());
                doc.jwks_uri
            }
        };
        parse_jwks(&self.get(&url).await?)
    }

    async fn get(&self, url: &str) -> crate::Result<Vec<u8>> {
        let mut res = self.http.get(url).header(reqwest::header::ACCEPT, "application/json").send().await?.error_for_status()?;
        if res.content_length().is_some_and(|l| l as usize > MAX_JWKS_BYTES) {
            anyhow::bail!("{url}: response too large");
        }
        let mut body = Vec::new();
        while let Some(chunk) = res.chunk().await? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_JWKS_BYTES {
                anyhow::bail!("{url}: response too large");
            }
        }
        Ok(body)
    }
}

fn needs_fetch(cache: &KeyCache, cfg: &JwtProvider, unknown_kid: bool) -> bool {
    let recently_attempted = cache.attempted_at.is_some_and(|t| t.elapsed() < cfg.refetch_interval);
    match cache.fetched_at {
        None => !recently_attempted,
        Some(_) if unknown_kid => !recently_attempted,
        Some(at) => at.elapsed() >= cfg.cache_ttl && !recently_attempted,
    }
}

fn decode_json(part: &str) -> Result<Value, TokenError> {
    let bytes = BASE64URL_NOPAD.decode(part.as_bytes()).map_err(|_| TokenError::Malformed)?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|_| TokenError::Malformed)?;
    if v.is_object() { Ok(v) } else { Err(TokenError::Malformed) }
}

fn number(claims: &Value, name: &str) -> Result<Option<u64>, TokenError> {
    match claims.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_f64().filter(|n| n.is_finite() && *n >= 0.0).map(|n| Some(n as u64)).ok_or(TokenError::InvalidClaims),
    }
}

fn check_claims(cfg: &JwtProvider, claims: &Value, now_s: u64) -> Result<ExternalClaims, TokenError> {
    let leeway = cfg.leeway.as_secs();
    let exp = number(claims, "exp")?.ok_or(TokenError::InvalidClaims)?;
    if exp.saturating_add(leeway) <= now_s {
        return Err(TokenError::Expired);
    }
    if let Some(nbf) = number(claims, "nbf")?
        && nbf > now_s.saturating_add(leeway)
    {
        return Err(TokenError::NotYetValid);
    }
    if !cfg.any_audience {
        let ok = match claims.get("aud") {
            Some(Value::String(a)) => cfg.audiences.iter().any(|x| x == a),
            Some(Value::Array(list)) => list.iter().filter_map(Value::as_str).any(|a| cfg.audiences.iter().any(|x| x == a)),
            _ => false,
        };
        if !ok {
            return Err(TokenError::InvalidClaims);
        }
    }
    let subject = match claims.get(&cfg.user_id_claim) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => return Err(TokenError::InvalidClaims),
    };
    if subject.is_empty() || subject.chars().count() > MAX_SUBJECT_LEN {
        return Err(TokenError::InvalidClaims);
    }
    let string = |name: &str| claims.get(name).and_then(Value::as_str).map(str::to_string).filter(|s| !s.is_empty());
    let email_verified = match claims.get("email_verified") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => false,
    };
    Ok(ExternalClaims {
        issuer: cfg.issuer.clone(),
        subject,
        expires_at: exp.saturating_add(leeway),
        email: string("email"),
        email_verified,
        name: string("name"),
        picture: string("picture"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The JWKS Apple, Google and Discord published on 2026-10-11: every key is usable (RS256 with a kid; Discord's
    /// is 4096-bit, within `RSA_PKCS1_2048_8192_SHA256`).
    #[test]
    fn real_provider_jwks_parse() {
        for (name, body, count, bits) in [
            ("apple", &include_bytes!("testdata/jwks_apple.json")[..], 3, 2048),
            ("google", &include_bytes!("testdata/jwks_google.json")[..], 2, 2048),
            ("discord", &include_bytes!("testdata/jwks_discord.json")[..], 1, 4096),
        ] {
            let keys = parse_jwks(body).unwrap();
            assert_eq!(keys.len(), count, "{name}");
            for key in keys {
                assert!(key.kid.is_some() && key.alg.as_deref() == Some("RS256"), "{name}");
                let PublicKey::Rsa { n, e } = key.key else { panic!("{name}: not RSA") };
                assert_eq!((n.len() * 8, e), (bits, vec![1, 0, 1]), "{name}");
            }
        }
    }

    fn cfg() -> JwtProvider {
        JwtProvider::new("https://idp.test").audience("api")
    }

    #[test]
    fn claims_are_checked_with_leeway() {
        let c = cfg();
        let ok = json!({ "iss": "https://idp.test", "sub": "u1", "aud": ["x", "api"], "exp": 1000, "nbf": 900 });
        assert_eq!(check_claims(&c, &ok, 999).unwrap().subject, "u1");
        assert_eq!(check_claims(&c, &ok, 1059).unwrap().expires_at, 1060, "exp within the leeway");
        assert_eq!(check_claims(&c, &ok, 1060).unwrap_err(), TokenError::Expired);
        assert_eq!(check_claims(&c, &ok, 830).unwrap_err(), TokenError::NotYetValid);
        assert!(check_claims(&c, &ok, 840).is_ok(), "nbf within the leeway");
        let wrong_aud = json!({ "sub": "u1", "aud": "other", "exp": 1000 });
        assert_eq!(check_claims(&c, &wrong_aud, 999).unwrap_err(), TokenError::InvalidClaims);
        let no_aud = json!({ "sub": "u1", "exp": 1000 });
        assert_eq!(check_claims(&c, &no_aud, 999).unwrap_err(), TokenError::InvalidClaims);
        assert!(check_claims(&cfg().any_audience(), &no_aud, 999).is_ok());
        let no_exp = json!({ "sub": "u1", "aud": "api" });
        assert_eq!(check_claims(&c, &no_exp, 999).unwrap_err(), TokenError::InvalidClaims);
        let empty_sub = json!({ "sub": "", "aud": "api", "exp": 1000 });
        assert_eq!(check_claims(&c, &empty_sub, 999).unwrap_err(), TokenError::InvalidClaims);
        let custom = json!({ "sub": "x", "user_id": 42, "aud": "api", "exp": 1000 });
        assert_eq!(check_claims(&cfg().user_id_claim("user_id"), &custom, 999).unwrap().subject, "42");
    }

    #[test]
    fn providers_must_choose_an_audience_policy() {
        assert!(JwtProvider::new("https://idp.test").validate().is_err());
        assert!(JwtProvider::new("https://idp.test").any_audience().validate().is_ok());
        assert!(JwtProvider::new("not a url").audience("a").validate().is_err());
        assert!(JwtVerifier::new(&[cfg(), cfg()]).is_err(), "duplicate issuer");
    }

    #[test]
    fn jwks_parsing_skips_unusable_keys() {
        let body = json!({ "keys": [
            { "kty": "RSA", "kid": "r", "n": "AQAB", "e": "AQAB" },
            { "kty": "EC", "crv": "P-384", "x": "AA", "y": "AA" },
            { "kty": "OKP", "crv": "Ed25519", "x": BASE64URL_NOPAD.encode(&[1u8; 32]), "use": "sig" },
            { "kty": "OKP", "crv": "Ed25519", "x": BASE64URL_NOPAD.encode(&[1u8; 32]), "use": "enc" },
            { "kty": "oct", "k": "c2VjcmV0" },
            "garbage"
        ] });
        let keys = parse_jwks(body.to_string().as_bytes()).unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys[0].key.fits(JwtAlgorithm::Rs256) && keys[1].key.fits(JwtAlgorithm::EdDsa));
        assert!(!keys[1].key.fits(JwtAlgorithm::Es256));
    }
}
