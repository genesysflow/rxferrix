//! Resource limits that keep a long-running server's memory bounded whatever its clients do: queued runs of dropped
//! queries are dropped with them, a connection's query count is capped, queries time out, and silent clients are
//! disconnected.

use std::time::Duration;

use ferrix::sync::protocol::{ClientMessage, QuerySetModification, ServerMessage};
use ferrix::sync::worker::{self, Inbound};
use ferrix::testing::TestApp;
use ferrix::{AppOptions, FunctionRef, Identity};
use serde_json::json;

use crate::shared::{Client, fns};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queries_added_and_removed_leave_no_queued_runs() {
    let t = TestApp::new().await.unwrap();
    let app = t.app().clone();
    let mut c = Client::new(&app).await;
    // Each slow query is queued when added; removing it at once must take its queued run along.
    for i in 0..300u64 {
        let add = QuerySetModification::Add {
            query_id: i,
            udf_path: fns::slow_count::path(),
            args: json!({ "ms": 500 + i }),
        };
        c.modify(vec![add, QuerySetModification::Remove { query_id: i }]).await;
    }
    let s = app.subscription_stats();
    // Only the runs already started can remain (a runner per reader at most).
    assert!(s.jobs <= 16, "{} query runs still queued or running for queries nobody has: {s:?}", s.jobs);
    assert_eq!((s.subscriptions, s.shared_queries), (0, 0), "{s:?}");
}

#[tokio::test]
async fn a_connection_has_at_most_1024_queries() {
    let t = TestApp::new().await.unwrap();
    let mut c = Client::new(t.app()).await;
    let modifications = (0..=worker::MAX_QUERIES as u64)
        .map(|i| QuerySetModification::Add { query_id: i, udf_path: fns::everyone::path(), args: json!({}) })
        .collect();
    c.send(ClientMessage::ModifyQuerySet { base_version: 0, new_version: 1, modifications }).await;
    loop {
        match c.recv().await {
            ServerMessage::FatalError { error } => {
                assert!(error.contains("too many queries"), "{error}");
                return;
            }
            ServerMessage::Transition { .. } => panic!("1025 queries were accepted"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn queries_time_out() {
    let options = AppOptions { query_timeout: Duration::from_millis(100), ..Default::default() };
    let t = TestApp::with_options(options).await.unwrap();
    let run = t.app().run_query(&fns::slow_count::path(), json!({ "ms": 5_000 }), &Identity::default()).await;
    let err = run.result.unwrap_err();
    assert!(err.downcast_ref::<ferrix::QueryTimeout>().is_some(), "{err:#}");
    // Fast ones are unaffected.
    let run = t.app().run_query(&fns::slow_count::path(), json!({ "ms": 1 }), &Identity::default()).await;
    assert_eq!(run.result.unwrap(), json!(0));
}

#[tokio::test(start_paused = true)]
async fn silent_clients_are_disconnected() {
    let t = TestApp::new().await.unwrap();
    let (tx, mut rx) = worker::spawn(t.app().clone());
    let connect = ClientMessage::Connect {
        session_id: "s".into(),
        connection_count: 0,
        last_close_reason: None,
        client_version: None,
        features: vec![],
    };
    tx.send(Inbound::Message(connect)).await.unwrap();
    // Answering pings keeps it open...
    let mut pings = 0;
    while pings < 5 {
        if let ServerMessage::Ping = tokio::time::timeout(Duration::from_secs(60), rx.recv()).await.unwrap().unwrap() {
            pings += 1;
            tx.send(Inbound::Message(ClientMessage::Pong)).await.unwrap();
        }
    }
    // ...and saying nothing closes it, within the limit (and a ping interval).
    let silent_from = tokio::time::Instant::now();
    loop {
        match tokio::time::timeout(Duration::from_secs(120), rx.recv()).await.expect("still open").unwrap() {
            ServerMessage::Ping => {}
            ServerMessage::FatalError { error } => {
                assert!(error.contains("silent"), "{error}");
                let waited = silent_from.elapsed();
                assert!(waited >= worker::SILENCE_LIMIT - worker::PING_INTERVAL, "closed after {waited:?}");
                assert!(waited <= worker::SILENCE_LIMIT + worker::PING_INTERVAL, "closed after {waited:?}");
                return;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// Actions that wait until the test lets them finish.
mod gated {
    use ferrix::prelude::*;

    pub static GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(0);

    #[action]
    pub async fn wait_for_gate(_ctx: &ActionCtx) -> Result<()> {
        GATE.acquire().await?.forget();
        Ok(())
    }
}

/// A connection with every mutation/action slot taken parks the next request (and reads nothing more), but stays
/// alive: query updates keep coming, the parked request runs once a slot frees up, and revocation still ends it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_connection_with_every_slot_taken_keeps_serving_updates_and_revocation() {
    use ferrix::auth::AuthConfig;

    let t = TestApp::with_auth(AuthConfig::password().rate_limits(None)).await.unwrap();
    let app = t.app().clone();
    let (_user, token) = crate::shared::sign_up(&app, "full@example.com").await;
    let mut c = Client::new(&app).await;
    c.authenticate(Some(&token)).await;
    assert_eq!(c.subscribe(1, &fns::slow_count::path(), json!({ "ms": 0 })).await, Ok(json!(0)));

    let actions = worker::MAX_IN_FLIGHT as u64 + 1;
    for request_id in 0..actions {
        let udf_path = gated::wait_for_gate::path();
        c.send(ClientMessage::Action { request_id, udf_path, args: json!({}) }).await;
    }
    // Every slot is taken and one action is parked. A write elsewhere still reaches this socket.
    tokio::time::sleep(Duration::from_millis(100)).await;
    app.run_mutation(&crate::common::todos::add::path(), json!({ "text": "x" }), &Identity::default()).await.unwrap();
    c.until(|c| c.results.get(&1) == Some(&Ok(json!(1)))).await;

    // Letting the actions finish frees slots: all of them answer, the parked one included.
    gated::GATE.add_permits(actions as usize);
    let mut answered = std::collections::BTreeSet::new();
    while answered.len() < actions as usize {
        if let ServerMessage::ActionResponse { request_id, result, .. } = c.recv().await {
            assert!(result.is_ok(), "{result:?}");
            answered.insert(request_id);
        }
    }

    // Full again: signing out still ends the socket at once.
    for request_id in actions..2 * actions {
        let udf_path = gated::wait_for_gate::path();
        c.send(ClientMessage::Action { request_id, udf_path, args: json!({}) }).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    app.auth().sign_out(&app, None, Some(&token)).await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ServerMessage::AuthError { .. } = c.recv().await {
                return;
            }
        }
    })
    .await;
    gated::GATE.add_permits(actions as usize);
    ended.expect("the socket ends when its session does, even with every slot taken");
}
