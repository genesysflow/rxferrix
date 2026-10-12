use rxferrix::testing::TestApp;
use rxferrix::{FunctionRef, Identity, UserError};
use serde_json::json;

use crate::common::items::{self, Item, ScanSpec};
use crate::common::todos;

async fn seed_items(t: &TestApp) {
    // Insert in a shuffled-ish order so index order differs from creation order.
    for v in [3, 0, 7, 1, 9, 5, 2, 8, 6, 4] {
        for g in [2, 1, 3] {
            t.mutation::<items::insert>((g, v, format!("g{g}v{v}"))).await.unwrap();
        }
    }
}

fn vals(docs: &[rxferrix::Doc<Item>]) -> Vec<(i64, i64)> {
    docs.iter().map(|d| (d.group, d.score)).collect()
}

async fn scan(t: &TestApp, spec: ScanSpec) -> rxferrix::Result<Vec<(i64, i64)>> {
    Ok(vals(&t.query::<items::scan>((spec,)).await?))
}

fn spec(group: Option<i64>) -> ScanSpec {
    ScanSpec { group, mode: "collect".into(), ..Default::default() }
}

#[tokio::test]
async fn index_eq_range_order_take() {
    let t = TestApp::new().await.unwrap();
    seed_items(&t).await;
    let g2: Vec<_> = (0..10).map(|v| (2, v)).collect();
    assert_eq!(scan(&t, spec(Some(2))).await.unwrap(), g2);

    let s = ScanSpec { lower: Some((false, 3)), upper: Some((true, 7)), ..spec(Some(2)) };
    assert_eq!(scan(&t, s).await.unwrap(), (4..=7).map(|v| (2, v)).collect::<Vec<_>>());
    let s = ScanSpec { lower: Some((true, 3)), upper: Some((false, 7)), ..spec(Some(2)) };
    assert_eq!(scan(&t, s).await.unwrap(), (3..7).map(|v| (2, v)).collect::<Vec<_>>());
    let s = ScanSpec { lower: Some((false, 9)), ..spec(Some(2)) };
    assert!(scan(&t, s).await.unwrap().is_empty(), "empty range");
    let s = ScanSpec { upper: Some((false, 0)), ..spec(Some(2)) };
    assert!(scan(&t, s).await.unwrap().is_empty());

    // Range on the first index field (no eq prefix).
    let s = ScanSpec { lower: Some((true, 2)), ..spec(None) };
    let expected: Vec<_> = [2, 3].iter().flat_map(|g| (0..10).map(move |v| (*g, v))).collect();
    assert_eq!(scan(&t, s).await.unwrap(), expected);
    let s = ScanSpec { lower: Some((false, 1)), upper: Some((false, 3)), ..spec(None) };
    assert_eq!(scan(&t, s).await.unwrap(), g2);

    // Desc + take.
    let s = ScanSpec { desc: true, mode: "take".into(), limit: Some(3), ..spec(Some(1)) };
    assert_eq!(scan(&t, s).await.unwrap(), vec![(1, 9), (1, 8), (1, 7)]);
    let s = ScanSpec { desc: true, mode: "take".into(), limit: Some(0), ..spec(Some(1)) };
    assert!(scan(&t, s).await.unwrap().is_empty());
    let s = ScanSpec { mode: "take".into(), limit: Some(100), ..spec(Some(1)) };
    assert_eq!(scan(&t, s).await.unwrap().len(), 10, "take more than exists");

    // first / unique.
    let s = ScanSpec { desc: true, mode: "first".into(), ..spec(Some(3)) };
    assert_eq!(scan(&t, s).await.unwrap(), vec![(3, 9)]);
    let s = ScanSpec { mode: "unique".into(), lower: Some((true, 5)), upper: Some((true, 5)), ..spec(Some(3)) };
    assert_eq!(scan(&t, s).await.unwrap(), vec![(3, 5)]);
    let s = ScanSpec { mode: "unique".into(), lower: Some((false, 50)), ..spec(Some(3)) };
    assert!(scan(&t, s).await.unwrap().is_empty());
    let s = ScanSpec { mode: "unique".into(), ..spec(Some(3)) };
    let err = scan(&t, s).await.unwrap_err();
    assert!(err.to_string().contains("more than one"), "{err:#}");

    // Filter, combined with take (the scan continues past rejected rows).
    let s = ScanSpec { min_value: Some(5), mode: "take".into(), limit: Some(2), ..spec(Some(3)) };
    assert_eq!(scan(&t, s).await.unwrap(), vec![(3, 5), (3, 6)]);
    let s = ScanSpec { min_value: Some(8), desc: true, ..spec(None) };
    let got = scan(&t, s).await.unwrap();
    assert_eq!(got.len(), 6);
    assert!(got.iter().all(|(_, v)| *v >= 8));

    // Default index is by_creation_time: insertion order.
    let all = scan(&t, spec(None)).await.unwrap();
    let mut expected = Vec::new();
    for v in [3, 0, 7, 1, 9, 5, 2, 8, 6, 4] {
        for g in [2, 1, 3] {
            expected.push((g, v));
        }
    }
    assert_eq!(all, expected);
}

#[tokio::test]
async fn large_scans_cross_batch_boundaries() {
    let t = TestApp::new().await.unwrap();
    // More rows than one scan batch (256), inserted in one mutation.
    let texts: Vec<String> = (0..600).map(|i| format!("t{i:04}")).collect();
    let seen = t.mutation::<todos::add_and_list>((texts.clone(),)).await.unwrap();
    assert_eq!(seen, texts);
    let latest = t.query::<todos::latest>((300u32,)).await.unwrap();
    assert_eq!(latest.len(), 300);
    assert_eq!(latest[0].text, "t0599");
    assert_eq!(latest[299].text, "t0300");
    assert_eq!(t.query::<todos::count>(()).await.unwrap(), 600);
}

#[tokio::test]
async fn invalid_ranges_are_clear_errors() {
    let t = TestApp::new().await.unwrap();
    let expectations = [
        (0, "expected `group` at position 0"),
        (1, "must come before range bounds"),
        (2, "more than one lower bound"),
        (3, "index `nope` not found"),
        (4, "by_id"),
        (5, "too many fields"),
        (6, "expected `_creationTime` at position 2, got `eq(name)`"),
    ];
    for (kind, needle) in expectations {
        let err = t.query::<items::bad_range>((kind,)).await.unwrap_err();
        assert!(format!("{err:#}").contains(needle), "kind {kind}: {err:#}");
    }
}

#[tokio::test]
async fn mutation_reads_see_own_writes() {
    let t = TestApp::new().await.unwrap();
    t.mutation::<todos::add>(("first".to_string(),)).await.unwrap();
    let seen = t.mutation::<todos::add_and_list>((vec!["a".to_string(), "b".to_string()],)).await.unwrap();
    assert_eq!(seen, vec!["first", "a", "b"]);
    // add_then_fail asserts internally that `get` sees the fresh insert.
    let err = t.mutation::<todos::add_then_fail>(("x".to_string(),)).await.unwrap_err();
    assert_eq!(err.downcast_ref::<UserError>().unwrap().code, "nope");
}

#[tokio::test]
async fn writes_keep_index_entries_consistent() {
    let t = TestApp::new().await.unwrap();
    let check = || async { assert_eq!(t.app().db().verify_indexes().await.unwrap(), Vec::<String>::new()) };
    seed_items(&t).await;
    check().await;
    let a = t.mutation::<todos::add>(("a".to_string(),)).await.unwrap();
    let b = t.mutation::<todos::add>(("b".to_string(),)).await.unwrap();
    t.mutation::<todos::set_done>((a, true)).await.unwrap();
    check().await;
    t.mutation::<todos::rename>((b, "bb".to_string())).await.unwrap();
    check().await;
    assert_eq!(t.query::<todos::find>(("bb".to_string(),)).await.unwrap().id, b);
    assert!(t.query::<todos::find>(("b".to_string(),)).await.is_err(), "old index entry is gone");
    t.mutation::<todos::remove>((a,)).await.unwrap();
    assert!(t.query::<todos::get>((a,)).await.unwrap().is_none());
    check().await;
    for g in 1..=3 {
        t.mutation::<items::bump>((g, 100)).await.unwrap();
        t.mutation::<items::delete_top>((g,)).await.unwrap();
        t.mutation::<items::move_newest>((g + 1,)).await.unwrap();
        check().await;
    }
    // Patch keeps the document valid: done is still a bool, created_by stays null.
    let doc = t.query::<todos::get>((b,)).await.unwrap().unwrap();
    assert_eq!((doc.text.as_str(), doc.done, doc.created_by.clone()), ("bb", false, None));

    // Double delete is an error and leaves things consistent.
    assert!(t.mutation::<todos::remove>((a,)).await.is_err());
    check().await;
}

#[tokio::test]
async fn failed_and_panicking_mutations_roll_back() {
    let t = TestApp::new().await.unwrap();
    t.mutation::<todos::add>(("keep".to_string(),)).await.unwrap();
    let ts_before = t.app().db().write_log().latest();

    let err = t.mutation::<todos::add_then_fail>(("gone".to_string(),)).await.unwrap_err();
    assert!(err.downcast_ref::<UserError>().is_some());
    let err = t.mutation::<todos::add_then_panic>(("gone2".to_string(),)).await.unwrap_err();
    assert!(err.to_string().contains("panicked: boom"), "{err:#}");

    assert_eq!(t.query::<todos::count>(()).await.unwrap(), 1);
    assert_eq!(t.app().db().write_log().latest(), ts_before, "no ts bump");
    assert_eq!(t.app().db().verify_indexes().await.unwrap(), Vec::<String>::new());
    // The writer is usable afterwards.
    let (_, ts) = t.mutation_ts::<todos::add>(("after".to_string(),)).await.unwrap();
    assert_eq!(ts, ts_before + 1);
}

#[tokio::test]
async fn wrong_table_ids_are_errors() {
    let t = TestApp::new().await.unwrap();
    t.mutation::<items::insert>((1, 1, "x".to_string())).await.unwrap();
    let item = &t.query::<items::scan>((spec(Some(1)),)).await.unwrap()[0];
    let item_id = item.id.to_string();

    let run = t.app().run_query(&todos::get::path(), json!({ "id": item_id }), &Identity::default()).await;
    let err = run.result.unwrap_err();
    assert!(format!("{err:#}").contains("belongs to table `items`, not `todos`"), "{err:#}");

    let err = t
        .app()
        .run_mutation(&todos::set_done::path(), json!({ "id": item_id, "done": true }), &Identity::default())
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("belongs to table `items`"), "{err:#}");

    // A malformed id is an args error.
    let run = t.app().run_query(&todos::get::path(), json!({ "id": "not-an-id" }), &Identity::default()).await;
    let msg = run.result.unwrap_err().to_string();
    assert!(msg.starts_with("Invalid args for get: "), "{msg}");
}

#[tokio::test]
async fn noop_mutation_does_not_bump_ts() {
    let t = TestApp::new().await.unwrap();
    let (_, ts1) = t.mutation_ts::<todos::add>(("a".to_string(),)).await.unwrap();
    let (out, ts2) = t.mutation_ts::<todos::noop>(()).await.unwrap();
    assert_eq!((out, ts2), (7, ts1));
    let (_, ts3) = t.mutation_ts::<todos::add>(("b".to_string(),)).await.unwrap();
    assert_eq!(ts3, ts1 + 1);
}

#[tokio::test]
async fn actions_and_internal_functions() {
    let t = TestApp::new().await.unwrap();
    t.action::<todos::add_many>((3u32,)).await.unwrap();
    assert_eq!(t.action::<todos::count_via_action>(()).await.unwrap(), 3);
    // Internal functions are invisible to clients...
    let run = t.app().run_query(&todos::count::path(), json!({}), &Identity::default()).await;
    assert!(run.result.unwrap_err().to_string().contains("Could not find public function"));
    // ...but callable from server code.
    assert_eq!(t.query::<todos::count>(()).await.unwrap(), 3);
    // Wrong kind.
    let err = t.app().run_mutation(&todos::list::path(), json!({}), &Identity::default()).await.unwrap_err();
    assert!(err.to_string().contains("as a mutation, but it is defined as a query"), "{err}");
}

#[tokio::test]
async fn creation_times_are_increasing_and_now_is_commit_time() {
    let t = TestApp::new().await.unwrap();
    t.mutation::<todos::add_and_list>(((0..20).map(|i| i.to_string()).collect::<Vec<_>>(),)).await.unwrap();
    let docs = t.query::<todos::latest>((20u32,)).await.unwrap();
    for w in docs.windows(2) {
        assert!(w[0].creation_time > w[1].creation_time);
    }
    let texts: Vec<_> = docs.iter().rev().map(|d| d.text.clone()).collect();
    assert_eq!(texts, (0..20).map(|i| i.to_string()).collect::<Vec<_>>());
}
