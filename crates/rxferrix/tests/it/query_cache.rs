//! The query cache (`sync::subscriptions`, "Query cache"): a shared query nobody watches any more stays cached until a
//! commit invalidates it (it is then dropped, never run), within a memory budget and an idle TTL; current results
//! (live or idle) answer HTTP queries and `ctx.run_query` without running, and never a result older than the caller's
//! own writes, nor one computed for someone else.

use std::time::Duration;

use rxferrix::auth::AuthConfig;
use rxferrix::sync::subscriptions::SubscriptionStats;
use rxferrix::testing::TestApp;
use rxferrix::{App, AppOptions, FunctionRef, QueryCacheOptions};
use serde_json::{Value, json};

use crate::common::todos;
use crate::shared::{Client, sign_up, total_executions};

pub mod fns {
    use rxferrix::prelude::*;

    use crate::common::todos;

    /// The snapshot's time: depends on more than the data.
    #[query]
    pub async fn qc_now(ctx: &QueryCtx) -> Result<f64> {
        Ok(ctx.now())
    }

    /// How many todos there are, optionally only those with this text.
    #[query]
    pub async fn qc_count(ctx: &QueryCtx, text: Option<String>) -> Result<usize> {
        let all = ctx.db.query::<todos::Todo>().collect().await?;
        Ok(all.iter().filter(|t| text.as_ref().is_none_or(|x| &t.value.text == x)).count())
    }

    /// `qc_count` with no text, through `ctx.run_query`.
    #[action]
    pub async fn qc_count_all(ctx: &ActionCtx) -> Result<usize> {
        ctx.run_query::<qc_count>((None,)).await
    }

    /// How many todos `todos:list` returns, through `ctx.run_query`.
    #[action]
    pub async fn qc_count_via_action(ctx: &ActionCtx) -> Result<usize> {
        Ok(ctx.run_query::<todos::list>((None,)).await?.len())
    }
}

/// The args `ctx.run_query::<todos::list>((None,))` sends: a cache entry is found by exactly these.
fn list_args() -> Value {
    json!({ "done": null })
}

async fn wait_stats(app: &App, what: &str, ok: impl Fn(&SubscriptionStats) -> bool) -> SubscriptionStats {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let s = app.subscription_stats();
        if ok(&s) {
            return s;
        }
        assert!(tokio::time::Instant::now() < deadline, "{what}: {s:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn texts(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|t| t["text"].as_str().unwrap().to_string()).collect()
}

struct Http {
    client: reqwest::Client,
    base: String,
}

impl Http {
    async fn new(t: &TestApp) -> Http {
        Http { client: reqwest::Client::new(), base: format!("http://{}", t.serve().await.unwrap()) }
    }

    async fn call(&self, kind: &str, path: &str, args: Value, token: Option<&str>) -> Value {
        let mut req =
            self.client.post(format!("{}/api/{kind}", self.base)).json(&json!({ "path": path, "args": args }));
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        let body: Value = req.send().await.unwrap().json().await.unwrap();
        assert_eq!(body["status"], "success", "{body}");
        body["value"].clone()
    }
}

#[tokio::test]
async fn a_query_nobody_watches_serves_the_next_subscriber_http_and_run_query() {
    let t = TestApp::new().await.unwrap();
    let app = t.app().clone();
    let list = todos::list::path();
    t.mutation::<todos::add>(("a".to_string(),)).await.unwrap();
    let mut first = Client::new(&app).await;
    first.subscribe(1, &list, list_args()).await.unwrap();
    first.unsubscribe(1).await;
    wait_stats(&app, "idle", |s| s.idle_queries == 1 && s.subscriptions == 0).await;
    assert_eq!(total_executions(&app, &list).await, 1);

    let mut second = Client::new(&app).await;
    assert_eq!(texts(&second.subscribe(1, &list, list_args()).await.unwrap()), ["a"]);
    let http = Http::new(&t).await;
    assert_eq!(texts(&http.call("query", &list, list_args(), None).await), ["a"]);
    assert_eq!(http.call("action", &fns::qc_count_via_action::path(), json!({}), None).await, json!(1));
    assert_eq!(total_executions(&app, &list).await, 1, "answered from the cache, not run");
    let s = app.subscription_stats();
    assert!(s.cache_hits >= 2, "{s:?}");
}

#[tokio::test]
async fn subscriptions_count_as_cache_lookups() {
    let t = TestApp::new().await.unwrap();
    let app = t.app().clone();
    let list = todos::list::path();
    let mut c = Client::new(&app).await;
    c.subscribe(1, &list, list_args()).await.unwrap();
    let s = app.subscription_stats();
    assert_eq!((s.cache_hits, s.cache_misses), (0, 1), "the first subscriber waits for a run");
    // Another subscriber of the live query, then one of the idle query after both left.
    c.subscribe(2, &list, list_args()).await.unwrap();
    c.unsubscribe(1).await;
    c.unsubscribe(2).await;
    wait_stats(&app, "idle", |s| s.idle_queries == 1).await;
    c.subscribe(3, &list, list_args()).await.unwrap();
    let s = app.subscription_stats();
    assert_eq!((s.cache_hits, s.cache_misses), (2, 1), "{s:?}");
    assert_eq!(total_executions(&app, &list).await, 1);
}

#[tokio::test]
async fn an_invalidated_idle_query_is_dropped_without_running() {
    let t = TestApp::new().await.unwrap();
    let app = t.app().clone();
    let list = todos::list::path();
    let mut c = Client::new(&app).await;
    c.subscribe(1, &list, list_args()).await.unwrap();
    c.unsubscribe(1).await;
    wait_stats(&app, "idle", |s| s.idle_queries == 1).await;

    t.mutation::<todos::add>(("new".to_string(),)).await.unwrap();
    wait_stats(&app, "dropped when invalidated", |s| s.shared_queries == 0 && s.idle_queries == 0).await;
    assert_eq!(total_executions(&app, &list).await, 1, "an idle query is never run");
    // The next caller runs it, at the new state.
    assert_eq!(texts(&c.subscribe(2, &list, list_args()).await.unwrap()), ["new"]);
    assert_eq!(total_executions(&app, &list).await, 2);
}

#[tokio::test]
async fn writes_elsewhere_leave_it_cached() {
    let t = TestApp::new().await.unwrap();
    let app = t.app().clone();
    let a = t.mutation::<todos::add>(("a".to_string(),)).await.unwrap();
    let b = t.mutation::<todos::add>(("b".to_string(),)).await.unwrap();
    let get = todos::get::path();
    let mut c = Client::new(&app).await;
    c.subscribe(1, &get, json!({ "id": a })).await.unwrap();
    c.unsubscribe(1).await;
    wait_stats(&app, "idle", |s| s.idle_queries == 1).await;

    // b's document is outside what `get(a)` read.
    t.mutation::<todos::rename>((b, "b2".to_string())).await.unwrap();
    let http = Http::new(&t).await;
    let latest = app.db().write_log().latest();
    wait_stats(&app, "frontier caught up", |_| {
        app.subscriptions().cache().get(&get, &json!({ "id": a }), &Default::default(), latest).is_some()
    })
    .await;
    assert_eq!(http.call("query", &get, json!({ "id": a }), None).await["text"], "a");
    assert_eq!(total_executions(&app, &get).await, 1, "still cached after an unrelated write");
}

#[tokio::test]
async fn http_callers_always_see_their_own_writes() {
    let t = TestApp::new().await.unwrap();
    let app = t.app().clone();
    let list = todos::list::path();
    // Live, so every query below can be answered from the cache once the manager has caught up.
    let mut watcher = Client::new(&app).await;
    watcher.subscribe(1, &list, list_args()).await.unwrap();
    let http = Http::new(&t).await;
    for i in 0..50 {
        let text = format!("t{i}");
        http.call("mutation", &todos::add::path(), json!({ "text": text }), None).await;
        let seen = texts(&http.call("query", &list, list_args(), None).await);
        assert!(seen.contains(&text), "query right after adding {text} missed it: {seen:?}");
    }
    let s = app.subscription_stats();
    assert!(s.cache_hits + s.cache_misses >= 50, "{s:?}");

    // Looking the cache up the instant a write returns, before the manager can have processed it: a hit must
    // already include the write (else there must be no hit).
    let (mut hits, mut misses) = (0, 0);
    for i in 0..300 {
        let text = format!("fast{i}");
        t.mutation::<todos::add>((text.clone(),)).await.unwrap();
        let latest = app.db().write_log().latest();
        match app.subscriptions().cache().get(&list, &list_args(), &Default::default(), latest) {
            Some(hit) => {
                assert!(texts(hit.value.value()).contains(&text), "a cached result from before {text}");
                hits += 1;
            }
            None => misses += 1,
        }
    }
    assert!(misses > 0, "the manager was never behind: the race wasn't exercised ({hits} hits)");
}

#[tokio::test]
async fn cached_results_never_cross_identities() {
    let t = TestApp::with_auth(AuthConfig::password().rate_limits(None)).await.unwrap();
    let app = t.app().clone();
    let (alice, alice_token) = sign_up(&app, "alice@example.com").await;
    let (bob, bob_token) = sign_up(&app, "bob@example.com").await;
    let my_id = crate::shared::fns::my_id::path();
    let mut c = Client::new(&app).await;
    c.authenticate(Some(&alice_token)).await;
    c.subscribe(1, &my_id, json!({})).await.unwrap();
    let executions = total_executions(&app, &my_id).await;

    let http = Http::new(&t).await;
    let as_alice = http.call("query", &my_id, json!({}), Some(&alice_token)).await;
    assert_eq!(as_alice, json!(format!("{alice}:0")));
    assert_eq!(total_executions(&app, &my_id).await, executions, "alice's own result, from the cache");
    let as_bob = http.call("query", &my_id, json!({}), Some(&bob_token)).await;
    assert_eq!(as_bob, json!(format!("{bob}:0")), "bob must not get alice's cached result");
    let anonymous = http.call("query", &my_id, json!({}), None).await;
    assert_eq!(anonymous, json!("anon:0"));
}

#[tokio::test]
async fn idle_queries_are_evicted_over_the_budget_and_after_the_ttl() {
    let options = AppOptions {
        query_cache: QueryCacheOptions { idle_ttl: Duration::from_millis(300), max_bytes: 4_000 },
        ..Default::default()
    };
    let t = TestApp::with_options(options).await.unwrap();
    let app = t.app().clone();
    let latest = todos::latest::path();
    let mut c = Client::new(&app).await;
    for n in 1..=10u64 {
        c.subscribe(n, &latest, json!({ "n": n })).await.unwrap();
        c.unsubscribe(n).await;
    }
    let s = wait_stats(&app, "over the budget", |s| s.subscriptions == 0 && s.cache_evictions > 0).await;
    assert!(s.idle_bytes <= 4_000 && s.idle_queries < 10, "{s:?}");
    // The most recently released are the ones kept.
    let mut again = Client::new(&app).await;
    let executions = total_executions(&app, &latest).await;
    again.subscribe(1, &latest, json!({ "n": 10 })).await.unwrap();
    assert_eq!(total_executions(&app, &latest).await, executions, "the newest idle query was kept");
    again.unsubscribe(1).await;

    tokio::time::sleep(Duration::from_millis(400)).await;
    // Any manager activity expires them (and so does its timer, within a minute).
    again.subscribe(2, &todos::list::path(), list_args()).await.unwrap();
    wait_stats(&app, "expired", |s| s.idle_queries == 0).await;
}

#[tokio::test]
async fn repeated_http_queries_are_adopted_one_offs_are_not() {
    let t = TestApp::new().await.unwrap();
    let app = t.app().clone();
    let list = todos::list::path();
    let http = Http::new(&t).await;
    t.mutation::<todos::add>(("a".to_string(),)).await.unwrap();
    // First miss: run. Second: run, and adopted (the doorkeeper saw it before). Then: from the cache.
    for _ in 0..2 {
        assert_eq!(texts(&http.call("query", &list, list_args(), None).await), ["a"]);
    }
    wait_stats(&app, "adopted", |s| s.idle_queries == 1).await;
    for _ in 0..5 {
        assert_eq!(texts(&http.call("query", &list, list_args(), None).await), ["a"]);
    }
    assert_eq!(total_executions(&app, &list).await, 2, "ran on the first two misses only");

    // A write drops it; the next answers see the write.
    t.mutation::<todos::add>(("b".to_string(),)).await.unwrap();
    assert_eq!(texts(&http.call("query", &list, list_args(), None).await), ["b", "a"]);

    // Queries asked once each never reach the cache.
    let get = todos::get::path();
    let id = t.mutation::<todos::add>(("c".to_string(),)).await.unwrap();
    wait_stats(&app, "invalidated", |s| s.idle_queries == 0).await;
    for n in 0..50 {
        http.call("query", &todos::latest::path(), json!({ "n": n }), None).await;
    }
    http.call("query", &get, json!({ "id": id }), None).await;
    assert_eq!(app.subscription_stats().idle_queries, 0, "one-off queries were adopted");
}

#[tokio::test]
async fn queries_that_read_the_time_are_never_cached() {
    let t = TestApp::new().await.unwrap();
    let app = t.app().clone();
    let now = fns::qc_now::path();
    let http = Http::new(&t).await;
    let mut times = Vec::new();
    for _ in 0..3 {
        times.push(http.call("query", &now, json!({}), None).await.as_f64().unwrap());
        // A commit moves the snapshot time forward (the clock is read at the snapshot).
        app.db().clock().advance(Duration::from_secs(60));
        t.mutation::<todos::add>(("tick".to_string(),)).await.unwrap();
    }
    assert!(times.windows(2).all(|w| w[1] > w[0]), "a cached time was served: {times:?}");
    assert_eq!(total_executions(&app, &now).await, 3);
    // Nor kept after its last subscriber leaves.
    let mut c = Client::new(&app).await;
    c.subscribe(1, &now, json!({})).await.unwrap();
    c.unsubscribe(1).await;
    wait_stats(&app, "dropped", |s| s.subscriptions == 0 && s.shared_queries == 0).await;
}

#[tokio::test]
async fn typed_calls_and_clients_share_entries_when_options_are_none() {
    let t = TestApp::new().await.unwrap();
    let app = t.app().clone();
    let count = fns::qc_count::path();
    t.mutation::<todos::add>(("a".to_string(),)).await.unwrap();
    // A client leaves the optional arg out; a typed call passes None: the same args, so the same cache entry.
    let mut c = Client::new(&app).await;
    assert_eq!(c.subscribe(1, &count, json!({})).await.unwrap(), json!(1));
    c.unsubscribe(1).await;
    wait_stats(&app, "idle", |s| s.idle_queries == 1).await;
    let http = Http::new(&t).await;
    assert_eq!(http.call("action", &fns::qc_count_all::path(), json!({}), None).await, json!(1));
    assert_eq!(total_executions(&app, &count).await, 1, "ctx.run_query((None,)) found the client's entry");
}
