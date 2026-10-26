//! Gating access to an app: the function guard (`AppOptions::guard`), the sign-up check (`AuthConfig::before_sign_up`,
//! `allowed_emails` / `allowed_domains`), and deleting users or ending sessions from app code (`ctx.users`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rxferrix::auth::{AuthConfig, Identity, MemoryMailer, SignUpAttempt, SignUpMethod};
use rxferrix::guard::Guard;
use rxferrix::runtime::AppOptions;
use rxferrix::testing::TestApp;
use rxferrix::{FunctionRef, UserError};
use serde_json::{Value, json};

use crate::authkit::{Http, Ws, code_of, token};

mod app {
    use rxferrix::QueryDb;
    use rxferrix::auth::{DeletedUser, Session, User};
    use rxferrix::prelude::*;

    /// Users the guard lets in.
    #[table(name = "access_grants")]
    #[index(by_user = [user])]
    pub struct AccessGrant {
        pub user: Id<User>,
    }

    #[table(name = "access_items")]
    pub struct AccessItem {
        pub text: String,
    }

    pub async fn granted(db: &QueryDb, user: Id<User>) -> Result<bool> {
        Ok(db
            .query::<AccessGrant>()
            .with_index(AccessGrant::by_user, |q| q.eq(AccessGrant::user, user))
            .first()
            .await?
            .is_some())
    }

    /// Exempt from the guard: whether the caller is in.
    #[query]
    pub async fn status(ctx: &QueryCtx) -> Result<bool> {
        let user = ctx.auth.require_user()?;
        granted(&ctx.db, user).await
    }

    #[query]
    pub async fn items(ctx: &QueryCtx) -> Result<Vec<String>> {
        Ok(ctx.db.query::<AccessItem>().collect().await?.into_iter().map(|i| i.value.text).collect())
    }

    #[mutation]
    pub async fn add_item(ctx: &MutationCtx, text: String) -> Result<()> {
        ctx.db.insert(AccessItem { text }).await?;
        Ok(())
    }

    #[action]
    pub async fn shout(_ctx: &ActionCtx, text: String) -> Result<String> {
        Ok(text.to_uppercase())
    }

    /// Exempt from the guard, and runs the guarded `items` from server code (which the guard doesn't check).
    #[action]
    pub async fn peek(ctx: &ActionCtx) -> Result<Vec<String>> {
        ctx.run_query::<items>(()).await
    }

    #[internal_query]
    pub async fn count_items(ctx: &QueryCtx) -> Result<usize> {
        Ok(ctx.db.query::<AccessItem>().collect().await?.len())
    }

    #[internal_mutation]
    pub async fn grant(ctx: &MutationCtx, user: Id<User>) -> Result<()> {
        ctx.db.insert(AccessGrant { user }).await?;
        Ok(())
    }

    #[internal_mutation]
    pub async fn remove_user(ctx: &MutationCtx, user: Id<User>) -> Result<Option<DeletedUser>> {
        ctx.users.delete(user).await
    }

    #[internal_mutation]
    pub async fn sign_out_everywhere(ctx: &MutationCtx, user: Id<User>) -> Result<u32> {
        ctx.users.revoke_sessions(user).await
    }

    #[query]
    pub async fn my_session(ctx: &QueryCtx) -> Result<Option<Id<Session>>> {
        Ok(ctx.auth.session_id())
    }

    /// Ends one session, then fails: the session must survive the rollback.
    #[internal_mutation]
    pub async fn revoke_then_fail(ctx: &MutationCtx, session: Id<Session>) -> Result<()> {
        assert!(ctx.users.revoke_session(session).await?);
        Err(UserError::new("nope", "rolled back").into())
    }
}

fn access_guard() -> Guard {
    Guard::new(|ctx, function| async move {
        if function.is::<app::status>() || function.is::<app::peek>() {
            return Ok(());
        }
        let user = ctx.auth.require_user()?;
        if !app::granted(&ctx.db, user).await? {
            return Err(UserError::new("access_required", "Ask for an invite first").into());
        }
        Ok(())
    })
}

async fn guarded_app() -> TestApp {
    TestApp::with_options(AppOptions {
        dev: true,
        auth: AuthConfig::password().rate_limits(None),
        guard: Some(access_guard()),
        ..Default::default()
    })
    .await
    .unwrap()
}

fn code(e: &rxferrix::Error) -> String {
    e.downcast_ref::<UserError>().map(|u| u.code.clone()).unwrap_or_else(|| format!("not a UserError: {e:#}"))
}

#[tokio::test]
async fn guard_refuses_client_calls_until_access_is_granted() {
    let t = guarded_app().await;
    let ann = t.create_user("ann@example.com", None).await.unwrap();
    let as_ann = t.as_user(ann);

    // Each kind of public function is refused, and a refused mutation writes nothing.
    assert_eq!(code(&as_ann.query::<app::items>(()).await.unwrap_err()), "access_required");
    assert_eq!(code(&as_ann.mutation::<app::add_item>(("x".to_string(),)).await.unwrap_err()), "access_required");
    assert_eq!(code(&as_ann.action::<app::shout>(("x".to_string(),)).await.unwrap_err()), "access_required");
    assert_eq!(t.query::<app::count_items>(()).await.unwrap(), 0);
    // Anonymous callers get what the guard says (here: sign in first).
    let anonymous = t.query_as::<app::items>(&Identity::anonymous(), ()).await.unwrap_err();
    assert_eq!(code(&anonymous), "unauthenticated");
    // The exempt function, internal functions and server code run as before.
    assert!(!as_ann.query::<app::status>(()).await.unwrap());
    t.mutation::<app::add_item>(("from the server".to_string(),)).await.unwrap();
    assert_eq!(t.query::<app::items>(()).await.unwrap(), vec!["from the server"]);

    t.mutation::<app::grant>((ann,)).await.unwrap();
    assert!(as_ann.query::<app::status>(()).await.unwrap());
    as_ann.mutation::<app::add_item>(("mine".to_string(),)).await.unwrap();
    assert_eq!(as_ann.query::<app::items>(()).await.unwrap(), vec!["from the server", "mine"]);
    assert_eq!(as_ann.action::<app::shout>(("hi".to_string(),)).await.unwrap(), "HI");
}

#[tokio::test]
async fn guard_runs_for_clients_and_a_subscription_follows_the_access_data() {
    let t = guarded_app().await;
    let addr = t.serve().await.unwrap();
    let http = Http::new(addr);
    let tokens = http.sign_up("bo@example.com", "correct horse").await;
    let access = token(&tokens, "accessToken");
    let bo = token(&tokens, "userId").parse().unwrap();

    let (s, b) = http.call("query", &app::items::path(), json!({}), Some(&access)).await;
    assert_eq!((s, code_of(&b)), (400, "access_required"), "{b}");
    let (s, b) = http.call("mutation", &app::add_item::path(), json!({ "text": "x" }), Some(&access)).await;
    assert_eq!(code_of(&b), "access_required", "{s} {b}");
    // System functions are never guarded.
    assert_eq!(http.ok("query", "_system/auth:me", json!({}), &access).await["email"], "bo@example.com");

    // A refused subscription turns into results once access is granted: the guard's reads are the query's.
    let mut ws = Ws::connect(addr).await;
    ws.authenticate(Some(&access)).await;
    ws.send(json!({ "type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 1, "modifications": [
        { "type": "Add", "queryId": 0, "udfPath": app::items::path(), "args": {} }
    ] }))
    .await;
    let first = next_modification(&mut ws, 0).await;
    assert_eq!((first["type"].as_str(), code_of(&first)), (Some("QueryFailed"), "access_required"), "{first}");
    t.mutation::<app::grant>((bo,)).await.unwrap();
    let granted = next_modification(&mut ws, 0).await;
    assert_eq!((granted["type"].as_str(), &granted["value"]), (Some("QueryUpdated"), &json!([])), "{granted}");
    http.ok("mutation", &app::add_item::path(), json!({ "text": "hello" }), &access).await;
    assert_eq!(next_modification(&mut ws, 0).await["value"], json!(["hello"]));
}

/// A query run by server code (unguarded) never answers a client's call from the query cache.
#[tokio::test]
async fn unguarded_runs_are_not_served_to_clients() {
    let t = guarded_app().await;
    let http = Http::new(t.serve().await.unwrap());
    let tokens = http.sign_up("cy@example.com", "correct horse").await;
    let access = token(&tokens, "accessToken");
    t.mutation::<app::add_item>(("secret".to_string(),)).await.unwrap();

    // `peek` is exempt and runs `items` as Cy from server code: allowed, and must not be cached for Cy's own calls.
    assert_eq!(http.ok("action", &app::peek::path(), json!({}), &access).await, json!(["secret"]));
    let (_, b) = http.call("query", &app::items::path(), json!({}), Some(&access)).await;
    assert_eq!(code_of(&b), "access_required", "{b}");
}

async fn next_modification(ws: &mut Ws, query_id: u64) -> Value {
    loop {
        let t = ws.recv().await;
        let found = t["modifications"].as_array().and_then(|m| m.iter().find(|m| m["queryId"] == query_id).cloned());
        if let Some(m) = found {
            return m;
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Sign-up check
// ---------------------------------------------------------------------------------------------------------------------

fn mail_config(mailer: &Arc<MemoryMailer>) -> AuthConfig {
    AuthConfig::password().magic_link().mailer(mailer.clone()).rate_limits(None).redirect_url("http://app.test")
}

async fn wait_for_email(mailer: &MemoryMailer, to: &str, count: usize) -> rxferrix::auth::Email {
    for _ in 0..200 {
        let mine: Vec<_> = mailer.emails().into_iter().filter(|e| e.to == to).collect();
        if mine.len() >= count {
            return mine[count - 1].clone();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no email #{count} to {to}");
}

fn email_token(body: &str, key: &str) -> String {
    let marker = format!("#{key}=");
    let at = body.find(&marker).unwrap_or_else(|| panic!("no {key} link in {body}")) + marker.len();
    body[at..].split_whitespace().next().unwrap().to_string()
}

/// A `before_sign_up` hook that wants `invite=yes` in the app URL, and records what it saw.
fn invite_only(config: AuthConfig, seen: Arc<Mutex<Vec<SignUpAttempt>>>) -> AuthConfig {
    config.before_sign_up(move |_app, attempt| {
        let seen = seen.clone();
        async move {
            let invited = attempt.redirect_to.as_deref().is_some_and(|r| r.contains("invite=yes"));
            seen.lock().unwrap().push(attempt);
            if !invited {
                return Err(UserError::new("invite_required", "You need an invite to sign up").into());
            }
            Ok(())
        }
    })
}

#[tokio::test]
async fn before_sign_up_gates_password_sign_ups() {
    let mailer = MemoryMailer::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let t = TestApp::with_auth(invite_only(mail_config(&mailer), seen.clone())).await.unwrap();
    let http = Http::new(t.serve().await.unwrap());
    let sign_up = |email: &str, redirect: Option<&str>| {
        let body = json!({ "email": email, "password": "correct horse", "name": "Ann", "redirectTo": redirect });
        let http = &http;
        async move { http.post("/api/auth/signup", None, body).await }
    };

    let (s, b) = sign_up("ann@example.com", None).await;
    assert_eq!((s, code_of(&b)), (403, "invite_required"), "{b}");
    assert_eq!(b["errorMessage"], "You need an invite to sign up");
    let (s, b) = sign_up("ann@example.com", Some("http://app.test/join?invite=no")).await;
    assert_eq!((s, code_of(&b)), (403, "invite_required"));
    assert_eq!(
        http.post("/api/auth/signin", None, json!({ "email": "ann@example.com", "password": "correct horse" })).await.0,
        401
    );

    let (s, b) = sign_up("ann@example.com", Some("http://app.test/join?invite=yes")).await;
    assert_eq!(s, 200, "{b}");
    {
        let seen = seen.lock().unwrap();
        let last = seen.last().unwrap();
        assert_eq!(last.email.as_deref(), Some("ann@example.com"));
        assert!(!last.email_verified);
        assert_eq!(last.method, SignUpMethod::Password);
        assert_eq!(last.name.as_deref(), Some("Ann"));
        assert_eq!(last.redirect_to.as_deref(), Some("http://app.test/join?invite=yes"));
        // No redirect given: the default app URL.
        assert_eq!(seen[0].redirect_to.as_deref(), Some("http://app.test/"));
    }
    // The verification email links to the sign-up's app URL.
    let email = wait_for_email(&mailer, "ann@example.com", 1).await;
    assert!(email.body.contains("http://app.test/join?invite=yes#"), "{}", email.body);

    // An address that is taken is answered as before, without asking the hook; sign-ins don't ask it either.
    let calls = seen.lock().unwrap().len();
    let (s, b) = sign_up("ann@example.com", Some("http://app.test/?invite=yes")).await;
    assert_eq!((s, code_of(&b)), (400, "email_in_use"));
    http.sign_in("ann@example.com", "correct horse").await;
    assert_eq!(seen.lock().unwrap().len(), calls);
    // A redirect outside the allowlist is refused before anything else.
    let (s, b) = sign_up("evil@example.com", Some("https://evil.test/?invite=yes")).await;
    assert_eq!((s, code_of(&b)), (400, "invalid_redirect"));
}

#[tokio::test]
async fn before_sign_up_gates_magic_link_sign_ups_when_the_link_is_used() {
    let mailer = MemoryMailer::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let t = TestApp::with_auth(invite_only(mail_config(&mailer), seen.clone())).await.unwrap();
    let http = Http::new(t.serve().await.unwrap());
    let request = |email: &str, redirect: &str| {
        let body = json!({ "email": email, "redirectTo": redirect });
        let http = &http;
        async move { http.post("/api/auth/magic/request", None, body).await }
    };

    // Requesting reveals nothing; using the link runs the hook, which refuses, and no account is made.
    assert_eq!(request("dee@example.com", "http://app.test/").await.0, 200);
    let link = email_token(&wait_for_email(&mailer, "dee@example.com", 1).await.body, "rxferrix_magic_token");
    let (s, b) = http.post("/api/auth/magic/verify", None, json!({ "token": link })).await;
    assert_eq!((s, code_of(&b)), (403, "invite_required"), "{b}");
    assert!(t.create_user("dee@example.com", None).await.is_ok(), "no account was created");
    let dee_calls = seen.lock().unwrap().len();

    // With the invite in the app URL, the link signs the new user up; the hook saw a proven address.
    assert_eq!(request("eve@example.com", "http://app.test/?invite=yes").await.0, 200);
    let link = email_token(&wait_for_email(&mailer, "eve@example.com", 1).await.body, "rxferrix_magic_token");
    let (s, b) = http.post("/api/auth/magic/verify", None, json!({ "token": link })).await;
    assert_eq!(s, 200, "{b}");
    {
        let seen = seen.lock().unwrap();
        let last = seen.last().unwrap();
        assert_eq!((last.email.as_deref(), last.email_verified), (Some("eve@example.com"), true));
        assert_eq!(last.method, SignUpMethod::MagicLink);
        assert_eq!(last.redirect_to.as_deref(), Some("http://app.test/?invite=yes"));
    }
    // An existing account signs in by magic link without the hook (Dee's account now exists too).
    assert_eq!(request("dee@example.com", "http://app.test/").await.0, 200);
    let link = email_token(&wait_for_email(&mailer, "dee@example.com", 2).await.body, "rxferrix_magic_token");
    assert_eq!(http.post("/api/auth/magic/verify", None, json!({ "token": link })).await.0, 200);
    assert_eq!(seen.lock().unwrap().len(), dee_calls + 1, "only Eve's sign-up asked the hook");
}

#[tokio::test]
async fn allowed_domains_and_emails_limit_sign_ups_to_proven_addresses() {
    let mailer = MemoryMailer::new();
    let auth = mail_config(&mailer).allowed_domains(["@Team.test"]).allowed_emails(["Guest@Example.com"]);
    let t = TestApp::with_auth(auth).await.unwrap();
    let http = Http::new(t.serve().await.unwrap());
    let sign_up = |email: &str| {
        let body = json!({ "email": email, "password": "correct horse" });
        let http = &http;
        async move { http.post("/api/auth/signup", None, body).await }
    };

    let (s, b) = sign_up("someone@example.com").await;
    assert_eq!((s, code_of(&b)), (403, "signup_not_allowed"), "{b}");
    let (s, b) = sign_up("ann@sub.team.test").await;
    assert_eq!((s, code_of(&b)), (403, "signup_not_allowed"), "subdomains are not included");

    // Allowed addresses sign up, but must prove the address before signing in.
    let (s, b) = sign_up("ann@team.test").await;
    assert_eq!((s, &b["verificationRequired"]), (200, &json!(true)), "{b}");
    let (s, b) =
        http.post("/api/auth/signin", None, json!({ "email": "ann@team.test", "password": "correct horse" })).await;
    assert_eq!((s, code_of(&b)), (403, "email_not_verified"));
    let link = email_token(&wait_for_email(&mailer, "ann@team.test", 1).await.body, "rxferrix_verify_token");
    assert_eq!(http.post("/api/auth/verify-email/confirm", None, json!({ "token": link })).await.0, 200);
    http.sign_in("ann@team.test", "correct horse").await;
    assert_eq!(sign_up("guest@example.com").await.0, 200);

    // No magic link goes to an address that couldn't sign up.
    assert_eq!(http.post("/api/auth/magic/request", None, json!({ "email": "out@example.com" })).await.0, 200);
    assert_eq!(http.post("/api/auth/magic/request", None, json!({ "email": "bo@team.test" })).await.0, 200);
    wait_for_email(&mailer, "bo@team.test", 1).await;
    assert!(mailer.emails().iter().all(|e| e.to != "out@example.com"));
}

#[tokio::test]
async fn a_failing_hook_is_a_server_error_and_creates_nothing() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let auth = AuthConfig::password().rate_limits(None).before_sign_up(move |_app, _attempt| {
        counted.fetch_add(1, Ordering::SeqCst);
        async { Err(anyhow::anyhow!("the invites service is down")) }
    });
    let t = TestApp::with_auth(auth).await.unwrap();
    let http = Http::new(t.serve().await.unwrap());
    let (s, b) =
        http.post("/api/auth/signup", None, json!({ "email": "fay@example.com", "password": "correct horse" })).await;
    assert_eq!((s, code_of(&b)), (500, "server_error"), "{b}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(t.create_user("fay@example.com", None).await.is_ok(), "no account was created");
}

// ---------------------------------------------------------------------------------------------------------------------
// Users from app code
// ---------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn app_code_deletes_users_and_ends_sessions() {
    let t = TestApp::with_auth(AuthConfig::password().rate_limits(None)).await.unwrap();
    let http = Http::new(t.serve().await.unwrap());
    let refresh = |tokens: &Value| {
        let body = json!({ "refreshToken": token(tokens, "refreshToken") });
        let http = &http;
        async move { http.post("/api/auth/refresh", None, body).await.0 }
    };

    // Ending all of a user's sessions: refresh tokens stop working, the account stays.
    let first = http.sign_up("gus@example.com", "correct horse").await;
    let second = http.sign_in("gus@example.com", "correct horse").await;
    let gus = token(&first, "userId").parse().unwrap();
    assert_eq!(t.mutation::<app::sign_out_everywhere>((gus,)).await.unwrap(), 2);
    assert_eq!((refresh(&first).await, refresh(&second).await), (401, 401));
    let again = http.sign_in("gus@example.com", "correct horse").await;

    // Ending one session is undone with the mutation that did it.
    let session = http.ok("query", &app::my_session::path(), json!({}), &token(&again, "accessToken")).await;
    let session = session.as_str().unwrap().parse().unwrap();
    let err = t.mutation::<app::revoke_then_fail>((session,)).await.unwrap_err();
    assert_eq!(code(&err), "nope");
    assert_eq!(refresh(&again).await, 200);

    // Deleting a user removes them with their sessions and password.
    let deleted = t.mutation::<app::remove_user>((gus,)).await.unwrap().expect("deleted");
    assert_eq!((deleted.sessions, deleted.credentials), (1, 1));
    assert_eq!(
        http.post("/api/auth/signin", None, json!({ "email": "gus@example.com", "password": "correct horse" })).await.0,
        401
    );
    assert!(t.mutation::<app::remove_user>((gus,)).await.unwrap().is_none(), "already gone");

    // The same from outside a mutation, through `app.auth()`.
    let tokens = http.sign_up("hal@example.com", "correct horse").await;
    let hal = token(&tokens, "userId").parse().unwrap();
    assert_eq!(t.app().auth().revoke_sessions(t.app(), hal).await.unwrap(), 1);
    assert!(t.app().auth().delete_user(t.app(), hal).await.unwrap().is_some());
    assert!(t.create_user("hal@example.com", None).await.is_ok());
}
