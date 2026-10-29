//! Phase 10 developer-experience APIs: dev mode (`ctx.is_dev()`, dev-only functions, the composable dev mailer),
//! `#[object]` types embedding `Doc<T>`, `PaginationResult::{map_async, try_map, try_map_concurrent}`,
//! `Query::count`, and `TestApp::{create_user, as_user}`.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use rxferrix::auth::{AuthConfig, DevLogMailer, Email, Mailer, User};
use rxferrix::db::readset::ReadSet;
use rxferrix::testing::TestApp;
use rxferrix::writelog::CommitEntry;
use rxferrix::{AppOptions, FunctionRef, Id, Identity, PaginationOpts};
use serde_json::{Value, json};

pub mod notes {
    use rxferrix::auth::User;
    use rxferrix::prelude::*;

    #[mutation]
    pub async fn remove(ctx: &MutationCtx, id: Id<Note>) -> Result<()> {
        ctx.db.delete(id).await
    }

    /// The user error helpers of the prelude: `ensure_user!`, `.or_user(..)` and `bail_user!`.
    #[mutation]
    pub async fn rename_note(ctx: &MutationCtx, id: Id<Note>, body: String) -> Result<()> {
        ensure_user!(!body.trim().is_empty(), "empty", "a note needs some text");
        let note = ctx.db.get(id).await?.or_user("not_found", "this note no longer exists")?;
        if note.body == body {
            bail_user!("unchanged", "the note already says {body:?}");
        }
        ctx.db.patch(id, json!({ "body": body })).await
    }

    #[table(name = "dx_notes")]
    #[index(by_board = [board])]
    #[derive(PartialEq)]
    pub struct Note {
        pub board: String,
        pub body: String,
        pub author: Option<Id<User>>,
    }

    /// A note as a list shows it: the document itself (flattened: `_id`, `_creationTime`, `board`, `body`, ...)
    /// plus derived fields.
    #[object]
    #[derive(Clone, Debug, PartialEq)]
    pub struct NoteView {
        #[serde(flatten)]
        pub note: Doc<Note>,
        /// The body's length in characters.
        pub body_length: u32,
        pub author_email: Option<String>,
    }

    #[object]
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum Mode {
        Sequential,
        TryMap,
        Concurrent,
        Failing,
    }

    #[object]
    #[derive(Clone, Debug, PartialEq)]
    pub struct DevFlags {
        pub query: bool,
        pub mutation: bool,
        pub action: bool,
    }

    #[mutation]
    pub async fn add(ctx: &MutationCtx, board: String, body: String) -> Result<Id<Note>> {
        let author = ctx.auth.user_id();
        ctx.db.insert(Note { board, body, author }).await
    }

    /// A page of a board's notes as views, built with `map_async` / `try_map` / `try_map_concurrent`.
    #[query]
    pub async fn views(
        ctx: &QueryCtx,
        board: String,
        mode: Mode,
        pagination_opts: PaginationOpts,
    ) -> Result<PaginationResult<NoteView>> {
        let page = ctx
            .db
            .query::<Note>()
            .with_index(Note::by_board, |q| q.eq(Note::board, board))
            .order(Order::Desc)
            .paginate(pagination_opts)
            .await?;
        // A cache shared across the page: behind a Mutex that is never held across an await.
        let emails: std::sync::Mutex<std::collections::HashMap<Id<User>, Option<String>>> = Default::default();
        let email_of = async |user: Option<Id<User>>| -> Result<Option<String>> {
            let Some(user) = user else { return Ok(None) };
            if let Some(hit) = emails.lock().unwrap().get(&user) {
                return Ok(hit.clone());
            }
            let email = ctx.db.get(user).await?.and_then(|u| u.value.email);
            emails.lock().unwrap().insert(user, email.clone());
            Ok(email)
        };
        let email_of = &email_of;
        match mode {
            Mode::Sequential => Ok(page
                .map_async(|note| async move {
                    let author_email = email_of(note.author).await.ok().flatten();
                    NoteView { body_length: note.body.chars().count() as u32, author_email, note }
                })
                .await),
            Mode::TryMap | Mode::Failing => {
                page.try_map(|note| async move {
                    if mode == Mode::Failing && note.body == "boom" {
                        return Err(UserError::new("boom", "a note said boom").into());
                    }
                    let author_email = email_of(note.author).await?;
                    Ok::<_, Error>(NoteView { body_length: note.body.chars().count() as u32, author_email, note })
                })
                .await
            }
            Mode::Concurrent => {
                page.try_map_concurrent(4, |note| async move {
                    let author_email = match note.author {
                        Some(a) => ctx.db.get(a).await?.and_then(|u| u.value.email),
                        None => None,
                    };
                    Ok::<_, Error>(NoteView { body_length: note.body.chars().count() as u32, author_email, note })
                })
                .await
            }
        }
    }

    /// How many notes a board has (optionally only those created after `after`, or whose body contains `needle`).
    #[query]
    pub async fn count(ctx: &QueryCtx, board: String, after: Option<f64>, needle: Option<String>) -> Result<u64> {
        let q = ctx.db.query::<Note>();
        let q = match after {
            Some(t) => q.with_index(Note::by_board, |r| r.eq(Note::board, board).gt(Note::_creation_time, t)),
            None => q.with_index(Note::by_board, |r| r.eq(Note::board, board)),
        };
        match needle {
            Some(n) => q.filter(move |d| d.body.contains(&n)).count().await,
            None => q.count().await,
        }
    }

    /// Who is calling (for `TestApp::as_user`).
    #[query]
    pub async fn whoami(ctx: &QueryCtx) -> Result<Option<String>> {
        Ok(ctx.auth.user(&ctx.db).await?.and_then(|u| u.value.email))
    }

    #[query]
    pub async fn dev_query(ctx: &QueryCtx) -> Result<bool> {
        Ok(ctx.is_dev())
    }

    #[mutation]
    pub async fn dev_mutation(ctx: &MutationCtx) -> Result<bool> {
        Ok(ctx.is_dev())
    }

    /// `is_dev()` as seen by every context.
    #[action]
    pub async fn dev_flags(ctx: &ActionCtx) -> Result<DevFlags> {
        Ok(DevFlags {
            query: ctx.run_query::<dev_query>(()).await?,
            mutation: ctx.run_mutation::<dev_mutation>(()).await?,
            action: ctx.is_dev(),
        })
    }

    /// Only in dev mode.
    #[query(dev_only)]
    pub async fn dev_secret(_ctx: &QueryCtx) -> Result<String> {
        Ok("only in dev".into())
    }

    /// An internal dev-only function called from a public one: refused outside dev too.
    #[internal_mutation(dev_only)]
    pub async fn dev_reset(ctx: &MutationCtx) -> Result<u32> {
        let all = ctx.db.query::<Note>().collect().await?;
        for n in &all {
            ctx.db.delete(n.id).await?;
        }
        Ok(all.len() as u32)
    }

    #[action]
    pub async fn reset_via_action(ctx: &ActionCtx) -> Result<u32> {
        ctx.run_mutation::<dev_reset>(()).await
    }
}

use notes::*;

#[tokio::test]
async fn is_dev_in_every_context() {
    let dev = TestApp::new().await.unwrap();
    assert_eq!(dev.action::<dev_flags>(()).await.unwrap(), DevFlags { query: true, mutation: true, action: true });
    let prod = TestApp::with_options(AppOptions::default()).await.unwrap();
    assert_eq!(prod.action::<dev_flags>(()).await.unwrap(), DevFlags { query: false, mutation: false, action: false });
}

#[tokio::test]
async fn dev_only_functions_are_refused_outside_dev() {
    let dev = TestApp::new().await.unwrap();
    assert_eq!(dev.query::<dev_secret>(()).await.unwrap(), "only in dev");
    dev.mutation::<add>(("b".to_string(), "x".to_string())).await.unwrap();
    assert_eq!(dev.action::<reset_via_action>(()).await.unwrap(), 1);

    let prod = TestApp::with_options(AppOptions::default()).await.unwrap();
    let err = prod.query::<dev_secret>(()).await.unwrap_err().to_string();
    assert_eq!(err, "'dx/notes:dev_secret' is a dev-only function: it only runs when the server is started with --dev");
    // Also when called from another function (here an internal one, from an action).
    let err = format!("{:#}", prod.action::<reset_via_action>(()).await.unwrap_err());
    assert!(err.contains("'dx/notes:dev_reset' is a dev-only function"), "{err}");
    // Clients get the message, whatever the mode (it is a "not found" error, not a server error).
    let run = prod.app().run_query(&dev_secret::path(), json!({}), &Identity::default()).await;
    let client = rxferrix::error::ClientError::from_anyhow(&run.result.unwrap_err(), false);
    assert_eq!(client.kind, rxferrix::error::ErrorKind::NotFound);
    assert!(client.message.contains("dev-only"), "{client:?}");
}

/// Keeps every email and forwards it to the built-in dev mailer (which logs it).
#[derive(Default)]
struct Inbox(Mutex<Vec<Email>>);

impl Mailer for Inbox {
    fn send(&self, email: Email) -> BoxFuture<'_, rxferrix::Result<()>> {
        self.0.lock().unwrap().push(email.clone());
        DevLogMailer.send(email)
    }
}

#[tokio::test]
async fn the_dev_mailer_is_replaceable_and_composable() {
    let inbox = Arc::new(Inbox::default());
    let auth = AuthConfig::password().magic_link().dev_mailer(inbox.clone());
    // In dev mode without SMTP, the dev mailer gets the emails.
    let t = TestApp::with_auth(auth.clone()).await.unwrap();
    t.app().auth().request_magic_link(t.app(), "ann@example.com", None).await.unwrap();
    // Emails are sent in the background.
    for _ in 0..200 {
        if !inbox.0.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let sent = inbox.0.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "ann@example.com");

    // Outside dev it is never used (there is no mailer at all: the request fails).
    let prod = TestApp::with_options(AppOptions { auth, ..Default::default() }).await.unwrap();
    assert!(prod.app().auth().request_magic_link(prod.app(), "bob@example.com", None).await.is_err());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(inbox.0.lock().unwrap().len(), 1, "no email outside dev");
}

#[tokio::test]
async fn objects_embed_documents() {
    let t = TestApp::new().await.unwrap();
    let ann = t.create_user("ann@example.com", None).await.unwrap();
    let id = t.as_user(ann).mutation::<add>(("b".to_string(), "héllo".to_string())).await.unwrap();
    let page = t.query::<views>(("b".to_string(), Mode::Sequential, PaginationOpts::first(10))).await.unwrap();
    let view = &page.page[0];
    assert_eq!(view.note.id, id);
    assert_eq!((view.body_length, view.author_email.as_deref()), (5, Some("ann@example.com")));

    // On the wire: one flat camelCase object.
    let run = t
        .app()
        .run_query(
            &views::path(),
            json!({ "board": "b", "mode": "sequential", "paginationOpts": { "numItems": 10, "cursor": null } }),
            &Identity::default(),
        )
        .await;
    let json = run.result.unwrap();
    let item = &json["page"][0];
    let mut keys: Vec<&str> = item.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, ["_creationTime", "_id", "author", "authorEmail", "board", "body", "bodyLength"]);
    assert_eq!(item["_id"], json!(id.to_string()));

    // TS: the derived fields, intersected with the document type.
    let files =
        rxferrix::codegen::generate(rxferrix::registry().unwrap(), &rxferrix::runtime::registered_tables()).unwrap();
    let dm = &files.data_model_ts;
    assert!(
        dm.contains(
            "/**\n * A note as a list shows it: the document itself (flattened: `_id`, `_creationTime`, `board`, `body`, ...)\n * plus derived fields.\n */\nexport type NoteView = {\n  /** The body's length in characters. */\n  bodyLength: number,\n  authorEmail: string | null,\n} & Doc<\"dx_notes\">;\n"
        ),
        "{dm}"
    );
    assert!(dm.contains("export type Mode = \"sequential\" | \"tryMap\" | \"concurrent\" | \"failing\";\n"), "{dm}");
    assert!(files.api_ts.contains("PaginationResult<NoteView>>(\"query\", \"dx/notes:views\")"), "{}", files.api_ts);
}

#[tokio::test]
async fn pagination_results_map_async() {
    let t = TestApp::new().await.unwrap();
    let ann = t.create_user("ann@example.com", Some("correct horse battery")).await.unwrap();
    for i in 0..7 {
        t.as_user(ann).mutation::<add>(("b".to_string(), format!("n{i}"))).await.unwrap();
    }
    t.mutation::<add>(("b".to_string(), "anonymous".to_string())).await.unwrap();
    let mut results = Vec::new();
    for mode in [Mode::Sequential, Mode::TryMap, Mode::Concurrent] {
        let mut bodies = Vec::new();
        let mut cursor = None;
        loop {
            let opts = PaginationOpts { num_items: 3, cursor: cursor.clone(), end_cursor: None };
            let page = t.query::<views>(("b".to_string(), mode, opts)).await.unwrap();
            bodies.extend(page.page.iter().map(|v| (v.note.body.clone(), v.author_email.clone())));
            if page.is_done {
                break;
            }
            cursor = Some(page.continue_cursor);
        }
        results.push(bodies);
    }
    // Newest first, same result whichever way the page was mapped.
    let ann_email = Some("ann@example.com".to_string());
    let expected: Vec<(String, Option<String>)> = std::iter::once(("anonymous".to_string(), None))
        .chain((0..7).rev().map(|i| (format!("n{i}"), ann_email.clone())))
        .collect();
    for r in &results {
        assert_eq!(r, &expected);
    }
    // try_map stops at the first error.
    t.mutation::<add>(("b".to_string(), "boom".to_string())).await.unwrap();
    let err = t.query::<views>(("b".to_string(), Mode::Failing, PaginationOpts::first(3))).await.unwrap_err();
    assert_eq!(err.downcast_ref::<rxferrix::UserError>().unwrap().code, "boom");
}

async fn read_set_of<F: FunctionRef>(t: &TestApp, args: Value) -> (Value, ReadSet) {
    let run = t.app().run_query(&F::path(), args, &Identity::default()).await;
    (run.result.unwrap(), run.read_set)
}

fn commit_at(t: &TestApp, ts: u64) -> Arc<CommitEntry> {
    t.app().db().write_log().entries_between(ts - 1, ts).unwrap()[0].clone()
}

#[tokio::test]
async fn count_over_an_index_range() {
    let t = TestApp::new().await.unwrap();
    for i in 0..5 {
        t.mutation::<add>(("a".to_string(), format!("a{i}"))).await.unwrap();
        t.mutation::<add>(("b".to_string(), format!("b{i}"))).await.unwrap();
    }
    let count = |board: &str, after: Option<f64>, needle: Option<&str>| {
        t.query::<notes::count>((board.to_string(), after, needle.map(String::from)))
    };
    assert_eq!(count("a", None, None).await.unwrap(), 5);
    assert_eq!(count("c", None, None).await.unwrap(), 0);
    assert_eq!(count("a", None, Some("3")).await.unwrap(), 1);
    // After the third note of `a`, by creation time.
    let page = t.query::<views>(("a".to_string(), Mode::Sequential, PaginationOpts::first(10))).await.unwrap();
    let third = page.page[2].note.creation_time; // newest first: a4, a3, a2
    assert_eq!(count("a", Some(third), None).await.unwrap(), 2);

    // The read set is the counted range: a write to board `b` doesn't invalidate a count of `a`, one to `a` does.
    let (n, rs) = read_set_of::<notes::count>(&t, json!({ "board": "a" })).await;
    assert_eq!(n, json!(5));
    let (_, ts) = t.mutation_ts::<add>(("b".to_string(), "more".to_string())).await.unwrap();
    assert!(!rs.overlaps(&commit_at(&t, ts)));
    let (_, ts) = t.mutation_ts::<add>(("a".to_string(), "more".to_string())).await.unwrap();
    assert!(rs.overlaps(&commit_at(&t, ts)));
    // A bounded count: older writes in `a` are outside it, newer ones inside.
    let (_, rs) = read_set_of::<notes::count>(&t, json!({ "board": "a", "after": third })).await;
    let (_, ts) = t.mutation_ts::<add>(("a".to_string(), "newest".to_string())).await.unwrap();
    assert!(rs.overlaps(&commit_at(&t, ts)));
}

#[tokio::test]
async fn test_users() {
    let t = TestApp::new().await.unwrap();
    let ann: Id<User> = t.create_user("Ann@Example.com", Some("correct horse battery")).await.unwrap();
    let bob = t.create_user("bob@example.com", None).await.unwrap();
    assert_ne!(ann, bob);
    assert_eq!(t.as_user(ann).query::<whoami>(()).await.unwrap().as_deref(), Some("ann@example.com"));
    assert_eq!(t.as_user(bob).query::<whoami>(()).await.unwrap().as_deref(), Some("bob@example.com"));
    assert_eq!(t.query::<whoami>(()).await.unwrap(), None);
    assert_eq!(t.as_user(ann).identity(), &Identity::user(ann, None));
    // Taken emails are refused, with or without a password.
    for password in [None, Some("another password 1")] {
        let err = t.create_user("ann@example.com", password).await.unwrap_err();
        assert_eq!(err.downcast_ref::<rxferrix::UserError>().unwrap().code, "email_in_use", "{password:?}");
    }
    // A user created with a password can sign in with it (when password sign-in is enabled).
    let t = TestApp::with_auth(AuthConfig::password()).await.unwrap();
    t.create_user("carol@example.com", Some("correct horse battery")).await.unwrap();
    let tokens = t
        .app()
        .auth()
        .sign_in(t.app(), "carol@example.com", "correct horse battery".into(), Default::default())
        .await
        .unwrap()
        .tokens()
        .expect("no second factor: a session");
    assert!(tokens.user_id.is_some());
}

#[tokio::test]
async fn user_error_helpers() {
    use rxferrix::UserError;
    let t = TestApp::new().await.unwrap();
    let id = t.mutation::<notes::add>(("b".to_string(), "hello".to_string())).await.unwrap();
    let code = |err: rxferrix::Error| UserError::of(&err).map(|e| (e.code.clone(), e.message.clone()));
    let err = t.mutation::<notes::rename_note>((id, " ".to_string())).await.unwrap_err();
    assert_eq!(code(err), Some(("empty".into(), "a note needs some text".into())));
    let err = t.mutation::<notes::rename_note>((id, "hello".to_string())).await.unwrap_err();
    assert_eq!(code(err), Some(("unchanged".into(), "the note already says \"hello\"".into())));
    t.mutation::<notes::rename_note>((id, "bye".to_string())).await.unwrap();
    // One parameter: the value itself, not a one-element tuple.
    t.mutation::<notes::remove>(id).await.unwrap();
    let err = t.mutation::<notes::rename_note>((id, "again".to_string())).await.unwrap_err();
    assert_eq!(code(err).map(|c| c.0), Some("not_found".into()));
    // Not a user error: none.
    assert!(UserError::of(&anyhow::anyhow!("boom")).is_none());
}
