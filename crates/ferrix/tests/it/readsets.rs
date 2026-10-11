//! Read-set recording and invalidation: a commit invalidates a query only if it wrote a key the query read.

use std::sync::Arc;

use ferrix::db::readset::ReadSet;
use ferrix::testing::TestApp;
use ferrix::writelog::CommitEntry;
use ferrix::{FunctionRef, Identity};
use serde_json::{Value, json};

use crate::common::items::{self, Item, ScanSpec};
use crate::common::todos;

async fn read_set_of<F: FunctionRef>(t: &TestApp, args: Value) -> ReadSet {
    let run = t.app().run_query(&F::path(), args, &Identity::default()).await;
    run.result.unwrap();
    run.read_set
}

fn commit_at(t: &TestApp, ts: u64) -> Arc<CommitEntry> {
    let entries = t.app().db().write_log().entries_between(ts - 1, ts).unwrap();
    assert_eq!(entries.len(), 1);
    entries[0].clone()
}

#[tokio::test]
async fn latest_n_desc_take() {
    let t = TestApp::new().await.unwrap();
    let mut ids = Vec::new();
    for i in 0..5 {
        ids.push(t.mutation::<todos::add>((format!("t{i}"),)).await.unwrap());
    }
    // latest(2) reads t4, t3 (descending by creation time) and nothing older.
    let rs = read_set_of::<todos::latest>(&t, json!({ "n": 2 })).await;

    // Writing an older todo (outside the scanned range) does not invalidate it...
    let (_, ts) = t.mutation_ts::<todos::set_done>((ids[0], true)).await.unwrap();
    assert!(!rs.overlaps(&commit_at(&t, ts)), "write to t0 must not invalidate latest(2)");
    let (_, ts) = t.mutation_ts::<todos::remove>((ids[1],)).await.unwrap();
    assert!(!rs.overlaps(&commit_at(&t, ts)));
    // ...but writing one it read does,
    let (_, ts) = t.mutation_ts::<todos::set_done>((ids[3], true)).await.unwrap();
    assert!(rs.overlaps(&commit_at(&t, ts)), "write to t3 must invalidate latest(2)");
    // and so does a newer insert (it lands inside [t3, +inf)).
    let (_, ts) = t.mutation_ts::<todos::add>(("t5".to_string(),)).await.unwrap();
    assert!(rs.overlaps(&commit_at(&t, ts)));

    // Without a limit the whole range is read: latest(100) is invalidated by writes to old todos too.
    let rs_all = read_set_of::<todos::latest>(&t, json!({ "n": 100 })).await;
    let (_, ts) = t.mutation_ts::<todos::set_done>((ids[0], false)).await.unwrap();
    assert!(rs_all.overlaps(&commit_at(&t, ts)));
}

#[tokio::test]
async fn asc_take_on_compound_index() {
    let t = TestApp::new().await.unwrap();
    for v in 0..6 {
        t.mutation::<items::insert>((1, v * 10, format!("n{v}"))).await.unwrap();
    }
    let spec = ScanSpec { group: Some(1), mode: "take".into(), limit: Some(2), ..Default::default() };
    // Reads group 1 scores 0, 10.
    let rs = read_set_of::<items::scan>(&t, json!({ "spec": spec })).await;

    let (_, ts) = t.mutation_ts::<items::insert>((1, 35, "a".to_string())).await.unwrap();
    assert!(!rs.overlaps(&commit_at(&t, ts)), "score 35 sorts after the 2nd row");
    let (_, ts) = t.mutation_ts::<items::insert>((2, 0, "b".to_string())).await.unwrap();
    assert!(!rs.overlaps(&commit_at(&t, ts)), "other group");
    let (_, ts) = t.mutation_ts::<items::insert>((1, 5, "c".to_string())).await.unwrap();
    assert!(rs.overlaps(&commit_at(&t, ts)), "score 5 sorts before the 2nd row");
    let (_, ts) = t.mutation_ts::<items::insert>((1, -1, "d".to_string())).await.unwrap();
    assert!(rs.overlaps(&commit_at(&t, ts)));
}

#[tokio::test]
async fn filtered_rows_still_count_as_read() {
    let t = TestApp::new().await.unwrap();
    for v in 0..4 {
        t.mutation::<items::insert>((1, v, format!("n{v}"))).await.unwrap();
    }
    // Nothing passes the filter, so the scan exhausts group 1: the whole group range was read.
    let spec =
        ScanSpec { group: Some(1), min_value: Some(100), mode: "take".into(), limit: Some(1), ..Default::default() };
    let rs = read_set_of::<items::scan>(&t, json!({ "spec": spec })).await;
    let (_, ts) = t.mutation_ts::<items::insert>((1, 50, "x".to_string())).await.unwrap();
    assert!(rs.overlaps(&commit_at(&t, ts)));
    let (_, ts) = t.mutation_ts::<items::insert>((3, 500, "y".to_string())).await.unwrap();
    assert!(!rs.overlaps(&commit_at(&t, ts)));
}

#[tokio::test]
async fn get_point_reads() {
    let t = TestApp::new().await.unwrap();
    let a = t.mutation::<todos::add>(("a".to_string(),)).await.unwrap();
    let b = t.mutation::<todos::add>(("b".to_string(),)).await.unwrap();
    let rs = read_set_of::<todos::get>(&t, json!({ "id": a })).await;
    assert_eq!(rs.ranges().count(), 1, "exactly one point range");

    let (_, ts) = t.mutation_ts::<todos::set_done>((b, true)).await.unwrap();
    assert!(!rs.overlaps(&commit_at(&t, ts)));
    let (_, ts) = t.mutation_ts::<todos::add>(("c".to_string(),)).await.unwrap();
    assert!(!rs.overlaps(&commit_at(&t, ts)));
    let (_, ts) = t.mutation_ts::<todos::set_done>((a, true)).await.unwrap();
    assert!(rs.overlaps(&commit_at(&t, ts)));
    let (_, ts) = t.mutation_ts::<todos::remove>((a,)).await.unwrap();
    assert!(rs.overlaps(&commit_at(&t, ts)));

    // A get of a missing doc still records the point, so it is invalidated if the doc appears (here: never,
    // ids are random), and unaffected by other writes.
    let rs = read_set_of::<todos::get>(&t, json!({ "id": a })).await;
    let (_, ts) = t.mutation_ts::<todos::remove>((b,)).await.unwrap();
    assert!(!rs.overlaps(&commit_at(&t, ts)));
}

#[tokio::test]
async fn commit_entries_cover_every_index() {
    let t = TestApp::new().await.unwrap();
    let (_, ts) = t.mutation_ts::<items::insert>((1, 1, "n".to_string())).await.unwrap();
    let entry = commit_at(&t, ts);
    let table = t.app().db().catalog().table("items").unwrap().clone();
    // Insert: new key in by_id, by_creation_time, by_group_score, by_name.
    for index in &table.indexes {
        assert!(entry.writes.iter().any(|(id, _)| *id == index.id), "missing index {}", index.name);
    }
    assert_eq!(entry.writes.len(), table.indexes.len());

    let doc: ferrix::Doc<Item> = t.query::<items::find_name>(("n".to_string(),)).await.unwrap();
    // Patch changing a key: old + new key for by_group_score, one key for the unchanged indexes.
    let (_, ts) = t.mutation_ts::<items::bump>((1, 5)).await.unwrap();
    let entry = commit_at(&t, ts);
    let by_gs = table.index("by_group_score").unwrap().id;
    assert_eq!(entry.writes.iter().filter(|(id, _)| *id == by_gs).count(), 2);
    assert_eq!(entry.writes.len(), table.indexes.len() + 1);
    assert!(entry.writes.contains(&(table.by_id().id, doc.id.as_bytes().to_vec())));
}
