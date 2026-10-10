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
