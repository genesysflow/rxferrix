//! Backend tests with `TestApp`: authorization, unread counts, typing expiry and reminders.

use std::time::Duration;

use ferrix::auth::{AuthConfig, ClientInfo, SignUpResult, User};
use ferrix::prelude::*;
use ferrix::testing::TestApp;
use ferrix::Identity;

use crate::model::Channel;
use crate::{channels, messages, profiles, reactions, typing};

async fn app() -> TestApp {
    TestApp::with_auth(AuthConfig::password()).await.unwrap()
}

/// Signs up `email` and creates the profile (joining #general); returns the identity to call functions as.
async fn user(t: &TestApp, email: &str) -> Identity {
    let result = t
        .app()
        .auth()
        .sign_up(t.app(), email, "correct horse battery".into(), None, ClientInfo::default())
        .await
        .unwrap_or_else(|e| panic!("sign up: {e:?}"));
    let SignUpResult::SignedIn(tokens) = result else { panic!("verification required") };
    let id: Id<User> = tokens.user_id.unwrap().parse().unwrap();
    let identity = Identity::user(id, None);
    t.mutation_as::<profiles::ensure>(&identity, ()).await.unwrap();
    identity
}

fn code(err: ferrix::Error) -> String {
    err.downcast_ref::<UserError>().map(|e| e.code.clone()).unwrap_or_else(|| format!("not a user error: {err:#}"))
}

async fn send(t: &TestApp, who: &Identity, channel: Id<Channel>, body: &str) -> messages::SendResult {
    t.mutation_as::<messages::send>(who, (channel, body.to_string(), None)).await.unwrap()
}

#[tokio::test]
async fn private_channels_need_membership() {
    let t = app().await;
    let ann = user(&t, "ann@example.com").await;
    let bob = user(&t, "bob@example.com").await;
    let secret = t.mutation_as::<channels::create>(&ann, ("secret".to_string(), None, true)).await.unwrap();
    send(&t, &ann, secret, "for members only").await;

    // Bob can't see, read, join or post to it.
    let listed = t.query_as::<channels::list>(&bob, ()).await.unwrap();
    assert!(listed.iter().all(|c| c.id != secret));
    let page = PaginationOpts::first(10);
    assert_eq!(code(t.query_as::<messages::list>(&bob, (secret, page)).await.unwrap_err()), "not_a_member");
    assert_eq!(code(t.query_as::<channels::get>(&bob, (secret,)).await.unwrap_err()), "not_a_member");
    assert_eq!(code(t.mutation_as::<channels::join>(&bob, (secret,)).await.unwrap_err()), "private_channel");
    let err = t.mutation_as::<messages::send>(&bob, (secret, "hi".to_string(), None)).await.unwrap_err();
    assert_eq!(code(err), "not_a_member");
    let err = t.query_as::<messages::search>(&bob, (secret, "members".to_string())).await.unwrap_err();
    assert_eq!(code(err), "not_a_member");

    // Signed out: unauthenticated.
    let err = t.query_as::<channels::list>(&Identity::anonymous(), ()).await.unwrap_err();
    assert_eq!(code(err), "unauthenticated");

    // An invitation lets him in.
    t.mutation_as::<channels::invite>(&ann, (secret, "BOB@example.com".to_string())).await.unwrap();
    let page = t.query_as::<messages::list>(&bob, (secret, PaginationOpts::first(10))).await.unwrap();
    assert_eq!(page.page.len(), 1);
    assert_eq!(page.page[0].author_name, "ann");
}

#[tokio::test]
async fn only_authors_edit_and_delete() {
    let t = app().await;
    let ann = user(&t, "ann@example.com").await;
    let bob = user(&t, "bob@example.com").await;
    let general = t.query_as::<channels::list>(&ann, ()).await.unwrap()[0].id;
    let id = send(&t, &ann, general, "hello").await.message.unwrap();

    let err = t.mutation_as::<messages::edit>(&bob, (id, "hacked".to_string())).await.unwrap_err();
    assert_eq!(code(err), "forbidden");
    assert_eq!(code(t.mutation_as::<messages::remove>(&bob, (id,)).await.unwrap_err()), "forbidden");

    t.mutation_as::<messages::edit>(&ann, (id, "hello, world".to_string())).await.unwrap();
    assert!(t.mutation_as::<reactions::toggle>(&bob, (id, "🎉".to_string())).await.unwrap());
    let page = t.query_as::<messages::list>(&ann, (general, PaginationOpts::first(10))).await.unwrap();
    assert_eq!((page.page[0].body.as_str(), page.page[0].edited), ("hello, world", true));
    assert_eq!(page.page[0].reactions[0].names, vec!["bob".to_string()]);
    assert!(!page.page[0].reactions[0].mine);

    t.mutation_as::<messages::remove>(&ann, (id,)).await.unwrap();
    let page = t.query_as::<messages::list>(&ann, (general, PaginationOpts::first(10))).await.unwrap();
    assert!(page.page.is_empty());
}

#[tokio::test]
async fn unread_counts_and_newest_first_pages() {
    let t = app().await;
    let ann = user(&t, "ann@example.com").await;
    let bob = user(&t, "bob@example.com").await;
    let general = t.query_as::<channels::list>(&ann, ()).await.unwrap()[0].id;
    for i in 0..3 {
        send(&t, &ann, general, &format!("m{i}")).await;
        t.advance_time(Duration::from_millis(5)).await.unwrap();
    }
    let unread = t.query_as::<channels::unread>(&bob, ()).await.unwrap();
    assert_eq!(unread.len(), 1);
    assert_eq!(unread[0].count, 3);
    assert!(t.query_as::<channels::unread>(&ann, ()).await.unwrap().is_empty(), "own messages are read");

    t.advance_time(Duration::from_millis(5)).await.unwrap();
    t.mutation_as::<channels::mark_read>(&bob, (general,)).await.unwrap();
    assert!(t.query_as::<channels::unread>(&bob, ()).await.unwrap().is_empty());

    // Seeded history is older than what's there, so it comes after it, newest first.
    t.action_as::<messages::seed>(&ann, (general, 120)).await.unwrap();
    let first = t.query_as::<messages::list>(&ann, (general, PaginationOpts::first(5))).await.unwrap();
    let bodies: Vec<_> = first.page.iter().map(|m| m.body.as_str()).collect();
    assert_eq!(bodies, ["m2", "m1", "m0", "Sample message #120 of 120", "Sample message #119 of 120"]);
    let mut cursor = first.continue_cursor;
    let mut all = 5;
    loop {
        let page = t.query_as::<messages::list>(&ann, (general, PaginationOpts::after(cursor, 50))).await.unwrap();
        all += page.page.len();
        if page.is_done {
            assert_eq!(page.page.last().unwrap().body, "Sample message #1 of 120");
            break;
        }
        cursor = page.continue_cursor;
    }
    assert_eq!(all, 123);
}

#[tokio::test]
async fn typing_rows_expire_on_schedule() {
    let t = app().await;
    let ann = user(&t, "ann@example.com").await;
    let bob = user(&t, "bob@example.com").await;
    let general = t.query_as::<channels::list>(&ann, ()).await.unwrap()[0].id;
    t.mutation_as::<channels::join>(&bob, (general,)).await.unwrap();

    t.mutation_as::<typing::start>(&ann, (general,)).await.unwrap();
    let typing = t.query_as::<typing::list>(&bob, (general,)).await.unwrap();
    assert_eq!(typing.iter().map(|u| u.name.as_str()).collect::<Vec<_>>(), ["ann"]);
    assert!(t.query_as::<typing::list>(&ann, (general,)).await.unwrap().is_empty(), "not shown to the typist");

    // Typing again extends it: the first job reschedules itself instead of deleting the row.
    t.advance_time(Duration::from_secs(2)).await.unwrap();
    t.mutation_as::<typing::start>(&ann, (general,)).await.unwrap();
    t.advance_time(Duration::from_secs(3)).await.unwrap();
    assert_eq!(t.query_as::<typing::list>(&bob, (general,)).await.unwrap().len(), 1);
    t.advance_time(Duration::from_secs(2)).await.unwrap();
    assert!(t.query_as::<typing::list>(&bob, (general,)).await.unwrap().is_empty());

    // Sending clears it right away.
    t.mutation_as::<typing::start>(&ann, (general,)).await.unwrap();
    send(&t, &ann, general, "done typing").await;
    assert!(t.query_as::<typing::list>(&bob, (general,)).await.unwrap().is_empty());
}

#[tokio::test]
async fn remind_posts_later() {
    let t = app().await;
    let ann = user(&t, "ann@example.com").await;
    let general = t.query_as::<channels::list>(&ann, ()).await.unwrap()[0].id;
    let sent = send(&t, &ann, general, "/remind me in 2 minutes to stretch").await;
    assert_eq!(sent.message, None);
    assert_eq!(sent.notice.as_deref(), Some("Okay, I'll remind you in 2 minutes: stretch"));
    let err = t.mutation_as::<messages::send>(&ann, (general, "/nope".to_string(), None)).await.unwrap_err();
    assert_eq!(code(err), "unknown_command");

    t.advance_time(Duration::from_secs(60)).await.unwrap();
    assert!(t.query_as::<messages::list>(&ann, (general, PaginationOpts::first(5))).await.unwrap().page.is_empty());
    t.advance_time(Duration::from_secs(61)).await.unwrap();
    let page = t.query_as::<messages::list>(&ann, (general, PaginationOpts::first(5))).await.unwrap();
    assert_eq!((page.page[0].body.as_str(), page.page[0].kind.as_str()), ("stretch", "reminder"));
}
