//! The Discord and Sign in with Apple presets against mock providers (local axum servers): Discord's verified-email
//! rule; Apple's `form_post` callback (state and cookie checks, `SameSite=None` on https), its ES256 client secret
//! (verified by the mock with the app's public key, cached across sign-ins), the name sent on the first sign-in only,
//! and ID token verification (signature against the mock's JWKS, issuer, audience, expiry, nonce); and both kinds
//! configured at runtime in `_settings` with the Apple private key encrypted at rest.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{self, EcdsaKeyPair, KeyPair};
use data_encoding::{BASE64, BASE64URL_NOPAD};
use rxferrix::auth::{Apple, AuthConfig, Discord};
use rxferrix::testing::TestApp;
use serde_json::{Value, json};

use crate::authkit::{Http, code_of, token};

const MASK: &str = "••••";

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn b64url_json(v: &Value) -> String {
    BASE64URL_NOPAD.encode(v.to_string().as_bytes())
}

fn decode_part(part: &str) -> Value {
    serde_json::from_slice(&BASE64URL_NOPAD.decode(part.as_bytes()).unwrap()).unwrap()
}

fn s256(verifier: &str) -> String {
    use sha2::Digest;
    BASE64URL_NOPAD.encode(&sha2::Sha256::digest(verifier.as_bytes()))
}

/// The `.p8` PEM of a fresh P-256 key, and its public point.
fn apple_key() -> (String, Vec<u8>) {
    let pkcs8 =
        EcdsaKeyPair::generate_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new()).unwrap();
    let public = EcdsaKeyPair::from_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref())
        .unwrap()
        .public_key()
        .as_ref()
        .to_vec();
    let b64 = BASE64.encode(pkcs8.as_ref());
    let lines: Vec<&str> = b64.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap()).collect();
    (format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", lines.join("\n")), public)
}

// ---------------------------------------------------------------------------------------------------------------------
// The mock provider: Discord's and Apple's endpoints on one local server
// ---------------------------------------------------------------------------------------------------------------------

/// How the mock Apple answers the token request.
#[derive(Clone, Copy, PartialEq, Debug)]
enum IdToken {
    Good,
    WrongNonce,
    WrongAudience,
    WrongIssuer,
    Expired,
    OtherKey,
}

struct Grant {
    challenge: String,
    redirect_uri: String,
    nonce: Option<String>,
    profile: Value,
}

struct MockState {
    base: String,
    grants: HashMap<String, Grant>,
    tokens: HashMap<String, Value>,
    /// Apple's ID token signing key, and one the JWKS doesn't have.
    rsa: aws_lc_rs::rsa::KeyPair,
    other_rsa: aws_lc_rs::rsa::KeyPair,
    /// The public key of the app's Apple private key, to check client secrets.
    app_public_key: Vec<u8>,
    client_secrets: Vec<String>,
    id_token: IdToken,
    token_requests: usize,
}

type Mock = Arc<Mutex<MockState>>;

fn sign_rs256(key: &aws_lc_rs::rsa::KeyPair, kid: &str, claims: &Value) -> String {
    let input = format!("{}.{}", b64url_json(&json!({ "alg": "RS256", "kid": kid })), b64url_json(claims));
    let mut sig = vec![0u8; key.public_modulus_len()];
    key.sign(&signature::RSA_PKCS1_SHA256, &SystemRandom::new(), input.as_bytes(), &mut sig).unwrap();
    format!("{input}.{}", BASE64URL_NOPAD.encode(&sig))
}

/// Checks the token request's code, PKCE verifier and redirect URI; returns the grant.
fn redeem(m: &mut MockState, form: &HashMap<String, String>) -> Option<Grant> {
    m.token_requests += 1;
    if form.get("grant_type").map(String::as_str) != Some("authorization_code") {
        return None;
    }
    let grant = form.get("code").and_then(|c| m.grants.remove(c))?;
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    (s256(&verifier) == grant.challenge && form.get("redirect_uri") == Some(&grant.redirect_uri)).then_some(grant)
}

/// The client secret must be an ES256 JWT signed with the app's key, for this team, key, client and audience.
fn check_client_secret(m: &MockState, secret: &str) -> Result<(), String> {
    let parts: Vec<&str> = secret.split('.').collect();
    if parts.len() != 3 {
        return Err("not a JWT".into());
    }
    let (header, claims) = (decode_part(parts[0]), decode_part(parts[1]));
    if header != json!({ "alg": "ES256", "kid": "KEYID12345" }) {
        return Err(format!("header {header}"));
    }
    let now = now_s();
    let ok = claims["iss"] == "TEAMID1234"
        && claims["sub"] == "com.example.web"
        && claims["aud"] == m.base.as_str()
        && claims["iat"].as_u64().is_some_and(|t| t <= now + 5)
        && claims["exp"].as_u64().is_some_and(|t| t > now && t <= now + 6 * 30 * 86400);
    if !ok {
        return Err(format!("claims {claims}"));
    }
    let sig = BASE64URL_NOPAD.decode(parts[2].as_bytes()).map_err(|e| e.to_string())?;
    signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, &m.app_public_key)
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
        .map_err(|_| "bad signature".to_string())
}

async fn start_mock(app_public_key: Vec<u8>) -> (String, Mock) {
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::{get, post};
    type Form = axum::Form<HashMap<String, String>>;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mock: Mock = Arc::new(Mutex::new(MockState {
        base: base.clone(),
        grants: HashMap::new(),
        tokens: HashMap::new(),
        rsa: aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap(),
        other_rsa: aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap(),
        app_public_key,
        client_secrets: Vec::new(),
        id_token: IdToken::Good,
        token_requests: 0,
    }));
    let router = axum::Router::new()
        // Discord
        .route(
            "/api/oauth2/token",
            post(|State(mock): State<Mock>, axum::Form(form): Form| async move {
                let mut m = mock.lock().unwrap();
                let client_ok = form.get("client_id").map(String::as_str) == Some("discord-client")
                    && form.get("client_secret").map(String::as_str) == Some("discord-secret");
                match redeem(&mut m, &form).filter(|_| client_ok) {
                    Some(grant) => {
                        let access = format!("dc-{}", m.tokens.len());
                        m.tokens.insert(access.clone(), grant.profile);
                        axum::Json(json!({ "access_token": access, "token_type": "Bearer", "scope": "identify email" }))
                    }
                    None => axum::Json(json!({ "error": "invalid_grant" })),
                }
            }),
        )
        .route(
            "/api/users/@me",
            get(|State(mock): State<Mock>, headers: HeaderMap| async move {
                let token = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
                match token.and_then(|t| mock.lock().unwrap().tokens.get(t).cloned()) {
                    Some(p) => (axum::http::StatusCode::OK, axum::Json(p)),
                    None => (axum::http::StatusCode::UNAUTHORIZED, axum::Json(json!({ "message": "401: Unauthorized" }))),
                }
            }),
        )
        // Apple
        .route(
            "/auth/keys",
            get(|State(mock): State<Mock>| async move {
                let m = mock.lock().unwrap();
                let p = m.rsa.public_key();
                axum::Json(json!({ "keys": [{ "kty": "RSA", "kid": "apple-1", "use": "sig", "alg": "RS256",
                    "n": BASE64URL_NOPAD.encode(p.modulus().big_endian_without_leading_zero()),
                    "e": BASE64URL_NOPAD.encode(p.exponent().big_endian_without_leading_zero()) }] }))
            }),
        )
        .route(
            "/auth/token",
            post(|State(mock): State<Mock>, axum::Form(form): Form| async move {
                let mut m = mock.lock().unwrap();
                let secret = form.get("client_secret").cloned().unwrap_or_default();
                if form.get("client_id").map(String::as_str) != Some("com.example.web") || check_client_secret(&m, &secret).is_err() {
                    m.token_requests += 1;
                    return axum::Json(json!({ "error": "invalid_client" }));
                }
                m.client_secrets.push(secret);
                let Some(grant) = redeem(&mut m, &form) else { return axum::Json(json!({ "error": "invalid_grant" })) };
                let now = now_s();
                let mode = m.id_token;
                let mut claims = json!({
                    "iss": m.base, "aud": "com.example.web", "exp": now + 600, "iat": now,
                    "nonce": grant.nonce, "auth_time": now, "nonce_supported": true,
                });
                for (k, v) in grant.profile.as_object().unwrap() {
                    claims[k] = v.clone();
                }
                match mode {
                    IdToken::WrongNonce => claims["nonce"] = json!("another flow's nonce"),
                    IdToken::WrongAudience => claims["aud"] = json!("com.other.app"),
                    IdToken::WrongIssuer => claims["iss"] = json!("https://evil.test"),
                    IdToken::Expired => claims["exp"] = json!(now - 3600),
                    IdToken::Good | IdToken::OtherKey => {}
                }
                let key = if mode == IdToken::OtherKey { &m.other_rsa } else { &m.rsa };
                let id_token = sign_rs256(key, "apple-1", &claims);
                axum::Json(json!({ "access_token": "apple-at", "token_type": "Bearer", "expires_in": 3600, "id_token": id_token }))
            }),
        )
        .with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (base, mock)
}

/// `GET /api/auth/oauth/<provider>`: the authorization URL's parameters and the state cookie.
async fn start_flow(http: &Http, provider: &str) -> (HashMap<String, String>, String) {
    let start = http
        .client
        .get(format!("{}/api/auth/oauth/{provider}", http.base))
        .query(&[("redirect_to", "http://app.test/after")])
        .send()
        .await
        .unwrap();
    assert_eq!(start.status(), 302);
    let cookie = start.headers()["set-cookie"].to_str().unwrap().to_string();
    let authorize = url::Url::parse(start.headers()["location"].to_str().unwrap()).unwrap();
    (authorize.query_pairs().into_owned().collect(), cookie)
}

fn grant(mock: &Mock, q: &HashMap<String, String>, profile: Value) -> String {
    let code = format!("code-{}", q["state"]);
    let g = Grant {
        challenge: q["code_challenge"].clone(),
        redirect_uri: q["redirect_uri"].clone(),
        nonce: q.get("nonce").cloned(),
        profile,
    };
    mock.lock().unwrap().grants.insert(code.clone(), g);
    code
}

fn fragment(location: &str) -> HashMap<String, String> {
    let url = url::Url::parse(location).unwrap();
    url::form_urlencoded::parse(url.fragment().unwrap_or_default().as_bytes()).into_owned().collect()
}

async fn me(http: &Http, f: &HashMap<String, String>) -> Value {
    http.ok("query", "_system/auth:me", json!({}), &f["rxferrix_access_token"]).await
}

// ---------------------------------------------------------------------------------------------------------------------
// Discord
// ---------------------------------------------------------------------------------------------------------------------

async fn discord_flow(http: &Http, mock: &Mock, profile: Value) -> HashMap<String, String> {
    let (q, cookie) = start_flow(http, "discord").await;
    assert_eq!((q["scope"].as_str(), q["code_challenge_method"].as_str()), ("identify email", "S256"));
    assert!(!q.contains_key("response_mode") && !q.contains_key("nonce"));
    assert!(cookie.contains("SameSite=Lax"), "{cookie}");
    let code = grant(mock, &q, profile);
    let back = http
        .client
        .get(&q["redirect_uri"])
        .query(&[("code", code.as_str()), ("state", q["state"].as_str())])
        .header("cookie", cookie.split(';').next().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(back.status(), 302);
    fragment(back.headers()["location"].to_str().unwrap())
}

#[tokio::test]
async fn discord_uses_only_verified_emails() {
    let (base, mock) = start_mock(Vec::new()).await;
    let auth = AuthConfig::password()
        .rate_limits(None)
        .redirect_url("http://app.test")
        .oauth(Discord::new("discord-client", "discord-secret").base_url(&base));
    let t = TestApp::with_auth(auth).await.unwrap();
    let http = Http::new(t.serve().await.unwrap());
    let (_, providers) = http.get("/api/auth/providers", None).await;
    assert_eq!(providers["oauth"], json!(["discord"]));

    let profile = json!({ "id": "80351110224678912", "username": "nelly", "global_name": "Nelly", "avatar": "8342729096ea3675442027381ff50dfe",
                          "email": "Nelly@Discord.com", "verified": true });
    let f = discord_flow(&http, &mock, profile.clone()).await;
    let user = me(&http, &f).await;
    assert_eq!((user["email"].as_str(), user["emailVerified"].as_bool()), (Some("nelly@discord.com"), Some(true)));
    assert_eq!(user["name"], "Nelly");
    assert_eq!(
        user["image"],
        "https://cdn.discordapp.com/avatars/80351110224678912/8342729096ea3675442027381ff50dfe.png"
    );
    assert_eq!(discord_flow(&http, &mock, profile).await["rxferrix_user_id"], f["rxferrix_user_id"], "same account");

    // An unverified Discord email is not used: a new user without an email, even when the address is taken.
    http.sign_up("taken@example.com", "correct horse").await;
    let unverified = json!({ "id": "42", "username": "sam", "global_name": null, "avatar": null, "email": "taken@example.com", "verified": false });
    let f = discord_flow(&http, &mock, unverified).await;
    let user = me(&http, &f).await;
    assert_eq!(
        (user["email"].clone(), user["emailVerified"].clone(), user["name"].clone()),
        (Value::Null, json!(false), json!("sam"))
    );
    assert_eq!(user["image"], Value::Null);

    // Discord redirects with a GET: its callback refuses POSTs.
    let r = http
        .client
        .post(format!("{}/api/auth/oauth/discord/callback", http.base))
        .form(&[("code", "x"), ("state", "y")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 405);
}

// ---------------------------------------------------------------------------------------------------------------------
// Apple
// ---------------------------------------------------------------------------------------------------------------------

/// Start an Apple flow and post the callback as Apple would; returns the response's status and location.
async fn apple_flow(http: &Http, mock: &Mock, profile: Value, user: Option<&str>, send_cookie: bool) -> (u16, String) {
    let (q, cookie) = start_flow(http, "apple").await;
    assert_eq!((q["response_mode"].as_str(), q["scope"].as_str()), ("form_post", "name email"));
    assert!(q["nonce"].len() >= 32 && q["code_challenge_method"] == "S256");
    assert!(q["redirect_uri"].starts_with("https://rxferrix.test/api/auth/oauth/apple/callback"));
    // The callback is https: Apple's cross-site POST must carry the cookie.
    assert!(cookie.contains("SameSite=None") && cookie.contains("Secure") && cookie.contains("HttpOnly"), "{cookie}");
    let code = grant(mock, &q, profile);
    let mut form = vec![("code", code.as_str()), ("state", q["state"].as_str())];
    if let Some(u) = user {
        form.push(("user", u));
    }
    let mut req = http.client.post(format!("{}/api/auth/oauth/apple/callback", http.base)).form(&form);
    if send_cookie {
        req = req.header("cookie", cookie.split(';').next().unwrap());
    }
    let back = req.send().await.unwrap();
    let status = back.status().as_u16();
    if status == 303 {
        let clear = back.headers()["set-cookie"].to_str().unwrap();
        assert!(clear.contains("Max-Age=0") && clear.contains("SameSite=None"), "{clear}");
    }
    (status, back.headers().get("location").map(|l| l.to_str().unwrap().to_string()).unwrap_or_default())
}

#[tokio::test]
async fn apple_form_post_client_secret_name_once_and_id_token_checks() {
    let (private_key, public_key) = apple_key();
    let (base, mock) = start_mock(public_key).await;
    let auth = AuthConfig::password()
        .rate_limits(None)
        .redirect_url("http://app.test")
        .public_url("https://rxferrix.test")
        .oauth(Apple::new("com.example.web", "TEAMID1234", "KEYID12345", private_key).base_url(&base));
    let t = TestApp::with_auth(auth).await.unwrap();
    let http = Http::new(t.serve().await.unwrap());
    let sub = json!({ "sub": "001234.abcdef.0987", "email": "ann@privaterelay.appleid.com", "email_verified": "true", "is_private_email": "true" });

    // First sign-in: Apple posts the name once (with an email that is ignored: the ID token's is used).
    let user_field = r#"{"name":{"firstName":"Ann","lastName":"Lee"},"email":"attacker@example.com"}"#;
    let (status, location) = apple_flow(&http, &mock, sub.clone(), Some(user_field), true).await;
    assert_eq!(status, 303, "a POST is answered with 303 See Other");
    assert!(location.starts_with("http://app.test/after#"), "{location}");
    let f = fragment(&location);
    let user = me(&http, &f).await;
    assert_eq!(user["email"], "ann@privaterelay.appleid.com");
    assert_eq!((user["emailVerified"].as_bool(), user["name"].as_str()), (Some(true), Some("Ann Lee")));

    // Later sign-ins carry no name: the same account, the name kept; a name posted again doesn't rename it.
    let (_, location) = apple_flow(&http, &mock, sub.clone(), None, true).await;
    let again = fragment(&location);
    assert_eq!(again["rxferrix_user_id"], f["rxferrix_user_id"]);
    assert_eq!(me(&http, &again).await["name"], "Ann Lee");
    let (_, location) = apple_flow(&http, &mock, sub.clone(), Some(r#"{"name":{"firstName":"Mallory"}}"#), true).await;
    assert_eq!(me(&http, &fragment(&location)).await["name"], "Ann Lee");
    // The client secret was generated once and reused (cached until shortly before it expires).
    {
        let m = mock.lock().unwrap();
        assert_eq!(m.client_secrets.len(), 3);
        assert!(m.client_secrets.iter().all(|s| *s == m.client_secrets[0]));
    }

    // Without the state cookie (the callback replayed in another browser): refused before the provider is called.
    let before = mock.lock().unwrap().token_requests;
    let (status, location) = apple_flow(&http, &mock, sub.clone(), None, false).await;
    assert_eq!((status, fragment(&location)["rxferrix_error"].as_str()), (303, "state_mismatch"));
    assert_eq!(mock.lock().unwrap().token_requests, before);
    // A forged state never reaches the provider either; an empty or malformed form is a plain 400.
    let r = http
        .client
        .post(format!("{}/api/auth/oauth/apple/callback", http.base))
        .form(&[("code", "x"), ("state", "forged")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = http
        .client
        .post(format!("{}/api/auth/oauth/apple/callback", http.base))
        .body("{}")
        .header("content-type", "application/json")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // ID tokens that fail any check sign nobody in.
    for mode in [IdToken::WrongNonce, IdToken::WrongAudience, IdToken::WrongIssuer, IdToken::Expired, IdToken::OtherKey]
    {
        mock.lock().unwrap().id_token = mode;
        let (status, location) = apple_flow(
            &http,
            &mock,
            json!({ "sub": format!("{mode:?}"), "email": "x@y.z", "email_verified": true }),
            None,
            true,
        )
        .await;
        let f = fragment(&location);
        assert_eq!((status, f.get("rxferrix_error").map(String::as_str)), (303, Some("oauth_failed")), "{mode:?}");
        assert!(!f.contains_key("rxferrix_access_token"), "{mode:?}");
    }
    mock.lock().unwrap().id_token = IdToken::Good;

    // A GET to the Apple callback (no form_post) works the same way, without a `user` field.
    let (q, cookie) = start_flow(&http, "apple").await;
    let code = grant(&mock, &q, sub);
    let back = http
        .client
        .get(format!("{}/api/auth/oauth/apple/callback", http.base))
        .query(&[
            ("code", code.as_str()),
            ("state", q["state"].as_str()),
            ("user", "{\"name\":{\"firstName\":\"Get\"}}"),
        ])
        .header("cookie", cookie.split(';').next().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(back.status(), 302);
    assert_eq!(fragment(back.headers()["location"].to_str().unwrap())["rxferrix_user_id"], f["rxferrix_user_id"]);
}

#[tokio::test]
async fn apple_over_plain_http_keeps_a_lax_cookie() {
    let (private_key, public_key) = apple_key();
    let (base, _mock) = start_mock(public_key).await;
    let auth = AuthConfig::password()
        .rate_limits(None)
        .redirect_url("http://app.test")
        .oauth(Apple::new("com.example.web", "TEAMID1234", "KEYID12345", private_key).base_url(&base));
    let t = TestApp::with_auth(auth).await.unwrap();
    let http = Http::new(t.serve().await.unwrap());
    let (_, cookie) = start_flow(&http, "apple").await;
    assert!(cookie.contains("SameSite=Lax") && !cookie.contains("Secure"), "SameSite=None needs Secure: {cookie}");
}

// ---------------------------------------------------------------------------------------------------------------------
// Runtime settings
// ---------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn discord_and_apple_in_runtime_settings() {
    let t = TestApp::with_auth(AuthConfig::password().rate_limits(None)).await.unwrap();
    let app = t.app().clone();
    let http = Http::new(t.serve().await.unwrap());
    app.auth().create_admin(&app, "admin@example.com", "admin password".into()).await.unwrap();
    let admin = token(&http.admin_sign_in("admin@example.com", "admin password").await, "accessToken");
    let (private_key, _) = apple_key();
    let apple = |key: Option<&str>, team: Option<&str>| {
        json!({ "id": "apple", "kind": "apple", "clientId": "com.example.web", "clientSecret": key, "teamId": team,
                "keyId": "KEYID12345", "enabled": true })
    };
    let discord =
        json!({ "id": "discord", "kind": "discord", "clientId": "dc", "clientSecret": "dc-secret", "enabled": true });

    // Apple needs a team id, a key id and a private key that loads.
    for bad in [
        apple(Some(&private_key), None),
        apple(Some("not a key"), Some("TEAMID1234")),
        apple(None, Some("TEAMID1234")),
        apple(Some(&private_key), Some("TEAM ID!")),
    ] {
        let (s, b) = http.call("action", "_system/settings:update", json!({ "oauth": [bad] }), Some(&admin)).await;
        assert_eq!((s, code_of(&b)), (400, "invalid_settings"), "{b}");
    }
    let view = http
        .ok(
            "action",
            "_system/settings:update",
            json!({ "oauth": [apple(Some(&private_key), Some("TEAMID1234")), discord] }),
            &admin,
        )
        .await;
    assert_eq!(view["effective"]["oauthProviders"], json!(["apple", "discord"]));
    let stored = &view["oauth"][0];
    assert_eq!(
        (stored["kind"].as_str(), stored["teamId"].as_str(), stored["keyId"].as_str()),
        (Some("apple"), Some("TEAMID1234"), Some("KEYID12345"))
    );
    assert_eq!(stored["clientSecret"], MASK);
    assert!(!view.to_string().contains("PRIVATE KEY"));
    let (_, providers) = http.get("/api/auth/providers", None).await;
    assert_eq!(providers["oauth"], json!(["apple", "discord"]));
    // Starting a flow uses the stored settings (Apple's real endpoint, form_post).
    let r = http
        .client
        .get(format!("{}/api/auth/oauth/apple?redirect_to=http://localhost:5173/", http.base))
        .send()
        .await
        .unwrap();
    let location = r.headers()["location"].to_str().unwrap();
    assert!(
        location.starts_with("https://appleid.apple.com/auth/authorize?")
            && location.contains("response_mode=form_post"),
        "{location}"
    );
    // Saving again with the mask keeps the key.
    let view = http
        .ok("action", "_system/settings:update", json!({ "oauth": [apple(Some(MASK), Some("TEAMID1234"))] }), &admin)
        .await;
    assert_eq!(view["effective"]["oauthProviders"], json!(["apple"]));

    // The private key is encrypted at rest: not in the database files, nor in the execution log.
    t.app().logs().flush().await.unwrap();
    let body: String = private_key.lines().filter(|l| !l.starts_with("-----")).collect::<Vec<_>>().join("");
    let needle = &body[..40];
    for file in ["data.db", "data.db-wal", "aux.db", "aux.db-wal"] {
        let bytes = std::fs::read(t.dir().join(file)).unwrap_or_default();
        assert!(!bytes.windows(needle.len()).any(|w| w == needle.as_bytes()), "the private key is in {file}");
    }
}
