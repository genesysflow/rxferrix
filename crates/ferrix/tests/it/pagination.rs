//! `Query::paginate`: page walking in both orders with a filter, end-cursor pinning, cursor validation, the scan cap
//! and read sets. Uses the real macros over the `items` table (`by_group_score = [group, score]`).

use std::sync::Arc;

use ferrix::db::readset::ReadSet;
use ferrix::testing::TestApp;
use ferrix::writelog::CommitEntry;
use ferrix::{Doc, FunctionRef, Identity, PaginationOpts, PaginationResult};
use serde_json::{Value, json};

use crate::common::items::{self, Item};

pub mod pages {
    use ferrix::prelude::*;

    use crate::common::items::Item;

    /// Items of a group by score, optionally descending and filtered by a minimum score.
    #[query]
    pub async fn by_score(
        ctx: &QueryCtx,
        group: i64,
        desc: bool,
        min_score: Option<i64>,
        pagination_opts: PaginationOpts,
    ) -> Result<PaginationResult<Doc<Item>>> {
        let mut q = ctx.db.query::<Item>().with_index(Item::by_group_score, |r| r.eq(Item::group, group));
        if desc {
            q = q.order(Order::Desc);
        }
        if let Some(min) = min_score {
            q = q.filter(move |d| d.score >= min);
        }
        q.paginate(pagination_opts).await
    }

    /// Every item by creation time (the default index), newest first.
    #[query]
    pub async fn newest(ctx: &QueryCtx, pagination_opts: PaginationOpts) -> Result<PaginationResult<Doc<Item>>> {
        ctx.db.query::<Item>().order(Order::Desc).paginate(pagination_opts).await
    }

    /// Inserts `n` items with scores `start..start + n` in one transaction.
    #[mutation]
    pub async fn seed(ctx: &MutationCtx, group: i64, start: i64, n: i64) -> Result<()> {
        for score in start..start + n {
            ctx.db.insert(Item { group, score, name: format!("s{score}") }).await?;
        }
        Ok(())
    }

    #[mutation]
    pub async fn remove(ctx: &MutationCtx, id: Id<Item>) -> Result<()> {
        ctx.db.delete(id).await
    }
}

type Page = PaginationResult<Doc<Item>>;

async fn page(t: &TestApp, group: i64, desc: bool, min: Option<i64>, opts: PaginationOpts) -> ferrix::Result<Page> {
    t.query::<pages::by_score>((group, desc, min, opts)).await
}

fn scores(p: &Page) -> Vec<i64> {
    p.page.iter().map(|d| d.score).collect()
}

async fn seed(t: &TestApp, group: i64, scores: impl IntoIterator<Item = i64>) {
    for s in scores {
        t.mutation::<items::insert>((group, s, format!("g{group}s{s}"))).await.unwrap();
    }
}

/// Walks every page of `by_score` with `n` items per page; returns the pages' scores.
async fn walk(t: &TestApp, group: i64, desc: bool, min: Option<i64>, n: usize) -> Vec<Vec<i64>> {
    let mut out = Vec::new();
    let mut cursor = None;
    loop {
        let p = page(t, group, desc, min, PaginationOpts { num_items: n, cursor: cursor.clone(), end_cursor: None }).await.unwrap();
        assert!(p.page.len() <= n);
        out.push(scores(&p));
        if p.is_done {
            // The end cursor stays at the end.
            let again = page(t, group, desc, min, PaginationOpts::after(&p.continue_cursor, n)).await.unwrap();
            assert!(again.page.is_empty() && again.is_done);
            assert_eq!(again.continue_cursor, p.continue_cursor);
            return out;
        }
        assert!(out.len() < 100, "runaway pagination");
        cursor = Some(p.continue_cursor);
    }
}

#[tokio::test]
async fn walks_pages_in_both_orders_with_filter() {
    let t = TestApp::new().await.unwrap();
    seed(&t, 1, [5, 0, 9, 3, 7, 1, 8, 2, 6, 4]).await;
    seed(&t, 0, [1, 2]).await;
    seed(&t, 2, [1, 2]).await;

    let asc = walk(&t, 1, false, None, 3).await;
    assert_eq!(asc, vec![vec![0, 1, 2], vec![3, 4, 5], vec![6, 7, 8], vec![9]]);
    let desc = walk(&t, 1, true, None, 4).await;
    assert_eq!(desc, vec![vec![9, 8, 7, 6], vec![5, 4, 3, 2], vec![1, 0]]);
    // An exact multiple: the last full page is not known to be the last one, so an empty, done page follows.
    let exact = walk(&t, 1, false, None, 5).await;
    assert_eq!(exact, vec![vec![0, 1, 2, 3, 4], vec![5, 6, 7, 8, 9], vec![]]);

    // With a filter, pages hold `n` matches; rejected documents are skipped but count as scanned.
    let asc = walk(&t, 1, false, Some(4), 2).await;
    assert_eq!(asc, vec![vec![4, 5], vec![6, 7], vec![8, 9], vec![]]);
    let desc = walk(&t, 1, true, Some(4), 4).await;
    assert_eq!(desc, vec![vec![9, 8, 7, 6], vec![5, 4]]);
    let none = walk(&t, 1, true, Some(100), 4).await;
    assert_eq!(none, vec![Vec::<i64>::new()]);

    // An empty range is done right away.
    assert_eq!(walk(&t, 7, false, None, 3).await, vec![Vec::<i64>::new()]);

    // The default index (by creation time), descending.
    let p: Page = t.query::<pages::newest>((PaginationOpts::first(2),)).await.unwrap();
    assert_eq!(p.page.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["g2s2", "g2s1"]);
    assert!(!p.is_done);
}

#[tokio::test]
async fn end_cursor_pins_the_page() {
    let t = TestApp::new().await.unwrap();
    seed(&t, 1, [10, 20, 30, 40, 50, 60]).await;

    let p1 = page(&t, 1, false, None, PaginationOpts::first(3)).await.unwrap();
    assert_eq!(scores(&p1), [10, 20, 30]);
    let c1 = p1.continue_cursor.clone();
    let p2 = page(&t, 1, false, None, PaginationOpts::after(&c1, 3)).await.unwrap();
    assert_eq!(scores(&p2), [40, 50, 60]);

    // Pinned to its own continue cursor, page 1 returns the same documents, whatever numItems says.
    let pinned = PaginationOpts::first(1).with_end_cursor(&c1);
    let p1_pinned = page(&t, 1, false, None, pinned.clone()).await.unwrap();
    assert_eq!(p1_pinned, p1, "same page, same continue cursor");
    assert!(!p1_pinned.is_done);

    // Inserts into page 1: the unpinned page shifts (30 falls off the end), the pinned one grows.
    seed(&t, 1, [15, 25]).await;
    let shifted = page(&t, 1, false, None, PaginationOpts::first(3)).await.unwrap();
    assert_eq!(scores(&shifted), [10, 15, 20]);
    let p1_pinned = page(&t, 1, false, None, pinned.clone()).await.unwrap();
    assert_eq!(scores(&p1_pinned), [10, 15, 20, 25, 30]);
    assert_eq!(p1_pinned.continue_cursor, c1);
    // Page 2 is unaffected: no item is lost or duplicated between the pinned pages.
    let p2_again = page(&t, 1, false, None, PaginationOpts::after(&c1, 3)).await.unwrap();
    assert_eq!(p2_again, p2);

    // Deletes shrink the pinned page; the boundary stays.
    let id_20 = p1_pinned.page[2].id;
    t.mutation::<pages::remove>((id_20,)).await.unwrap();
    let id_30 = p1_pinned.page[4].id;
    t.mutation::<pages::remove>((id_30,)).await.unwrap();
    let p1_pinned = page(&t, 1, false, None, pinned.clone()).await.unwrap();
    assert_eq!(scores(&p1_pinned), [10, 15, 25]);
    // Inserting right at the boundary (after 30, before 40) lands on page 2.
    seed(&t, 1, [35]).await;
    assert_eq!(scores(&page(&t, 1, false, None, pinned).await.unwrap()), [10, 15, 25]);
    assert_eq!(scores(&page(&t, 1, false, None, PaginationOpts::after(&c1, 3)).await.unwrap()), [35, 40, 50]);

    // Pinning the last page to an end-of-range cursor returns everything after the start, and is done.
    let last = page(&t, 1, false, None, PaginationOpts::after(&c1, 10)).await.unwrap();
    assert!(last.is_done);
    let last_pinned =
        page(&t, 1, false, None, PaginationOpts::after(&c1, 1).with_end_cursor(&last.continue_cursor)).await.unwrap();
    assert_eq!(last_pinned, last);
    seed(&t, 1, [70]).await;
    let last_pinned =
        page(&t, 1, false, None, PaginationOpts::after(&c1, 1).with_end_cursor(&last.continue_cursor)).await.unwrap();
    assert_eq!(scores(&last_pinned), [35, 40, 50, 60, 70]);
    assert!(last_pinned.is_done);

    // Descending works the same way.
    let d1 = page(&t, 1, true, Some(30), PaginationOpts::first(2)).await.unwrap();
    assert_eq!(scores(&d1), [70, 60]);
    seed(&t, 1, [65, 5, 45]).await;
    let d1_pinned =
        page(&t, 1, true, Some(30), PaginationOpts::first(1).with_end_cursor(&d1.continue_cursor)).await.unwrap();
    assert_eq!(scores(&d1_pinned), [70, 65, 60]);
    let d2 = page(&t, 1, true, Some(30), PaginationOpts::after(&d1.continue_cursor, 10)).await.unwrap();
    assert_eq!(scores(&d2), [50, 45, 40, 35]);
    assert!(d2.is_done);
}

#[tokio::test]
async fn bad_cursors_are_rejected() {
    let t = TestApp::new().await.unwrap();
    seed(&t, 1, [1, 2, 3, 4]).await;
    seed(&t, 2, [1, 2, 3, 4]).await;
    let p1 = page(&t, 1, false, None, PaginationOpts::first(2)).await.unwrap();
    let c = p1.continue_cursor.clone();
    let err = |r: ferrix::Result<Page>| r.unwrap_err().to_string();

    assert!(err(page(&t, 1, false, None, PaginationOpts::after("garbage!", 2)).await).contains("malformed cursor"));
    assert!(err(page(&t, 1, false, None, PaginationOpts::after("AAAA", 2)).await).contains("malformed cursor"));
    assert!(
        err(page(&t, 1, false, None, PaginationOpts::first(2).with_end_cursor("nope")).await).contains("malformed endCursor")
    );
    // Same index, another range (group 2), another order, another index.
    let foreign = "does not belong to this query";
    assert!(err(page(&t, 2, false, None, PaginationOpts::after(&c, 2)).await).contains(foreign));
    assert!(err(page(&t, 1, true, None, PaginationOpts::after(&c, 2)).await).contains(foreign));
    let r: ferrix::Result<Page> = t.query::<pages::newest>((PaginationOpts::after(&c, 2),)).await;
    assert!(err(r).contains(foreign));
    // The filter is not part of the range: a cursor stays valid when only the filter changes.
    page(&t, 1, false, Some(3), PaginationOpts::after(&c, 2)).await.unwrap();

    // endCursor before cursor, and an empty page request.
    let p2 = page(&t, 1, false, None, PaginationOpts::after(&c, 1)).await.unwrap();
    let r = page(&t, 1, false, None, PaginationOpts::after(&p2.continue_cursor, 2).with_end_cursor(&c)).await;
    assert!(err(r).contains("endCursor is before cursor"));
    assert!(err(page(&t, 1, false, None, PaginationOpts::first(0)).await).contains("numItems must be at least 1"));
    // An empty pinned page (cursor == endCursor) is fine.
    let empty = page(&t, 1, false, None, PaginationOpts::after(&c, 2).with_end_cursor(&c)).await.unwrap();
    assert!(empty.page.is_empty() && !empty.is_done);

    // Over the wire, args are validated too: a missing cursor is null, a wrong type is an args error.
    let run = t.app().run_query(&pages::by_score::path(), json!({ "group": 1, "desc": false, "paginationOpts": { "numItems": 1 } }), &Identity::default()).await;
    assert_eq!(run.result.unwrap()["page"].as_array().unwrap().len(), 1);
    let run = t.app().run_query(&pages::by_score::path(), json!({ "group": 1, "desc": false, "paginationOpts": { "numItems": "x" } }), &Identity::default()).await;
    assert!(run.result.unwrap_err().to_string().starts_with("Invalid args for by_score"));
}

#[tokio::test]
async fn a_page_scans_at_most_max_page_scan_documents() {
    let t = TestApp::new().await.unwrap();
    let n = ferrix::db::pagination::MAX_PAGE_SCAN as i64 + 50;
    t.mutation::<pages::seed>((1, 0, n)).await.unwrap();
    // Only the last 10 items match: the first page stops after MAX_PAGE_SCAN scanned documents with no match.
    let min = Some(n - 10);
    let p1 = page(&t, 1, false, min, PaginationOpts::first(5)).await.unwrap();
    assert!(p1.page.is_empty());
    assert!(!p1.is_done);
    let p2 = page(&t, 1, false, min, PaginationOpts::after(&p1.continue_cursor, 5)).await.unwrap();
    assert_eq!(scores(&p2), ((n - 10)..(n - 5)).collect::<Vec<_>>());
    let p3 = page(&t, 1, false, min, PaginationOpts::after(&p2.continue_cursor, 5)).await.unwrap();
    assert_eq!(scores(&p3), ((n - 5)..n).collect::<Vec<_>>());
    // A pinned page is never capped.
    let all = page(&t, 1, false, min, PaginationOpts::first(1).with_end_cursor(&p2.continue_cursor)).await.unwrap();
    assert_eq!(scores(&all), ((n - 10)..(n - 5)).collect::<Vec<_>>());
}

// ---- Read sets ----

async fn read_set(t: &TestApp, group: i64, desc: bool, opts: PaginationOpts) -> (Value, ReadSet) {
    let args = json!({ "group": group, "desc": desc, "paginationOpts": opts });
    let run = t.app().run_query(&pages::by_score::path(), args, &Identity::default()).await;
    (run.result.unwrap(), run.read_set)
}

fn commit_at(t: &TestApp, ts: u64) -> Arc<CommitEntry> {
    let entries = t.app().db().write_log().entries_between(ts - 1, ts).unwrap();
    assert_eq!(entries.len(), 1);
    entries[0].clone()
}

async fn insert_commit(t: &TestApp, group: i64, score: i64) -> Arc<CommitEntry> {
    let (_, ts) = t.mutation_ts::<items::insert>((group, score, format!("x{score}"))).await.unwrap();
    commit_at(t, ts)
}

#[tokio::test]
async fn read_sets_cover_exactly_the_scanned_span() {
    for desc in [false, true] {
        let t = TestApp::new().await.unwrap();
        seed(&t, 1, [10, 20, 30, 40, 50, 60, 70]).await;
        // Asc: page 1 = 10, 20, 30; page 2 = 40, 50, 60; page 3 = 70 (done).
        // Desc: page 1 = 70, 60, 50; page 2 = 40, 30, 20; page 3 = 10 (done).
        let (v1, rs1) = read_set(&t, 1, desc, PaginationOpts::first(3)).await;
        let c1 = v1["continueCursor"].as_str().unwrap().to_string();
        let (v2, rs2) = read_set(&t, 1, desc, PaginationOpts::after(&c1, 3)).await;
        let c2 = v2["continueCursor"].as_str().unwrap().to_string();
        let (v3, rs3) = read_set(&t, 1, desc, PaginationOpts::after(&c2, 3)).await;
        assert_eq!(v3["isDone"], true);
        // The same pages pinned.
        let (_, rs1p) = read_set(&t, 1, desc, PaginationOpts::first(3).with_end_cursor(&c1)).await;
        let (_, rs2p) = read_set(&t, 1, desc, PaginationOpts::after(&c1, 3).with_end_cursor(&c2)).await;

        let (in_p1, in_p2, past_p3, other) = if desc { (55, 25, 5, 35) } else { (15, 45, 75, 35) };
        let all = [&rs1, &rs2, &rs3, &rs1p, &rs2p];
        let hits = |c: &Arc<CommitEntry>| all.iter().map(|rs| rs.overlaps(c)).collect::<Vec<_>>();

        // An insert into page 2 invalidates page 2 only (pinned or not).
        assert_eq!(hits(&insert_commit(&t, 1, in_p2).await), [false, true, false, false, true], "desc={desc}");
        // An insert into page 1 invalidates page 1 only.
        assert_eq!(hits(&insert_commit(&t, 1, in_p1).await), [true, false, false, true, false], "desc={desc}");
        // Past the end of the last page: the done page reads to the end of the range.
        assert_eq!(hits(&insert_commit(&t, 1, past_p3).await), [false, false, true, false, false], "desc={desc}");
        // Another group: nothing.
        assert_eq!(hits(&insert_commit(&t, 2, other).await), [false; 5], "desc={desc}");
    }
}

// ---- Over a real socket ----

mod ws {
    //! A minimal sync-protocol client (the full one lives in `websocket.rs`).

    use std::net::SocketAddr;
    use std::time::Duration;

    use futures::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio::net::TcpStream;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

    pub struct Ws {
        stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
        pub version: Value,
    }

    impl Ws {
        pub async fn connect(addr: SocketAddr) -> Ws {
            let (stream, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/sync")).await.unwrap();
            let mut ws = Ws { stream, version: json!({ "querySet": 0, "identity": 0, "ts": 0 }) };
            ws.send(json!({ "type": "Connect", "sessionId": "p", "connectionCount": 0, "lastCloseReason": null, "clientVersion": "0.1.0" })).await;
            ws
        }

        pub async fn send(&mut self, v: Value) {
            self.stream.send(Message::text(v.to_string())).await.unwrap();
        }

        /// Next non-Ping message; Transitions are checked for version continuity.
        pub async fn recv(&mut self) -> Value {
            loop {
                let msg = tokio::time::timeout(Duration::from_secs(5), self.stream.next())
                    .await
                    .expect("timed out waiting for a message")
                    .expect("socket closed")
                    .unwrap();
                let Message::Text(text) = msg else { continue };
                let v: Value = serde_json::from_str(text.as_str()).unwrap();
                match v["type"].as_str().unwrap() {
                    "Ping" => continue,
                    "Transition" => {
                        assert_eq!(v["startVersion"], self.version);
                        self.version = v["endVersion"].clone();
                    }
                    _ => {}
                }
                return v;
            }
        }

        /// Runs a mutation and returns every Transition received until it is visible (response + ts reached).
        pub async fn mutate(&mut self, request_id: u64, path: &str, args: Value) -> Vec<Value> {
            self.send(json!({ "type": "Mutation", "requestId": request_id, "udfPath": path, "args": args })).await;
            let mut seen = Vec::new();
            let mut commit_ts = None;
            loop {
                if let Some(m) = commit_ts
                    && self.version["ts"].as_u64().unwrap() >= m
                {
                    return seen;
                }
                let v = self.recv().await;
                match v["type"].as_str().unwrap() {
                    "Transition" => seen.push(v),
                    "MutationResponse" => {
                        assert_eq!(v["success"], true, "{v}");
                        commit_ts = Some(v["ts"].as_u64().unwrap());
                    }
                    other => panic!("unexpected {other}: {v}"),
                }
            }
        }
    }
}

/// The values of `query_id` in `transitions`' QueryUpdated modifications, in order.
fn updates_of(transitions: &[Value], query_id: u64) -> Vec<Value> {
    transitions
        .iter()
        .flat_map(|t| t["modifications"].as_array().unwrap().iter())
        .filter(|m| m["queryId"] == query_id)
        .map(|m| {
            assert_eq!(m["type"], "QueryUpdated", "{m}");
            m["value"].clone()
        })
        .collect()
}

fn value_scores(page: &Value) -> Vec<i64> {
    page["page"].as_array().unwrap().iter().map(|d| d["score"].as_i64().unwrap()).collect()
}

#[tokio::test]
async fn pinned_page_subscription_is_stable_under_inserts_after_it() {
    let t = TestApp::new().await.unwrap();
    let addr = t.serve().await.unwrap();
    seed(&t, 1, [10, 20, 30, 40]).await;
    let path = pages::by_score::path();
    let insert = items::insert::path();
    let args = |opts: Value| json!({ "group": 1, "desc": false, "paginationOpts": opts });

    let mut ws = ws::Ws::connect(addr).await;
    ws.send(json!({ "type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 1, "modifications": [
        { "type": "Add", "queryId": 0, "udfPath": path, "args": args(json!({ "numItems": 2, "cursor": null })) }] }))
    .await;
    let t1 = ws.recv().await;
    let p1 = updates_of(&[t1], 0).remove(0);
    assert_eq!(value_scores(&p1), [10, 20]);
    let c1 = p1["continueCursor"].clone();

    // What the client does on loadMore: add page 2 from page 1's continue cursor, and swap page 1 for a copy pinned
    // to that cursor.
    ws.send(json!({ "type": "ModifyQuerySet", "baseVersion": 1, "newVersion": 2, "modifications": [
        { "type": "Add", "queryId": 1, "udfPath": path, "args": args(json!({ "numItems": 2, "cursor": c1 })) },
        { "type": "Add", "queryId": 2, "udfPath": path, "args": args(json!({ "numItems": 2, "cursor": null, "endCursor": c1 })) },
        { "type": "Remove", "queryId": 0 }] }))
    .await;
    let t2 = [ws.recv().await];
    assert_eq!(t2[0]["endVersion"]["querySet"], 2);
    assert_eq!(value_scores(&updates_of(&t2, 1)[0]), [30, 40]);
    assert_eq!(updates_of(&t2, 2)[0], p1, "the pinned page shows the same page");

    // Inserts after page 1 change page 2 but never touch the pinned page 1.
    let seen = ws.mutate(0, &insert, json!({ "group": 1, "value": 35, "name": "a" })).await;
    assert!(updates_of(&seen, 2).is_empty(), "pinned page 1 must not update: {seen:?}");
    assert_eq!(updates_of(&seen, 1).iter().map(value_scores).collect::<Vec<_>>(), [vec![30, 35]]);
    let seen = ws.mutate(1, &insert, json!({ "group": 1, "value": 99, "name": "b" })).await;
    assert!(updates_of(&seen, 2).is_empty() && updates_of(&seen, 1).is_empty(), "{seen:?}");
    let seen = ws.mutate(2, &insert, json!({ "group": 2, "value": 15, "name": "c" })).await;
    assert!(updates_of(&seen, 2).is_empty() && updates_of(&seen, 1).is_empty(), "{seen:?}");

    // An insert inside page 1 grows it (the boundary stays) and leaves page 2 alone.
    let seen = ws.mutate(3, &insert, json!({ "group": 1, "value": 15, "name": "d" })).await;
    let p1_updates = updates_of(&seen, 2);
    assert_eq!(p1_updates.iter().map(value_scores).collect::<Vec<_>>(), [vec![10, 15, 20]]);
    assert_eq!(p1_updates[0]["continueCursor"], c1);
    assert!(updates_of(&seen, 1).is_empty(), "{seen:?}");
}
