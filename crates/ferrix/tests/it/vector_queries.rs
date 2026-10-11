//! Vector search in queries (`ctx.db.vector_search`): exact and deterministic (also on HNSW indexes), reactive with
//! read sets narrowed per filter value, live over WebSocket. And tunable HNSW parameters: they reach the graph, and
//! changing them rebuilds it (and its saved file).

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::Duration;

use ferrix::db::Database;
use ferrix::db::readset::ReadSet;
use ferrix::testing::TestApp;
use ferrix::writelog::CommitEntry;
use ferrix::{App, AppOptions, FunctionRef, HnswParams, Identity, Table, TableDef};
use futures::{SinkExt, StreamExt};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMessage;

pub mod vq {
    use ferrix::prelude::*;

    pub const DIMS: usize = 4;

    #[table(name = "vq_items")]
    #[vector_index(by_vec, field = vec, dims = 4, filter = [group, tier])]
    pub struct Item {
        pub name: String,
        pub group: String,
        pub tier: i64,
        pub vec: Option<Vec<f32>>,
    }

    /// An HNSW index with its own parameters, searched exactly by queries.
    #[table(name = "vq_ann")]
    #[vector_index(by_vec, field = vec, dims = 4, filter = [group], kind = "hnsw", m = 8, ef_construction = 64, ef_search = 40)]
    pub struct AnnItem {
        pub group: String,
        pub vec: Vec<f32>,
    }

    #[object]
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct VqHit {
        pub id: Id<Item>,
        pub score: f32,
    }

    fn opts(limit: u32, groups: Option<Vec<String>>, tier: Option<i64>) -> VectorSearchOpts<Item> {
        let mut filter = groups.map(|g| VectorFilter::any_of(Item::group, g));
        if let Some(t) = tier {
            let by_tier = VectorFilter::eq(Item::tier, t);
            filter = Some(match filter {
                Some(f) => f.and(by_tier),
                None => by_tier,
            });
        }
        let opts = VectorSearchOpts::new(limit as usize);
        match filter {
            Some(f) => opts.filter(f),
            None => opts,
        }
    }

    /// The nearest items as `(id, score)`, optionally in some groups and one tier.
    #[query]
    pub async fn nearest(
        ctx: &QueryCtx,
        vector: Vec<f32>,
        limit: u32,
        groups: Option<Vec<String>>,
        tier: Option<i64>,
    ) -> Result<Vec<VqHit>> {
        let hits = ctx.db.vector_search(Item::by_vec, &vector, opts(limit, groups, tier)).await?;
        Ok(hits.into_iter().map(|(id, score)| VqHit { id, score }).collect())
    }

    /// The names of the nearest items (loads the documents: also reads their `by_id` points).
    #[query]
    pub async fn nearest_names(
        ctx: &QueryCtx,
        vector: Vec<f32>,
        limit: u32,
        group: Option<String>,
    ) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for (id, _) in ctx.db.vector_search(Item::by_vec, &vector, opts(limit, group.map(|g| vec![g]), None)).await? {
            if let Some(doc) = ctx.db.get(id).await? {
                out.push(doc.value.name);
            }
        }
        Ok(out)
    }

    /// The same search from an action (exact or not).
    #[action]
    pub async fn nearest_action(
        ctx: &ActionCtx,
        vector: Vec<f32>,
        limit: u32,
        groups: Option<Vec<String>>,
        tier: Option<i64>,
    ) -> Result<Vec<VqHit>> {
        let hits = ctx.vector_search(Item::by_vec, &vector, opts(limit, groups, tier)).await?;
        Ok(hits.into_iter().map(|(id, score)| VqHit { id, score }).collect())
    }

    /// A query over the HNSW index (always exact) as ids.
    #[query]
    pub async fn ann_nearest(ctx: &QueryCtx, vector: Vec<f32>, limit: u32) -> Result<Vec<Id<AnnItem>>> {
        let opts = VectorSearchOpts::new(limit as usize).ef(1);
        Ok(ctx.db.vector_search(AnnItem::by_vec, &vector, opts).await?.into_iter().map(|(id, _)| id).collect())
    }

    /// The action over the HNSW index: the graph (with an optional `ef`), or exact.
    #[action]
    pub async fn ann_nearest_action(
        ctx: &ActionCtx,
        vector: Vec<f32>,
        limit: u32,
        exact: bool,
        ef: Option<u32>,
    ) -> Result<Vec<Id<AnnItem>>> {
        let mut opts = VectorSearchOpts::new(limit as usize);
        if exact {
            opts = opts.exact();
        }
        if let Some(ef) = ef {
            opts = opts.ef(ef as usize);
        }
        Ok(ctx.vector_search(AnnItem::by_vec, &vector, opts).await?.into_iter().map(|(id, _)| id).collect())
    }

    /// A mutation searching inside its own transaction (sees its own insert).
    #[mutation]
    pub async fn add_and_search(ctx: &MutationCtx, name: String, vec: Vec<f32>) -> Result<Option<Id<Item>>> {
        let id = ctx.db.insert(Item { name, group: "m".into(), tier: 0, vec: Some(vec.clone()) }).await?;
        let opts = VectorSearchOpts::new(1).filter(VectorFilter::eq(Item::group, "m".to_string()));
        let hits = ctx.db.vector_search(Item::by_vec, &vec, opts).await?;
        Ok(hits.first().map(|(found, _)| *found).filter(|found| *found == id))
    }

    #[mutation]
    pub async fn add(
        ctx: &MutationCtx,
        name: String,
        group: String,
        tier: i64,
        vec: Option<Vec<f32>>,
    ) -> Result<Id<Item>> {
        ctx.db.insert(Item { name, group, tier, vec }).await
    }

    #[mutation]
    pub async fn set_vec(ctx: &MutationCtx, id: Id<Item>, vec: Option<Vec<f32>>) -> Result<()> {
        ctx.db.patch(id, json!({ "vec": vec })).await
    }

    #[mutation]
    pub async fn set_group(ctx: &MutationCtx, id: Id<Item>, group: String) -> Result<()> {
        ctx.db.patch(id, json!({ "group": group })).await
    }

    #[mutation]
    pub async fn rename(ctx: &MutationCtx, id: Id<Item>, name: String) -> Result<()> {
        ctx.db.patch(id, json!({ "name": name })).await
    }

    #[mutation]
    pub async fn remove(ctx: &MutationCtx, id: Id<Item>) -> Result<()> {
        ctx.db.delete(id).await
    }

    #[mutation]
    pub async fn add_ann(ctx: &MutationCtx, items: Vec<(String, Vec<f32>)>) -> Result<Vec<Id<AnnItem>>> {
        let mut ids = Vec::new();
        for (group, vec) in items {
            ids.push(ctx.db.insert(AnnItem { group, vec }).await?);
        }
        Ok(ids)
    }
}

use vq::VqHit;

fn random_vector(rng: &mut StdRng) -> Vec<f32> {
    (0..vq::DIMS).map(|_| rng.random_range(-1.0f32..1.0)).collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb)
}

fn commit_at(t: &TestApp, ts: u64) -> std::sync::Arc<CommitEntry> {
    let entries = t.app().db().write_log().entries_between(ts - 1, ts).unwrap();
    assert_eq!(entries.len(), 1);
    entries[0].clone()
}

async fn run(t: &TestApp, args: Value) -> (Value, ReadSet) {
    let r = t.app().run_query(&vq::nearest::path(), args, &Identity::default()).await;
    (r.result.unwrap(), r.read_set)
}

fn args(v: &[f32], limit: u32, groups: Option<&[&str]>, tier: Option<i64>) -> Value {
    json!({ "vector": v, "limit": limit, "groups": groups, "tier": tier })
}

fn s(v: &str) -> String {
    v.to_string()
}

/// Exact results (equal to brute force and to the action's exact search), deterministic, also on an HNSW index,
/// and inside a mutation's transaction.
#[tokio::test]
async fn query_vector_search_is_exact_and_deterministic() {
    let t = TestApp::new().await.unwrap();
    let mut rng = StdRng::seed_from_u64(21);
    let groups = ["a", "b", "c"];
    let mut items = Vec::new();
    for i in 0..300 {
        let (g, tier, v) = (groups[i % 3], (i % 4) as i64, random_vector(&mut rng));
        let id = t.mutation::<vq::add>((format!("i{i}"), s(g), tier, Some(v.clone()))).await.unwrap();
        items.push((id, g, tier, v));
    }
    t.mutation::<vq::add>((s("no vector"), s("a"), 0, None)).await.unwrap();
    for round in 0..12 {
        let q = random_vector(&mut rng);
        let gs: Option<Vec<String>> = match round % 3 {
            0 => None,
            1 => Some(vec![s("a")]),
            _ => Some(vec![s("a"), s("c")]),
        };
        let tier = (round % 2 == 0).then_some(1);
        let limit = [1u32, 7, 30][round % 3];
        let got: Vec<VqHit> = t.query::<vq::nearest>((q.clone(), limit, gs.clone(), tier)).await.unwrap();
        // Brute force.
        let mut expected: Vec<(ferrix::Id<vq::Item>, f32)> = items
            .iter()
            .filter(|(_, g, t, _)| {
                gs.as_ref().is_none_or(|gs| gs.iter().any(|x| x == g)) && tier.is_none_or(|x| x == *t)
            })
            .map(|(id, _, _, v)| (*id, cosine(&q, v)))
            .collect();
        expected.sort_by(|a, b| b.1.total_cmp(&a.1));
        expected.truncate(limit as usize);
        assert_eq!(got.len(), expected.len(), "round {round}");
        for (h, (id, score)) in got.iter().zip(&expected) {
            assert!((h.score - score).abs() < 1e-4, "round {round}");
            assert_eq!(h.id, *id, "round {round}");
        }
        // The action's exact search agrees, and repeated runs are identical.
        let action: Vec<VqHit> = t.action::<vq::nearest_action>((q.clone(), limit, gs.clone(), tier)).await.unwrap();
        assert_eq!(action, got, "round {round}");
        for _ in 0..3 {
            assert_eq!(t.query::<vq::nearest>((q.clone(), limit, gs.clone(), tier)).await.unwrap(), got);
        }
    }

    // On an HNSW index, a query searches exactly (whatever `ef` says): the same as the action's exact search.
    let ann: Vec<(String, Vec<f32>)> = (0..600).map(|i| (format!("g{}", i % 2), random_vector(&mut rng))).collect();
    t.mutation::<vq::add_ann>((ann,)).await.unwrap();
    for _ in 0..10 {
        let q = random_vector(&mut rng);
        let exact = t.action::<vq::ann_nearest_action>((q.clone(), 10, true, None)).await.unwrap();
        let first = t.query::<vq::ann_nearest>((q.clone(), 10)).await.unwrap();
        assert_eq!(first, exact);
        assert_eq!(t.query::<vq::ann_nearest>((q.clone(), 10)).await.unwrap(), first);
    }

    // In a mutation: sees its own (uncommitted) insert.
    let v = random_vector(&mut rng);
    assert!(t.mutation::<vq::add_and_search>((s("mine"), v)).await.unwrap().is_some());

    // Errors as in actions.
    let err = t.query::<vq::nearest>((vec![1.0, 2.0], 3, None, None)).await.unwrap_err();
    assert!(format!("{err:#}").contains("has 2 dimensions"), "{err:#}");
    assert!(t.query::<vq::nearest>((random_vector(&mut rng), 0, None, None)).await.is_err());
}

/// The read set: per filter value (the lowest filter column of each `or` branch), the whole index without a filter,
/// nothing for a filter that matches nothing; no `by_id` points.
#[tokio::test]
async fn read_sets_narrow_per_filter_value() {
    let t = TestApp::new().await.unwrap();
    let mut rng = StdRng::seed_from_u64(3);
    let a = t.mutation::<vq::add>((s("a1"), s("a"), 1, Some(random_vector(&mut rng)))).await.unwrap();
    let b = t.mutation::<vq::add>((s("b1"), s("b"), 1, Some(random_vector(&mut rng)))).await.unwrap();
    let index_id = t.app().db().catalog().table("vq_items").unwrap().search_indexes[0].id;
    let q = random_vector(&mut rng);

    let (_, in_a) = run(&t, args(&q, 5, Some(&["a"]), None)).await;
    let ranges: Vec<_> = in_a.ranges().collect();
    assert_eq!(ranges.len(), 1, "{in_a:?}");
    assert_eq!(ranges[0].0, index_id);
    assert!(ranges[0].1.start[0] == 0 && ranges[0].1.end.is_some(), "the prefix of group (column 0) = a");
    let invalidates = |rs: &ReadSet, ts: u64| rs.overlaps(&commit_at(&t, ts));

    // Writes in group b never invalidate it: inserts, vector changes, deletes.
    let (_, ts) = t.mutation_ts::<vq::add>((s("b2"), s("b"), 1, Some(random_vector(&mut rng)))).await.unwrap();
    assert!(!invalidates(&in_a, ts));
    let (_, ts) = t.mutation_ts::<vq::set_vec>((b, Some(random_vector(&mut rng)))).await.unwrap();
    assert!(!invalidates(&in_a, ts));
    // Writes in group a do: an insert (a nearer vector could be one), a vector change, removing the vector.
    let (_, ts) = t.mutation_ts::<vq::add>((s("a2"), s("a"), 2, Some(random_vector(&mut rng)))).await.unwrap();
    assert!(invalidates(&in_a, ts));
    let (_, ts) = t.mutation_ts::<vq::set_vec>((a, Some(random_vector(&mut rng)))).await.unwrap();
    assert!(invalidates(&in_a, ts));
    // A document moving into group a (its new value) or out of it (its old value).
    let (_, ts) = t.mutation_ts::<vq::set_group>((b, s("a"))).await.unwrap();
    assert!(invalidates(&in_a, ts));
    let (_, in_a) = run(&t, args(&q, 5, Some(&["a"]), None)).await;
    let (_, ts) = t.mutation_ts::<vq::set_group>((b, s("b"))).await.unwrap();
    assert!(invalidates(&in_a, ts));
    // Fields outside the index don't (the results are ids and scores), nor documents without a vector.
    let (_, ts) = t.mutation_ts::<vq::rename>((a, s("renamed"))).await.unwrap();
    assert!(!invalidates(&in_a, ts));
    let (_, ts) = t.mutation_ts::<vq::add>((s("a-novec"), s("a"), 1, None)).await.unwrap();
    assert!(!invalidates(&in_a, ts));
    let (_, ts) = t.mutation_ts::<vq::remove>((a,)).await.unwrap();
    assert!(invalidates(&in_a, ts), "deleting an indexed document");

    // `any_of`: one prefix per value; a group outside it is ignored.
    let (_, a_or_c) = run(&t, args(&q, 5, Some(&["a", "c"]), None)).await;
    assert_eq!(a_or_c.ranges().count(), 2);
    let (_, ts) = t.mutation_ts::<vq::add>((s("c1"), s("c"), 1, Some(random_vector(&mut rng)))).await.unwrap();
    assert!(invalidates(&a_or_c, ts));
    let (_, ts) = t.mutation_ts::<vq::add>((s("b3"), s("b"), 1, Some(random_vector(&mut rng)))).await.unwrap();
    assert!(!invalidates(&a_or_c, ts));
    // Only a tier: the prefix of the tier column (1).
    let (_, tier2) = run(&t, args(&q, 5, None, Some(2))).await;
    assert_eq!(tier2.ranges().next().unwrap().1.start[0], 1);
    let (_, ts) = t.mutation_ts::<vq::add>((s("t1"), s("b"), 1, Some(random_vector(&mut rng)))).await.unwrap();
    assert!(!invalidates(&tier2, ts));
    // Unfiltered: the whole index, so any indexed write invalidates it.
    let (_, all) = run(&t, args(&q, 5, None, None)).await;
    let r = all.ranges().next().unwrap().1;
    assert!(r.start.is_empty() && r.end.is_none());
    let (_, ts) = t.mutation_ts::<vq::add>((s("z"), s("z"), 9, Some(random_vector(&mut rng)))).await.unwrap();
    assert!(invalidates(&all, ts));
    // A filter that can match nothing reads nothing.
    let (result, none) = run(&t, args(&q, 5, Some(&[]), None)).await;
    assert_eq!(result, json!([]));
    assert!(none.is_empty());
}

/// Randomized soundness: after every random write, every query whose read set the commit doesn't touch still returns
/// exactly what a fresh run returns.
#[tokio::test]
async fn filtered_invalidation_never_misses_a_change() {
    let t = TestApp::new().await.unwrap();
    let mut rng = StdRng::seed_from_u64(17);
    let groups = ["g0", "g1", "g2"];
    let mut ids = Vec::new();
    for i in 0..40 {
        let g = groups[i % 3];
        ids.push(
            t.mutation::<vq::add>((format!("n{i}"), s(g), (i % 2) as i64, Some(random_vector(&mut rng))))
                .await
                .unwrap(),
        );
    }
    let q = random_vector(&mut rng);
    let queries = [
        args(&q, 3, None, None),
        args(&q, 3, Some(&["g0"]), None),
        args(&q, 5, Some(&["g1", "g2"]), None),
        args(&q, 2, Some(&["g2"]), Some(1)),
        args(&q, 4, None, Some(0)),
    ];
    let mut state = Vec::new();
    for a in &queries {
        state.push(run(&t, a.clone()).await);
    }
    let mut skipped = 0;
    for step in 0..250 {
        let id = ids[rng.random_range(0..ids.len())];
        let ts = match rng.random_range(0..6) {
            0 => t.mutation_ts::<vq::set_vec>((id, Some(random_vector(&mut rng)))).await.unwrap().1,
            1 => t.mutation_ts::<vq::set_vec>((id, None)).await.unwrap().1,
            2 => t.mutation_ts::<vq::set_group>((id, s(groups[rng.random_range(0..3)]))).await.unwrap().1,
            3 => t.mutation_ts::<vq::rename>((id, format!("r{step}"))).await.unwrap().1,
            _ => {
                let g = s(groups[rng.random_range(0..3)]);
                let (new, ts) = t
                    .mutation_ts::<vq::add>((
                        format!("s{step}"),
                        g,
                        rng.random_range(0..2),
                        Some(random_vector(&mut rng)),
                    ))
                    .await
                    .unwrap();
                ids.push(new);
                ts
            }
        };
        let commit = commit_at(&t, ts);
        for (a, (result, rs)) in queries.iter().zip(state.iter_mut()) {
            let fresh = run(&t, a.clone()).await;
            if rs.overlaps(&commit) {
                *result = fresh.0;
                *rs = fresh.1;
            } else {
                skipped += 1;
                assert_eq!(*result, fresh.0, "step {step}: {a} changed but was not invalidated");
            }
        }
    }
    assert!(skipped > 300, "filters should spare many reruns ({skipped})");
}

/// Minimal sync-protocol client: subscribe to one query and wait for its updates.
struct Ws(tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>);

impl Ws {
    async fn connect(addr: SocketAddr) -> Ws {
        let (stream, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/sync")).await.unwrap();
        let mut ws = Ws(stream);
        ws.send(json!({ "type": "Connect", "sessionId": "vector-test", "connectionCount": 0, "lastCloseReason": null, "clientVersion": "0.1.0" })).await;
        ws
    }

    async fn send(&mut self, v: Value) {
        self.0.send(WsMessage::text(v.to_string())).await.unwrap();
    }

    /// The next value pushed for query 0, or `None` if none arrives within `wait`.
    async fn next_value(&mut self, wait: Duration) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let msg = tokio::time::timeout_at(deadline, self.0.next()).await.ok()?.unwrap().unwrap();
            let WsMessage::Text(text) = msg else { continue };
            let v: Value = serde_json::from_str(text.as_str()).unwrap();
            if v["type"] != "Transition" {
                continue;
            }
            if let Some(m) = v["modifications"].as_array().unwrap().iter().find(|m| m["queryId"] == 0) {
                assert_eq!(m["type"], "QueryUpdated", "{m}");
                return Some(m["value"].clone());
            }
        }
    }
}

fn names(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect()
}

/// A subscribed query reruns when a nearer vector is inserted in its group (and not for other groups).
#[tokio::test]
async fn subscriptions_follow_nearer_vectors() {
    let t = TestApp::new().await.unwrap();
    let addr = t.serve().await.unwrap();
    let q = vec![1.0f32, 0.0, 0.0, 0.0];
    t.mutation::<vq::add>((s("far"), s("live"), 0, Some(vec![0.0, 1.0, 0.0, 0.0]))).await.unwrap();
    let mid = t.mutation::<vq::add>((s("mid"), s("live"), 0, Some(vec![1.0, 1.0, 0.0, 0.0]))).await.unwrap();
    let mut ws = Ws::connect(addr).await;
    ws.send(json!({ "type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 1,
        "modifications": [{ "type": "Add", "queryId": 0, "udfPath": vq::nearest_names::path(),
                            "args": { "vector": q, "limit": 2, "group": "live" } }] }))
        .await;
    let wait = Duration::from_secs(5);
    assert_eq!(names(&ws.next_value(wait).await.unwrap()), vec!["mid", "far"]);

    // A nearer vector in the group: pushed, first.
    t.mutation::<vq::add>((s("near"), s("live"), 0, Some(vec![1.0, 0.1, 0.0, 0.0]))).await.unwrap();
    assert_eq!(names(&ws.next_value(wait).await.unwrap()), vec!["near", "mid"]);
    // An even nearer one in another group changes nothing (no update arrives); then one in the group does.
    t.mutation::<vq::add>((s("other"), s("elsewhere"), 0, Some(q.clone()))).await.unwrap();
    assert!(ws.next_value(Duration::from_millis(300)).await.is_none(), "a write in another group reran the query");
    t.mutation::<vq::add>((s("exact"), s("live"), 0, Some(q.clone()))).await.unwrap();
    assert_eq!(names(&ws.next_value(wait).await.unwrap()), vec!["exact", "near"]);
    // Moving a vector away, and renaming a returned document (its by_id point), are pushed too.
    t.mutation::<vq::set_vec>((mid, Some(vec![1.0, 0.0, 0.0, 0.0]))).await.unwrap();
    let v = names(&ws.next_value(wait).await.unwrap());
    assert!(v.contains(&s("mid")), "{v:?}");
    t.mutation::<vq::rename>((mid, s("mid2"))).await.unwrap();
    let v = names(&ws.next_value(wait).await.unwrap());
    assert!(v.contains(&s("mid2")), "{v:?}");
}

// ------------------------------------------------------------------------------------------------ HNSW params

async fn ann_index(t: &TestApp, table: &str) -> std::sync::Arc<ferrix::db::search::ann::AnnIndex> {
    let id = t.app().db().catalog().table(table).unwrap().search_indexes[0].id;
    let index = t.app().db().ann().index(id).expect("an HNSW index").clone();
    assert!(index.wait_ready(Duration::from_secs(60)).await, "graph not ready");
    index
}

/// `m`, `ef_construction` and `ef_search` reach the graph; invalid values are refused at startup; `ef` per search is
/// validated.
#[tokio::test]
async fn hnsw_parameters_reach_the_graph() {
    let t = TestApp::new().await.unwrap();
    let mut rng = StdRng::seed_from_u64(8);
    let items: Vec<(String, Vec<f32>)> = (0..200).map(|i| (format!("g{}", i % 2), random_vector(&mut rng))).collect();
    t.mutation::<vq::add_ann>((items,)).await.unwrap();
    let index = ann_index(&t, "vq_ann").await;
    assert_eq!(index.params(), HnswParams { m: 8, ef_construction: 64, ef_search: 40 });
    assert_eq!(index.graph_params(), Some((8, 64)));
    let info = t.app().db().catalog().table("vq_ann").unwrap().search_indexes[0].clone();
    assert_eq!(info.hnsw_params(), Some(HnswParams { m: 8, ef_construction: 64, ef_search: 40 }));

    // The dashboard lists them.
    let admin = t.app().auth().create_admin(t.app(), "admin@example.com", "admin password".into()).await.unwrap();
    let tables =
        t.app().run_query("_system/dashboard:tables", json!({}), &Identity::admin(admin, None)).await.result.unwrap();
    let ann = tables.as_array().unwrap().iter().find(|t| t["name"] == "vq_ann").unwrap();
    assert_eq!(ann["searchIndexes"][0]["hnsw"], json!({ "m": 8, "efConstruction": 64, "efSearch": 40 }));

    // `ef` per search: in range, else an error.
    let q = random_vector(&mut rng);
    assert_eq!(t.action::<vq::ann_nearest_action>((q.clone(), 5, false, Some(200))).await.unwrap().len(), 5);
    for bad in [0, 5000] {
        let err = t.action::<vq::ann_nearest_action>((q.clone(), 5, false, Some(bad))).await.unwrap_err();
        assert!(format!("{err:#}").contains("ef must be between 1 and 4096"), "{err:#}");
    }

    // Out-of-range parameters, or parameters on an exact index, are refused at startup.
    let dir = tempfile::tempdir().unwrap();
    let with = |hnsw: HnswParams, kind: ferrix::VectorIndexKind| {
        let def = vq::AnnItem::table_def();
        TableDef { vector_indexes: vec![ferrix::VectorIndexDef { hnsw, kind, ..def.vector_indexes[0].clone() }], ..def }
    };
    let hnsw = ferrix::VectorIndexKind::Hnsw;
    for (params, kind, msg) in [
        (HnswParams { m: 2, ..HnswParams::DEFAULT }, hnsw, "`m` must be between 4 and 64"),
        (
            HnswParams { ef_construction: 4096, ..HnswParams::DEFAULT },
            hnsw,
            "`ef_construction` must be between 16 and 2048",
        ),
        (HnswParams { ef_search: 0, ..HnswParams::DEFAULT }, hnsw, "`ef_search` must be between 1 and 4096"),
        (HnswParams { m: 8, ..HnswParams::DEFAULT }, ferrix::VectorIndexKind::Exact, "only apply to kind = \"hnsw\""),
    ] {
        let err = Database::open(dir.path(), &[with(params, kind)], 1).err().expect("refused");
        assert!(format!("{err:#}").contains(msg), "{err:#}");
    }
}

/// Changing `m` or `ef_construction` rebuilds the graph and replaces its saved file (the vec0 table is kept);
/// changing only `ef_search` keeps the graph.
#[tokio::test]
async fn changing_hnsw_parameters_rebuilds_the_graph() {
    ferrix::db::search::ann::set_persist_threshold(1);
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let mut rng = StdRng::seed_from_u64(12);
    let items: Vec<(String, Vec<f32>)> = (0..400).map(|i| (format!("g{}", i % 2), random_vector(&mut rng))).collect();
    let (id, ids) = {
        let app = App::open(&dir, AppOptions::default()).await.unwrap();
        let ids = app.mutation::<vq::add_ann>((items.clone(),)).await.unwrap();
        let id = app.db().catalog().table("vq_ann").unwrap().search_indexes[0].id;
        let index = app.db().ann().index(id).unwrap().clone();
        assert!(index.wait_ready(Duration::from_secs(60)).await);
        index.save().unwrap();
        (id, ids)
    };
    let file = dir.join(format!("vector_index_{id}.hnsw"));
    let saved = std::fs::read(&file).unwrap();
    let def = vq::AnnItem::table_def();
    let with = |hnsw: HnswParams| TableDef {
        vector_indexes: vec![ferrix::VectorIndexDef { hnsw, ..def.vector_indexes[0].clone() }],
        ..def.clone()
    };

    // ef_search only: same graph (loaded from the file, as saved), the new default width.
    let db = Database::open(&dir, &[with(HnswParams { m: 8, ef_construction: 64, ef_search: 99 })], 1).unwrap();
    let index = db.ann().index(id).unwrap().clone();
    assert!(index.wait_ready(Duration::from_secs(60)).await);
    assert_eq!(index.params().ef_search, 99);
    assert_eq!(index.graph_params(), Some((8, 64)));
    assert_eq!(db.catalog().table("vq_ann").unwrap().search_indexes[0].id, id, "same index, vec0 kept");
    assert!(db.verify_indexes().await.unwrap().is_empty());
    drop(index);
    drop(db);

    // m: the saved file (built with m = 8) is discarded; the graph is rebuilt with m = 12 and saved again.
    let db = Database::open(&dir, &[with(HnswParams { m: 12, ef_construction: 64, ef_search: 40 })], 1).unwrap();
    let index = db.ann().index(id).unwrap().clone();
    assert!(index.wait_ready(Duration::from_secs(60)).await);
    assert_eq!(index.graph_params(), Some((12, 64)));
    assert_eq!(index.status().live, ids.len());
    assert_eq!(index.status().tombstones, 0);
    assert!(db.verify_indexes().await.unwrap().is_empty());
    let start = std::time::Instant::now();
    while std::fs::read(&file).map(|b| b == saved).unwrap_or(true) {
        assert!(start.elapsed() < Duration::from_secs(30), "the saved graph was not replaced");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(index);
    drop(db);

    // ef_construction too; and the rebuilt graph answers searches.
    let db = Database::open(&dir, &[with(HnswParams { m: 12, ef_construction: 32, ef_search: 40 })], 1).unwrap();
    let index = db.ann().index(id).unwrap().clone();
    assert!(index.wait_ready(Duration::from_secs(60)).await);
    assert_eq!(index.graph_params(), Some((12, 32)));
    assert!(db.verify_indexes().await.unwrap().is_empty());
}

/// A wider search finds more of the true nearest neighbours: recall@10 with `ef = 10` vs `ef = 400` on the HNSW
/// recall test's data shape (10 000 random 24-dimensional vectors, default parameters).
#[tokio::test]
async fn recall_improves_with_ef() {
    use super::search_pages::ann;
    let t = TestApp::new().await.unwrap();
    let mut rng = StdRng::seed_from_u64(42);
    let n = 10_000;
    for chunk in 0..5 {
        let items: Vec<(String, Vec<f32>)> = (0..n / 5)
            .map(|i| {
                (format!("c{}", (chunk + i) % 2), (0..ann::DIMS).map(|_| rng.random_range(-1.0f32..1.0)).collect())
            })
            .collect();
        t.mutation::<ann::add_many>((items,)).await.unwrap();
    }
    let id = t.app().db().catalog().table("ann_items").unwrap().search_indexes[0].id;
    assert!(t.app().db().ann().index(id).unwrap().wait_ready(Duration::from_secs(60)).await);
    let queries: Vec<Vec<f32>> =
        (0..60).map(|_| (0..ann::DIMS).map(|_| rng.random_range(-1.0f32..1.0)).collect()).collect();
    let mut recall = Vec::new();
    for ef in [10u32, 400] {
        let mut hits = 0;
        for q in &queries {
            let truth: HashSet<_> = t
                .action::<ann::nearest>((q.clone(), 10, None, true))
                .await
                .unwrap()
                .into_iter()
                .map(|h| h.id)
                .collect();
            let got = t.action::<ann::nearest_ef>((q.clone(), 10, ef)).await.unwrap();
            assert_eq!(got.len(), 10);
            hits += got.iter().filter(|h| truth.contains(&h.id)).count();
        }
        recall.push(hits as f64 / (queries.len() * 10) as f64);
    }
    println!("HNSW recall@10: ef=10 {:.3}, ef=400 {:.3}", recall[0], recall[1]);
    assert!(recall[1] > recall[0], "{recall:?}");
    assert!(recall[1] >= 0.97, "{recall:?}");
}

/// Dropping a `Database` stops its vector index thread and closes that thread's connection before returning: the
/// last connection to close checkpoints and removes the WAL, so the WAL is gone right after the drop, and reopening
/// at once never races with a lingering connection.
#[tokio::test]
async fn dropping_the_database_stops_the_vector_index_thread() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let mut rng = StdRng::seed_from_u64(30);
    {
        let app = App::open(dir, AppOptions::default()).await.unwrap();
        let items: Vec<(String, Vec<f32>)> =
            (0..50).map(|i| (format!("g{}", i % 2), random_vector(&mut rng))).collect();
        app.mutation::<vq::add_ann>((items,)).await.unwrap();
    }
    let defs = [vq::AnnItem::table_def()];
    for round in 0..20 {
        let db = Database::open(dir, &defs, 1).unwrap();
        let id = db.catalog().table("vq_ann").unwrap().search_indexes[0].id;
        let index = db.ann().index(id).unwrap().clone();
        assert!(index.wait_ready(Duration::from_secs(30)).await);
        assert_eq!(index.status().live, 50);
        drop(index);
        drop(db);
        assert!(!dir.join("data.db-wal").exists(), "round {round}: a connection outlived the database");
    }
}
