//! Model-based reactivity test.
//!
//! Several clients (each a real sync worker, driven through its channels) randomly add and remove subscriptions and
//! send mutations, while background writers commit more mutations directly. Every test mutation returns the whole
//! table as it saw it at the end of its transaction, which is exactly the database state at its commit ts, so we
//! learn the full state at every ts. A pure Rust model then evaluates each query at any ts.
//!
//! Checked:
//! - every Transition's `startVersion` equals the previous `endVersion`; ts and querySet never go backwards;
//! - after each Transition, the client's result set is exactly the server's live query set at that querySet version
//!   (first results arrive in the answering Transition; removals are applied);
//! - after each Transition, EVERY result the client holds equals the model evaluated at `endVersion.ts` (no tearing,
//!   no stale results);
//! - after a MutationResponse at ts M, a Transition with ts >= M arrives (whichever order they come in);
//! - after quiescence, without any prompting, every client's results equal the model at the final ts; a final
//!   ModifyQuerySet then answers at exactly the final ts.
//!
//! Some queries are artificially slow (`fns::slow_group` sleeps between its reads and depends on every commit), so
//! their reruns overlap later commits and generations: runs finish already invalidated, results are sent at the ts of
//! an older run, and clients holding several of them become starving, which holds new generations until they are
//! served (the test checks that runs finished stale).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use rxferrix::sync::protocol::{ClientMessage, QuerySetModification, ServerMessage, StateModification, StateVersion};
use rxferrix::sync::worker::{self, Inbound};
use rxferrix::testing::TestApp;
use rxferrix::{App, Doc, FunctionRef, Identity};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use crate::common::items::{self, Item};

type Dump = Vec<Doc<Item>>;
type QResult = Result<Value, String>;

const CLIENTS: usize = 6;
const CLIENT_STEPS: usize = 150;
const WRITERS: usize = 2;
const WRITER_STEPS: usize = 80;
const GROUPS: i64 = 4;
/// Durations (ms) of the slow queries.
const SLOW_MS: [u32; 4] = [1, 4, 12, 30];

static NAMES: AtomicU64 = AtomicU64::new(0);

pub mod fns {
    use rxferrix::prelude::*;

    use crate::common::items::Item;

    /// The items of `group`, taking at least `ms` milliseconds: it sleeps between its two reads (which still see one
    /// snapshot). Its second read covers the whole table, so every commit invalidates it.
    #[query]
    pub async fn slow_group(ctx: &QueryCtx, group: i64, ms: u32) -> Result<Vec<Doc<Item>>> {
        let docs =
            ctx.db.query::<Item>().with_index(Item::by_group_score, |r| r.eq(Item::group, group)).collect().await?;
        tokio::time::sleep(std::time::Duration::from_millis(ms.into())).await;
        let all = ctx.db.query::<Item>().collect().await?;
        assert!(docs.iter().all(|d| all.contains(d)), "one snapshot across the sleep");
        Ok(docs)
    }
}

#[derive(Clone, Debug)]
enum Q {
    InGroup(i64),
    Top(i64, u32),
    CountAll,
    Newest(u32),
    ValueRange(i64, i64, i64),
    Filtered(i64),
    FindName(String),
    Slow(i64, u32),
}

impl Q {
    fn random(rng: &mut StdRng) -> Q {
        // Clients share the server's query executions: often pick from a small pool every client draws from, so
        // several clients hold identical (and overlapping) query sets.
        if rng.random_bool(0.12) {
            return Q::Slow(rng.random_range(0..2), SLOW_MS[rng.random_range(0..SLOW_MS.len())]);
        }
        if rng.random_bool(0.4) {
            return match rng.random_range(0..5) {
                0 => Q::CountAll,
                1 => Q::InGroup(0),
                2 => Q::Top(1, 2),
                3 => Q::Newest(3),
                _ => Q::Filtered(10),
            };
        }
        let g = rng.random_range(0..GROUPS);
        match rng.random_range(0..7) {
            0 => Q::InGroup(g),
            1 => Q::Top(g, rng.random_range(1..4)),
            2 => Q::CountAll,
            3 => Q::Newest(rng.random_range(1..5)),
            4 => {
                let lo = rng.random_range(0..15);
                Q::ValueRange(g, lo, lo + rng.random_range(1..10))
            }
            5 => Q::Filtered(rng.random_range(0..25)),
            _ => {
                let max = NAMES.load(Ordering::Relaxed) + 3;
                Q::FindName(format!("n{}", rng.random_range(0..max)))
            }
        }
    }

    fn call(&self) -> (String, Value) {
        match self {
            Q::InGroup(g) => (items::in_group::path(), json!({ "group": g })),
            Q::Top(g, n) => (items::top::path(), json!({ "group": g, "n": n })),
            Q::CountAll => (items::count_all::path(), json!({})),
            Q::Newest(n) => (items::newest::path(), json!({ "n": n })),
            Q::ValueRange(g, lo, hi) => (items::value_range::path(), json!({ "group": g, "lo": lo, "hi": hi })),
            Q::Filtered(min) => (items::filtered::path(), json!({ "min": min })),
            Q::FindName(name) => (items::find_name::path(), json!({ "name": name })),
            Q::Slow(g, ms) => (fns::slow_group::path(), json!({ "group": g, "ms": ms })),
        }
    }

    /// The reference semantics, written independently of the query engine.
    fn eval(&self, docs: &[Doc<Item>]) -> QResult {
        fn by_group_score(mut v: Vec<&Doc<Item>>) -> Vec<&Doc<Item>> {
            // Index keys end in `_creationTime, _id`.
            v.sort_by(|a, b| {
                (a.group, a.score)
                    .cmp(&(b.group, b.score))
                    .then(a.creation_time.total_cmp(&b.creation_time))
                    .then(a.id.cmp(&b.id))
            });
            v
        }
        fn by_creation(mut v: Vec<&Doc<Item>>) -> Vec<&Doc<Item>> {
            v.sort_by(|a, b| a.creation_time.total_cmp(&b.creation_time).then(a.id.cmp(&b.id)));
            v
        }
        let to_json = |v: Vec<&Doc<Item>>| serde_json::to_value(v).unwrap();
        Ok(match self {
            Q::InGroup(g) => to_json(by_group_score(docs.iter().filter(|d| d.group == *g).collect())),
            Q::Top(g, n) => {
                let mut v = by_group_score(docs.iter().filter(|d| d.group == *g).collect());
                v.reverse();
                v.truncate(*n as usize);
                to_json(v)
            }
            Q::CountAll => json!(docs.len()),
            Q::Newest(n) => {
                let mut v = by_creation(docs.iter().collect());
                v.reverse();
                v.truncate(*n as usize);
                to_json(v)
            }
            Q::ValueRange(g, lo, hi) => to_json(by_group_score(
                docs.iter().filter(|d| d.group == *g && d.score >= *lo && d.score < *hi).collect(),
            )),
            Q::Filtered(min) => {
                let mut v: Vec<_> =
                    by_creation(docs.iter().collect()).into_iter().filter(|d| d.score >= *min).collect();
                v.truncate(3);
                to_json(v)
            }
            Q::Slow(g, _) => to_json(by_group_score(docs.iter().filter(|d| d.group == *g).collect())),
            Q::FindName(name) => {
                let found: Vec<_> = docs.iter().filter(|d| &d.name == name).collect();
                match found.as_slice() {
                    [] => return Err(format!("no item named {name}")),
                    [one] => serde_json::to_value(one).unwrap(),
                    _ => panic!("names are unique"),
                }
            }
        })
    }
}

fn random_mutation(rng: &mut StdRng) -> (String, Value) {
    let g = rng.random_range(0..GROUPS);
    match rng.random_range(0..10) {
        0..=4 => {
            let name = format!("n{}", NAMES.fetch_add(1, Ordering::Relaxed));
            (items::insert::path(), json!({ "group": g, "value": rng.random_range(0..20), "name": name }))
        }
        5 | 6 => (items::bump::path(), json!({ "group": g, "delta": rng.random_range(-5..6) })),
        7 | 8 => (items::delete_top::path(), json!({ "group": g })),
        _ => (items::move_newest::path(), json!({ "group": g })),
    }
}

/// Database state at every ts.
#[derive(Default)]
struct Dumps(Mutex<BTreeMap<u64, Dump>>);

impl Dumps {
    fn record(&self, ts: u64, dump: Value) {
        let dump: Dump = serde_json::from_value(dump).unwrap();
        let mut map = self.0.lock().unwrap();
        if let Some(prev) = map.get(&ts) {
            assert_eq!(prev, &dump, "two different states observed at ts {ts}");
        } else {
            map.insert(ts, dump);
        }
    }

    fn at(&self, ts: u64) -> Dump {
        self.0.lock().unwrap().get(&ts).cloned().unwrap_or_else(|| panic!("no state recorded for ts {ts}"))
    }
}

/// What a client saw, for verification after the run.
struct ClientLog {
    /// (endVersion.ts, results held after applying that Transition)
    states: Vec<(u64, BTreeMap<u64, QResult>)>,
    queries: BTreeMap<u64, Q>,
    final_results: BTreeMap<u64, QResult>,
    response_first: usize,
    transition_first: usize,
    /// Results that arrived as diffs (each checked against the value: `check_wire_diffs`).
    diffs: usize,
}

struct Client {
    id: usize,
    tx: mpsc::Sender<Inbound>,
    rx: mpsc::Receiver<ServerMessage>,
    version: StateVersion,
    results: BTreeMap<u64, QResult>,
    /// Every query ever added (ids are never reused).
    queries: BTreeMap<u64, Q>,
    /// Client-side live set (what it has asked for so far).
    live: BTreeSet<u64>,
    /// Live set as of each querySet version the client sent.
    live_at_version: BTreeMap<u64, BTreeSet<u64>>,
    next_query_id: u64,
    next_request_id: u64,
    query_set_version: u64,
    /// Identity version of the last Authenticate sent (the identity stays anonymous; this exercises re-keying).
    identity_version: u64,
    outstanding_mutations: BTreeSet<u64>,
    /// Commit ts of mutation responses still waiting for a covering Transition.
    uncovered: Vec<u64>,
    log: ClientLog,
    dumps: Arc<Dumps>,
}

impl Client {
    fn new(id: usize, app: App, dumps: Arc<Dumps>) -> Client {
        let (tx, rx) = worker::spawn(app);
        let mut live_at_version = BTreeMap::new();
        live_at_version.insert(0, BTreeSet::new());
        Client {
            id,
            tx,
            rx,
            version: StateVersion::default(),
            results: BTreeMap::new(),
            queries: BTreeMap::new(),
            live: BTreeSet::new(),
            live_at_version,
            // Arbitrary, non-contiguous ids, as a reconnecting client would send.
            next_query_id: 1000 * id as u64 + 7,
            next_request_id: 0,
            query_set_version: 0,
            identity_version: 0,
            outstanding_mutations: BTreeSet::new(),
            uncovered: Vec::new(),
            log: ClientLog {
                states: Vec::new(),
                queries: BTreeMap::new(),
                final_results: BTreeMap::new(),
                response_first: 0,
                transition_first: 0,
                diffs: 0,
            },
            dumps,
        }
    }

    async fn send(&self, m: ClientMessage) {
        self.tx.send(Inbound::Message(m)).await.unwrap();
    }

    async fn modify(&mut self, modifications: Vec<QuerySetModification>) {
        for m in &modifications {
            match m {
                QuerySetModification::Add { query_id, .. } => self.live.insert(*query_id),
                QuerySetModification::Remove { query_id } => self.live.remove(query_id),
            };
        }
        let base = self.query_set_version;
        self.query_set_version += 1;
        self.live_at_version.insert(self.query_set_version, self.live.clone());
        self.send(ClientMessage::ModifyQuerySet { base_version: base, new_version: base + 1, modifications }).await;
    }

    fn add_mod(&mut self, q: Q) -> QuerySetModification {
        let query_id = self.next_query_id;
        self.next_query_id += 1 + (query_id % 3);
        let (udf_path, args) = q.call();
        self.queries.insert(query_id, q);
        QuerySetModification::Add { query_id, udf_path, args }
    }

    async fn step(&mut self, rng: &mut StdRng) {
        match rng.random_range(0..20) {
            0..=7 => {
                let mut mods = vec![self.add_mod(Q::random(rng))];
                if rng.random_bool(0.3) {
                    mods.push(self.add_mod(Q::random(rng)));
                }
                if rng.random_bool(0.3) && !self.live.is_empty() {
                    let victim = *self.live.iter().nth(rng.random_range(0..self.live.len())).unwrap();
                    mods.push(QuerySetModification::Remove { query_id: victim });
                }
                self.modify(mods).await;
            }
            8..=10 if !self.live.is_empty() => {
                let victim = *self.live.iter().nth(rng.random_range(0..self.live.len())).unwrap();
                self.modify(vec![QuerySetModification::Remove { query_id: victim }]).await;
            }
            11 => {
                let base = self.identity_version;
                self.identity_version += 1;
                self.send(ClientMessage::Authenticate { base_version: base, token: None }).await;
            }
            _ => {
                let (udf_path, args) = random_mutation(rng);
                let request_id = self.next_request_id;
                self.next_request_id += 1;
                self.outstanding_mutations.insert(request_id);
                self.send(ClientMessage::Mutation { request_id, udf_path, args }).await;
            }
        }
    }

    fn handle(&mut self, msg: ServerMessage) {
        self.log.diffs += crate::common::check_wire_diffs(&msg, &self.results);
        match msg {
            ServerMessage::Transition { start_version, end_version, modifications } => {
                assert_eq!(start_version, self.version, "client {}: startVersion must equal current version", self.id);
                assert!(end_version.ts >= start_version.ts, "ts went backwards");
                assert!(end_version.query_set >= start_version.query_set);
                assert!(
                    end_version.identity >= start_version.identity && end_version.identity <= self.identity_version
                );
                for m in modifications {
                    match m {
                        StateModification::QueryUpdated { query_id, value, .. } => {
                            self.results.insert(query_id, Ok(value.value().clone()));
                        }
                        StateModification::QueryFailed { query_id, error, .. } => {
                            self.results.insert(query_id, Err(error.message));
                        }
                        StateModification::QueryRemoved { query_id } => {
                            assert!(self.results.remove(&query_id).is_some(), "removed a query without results");
                        }
                    }
                }
                self.version = end_version;
                let expected_live = &self.live_at_version[&end_version.query_set];
                let have: BTreeSet<u64> = self.results.keys().copied().collect();
                assert_eq!(
                    &have, expected_live,
                    "client {}: result set != live set at querySet {}",
                    self.id, end_version.query_set
                );
                self.log.states.push((end_version.ts, self.results.clone()));
                self.uncovered.retain(|m| *m > end_version.ts);
            }
            ServerMessage::MutationResponse { request_id, result, .. } => {
                assert!(self.outstanding_mutations.remove(&request_id));
                let (dump, ts) = result.expect("test mutations succeed");
                self.dumps.record(ts, dump);
                if self.version.ts >= ts {
                    self.log.transition_first += 1;
                } else {
                    self.log.response_first += 1;
                    self.uncovered.push(ts);
                }
            }
            ServerMessage::Ping => {}
            other => panic!("client {}: unexpected {other:?}", self.id),
        }
    }

    fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(msg);
        }
    }

    /// Process messages until `cond` holds (panics after a timeout).
    async fn until(&mut self, what: &str, cond: impl Fn(&Client) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !cond(self) {
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Some(msg)) => self.handle(msg),
                Ok(None) => panic!("client {}: worker stopped", self.id),
                Err(_) => panic!("client {}: timed out waiting for {what}", self.id),
            }
        }
    }

    /// Process messages until none arrive for `quiet`.
    async fn settle(&mut self, quiet: Duration) {
        while let Ok(Some(msg)) = tokio::time::timeout(quiet, self.rx.recv()).await {
            self.handle(msg);
        }
    }
}

async fn run_client(
    id: usize,
    app: App,
    dumps: Arc<Dumps>,
    seed: u64,
    steps_done: mpsc::Sender<()>,
    mut quiesce: watch::Receiver<bool>,
) -> ClientLog {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut c = Client::new(id, app.clone(), dumps.clone());
    c.send(ClientMessage::Connect {
        session_id: format!("client-{id}"),
        connection_count: 0,
        last_close_reason: None,
        client_version: Some("test".into()),
        features: vec!["diffs".into()],
    })
    .await;
    // Start with two slow queries of different durations (later steps may remove them).
    let slow =
        [c.add_mod(Q::Slow(id as i64 % 2, SLOW_MS[3])), c.add_mod(Q::Slow((id as i64 + 1) % 2, SLOW_MS[id % 3]))];
    c.modify(slow.into()).await;
    for _ in 0..CLIENT_STEPS {
        c.step(&mut rng).await;
        c.drain();
        if rng.random_bool(0.3) {
            tokio::time::sleep(Duration::from_millis(rng.random_range(0..3))).await;
        }
    }
    // All own mutations answered, and each answer covered by a Transition (read-your-writes).
    c.until("mutation responses", |c| c.outstanding_mutations.is_empty()).await;
    c.until("covering transitions", |c| c.uncovered.is_empty()).await;
    let qs = c.query_set_version;
    c.until("last query set", move |c| c.version.query_set == qs).await;
    let identity = c.identity_version;
    c.until("last identity", move |c| c.version.identity == identity).await;
    steps_done.send(()).await.unwrap();

    // Keep processing until every writer is done.
    loop {
        tokio::select! {
            _ = quiesce.changed() => if *quiesce.borrow() { break },
            msg = c.rx.recv() => c.handle(msg.expect("worker alive")),
        }
    }
    let final_ts = app.db().write_log().latest();
    // Without prompting, results must converge to the final state.
    c.settle(Duration::from_millis(300)).await;
    let model = dumps.at(final_ts);
    for (qid, result) in &c.results {
        assert_eq!(
            result,
            &c.queries[qid].eval(&model),
            "client {id}: query {qid} ({:?}) is stale after quiescence",
            c.queries[qid]
        );
    }
    // A probe ModifyQuerySet is answered at exactly the final ts.
    let probe = c.add_mod(Q::CountAll);
    c.modify(vec![probe]).await;
    let qs = c.query_set_version;
    c.until("probe", move |c| c.version.query_set == qs).await;
    assert_eq!(c.version.ts, final_ts);

    c.log.final_results = c.results.clone();
    c.log.queries = std::mem::take(&mut c.queries);
    c.log
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn model_based_reactivity() {
    let seed: u64 = std::env::var("RXFERRIX_TEST_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or_else(rand::random);
    println!("model test seed: {seed} (set RXFERRIX_TEST_SEED to reproduce the random choices)");

    let t = TestApp::new().await.unwrap();
    let app = t.app().clone();
    let dumps = Arc::new(Dumps::default());
    // The app's tables are empty at every ts startup committed (e.g. the reconcile of rxferrix's system crons).
    for ts in 0..=app.db().write_log().latest() {
        dumps.record(ts, json!([]));
    }

    let (steps_tx, mut steps_rx) = mpsc::channel(CLIENTS);
    let (quiesce_tx, quiesce_rx) = watch::channel(false);
    let mut clients = Vec::new();
    for i in 0..CLIENTS {
        clients.push(tokio::spawn(run_client(
            i,
            app.clone(),
            dumps.clone(),
            seed.wrapping_add(i as u64),
            steps_tx.clone(),
            quiesce_rx.clone(),
        )));
    }
    let mut writers = Vec::new();
    for w in 0..WRITERS {
        let (app, dumps) = (app.clone(), dumps.clone());
        let mut rng = StdRng::seed_from_u64(seed.wrapping_add(100 + w as u64));
        writers.push(tokio::spawn(async move {
            for _ in 0..WRITER_STEPS {
                let (path, args) = random_mutation(&mut rng);
                let (dump, ts) = app.run_mutation(&path, args, &Identity::default()).await.unwrap();
                dumps.record(ts, dump);
                if rng.random_bool(0.5) {
                    tokio::time::sleep(Duration::from_millis(rng.random_range(0..4))).await;
                }
            }
        }));
    }
    for w in writers {
        w.await.unwrap();
    }
    for _ in 0..CLIENTS {
        steps_rx.recv().await.unwrap();
    }
    // Identical subscriptions across clients share one execution.
    let stats = app.subscription_stats();
    assert!(stats.shared_queries < stats.subscriptions, "overlapping query sets were deduplicated: {stats:?}");
    quiesce_tx.send(true).unwrap();

    let mut logs = Vec::new();
    for c in clients {
        logs.push(c.await.unwrap());
    }
    // Every client has disconnected: nothing is left behind.
    crate::shared::assert_manager_empty(&app).await;
    println!("subscription manager while all clients were live: {stats:?}; finally {:?}", app.subscription_stats());

    // Every state a client ever held must equal the model at that Transition's ts.
    let final_ts = app.db().write_log().latest();
    let mut transitions = 0;
    let (mut response_first, mut transition_first, mut diffs) = (0, 0, 0);
    for (i, log) in logs.iter().enumerate() {
        for (ts, results) in &log.states {
            let model = dumps.at(*ts);
            for (qid, result) in results {
                let expected = log.queries[qid].eval(&model);
                assert_eq!(result, &expected, "client {i}: query {qid} ({:?}) wrong at ts {ts}", log.queries[qid]);
            }
            transitions += 1;
        }
        let model = dumps.at(final_ts);
        for (qid, result) in &log.final_results {
            assert_eq!(result, &log.queries[qid].eval(&model));
        }
        response_first += log.response_first;
        transition_first += log.transition_first;
        diffs += log.diffs;
    }
    println!(
        "verified {transitions} transitions over {final_ts} commits; mutation responses: {response_first} before \
         their covering transition, {transition_first} after; {diffs} results arrived as diffs"
    );
    assert!(diffs > 0, "result diffs were exercised");
    assert!(final_ts > (WRITERS * WRITER_STEPS / 2) as u64, "enough commits happened");
    assert!(response_first > 0, "the response-before-transition order was exercised");
    assert!(transitions > CLIENTS * 20);
    // The slow queries made runs overlap commits.
    let stats = app.subscription_stats();
    assert!(stats.stale_runs > 0, "no run finished invalidated: {stats:?}");
    assert_eq!(t.app().db().verify_indexes().await.unwrap(), Vec::<String>::new());
}
