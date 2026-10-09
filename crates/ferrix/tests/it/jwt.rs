//! External JWTs: RS256 / ES256 / EdDSA tokens from a mock identity provider (keys generated here, JWKS served by a
//! local server with OIDC discovery), over HTTP Bearer and WebSocket `Authenticate`; claim checks; key rotation and
//! the refetch rate limit; user provisioning; and live revocation of a provisioned user.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{self, EcdsaKeyPair, Ed25519KeyPair, KeyPair};
use data_encoding::BASE64URL_NOPAD;
use ferrix::FunctionRef;
use ferrix::auth::{AuthConfig, JwtProvider};
use ferrix::testing::TestApp;
use serde_json::{Value, json};

use crate::authkit::{Http, Ws, code_of};

mod app {
    use ferrix::prelude::*;

    /// `[user id, email]` of the caller (through `_users`).
    #[query]
    pub async fn jwt_whoami(ctx: &QueryCtx) -> Result<Option<Vec<Option<String>>>> {
        Ok(ctx.auth.user(&ctx.db).await?.map(|u| vec![Some(u.id.to_string()), u.value.email]))
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// A mock identity provider
// ---------------------------------------------------------------------------------------------------------------------

enum Signer {
    Rsa(aws_lc_rs::rsa::KeyPair),
    Ec(EcdsaKeyPair),
    Ed(Ed25519KeyPair),
}

struct TestKey {
    kid: String,
    signer: Signer,
}

impl TestKey {
    fn rsa(kid: &str) -> TestKey {
        TestKey { kid: kid.into(), signer: Signer::Rsa(aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap()) }
    }
    fn ec(kid: &str) -> TestKey {
        TestKey { kid: kid.into(), signer: Signer::Ec(EcdsaKeyPair::generate(&signature::ECDSA_P256_SHA256_FIXED_SIGNING).unwrap()) }
    }
    fn ed(kid: &str) -> TestKey {
        TestKey { kid: kid.into(), signer: Signer::Ed(Ed25519KeyPair::generate().unwrap()) }
    }

    fn alg(&self) -> &'static str {
        match self.signer {
            Signer::Rsa(_) => "RS256",
            Signer::Ec(_) => "ES256",
            Signer::Ed(_) => "EdDSA",
        }
    }

    fn jwk(&self) -> Value {
        let b = |bytes: &[u8]| BASE64URL_NOPAD.encode(bytes);
        match &self.signer {
            Signer::Rsa(k) => {
                let p = k.public_key();
                json!({ "kty": "RSA", "kid": self.kid, "use": "sig", "alg": "RS256",
                        "n": b(p.modulus().big_endian_without_leading_zero()),
                        "e": b(p.exponent().big_endian_without_leading_zero()) })
            }
            Signer::Ec(k) => {
                let point = k.public_key().as_ref();
                json!({ "kty": "EC", "kid": self.kid, "crv": "P-256", "x": b(&point[1..33]), "y": b(&point[33..65]) })
            }
            Signer::Ed(k) => json!({ "kty": "OKP", "kid": self.kid, "crv": "Ed25519", "x": b(k.public_key().as_ref()) }),
        }
    }

    /// A compact JWS with this key (header `alg`, `kid` unless `kid` is overridden with `Some(None)`).
    fn sign(&self, claims: &Value) -> String {
        self.sign_with(json!({ "alg": self.alg(), "typ": "JWT", "kid": self.kid }), claims)
    }

    fn sign_with(&self, header: Value, claims: &Value) -> String {
        let input = format!(
            "{}.{}",
            BASE64URL_NOPAD.encode(header.to_string().as_bytes()),
            BASE64URL_NOPAD.encode(claims.to_string().as_bytes())
        );
        let sig = match &self.signer {
            Signer::Rsa(k) => {
                let mut sig = vec![0u8; k.public_modulus_len()];
                k.sign(&signature::RSA_PKCS1_SHA256, &SystemRandom::new(), input.as_bytes(), &mut sig).unwrap();
                sig
            }
            Signer::Ec(k) => k.sign(&SystemRandom::new(), input.as_bytes()).unwrap().as_ref().to_vec(),
            Signer::Ed(k) => k.sign(input.as_bytes()).as_ref().to_vec(),
        };
        format!("{input}.{}", BASE64URL_NOPAD.encode(&sig))
    }
}

#[derive(Default)]
struct IdpState {
    issuer: String,
    jwks: Vec<Value>,
    jwks_fetches: usize,
    discovery_fetches: usize,
}

/// Serves `/.well-known/openid-configuration` and `/jwks`.
async fn mock_idp() -> (String, Arc<Mutex<IdpState>>) {
    use axum::routing::get;
    let state = Arc::new(Mutex::new(IdpState::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let issuer = format!("http://{addr}");
    state.lock().unwrap().issuer = issuer.clone();
    let (s1, s2) = (state.clone(), state.clone());
    let router = axum::Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(move || {
                let s = s1.clone();
                async move {
                    let mut s = s.lock().unwrap();
                    s.discovery_fetches += 1;
                    axum::Json(json!({ "issuer": s.issuer, "jwks_uri": format!("{}/jwks", s.issuer) }))
                }
            }),
        )
        .route(
            "/jwks",
            get(move || {
                let s = s2.clone();
                async move {
                    let mut s = s.lock().unwrap();
                    s.jwks_fetches += 1;
                    axum::Json(json!({ "keys": s.jwks }))
                }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (issuer, state)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn claims(issuer: &str, sub: &str) -> Value {
    json!({ "iss": issuer, "sub": sub, "aud": "my-api", "iat": now(), "exp": now() + 300 })
}

async fn setup(provider: impl FnOnce(&str) -> JwtProvider) -> (TestApp, Http, SocketAddr, String, Arc<Mutex<IdpState>>) {
    let (issuer, idp) = mock_idp().await;
    let t = TestApp::with_auth(AuthConfig::password().rate_limits(None).jwt(provider(&issuer))).await.unwrap();
    let addr = t.serve().await.unwrap();
    (t, Http::new(addr), addr, issuer, idp)
}

async fn whoami(http: &Http, token: &str) -> (u16, Value) {
    http.call("query", &app::jwt_whoami::path(), json!({}), Some(token)).await
}

// ---------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn rs256_es256_and_eddsa_tokens_map_to_provisioned_users() {
    let (t, http, addr, issuer, idp) = setup(|iss| JwtProvider::new(iss).audience("my-api")).await;
    let keys = [TestKey::rsa("r1"), TestKey::ec("e1"), TestKey::ed("d1")];
    idp.lock().unwrap().jwks = keys.iter().map(TestKey::jwk).collect();

    let mut users = Vec::new();
    for (i, key) in keys.iter().enumerate() {
        let mut c = claims(&issuer, &format!("user-{i}"));
        c["email"] = json!(format!("ext{i}@example.com"));
        c["email_verified"] = json!(true);
        let token = key.sign(&c);
        let (s, b) = whoami(&http, &token).await;
        assert_eq!(s, 200, "{}: {b}", key.alg());
        let (id, email) = (b["value"][0].as_str().unwrap().to_string(), b["value"][1].clone());
        assert_eq!(email, json!(format!("ext{i}@example.com")));
        // The same subject is the same user, whatever key signed it.
        let again = keys[(i + 1) % 3].sign(&claims(&issuer, &format!("user-{i}")));
        assert_eq!(whoami(&http, &again).await.1["value"][0], json!(id));
        users.push(id);
    }
    assert_eq!(idp.lock().unwrap().discovery_fetches, 1, "discovery once");
    assert_eq!(idp.lock().unwrap().jwks_fetches, 1, "keys cached");

    // Linked as `_accounts { provider: issuer, providerAccountId: sub }`.
    let app = t.app().clone();
    app.auth().create_admin(&app, "admin@example.com", "admin password".into()).await.unwrap();
    let admin = http.admin_sign_in("admin@example.com", "admin password").await;
    let admin = admin["accessToken"].as_str().unwrap();
    let page = json!({ "table": "_accounts", "paginationOpts": { "numItems": 10, "cursor": null } });
    let accounts = http.ok("query", "_system/dashboard:documents", page, admin).await;
    let accounts = accounts["page"].as_array().unwrap();
    assert_eq!(accounts.len(), 3);
    assert!(accounts.iter().all(|a| a["provider"] == json!(issuer)));
    assert!(accounts.iter().any(|a| a["providerAccountId"] == "user-0" && a["user"] == json!(users[0])));

    // Over the WebSocket: Authenticate with the external token; the identity is the provisioned user.
    let mut ws = Ws::connect(addr).await;
    ws.authenticate(Some(&keys[2].sign(&claims(&issuer, "user-2")))).await;
    let (_, v) = ws.subscribe(&app::jwt_whoami::path(), json!({})).await;
    assert_eq!(v[0], json!(users[2]));

    // Deleting the user ends that socket at once (external tokens have no session row; the user is watched).
    let at = Instant::now();
    http.ok("mutation", "_system/dashboard:delete_user", json!({ "user": users[2] }), admin).await;
    let msg = loop {
        let m = ws.recv().await;
        if m["type"] != "Transition" {
            break m;
        }
    };
    assert_eq!((msg["type"].as_str(), msg["error"].as_str()), (Some("AuthError"), Some("session has ended")));
    assert!(at.elapsed() < Duration::from_millis(100), "{:?}", at.elapsed());
}

#[tokio::test]
async fn claims_algorithms_and_signatures_are_checked() {
    let (_t, http, addr, issuer, idp) = setup(|iss| JwtProvider::new(iss).audience("my-api").leeway(Duration::from_secs(5))).await;
    let key = TestKey::ec("k");
    let other = TestKey::ec("k");
    idp.lock().unwrap().jwks = vec![key.jwk()];
    let base = claims(&issuer, "alice");
    assert_eq!(whoami(&http, &key.sign(&base)).await.0, 200);

    let mut cases: Vec<(&str, String, &str)> = Vec::new();
    let mut c = base.clone();
    c["aud"] = json!("someone-else");
    cases.push(("wrong audience", key.sign(&c), "invalid access token claims"));
    let mut c = base.clone();
    c["aud"] = json!(["a", "b"]);
    cases.push(("audience list without ours", key.sign(&c), "invalid access token claims"));
    let mut c = base.clone();
    c["exp"] = json!(now() - 10);
    cases.push(("expired past the leeway", key.sign(&c), "access token expired"));
    let mut c = base.clone();
    c["nbf"] = json!(now() + 60);
    cases.push(("not yet valid", key.sign(&c), "access token not yet valid"));
    let mut c = base.clone();
    c.as_object_mut().unwrap().remove("exp");
    cases.push(("no exp", key.sign(&c), "invalid access token claims"));
    let mut c = base.clone();
    c["sub"] = json!("");
    cases.push(("empty subject", key.sign(&c), "invalid access token claims"));
    let mut c = base.clone();
    c["iss"] = json!("https://evil.example.com");
    cases.push(("unknown issuer", key.sign(&c), "unknown token issuer"));
    cases.push(("signed by another key with the same kid", other.sign(&base), "invalid access token signature"));
    // Algorithm confusion: claiming RS256 with an EC key, `none`, and HS256 (which is routed to ferrix's own verifier).
    cases.push(("alg mismatch", key.sign_with(json!({ "alg": "RS256", "kid": "k" }), &base), "unknown token signing key"));
    let none = format!(
        "{}.{}.",
        BASE64URL_NOPAD.encode(br#"{"alg":"none"}"#),
        BASE64URL_NOPAD.encode(base.to_string().as_bytes())
    );
    cases.push(("alg none", none, "malformed access token"));
    cases.push(("alg HS256", key.sign_with(json!({ "alg": "HS256", "kid": "k" }), &base), "invalid access token signature"));
    let mut tampered = key.sign(&base);
    let mut c = base.clone();
    c["sub"] = json!("mallory");
    let parts: Vec<&str> = tampered.split('.').collect();
    tampered = format!("{}.{}.{}", parts[0], BASE64URL_NOPAD.encode(c.to_string().as_bytes()), parts[2]);
    cases.push(("tampered payload", tampered, "invalid access token signature"));

    for (name, token, error) in &cases {
        let (s, b) = whoami(&http, token).await;
        assert_eq!((s, code_of(&b)), (401, "invalid_token"), "{name}: {b}");
        assert_eq!(b["errorMessage"], *error, "{name}");
        // The same token on a socket is an AuthError.
        let mut ws = Ws::connect(addr).await;
        let reply = ws.try_authenticate(token).await;
        assert_eq!((reply["type"].as_str(), reply["error"].as_str()), (Some("AuthError"), Some(*error)), "{name}");
    }

    // Inside the leeway, an expired token is still accepted.
    let mut c = base.clone();
    c["exp"] = json!(now() - 2);
    assert_eq!(whoami(&http, &key.sign(&c)).await.0, 200);
}

#[tokio::test]
async fn key_rotation_refetches_at_most_once_per_interval() {
    let (_t, http, _addr, issuer, idp) = setup(|iss| {
        JwtProvider::new(iss).jwks_url(format!("{iss}/jwks")).any_audience().jwks_refetch_interval(Duration::from_millis(300))
    })
    .await;
    let old = TestKey::ed("2024");
    idp.lock().unwrap().jwks = vec![old.jwk()];
    let mut c = claims(&issuer, "bob");
    c.as_object_mut().unwrap().remove("aud");
    assert_eq!(whoami(&http, &old.sign(&c)).await.0, 200, "no aud needed with any_audience");
    assert_eq!(idp.lock().unwrap().discovery_fetches, 0, "explicit jwks_url: no discovery");

    // Rotation: the provider publishes a new key; a token with the new kid triggers a refetch (once the refetch
    // interval since the last fetch has passed).
    tokio::time::sleep(Duration::from_millis(350)).await;
    let new = TestKey::rsa("2025");
    idp.lock().unwrap().jwks = vec![old.jwk(), new.jwk()];
    assert_eq!(whoami(&http, &new.sign(&c)).await.0, 200);
    assert_eq!(idp.lock().unwrap().jwks_fetches, 2);

    // Unknown kids (forged or not yet published) can't make us hammer the provider.
    let rogue = TestKey::ed("rogue");
    for _ in 0..5 {
        let (s, b) = whoami(&http, &rogue.sign(&c)).await;
        assert_eq!((s, b["errorMessage"].as_str()), (401, Some("unknown token signing key")));
    }
    assert_eq!(idp.lock().unwrap().jwks_fetches, 2, "no refetch within the interval");
    tokio::time::sleep(Duration::from_millis(350)).await;
    idp.lock().unwrap().jwks.push(rogue.jwk());
    assert_eq!(whoami(&http, &rogue.sign(&c)).await.0, 200, "refetched after the interval");
    assert_eq!(idp.lock().unwrap().jwks_fetches, 3);
}

#[tokio::test]
async fn provisioning_never_links_or_steals_an_email() {
    let (_t, http, _addr, issuer, idp) = setup(|iss| JwtProvider::new(iss).audience("my-api").user_id_claim("uid")).await;
    let key = TestKey::ed("k");
    idp.lock().unwrap().jwks = vec![key.jwk()];
    let pw = http.sign_up("taken@example.com", "correct horse").await;

    let mut c = claims(&issuer, "ignored");
    c["uid"] = json!("ext-1");
    c["email"] = json!("taken@example.com");
    c["email_verified"] = json!(true);
    let (s, b) = whoami(&http, &key.sign(&c)).await;
    assert_eq!(s, 200, "{b}");
    assert_ne!(b["value"][0], pw["userId"], "a separate user, not the password account");
    assert_eq!(b["value"][1], Value::Null, "the email is already taken, so it isn't copied");

    let mut c = claims(&issuer, "ignored");
    c["uid"] = json!("ext-2");
    c["email"] = json!("unverified@example.com");
    assert_eq!(whoami(&http, &key.sign(&c)).await.1["value"][1], Value::Null, "unverified emails are not copied");

    let mut c = claims(&issuer, "x");
    c.as_object_mut().unwrap().remove("uid");
    let (s, b) = whoami(&http, &key.sign(&c)).await;
    assert_eq!((s, b["errorMessage"].as_str()), (401, Some("invalid access token claims")), "the configured claim is required");
}

#[tokio::test]
async fn misconfigured_providers_fail_at_startup() {
    let err = TestApp::with_auth(AuthConfig::default().jwt(JwtProvider::new("https://idp.example.com"))).await.err().unwrap();
    assert!(err.to_string().contains("audience"), "{err}");
}
