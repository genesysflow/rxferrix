//! Backend tests with `TestApp`: authorization, unread counts, typing expiry and reminders.

use std::time::Duration;

use ferrix::auth::User;
use ferrix::prelude::*;
use ferrix::testing::TestApp;

use crate::model::Channel;
use crate::{channels, messages, profiles, reactions, typing};

async fn app() -> TestApp {
    TestApp::new().await.unwrap()
}

/// Creates the user `email` and their profile (which joins #general).
async fn user(t: &TestApp, email: &str) -> Id<User> {
    let id = t.create_user(email, Some("correct horse battery")).await.unwrap();
    t.as_user(id).mutation::<profiles::ensure>(()).await.unwrap();
    id
}

fn code(err: ferrix::Error) -> String {
    err.downcast_ref::<UserError>().map(|e| e.code.clone()).unwrap_or_else(|| format!("not a user error: {err:#}"))
}

async fn send(t: &TestApp, who: Id<User>, channel: Id<Channel>, body: &str) -> messages::SendResult {
    t.as_user(who).mutation::<messages::send>((channel, body.to_string(), None)).await.unwrap()
}

async fn general(t: &TestApp, who: Id<User>) -> Id<Channel> {
    t.as_user(who).query::<channels::list>(()).await.unwrap()[0].channel.id
}

#[tokio::test]
async fn private_channels_need_membership() {
    let t = app().await;
    let (ann, bob) = (user(&t, "ann@example.com").await, user(&t, "bob@example.com").await);
    let (as_ann, as_bob) = (t.as_user(ann), t.as_user(bob));
    let secret = as_ann.mutation::<channels::create>(("secret".to_string(), None, true)).await.unwrap();
    send(&t, ann, secret, "for members only").await;

    // Bob can't see, read, join or post to it.
    let listed = as_bob.query::<channels::list>(()).await.unwrap();
    assert!(listed.iter().all(|c| c.channel.id != secret));
    let page = PaginationOpts::first(10);
    assert_eq!(code(as_bob.query::<messages::list>((secret, page)).await.unwrap_err()), "not_a_member");
    assert_eq!(code(as_bob.query::<channels::get>((secret,)).await.unwrap_err()), "not_a_member");
    assert_eq!(code(as_bob.mutation::<channels::join>((secret,)).await.unwrap_err()), "private_channel");
    let err = as_bob.mutation::<messages::send>((secret, "hi".to_string(), None)).await.unwrap_err();
    assert_eq!(code(err), "not_a_member");
    let err = as_bob.query::<messages::search>((secret, "members".to_string())).await.unwrap_err();
    assert_eq!(code(err), "not_a_member");

    // Signed out: unauthenticated.
    let err = t.query::<channels::list>(()).await.unwrap_err();
    assert_eq!(code(err), "unauthenticated");

    // An invitation lets him in.
    as_ann.mutation::<channels::invite>((secret, "BOB@example.com".to_string())).await.unwrap();
    let page = as_bob.query::<messages::list>((secret, PaginationOpts::first(10))).await.unwrap();
    assert_eq!(page.page.len(), 1);
    assert_eq!(page.page[0].author_name, "ann");
}

#[tokio::test]
async fn only_authors_edit_and_delete() {
    let t = app().await;
    let (ann, bob) = (user(&t, "ann@example.com").await, user(&t, "bob@example.com").await);
    let (as_ann, as_bob) = (t.as_user(ann), t.as_user(bob));
    let general = general(&t, ann).await;
    let id = send(&t, ann, general, "hello").await.message.unwrap();

    let err = as_bob.mutation::<messages::edit>((id, "hacked".to_string())).await.unwrap_err();
    assert_eq!(code(err), "forbidden");
    assert_eq!(code(as_bob.mutation::<messages::remove>((id,)).await.unwrap_err()), "forbidden");

    as_ann.mutation::<messages::edit>((id, "hello, world".to_string())).await.unwrap();
    assert!(as_bob.mutation::<reactions::toggle>((id, "🎉".to_string())).await.unwrap());
    let page = as_ann.query::<messages::list>((general, PaginationOpts::first(10))).await.unwrap();
    assert_eq!((page.page[0].message.body.as_str(), page.page[0].message.edited), ("hello, world", true));
    assert_eq!(page.page[0].reactions[0].names, vec!["bob".to_string()]);
    assert!(!page.page[0].reactions[0].mine);

    as_ann.mutation::<messages::remove>((id,)).await.unwrap();
    let page = as_ann.query::<messages::list>((general, PaginationOpts::first(10))).await.unwrap();
    assert!(page.page.is_empty());
}

#[tokio::test]
async fn unread_counts_and_newest_first_pages() {
    let t = app().await;
    let (ann, bob) = (user(&t, "ann@example.com").await, user(&t, "bob@example.com").await);
    let (as_ann, as_bob) = (t.as_user(ann), t.as_user(bob));
    let general = general(&t, ann).await;
    // Several messages in the same millisecond: still in the order they were sent (creation time, not the id).
    for i in 0..3 {
        send(&t, ann, general, &format!("m{i}")).await;
    }
    let unread = as_bob.query::<channels::unread>(()).await.unwrap();
    assert_eq!(unread.len(), 1);
    assert_eq!(unread[0].count, 3);
    assert!(as_ann.query::<channels::unread>(()).await.unwrap().is_empty(), "own messages are read");

    t.advance_time(Duration::from_millis(5)).await.unwrap();
    as_bob.mutation::<channels::mark_read>((general,)).await.unwrap();
    assert!(as_bob.query::<channels::unread>(()).await.unwrap().is_empty());

    // Seeded messages are posted after what's there, #1 first: newest first, they come before it.
    as_ann.action::<messages::seed>((general, 120)).await.unwrap();
    let first = as_ann.query::<messages::list>((general, PaginationOpts::first(3))).await.unwrap();
    let bodies: Vec<_> = first.page.iter().map(|m| m.message.body.as_str()).collect();
    assert_eq!(bodies, ["Sample message #120 of 120", "Sample message #119 of 120", "Sample message #118 of 120"]);
    let mut cursor = first.continue_cursor;
    let mut all: Vec<String> = bodies.iter().map(|b| b.to_string()).collect();
    loop {
        let page = as_ann.query::<messages::list>((general, PaginationOpts::after(cursor, 50))).await.unwrap();
        all.extend(page.page.iter().map(|m| m.message.body.clone()));
        if page.is_done {
            break;
        }
        cursor = page.continue_cursor;
    }
    assert_eq!(all.len(), 123);
    assert_eq!(all[119], "Sample message #1 of 120");
    assert_eq!(all[120..], ["m2", "m1", "m0"]);
}

#[tokio::test]
async fn typing_rows_expire_on_schedule() {
    let t = app().await;
    let (ann, bob) = (user(&t, "ann@example.com").await, user(&t, "bob@example.com").await);
    let (as_ann, as_bob) = (t.as_user(ann), t.as_user(bob));
    let general = general(&t, ann).await;
    as_bob.mutation::<channels::join>((general,)).await.unwrap();

    as_ann.mutation::<typing::start>((general,)).await.unwrap();
    let typing = as_bob.query::<typing::list>((general,)).await.unwrap();
    assert_eq!(typing.iter().map(|u| u.name.as_str()).collect::<Vec<_>>(), ["ann"]);
    assert!(as_ann.query::<typing::list>((general,)).await.unwrap().is_empty(), "not shown to the typist");

    // Typing again extends it: the first job reschedules itself instead of deleting the row.
    t.advance_time(Duration::from_secs(2)).await.unwrap();
    as_ann.mutation::<typing::start>((general,)).await.unwrap();
    t.advance_time(Duration::from_secs(3)).await.unwrap();
    assert_eq!(as_bob.query::<typing::list>((general,)).await.unwrap().len(), 1);
    t.advance_time(Duration::from_secs(2)).await.unwrap();
    assert!(as_bob.query::<typing::list>((general,)).await.unwrap().is_empty());

    // Sending clears it right away.
    as_ann.mutation::<typing::start>((general,)).await.unwrap();
    send(&t, ann, general, "done typing").await;
    assert!(as_bob.query::<typing::list>((general,)).await.unwrap().is_empty());
}

#[tokio::test]
async fn remind_posts_later() {
    let t = app().await;
    let ann = user(&t, "ann@example.com").await;
    let as_ann = t.as_user(ann);
    let general = general(&t, ann).await;
    let sent = send(&t, ann, general, "/remind me in 2 minutes to stretch").await;
    assert_eq!(sent.message, None);
    assert_eq!(sent.notice.as_deref(), Some("Okay, I'll remind you in 2 minutes: stretch"));
    let err = as_ann.mutation::<messages::send>((general, "/nope".to_string(), None)).await.unwrap_err();
    assert_eq!(code(err), "unknown_command");

    t.advance_time(Duration::from_secs(60)).await.unwrap();
    assert!(as_ann.query::<messages::list>((general, PaginationOpts::first(5))).await.unwrap().page.is_empty());
    t.advance_time(Duration::from_secs(61)).await.unwrap();
    let page = as_ann.query::<messages::list>((general, PaginationOpts::first(5))).await.unwrap();
    assert_eq!((page.page[0].message.body.as_str(), page.page[0].message.kind.as_str()), ("stretch", "reminder"));
}

#[tokio::test]
async fn the_dev_inbox_is_dev_only() {
    // `TestApp` runs in dev mode: the action works (and finds nothing for an unknown address).
    let t = app().await;
    assert_eq!(t.action::<crate::dev::last_code>(("nobody@example.com".to_string(),)).await.unwrap(), None);
    // Outside --dev it is refused.
    let prod = TestApp::with_options(ferrix::AppOptions::default()).await.unwrap();
    let err = prod.action::<crate::dev::last_code>(("nobody@example.com".to_string(),)).await.unwrap_err();
    assert!(err.to_string().contains("dev-only"), "{err}");
}
