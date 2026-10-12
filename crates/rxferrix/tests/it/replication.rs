//! Continuous replication and point-in-time restores (`rxferrix::ops::replicate`): the change journal's exactly-once
//! guarantee under concurrent writers and rollbacks, shipping segments to a directory and to an object store (also
//! through target failures, crashes between upload and trim, and an overflowing journal), point-in-time restores
//! of a randomized workload compared with a model, retention, and the staged point-in-time restore over HTTP.
//!
//! `bench_journal_overhead` (ignored) measures the commit throughput with and without journaling:
//! `cargo test --release -p rxferrix --test it bench_journal_overhead -- --ignored --nocapture`.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use futures::stream::BoxStream;
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore, PutMultipartOptions,
    PutOptions, PutPayload, PutResult,
};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use rxferrix::auth::AuthConfig;
use rxferrix::ops::OpsOptions;
use rxferrix::ops::backup::{BackupTarget, RestoreOptions};
use rxferrix::ops::replicate::{
    self, Generation, PointInTime, ReplicationOptions, Replicator, SegmentInfo, SnapshotInfo, plan_retention,
};
use rxferrix::ops::restore;
use rxferrix::testing::TestApp;
use rxferrix::{App, AppOptions, Id};
use serde_json::{Value, json};

pub mod pitr {
    use rxferrix::prelude::*;

    /// Indexed, searchable and with an HNSW vector index, so a replay must rebuild all three.
    #[table(name = "pitr_notes")]
    #[index(by_owner = [owner])]
    #[search_index(search_body, search = body, filter = [owner])]
    #[vector_index(by_embedding, field = embedding, dims = 3, kind = "hnsw")]
    pub struct Note {
        pub owner: i64,
        pub body: String,
        pub embedding: Vec<f32>,
        pub n: i64,
    }

    /// `op`: `insert` (returns the new id), `patch` or `delete` (of `id`).
    #[internal_mutation]
    pub async fn pitr_op(
        ctx: &MutationCtx,
        op: String,
        id: Option<Id<Note>>,
        owner: i64,
        body: String,
        n: i64,
    ) -> Result<Option<Id<Note>>> {
        let embedding = vec![owner as f32 / 8.0, n as f32 / 8.0, 1.0];
        match (op.as_str(), id) {
            ("insert", _) => Ok(Some(ctx.db.insert(Note { owner, body, embedding, n }).await?)),
            ("patch", Some(id)) => {
                ctx.db
                    .patch(id, serde_json::json!({ "owner": owner, "body": body, "n": n, "embedding": embedding }))
                    .await?;
                Ok(None)
            }
            ("delete", Some(id)) => {
                ctx.db.delete(id).await?;
                Ok(None)
            }
            _ => Err(UserError::new("bad_op", "unknown op").into()),
        }
    }

    /// Writes, then fails: rolled back.
    #[internal_mutation]
    pub async fn pitr_fail(ctx: &MutationCtx, body: String) -> Result<()> {
        ctx.db.insert(Note { owner: -1, body, embedding: vec![0.0, 0.0, 1.0], n: -1 }).await?;
        Err(UserError::new("nope", "rolled back on purpose").into())
    }

    /// Writes, then panics: rolled back.
    #[internal_mutation]
    pub async fn pitr_panic(ctx: &MutationCtx, body: String) -> Result<()> {
        ctx.db.insert(Note { owner: -2, body, embedding: vec![0.0, 0.0, 1.0], n: -2 }).await?;
        panic!("rolled back on purpose");
    }

    /// Inserts and deletes the same document: a commit whose journaled changes cancel out.
    #[internal_mutation]
    pub async fn pitr_transient(ctx: &MutationCtx) -> Result<()> {
        let id =
            ctx.db.insert(Note { owner: 0, body: "transient".into(), embedding: vec![0.0, 1.0, 0.0], n: 0 }).await?;
        ctx.db.delete(id).await
    }

    #[internal_query]
    pub async fn pitr_all(ctx: &QueryCtx) -> Result<Vec<Doc<Note>>> {
        ctx.db.query::<Note>().collect().await
    }

    #[internal_query]
    pub async fn pitr_search(ctx: &QueryCtx, text: String) -> Result<Vec<Id<Note>>> {
        let hits = ctx
            .db
            .query::<Note>()
            .with_search_index(Note::search_body, |q| q.search(Note::body, text))
            .take(1000)
            .await?;
        Ok(hits.into_iter().map(|d| d.id).collect())
    }
}

use pitr::*;

// ---------------------------------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------------------------------

fn options(target: BackupTarget) -> ReplicationOptions {
    let mut r = ReplicationOptions::with_target(target);
    // Tests drive the replicator with `sync_now` only: a background step running at an arbitrary moment would make
    // their lag, failure and segment counts depend on timing.
    r.background = false;
    r
}

async fn replicated(r: ReplicationOptions) -> TestApp {
    let options =
        AppOptions { dev: true, ops: OpsOptions { replication: Some(r), ..Default::default() }, ..Default::default() };
    TestApp::with_options(options).await.unwrap()
}

fn rep(t: &TestApp) -> &Arc<Replicator> {
    t.app().replication().expect("replication is on")
}

fn dir_target(dir: &Path) -> BackupTarget {
    BackupTarget::parse(dir.to_str().unwrap(), dir).unwrap()
}

/// `(ts, changes)` of every journal row, read directly from `data.db`.
fn journal_rows(dir: &Path) -> Vec<(u64, String)> {
    let conn =
        rusqlite::Connection::open_with_flags(dir.join("data.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut stmt = conn.prepare("SELECT ts, changes FROM _journal ORDER BY ts").unwrap();
    stmt.query_map([], |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?))).unwrap().map(Result::unwrap).collect()
}

/// Every journal entry stored for `generation`, in ts order, each ts once (overlapping segments merged).
async fn shipped_entries(target: &BackupTarget, generation: &Generation) -> Vec<rxferrix::db::journal::JournalEntry> {
    let mut out: BTreeMap<u64, rxferrix::db::journal::JournalEntry> = BTreeMap::new();
    for seg in &generation.segments {
        let (_, entries) = replicate::read_segment(target, &generation.id, seg.from, seg.to).await.unwrap();
        for e in entries {
            if let Some(prev) = out.insert(e.ts, e.clone()) {
                assert_eq!(prev, e, "overlapping segments agree");
            }
        }
    }
    out.into_values().collect()
}

fn store_target(store: Arc<dyn ObjectStore>, prefix: &str) -> BackupTarget {
    BackupTarget::object_store(store, prefix)
}

/// An in-memory store that fails on request: `0` works, `1` fails every call, `2` stores puts but reports them as
/// failed (an upload whose answer was lost: the replicator must not trim, and the retry overlaps).
#[derive(Debug, Default)]
struct FlakyStore {
    inner: InMemory,
    mode: AtomicU8,
}

impl fmt::Display for FlakyStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FlakyStore")
    }
}

fn down() -> object_store::Error {
    object_store::Error::Generic { store: "flaky", source: Box::new(std::io::Error::other("the target is down")) }
}

impl FlakyStore {
    fn check(&self) -> object_store::Result<()> {
        if self.mode.load(Ordering::SeqCst) == 1 { Err(down()) } else { Ok(()) }
    }
}

#[async_trait::async_trait]
impl ObjectStore for FlakyStore {
    async fn put_opts(
        &self,
        location: &ObjectPath,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.check()?;
        let r = self.inner.put_opts(location, payload, opts).await?;
        if self.mode.load(Ordering::SeqCst) == 2 { Err(down()) } else { Ok(r) }
    }

    async fn put_multipart_opts(
        &self,
        location: &ObjectPath,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.check()?;
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(&self, location: &ObjectPath, options: GetOptions) -> object_store::Result<GetResult> {
        self.check()?;
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<ObjectPath>>,
    ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
        if self.check().is_err() {
            return Box::pin(futures::stream::once(async { Err(down()) }));
        }
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&ObjectPath>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        if self.check().is_err() {
            return Box::pin(futures::stream::once(async { Err(down()) }));
        }
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&ObjectPath>) -> object_store::Result<ListResult> {
        self.check()?;
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &ObjectPath, to: &ObjectPath, options: CopyOptions) -> object_store::Result<()> {
        self.check()?;
        self.inner.copy_opts(from, to, options).await
    }
}

/// The model of `pitr_notes`: id → (owner, body, n, creation time).
type Model = BTreeMap<String, (i64, String, i64, f64)>;

fn model_of(docs: &[rxferrix::Doc<Note>]) -> Model {
    docs.iter().map(|d| (d.id.to_string(), (d.owner, d.body.clone(), d.n, d.creation_time))).collect()
}

async fn verify(app: &App) {
    let problems = app.db().verify_indexes().await.unwrap();
    assert!(problems.is_empty(), "{problems:?}");
}

fn meta_ts(dir: &Path) -> u64 {
    let conn =
        rusqlite::Connection::open_with_flags(dir.join("data.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    conn.query_row("SELECT v FROM _meta WHERE k = 'ts'", [], |r| r.get::<_, i64>(0)).unwrap() as u64
}

async fn op(t: &TestApp, op: &str, id: Option<Id<Note>>, owner: i64, body: &str, n: i64) -> (Option<Id<Note>>, u64) {
    t.mutation_ts::<pitr_op>((op.to_string(), id, owner, body.to_string(), n)).await.unwrap()
}

// ---------------------------------------------------------------------------------------------------------------------
// The journal
// ---------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn journal_has_every_commit_exactly_once_and_nothing_rolled_back() {
    let target_dir = tempfile::tempdir().unwrap();
    let target = dir_target(target_dir.path());
    let t = Arc::new(replicated(options(target.clone())).await);
    rep(&t).sync_now(t.app()).await.unwrap();
    let base = rep(&t).status(t.app()).await.shipped_ts;
    assert!(journal_rows(t.dir()).iter().all(|(ts, _)| *ts > base), "shipped rows are trimmed");

    let mut writers = Vec::new();
    for w in 0..4i64 {
        let app = t.clone();
        writers.push(tokio::spawn(async move {
            let mut committed: Vec<(u64, String)> = Vec::new();
            let mut mine: Vec<Id<Note>> = Vec::new();
            for i in 0..40i64 {
                let call = |op: &str, id: Option<Id<Note>>| (op.to_string(), id, w, format!("w{w} i{i}"), i);
                let (op, id) = match (i % 5, mine.last().copied()) {
                    (3, Some(id)) => ("patch", Some(id)),
                    (4, Some(id)) => ("delete", Some(id)),
                    _ => ("insert", None),
                };
                let (result, ts) = app.mutation_ts::<pitr_op>(call(op, id)).await.unwrap();
                match (op, result, id) {
                    ("insert", Some(new), _) => {
                        mine.push(new);
                        committed.push((ts, new.to_string()));
                    }
                    ("delete", _, Some(id)) => {
                        mine.pop();
                        committed.push((ts, id.to_string()));
                    }
                    (_, _, Some(id)) => committed.push((ts, id.to_string())),
                    _ => unreachable!(),
                }
                if i % 7 == 0 {
                    assert!(app.mutation::<pitr_fail>((format!("ROLLED-BACK w{w} i{i}"),)).await.is_err());
                }
                if i % 13 == 0 {
                    assert!(app.mutation::<pitr_panic>((format!("ROLLED-BACK panic w{w} i{i}"),)).await.is_err());
                }
                if i % 11 == 0 {
                    let (_, ts) = app.mutation_ts::<pitr_transient>(()).await.unwrap();
                    committed.push((ts, String::new()));
                }
            }
            committed
        }));
    }
    let mut committed = Vec::new();
    for w in writers {
        committed.extend(w.await.unwrap());
    }
    let latest = t.app().db().write_log().latest();
    let rows = journal_rows(t.dir());
    let tss: Vec<u64> = rows.iter().map(|(ts, _)| *ts).collect();
    assert_eq!(tss, (base + 1..=latest).collect::<Vec<_>>(), "every committed ts exactly once, in order");
    let by_ts: BTreeMap<u64, &String> = rows.iter().map(|(ts, c)| (*ts, c)).collect();
    for (ts, id) in &committed {
        let changes = by_ts[ts];
        if id.is_empty() {
            assert_eq!(changes, "[]", "insert + delete in one transaction leaves an empty change list at ts {ts}");
        } else {
            assert!(changes.contains(id.as_str()), "ts {ts} journals {id}: {changes}");
        }
    }
    assert!(rows.iter().all(|(_, c)| !c.contains("ROLLED-BACK")), "rolled-back writes are never journaled");
    assert_eq!(committed.len() as u64, latest - base, "only our commits happened");

    // Shipping moves exactly these entries to the target and empties the journal.
    rep(&t).sync_now(t.app()).await.unwrap();
    assert!(journal_rows(t.dir()).is_empty());
    let gens = replicate::list_generations(&target).await.unwrap();
    assert_eq!(gens.len(), 1);
    let shipped = shipped_entries(&target, &gens[0]).await;
    let shipped: Vec<(u64, String)> = shipped.into_iter().map(|e| (e.ts, e.changes)).collect();
    assert_eq!(shipped, rows);
    let st = rep(&t).status(t.app()).await;
    assert_eq!((st.shipped_ts, st.lag_commits, st.lag_ms, st.pending_bytes), (latest, 0, 0.0, 0));
}

// ---------------------------------------------------------------------------------------------------------------------
// Shipping
// ---------------------------------------------------------------------------------------------------------------------

async fn ships_to(target: BackupTarget) -> TestApp {
    let mut r = options(target.clone());
    r.segment_max_entries = 7;
    let t = replicated(r).await;
    rep(&t).sync_now(t.app()).await.unwrap();
    let st = rep(&t).status(t.app()).await;
    let generation = st.generation.clone().expect("a generation");
    assert_eq!(st.snapshots_taken, 1);
    let base = st.shipped_ts;
    let mut id = None;
    for i in 0..30 {
        let (new, _) = op(&t, if i % 4 == 3 { "patch" } else { "insert" }, id, i, &format!("note {i}"), i).await;
        id = new.or(id);
    }
    let latest = t.app().db().write_log().latest();
    let before = rep(&t).status(t.app()).await;
    assert_eq!(before.lag_commits, latest - base);
    assert!(before.lag_ms >= 0.0 && before.pending_bytes > 0);
    rep(&t).sync_now(t.app()).await.unwrap();
    let gens = replicate::list_generations(&target).await.unwrap();
    assert_eq!(gens.len(), 1);
    let g = &gens[0];
    assert_eq!(g.id, generation);
    assert_eq!(g.snapshots.len(), 1);
    assert_eq!(g.snapshots[0].ts, base);
    assert!(g.segments.len() >= 5, "segments of at most 7 commits: {:?}", g.segments);
    assert!(g.segments.iter().all(|s| s.to - s.from < 7));
    assert_eq!((g.segments.first().unwrap().from, g.segments.last().unwrap().to), (base + 1, latest));
    let entries = shipped_entries(&target, g).await;
    assert_eq!(entries.iter().map(|e| e.ts).collect::<Vec<_>>(), (base + 1..=latest).collect::<Vec<_>>());
    let st = rep(&t).status(t.app()).await;
    assert_eq!((st.shipped_ts, st.lag_commits, st.pending_bytes), (latest, 0, 0));
    assert_eq!(st.last_segment.as_ref().unwrap().to, latest);
    assert!(journal_rows(t.dir()).is_empty());
    assert_eq!(st.generations.len(), 1);
    assert_eq!((st.generations[0].from_ts, st.generations[0].to_ts), (Some(base), Some(latest)));
    t
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ships_segments_to_a_directory() {
    let target_dir = tempfile::tempdir().unwrap();
    let t = ships_to(dir_target(target_dir.path())).await;
    let generation = rep(&t).status(t.app()).await.generation.unwrap();
    let journal = target_dir.path().join("generations").join(&generation).join("journal");
    let names: Vec<String> =
        std::fs::read_dir(&journal).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(names.iter().all(|n| n.len() == 45 && n.ends_with(".zst")), "{names:?}");
    let snapshots = target_dir.path().join("generations").join(&generation).join("snapshots");
    assert_eq!(std::fs::read_dir(snapshots).unwrap().count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ships_segments_to_an_object_store() {
    ships_to(store_target(Arc::new(InMemory::new()), "replica/one")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shipping_survives_target_errors_lost_answers_and_restarts() {
    let flaky = Arc::new(FlakyStore::default());
    let target = store_target(flaky.clone(), "r");
    let t = replicated(options(target.clone())).await;
    rep(&t).sync_now(t.app()).await.unwrap();
    let base = rep(&t).status(t.app()).await.shipped_ts;

    // The target is down: nothing is trimmed, the journal keeps every commit, the error is reported.
    flaky.mode.store(1, Ordering::SeqCst);
    for i in 0..10 {
        op(&t, "insert", None, i, "while down", i).await;
    }
    let err = format!("{:#}", rep(&t).sync_now(t.app()).await.unwrap_err());
    assert!(err.contains("down"), "{err}");
    let st = rep(&t).status(t.app()).await;
    let latest = t.app().db().write_log().latest();
    assert_eq!((st.failures, st.shipped_ts, st.lag_commits), (1, base, latest - base));
    assert!(st.last_error.as_deref().unwrap().contains("down") && st.pending_bytes > 0 && st.journaling);
    assert_eq!(journal_rows(t.dir()).len() as u64, latest - base, "nothing trimmed without an upload");

    // An upload that succeeds but whose answer is lost: still nothing trimmed.
    flaky.mode.store(2, Ordering::SeqCst);
    assert!(rep(&t).sync_now(t.app()).await.is_err());
    assert_eq!(journal_rows(t.dir()).len() as u64, latest - base);

    // Back up: the retry ships everything (overlapping the lost upload), then trims.
    flaky.mode.store(0, Ordering::SeqCst);
    for i in 10..15 {
        op(&t, "insert", None, i, "after", i).await;
    }
    rep(&t).sync_now(t.app()).await.unwrap();
    let latest = t.app().db().write_log().latest();
    let st = rep(&t).status(t.app()).await;
    assert_eq!((st.failures, st.shipped_ts, st.lag_commits, st.last_error.clone()), (0, latest, 0, None));
    assert!(journal_rows(t.dir()).is_empty());
    let gens = replicate::list_generations(&target).await.unwrap();
    assert_eq!(gens.len(), 1);
    assert!(gens[0].segments.len() >= 2, "the lost upload and the retry: {:?}", gens[0].segments);
    let shipped = shipped_entries(&target, &gens[0]).await;
    assert_eq!(shipped.iter().map(|e| e.ts).collect::<Vec<_>>(), (base + 1..=latest).collect::<Vec<_>>());

    // A crash between an upload and its trim: the next start finds the segment in the target and continues the
    // same generation.
    flaky.mode.store(2, Ordering::SeqCst);
    for i in 0..3 {
        op(&t, "insert", None, i, "lost answer", i).await;
    }
    assert!(rep(&t).sync_now(t.app()).await.is_err());
    flaky.mode.store(0, Ordering::SeqCst);
    let generation = rep(&t).status(t.app()).await.generation.unwrap();
    let t = t.restart().await.unwrap();
    rep(&t).sync_now(t.app()).await.unwrap();
    let st = rep(&t).status(t.app()).await;
    assert_eq!(st.generation.as_deref(), Some(generation.as_str()), "resumed, not restarted");
    assert_eq!((st.shipped_ts, st.lag_commits), (t.app().db().write_log().latest(), 0));
    assert_eq!(replicate::list_generations(&target).await.unwrap().len(), 1);

    // Down for too long: past the bound the journal is dropped; once the target is back a new generation starts.
    let mut small = options(target.clone());
    small.max_pending_bytes = 1;
    let t = t
        .restart_with(AppOptions {
            dev: true,
            ops: OpsOptions { replication: Some(small), ..Default::default() },
            ..Default::default()
        })
        .await
        .unwrap();
    rep(&t).sync_now(t.app()).await.unwrap();
    flaky.mode.store(1, Ordering::SeqCst);
    op(&t, "insert", None, 1, "overflow", 1).await;
    assert!(rep(&t).sync_now(t.app()).await.is_err());
    let st = rep(&t).status(t.app()).await;
    assert!(!st.journaling && st.generation.is_none() && st.pending_bytes == 0, "{st:?}");
    assert!(journal_rows(t.dir()).is_empty());
    op(&t, "insert", None, 2, "not journaled", 2).await;
    assert!(journal_rows(t.dir()).is_empty());
    flaky.mode.store(0, Ordering::SeqCst);
    rep(&t).sync_now(t.app()).await.unwrap();
    let st = rep(&t).status(t.app()).await;
    assert!(st.journaling && st.generation.is_some() && st.generation.as_deref() != Some(generation.as_str()));
    assert_eq!(replicate::list_generations(&target).await.unwrap().len(), 2);
    op(&t, "insert", None, 3, "journaled again", 3).await;
    assert_eq!(journal_rows(t.dir()).len(), 1);
}

/// Commits made without journaling (here: the app opened without replication) start a new generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gap_in_the_journal_starts_a_new_generation() {
    let target = store_target(Arc::new(InMemory::new()), "gap");
    let t = replicated(options(target.clone())).await;
    op(&t, "insert", None, 1, "one", 1).await;
    rep(&t).sync_now(t.app()).await.unwrap();
    let first = rep(&t).status(t.app()).await.generation.unwrap();
    // Unchanged restart: same generation.
    let t = t.restart().await.unwrap();
    rep(&t).sync_now(t.app()).await.unwrap();
    assert_eq!(rep(&t).status(t.app()).await.generation.unwrap(), first);
    let t = t.restart_with(AppOptions { dev: true, ..Default::default() }).await.unwrap();
    op(&t, "insert", None, 2, "unjournaled", 2).await;
    let t = t
        .restart_with(AppOptions {
            dev: true,
            ops: OpsOptions { replication: Some(options(target.clone())), ..Default::default() },
            ..Default::default()
        })
        .await
        .unwrap();
    rep(&t).sync_now(t.app()).await.unwrap();
    let second = rep(&t).status(t.app()).await.generation.unwrap();
    assert_ne!(first, second);
    let gens = replicate::list_generations(&target).await.unwrap();
    assert_eq!(gens.iter().map(|g| g.id.clone()).collect::<Vec<_>>(), [first, second]);
}

// ---------------------------------------------------------------------------------------------------------------------
// Point-in-time restores
// ---------------------------------------------------------------------------------------------------------------------

/// Restore `point` into a fresh directory and return it with what was restored.
async fn restore_to(target: &BackupTarget, point: PointInTime) -> (tempfile::TempDir, replicate::PointInTimeInfo) {
    let dir = tempfile::tempdir().unwrap();
    let (_, info) =
        replicate::restore_point_in_time(target, dir.path(), &point, &RestoreOptions::default()).await.unwrap();
    (dir, info)
}

async fn check_restored(dir: &Path, expected_ts: u64, expected: &Model) {
    assert_eq!(meta_ts(dir), expected_ts, "the restored database is at the target ts");
    let app = App::open(dir, AppOptions { dev: true, ..Default::default() }).await.unwrap();
    verify(&app).await;
    let docs = app.query::<pitr_all>(()).await.unwrap();
    assert_eq!(&model_of(&docs), expected, "documents at ts {expected_ts}");
    // The search index answers from the replayed documents.
    let hits: Vec<String> =
        app.query::<pitr_search>(("alpha".to_string(),)).await.unwrap().iter().map(|i| i.to_string()).collect();
    let mut want: Vec<String> =
        expected.iter().filter(|(_, v)| v.1.contains("alpha")).map(|(k, _)| k.clone()).collect();
    let mut got = hits;
    want.sort();
    got.sort();
    assert_eq!(got, want, "search at ts {expected_ts}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn point_in_time_restores_match_a_model_at_random_points() {
    let target = store_target(Arc::new(InMemory::new()), "pitr");
    let mut r = options(target.clone());
    r.segment_max_entries = 9;
    let t = replicated(r).await;
    rep(&t).sync_now(t.app()).await.unwrap();
    let mut rng = StdRng::seed_from_u64(7);
    let mut model: Model = BTreeMap::new();
    // (ts, commit time, model after it), for every commit of ours.
    let mut history: Vec<(u64, f64, Model)> = Vec::new();
    let start_ts = rep(&t).status(t.app()).await.shipped_ts;
    history.push((start_ts, 0.0, model.clone()));
    let words = ["alpha", "beta", "gamma", "delta"];
    for i in 0..240i64 {
        let ids: Vec<String> = model.keys().cloned().collect();
        let owner = rng.random_range(0..5i64);
        let body =
            format!("{} {} {i}", words[rng.random_range(0..words.len())], words[rng.random_range(0..words.len())]);
        let pick = |rng: &mut StdRng| ids[rng.random_range(0..ids.len())].parse::<Id<Note>>().unwrap();
        let roll = rng.random_range(0..10);
        let ts = if ids.is_empty() || roll < 5 {
            let (id, ts) = op(&t, "insert", None, owner, &body, i).await;
            let id = id.unwrap();
            let doc = t.query::<pitr_all>(()).await.unwrap().into_iter().find(|d| d.id == id).unwrap();
            model.insert(id.to_string(), (owner, body, i, doc.creation_time));
            ts
        } else if roll < 8 {
            let id = pick(&mut rng);
            let (_, ts) = op(&t, "patch", Some(id), owner, &body, i).await;
            let ct = model[&id.to_string()].3;
            model.insert(id.to_string(), (owner, body, i, ct));
            ts
        } else {
            let id = pick(&mut rng);
            let (_, ts) = op(&t, "delete", Some(id), 0, "", 0).await;
            model.remove(&id.to_string());
            ts
        };
        if i % 17 == 0 {
            let _ = t.mutation::<pitr_fail>(("ROLLED-BACK".to_string(),)).await;
            t.mutation::<pitr_transient>(()).await.unwrap();
        }
        let time = t.app().db().snapshot().await.unwrap().time;
        history.push((ts, time, model.clone()));
        // Ship now and then, and take a second snapshot halfway (later targets restore from it).
        if i % 25 == 0 {
            rep(&t).sync_now(t.app()).await.unwrap();
        }
        if i == 120 {
            rep(&t).snapshot_now(t.app()).await.unwrap();
        }
    }
    rep(&t).sync_now(t.app()).await.unwrap();
    let latest = t.app().db().write_log().latest();
    let gens = replicate::list_generations(&target).await.unwrap();
    assert_eq!((gens.len(), gens[0].snapshots.len()), (1, 2));
    let model_at = |ts: u64| history.iter().rev().find(|(t, _, _)| *t <= ts).unwrap().2.clone();

    // Random ts targets (before and after the second snapshot), the oldest point, and the newest.
    let mut targets: Vec<u64> = (0..5).map(|_| rng.random_range(start_ts..=latest)).collect();
    targets.push(start_ts);
    targets.push(gens[0].snapshots[1].ts + 1);
    for ts in targets {
        let (dir, info) = restore_to(&target, PointInTime { to_ts: Some(ts), ..Default::default() }).await;
        assert!(info.reached && info.ts == ts, "{info:?}");
        let expected_snapshot = gens[0].snapshots.iter().rev().find(|s| s.ts <= ts).unwrap();
        assert_eq!(info.snapshot_ts, expected_snapshot.ts);
        assert_eq!(info.replayed, ts - expected_snapshot.ts);
        check_restored(dir.path(), ts, &model_at(ts)).await;
    }
    let (dir, info) = restore_to(&target, PointInTime::default()).await;
    assert_eq!((info.ts, info.reached), (latest, true));
    check_restored(dir.path(), latest, &model).await;
    let (_, info) = restore_to(&target, PointInTime { to_ts: Some(latest + 50), ..Default::default() }).await;
    assert_eq!((info.ts, info.reached), (latest, false), "beyond the newest commit: the newest is restored");

    // By time: the state after the last commit at or before it.
    let (k_ts, k_time, k_model) = history[history.len() / 3].clone();
    let (dir, info) = restore_to(&target, PointInTime { to_time: Some(k_time), ..Default::default() }).await;
    assert!(info.reached && info.ts >= k_ts && info.time <= k_time, "{info:?}");
    assert_eq!(model_at(info.ts), k_model);
    check_restored(dir.path(), info.ts, &k_model).await;

    // Before the oldest snapshot: refused with the reason.
    let err = replicate::restore_point_in_time(
        &target,
        tempfile::tempdir().unwrap().path(),
        &PointInTime { to_ts: Some(start_ts - 1), ..Default::default() },
        &RestoreOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(format!("{err:#}").contains("before the oldest snapshot"), "{err:#}");

    // A restored database starts a new generation when it replicates.
    let restored = tempfile::tempdir().unwrap();
    replicate::restore_point_in_time(
        &target,
        restored.path(),
        &PointInTime { to_ts: Some(start_ts + 10), ..Default::default() },
        &RestoreOptions::default(),
    )
    .await
    .unwrap();
    let app = App::open(
        restored.path(),
        AppOptions {
            dev: true,
            ops: OpsOptions { replication: Some(options(target.clone())), ..Default::default() },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    app.replication().unwrap().sync_now(&app).await.unwrap();
    let st = app.replication().unwrap().status(&app).await;
    assert_ne!(st.generation.as_deref(), Some(gens[0].id.as_str()));
    assert_eq!(replicate::list_generations(&target).await.unwrap().len(), 2);
}

// ---------------------------------------------------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------------------------------------------------

fn snap(ts: u64, at_h: f64) -> SnapshotInfo {
    let at = 1_760_000_000_000.0 + at_h * 3_600_000.0;
    SnapshotInfo {
        name: format!("rxferrix-auto-{}-ts{ts}.tar.zst", rxferrix::ops::file_timestamp(at)),
        ts,
        created_at: at,
        bytes: 1,
    }
}

fn generation(at_h: f64, snapshots: Vec<SnapshotInfo>, segments: &[(u64, u64)]) -> Generation {
    let at = 1_760_000_000_000.0 + at_h * 3_600_000.0;
    Generation {
        id: format!("{}-0000beef", rxferrix::ops::file_timestamp(at)),
        created_at: at,
        snapshots,
        segments: segments.iter().map(|&(from, to)| SegmentInfo { from, to, bytes: 1 }).collect(),
    }
}

#[test]
fn retention_keeps_exactly_what_point_in_time_restores_need() {
    let now = 1_760_000_000_000.0 + 100.0 * 3_600_000.0;
    let keep = Duration::from_secs(24 * 3600); // cutoff at hour 76
    let g1 = generation(0.0, vec![snap(1, 0.0), snap(40, 30.0)], &[(2, 20), (21, 45), (46, 60)]);
    let g2 = generation(
        50.0,
        vec![snap(10, 50.0), snap(30, 70.0), snap(50, 80.0), snap(70, 95.0)],
        &[(11, 25), (26, 30), (31, 55), (56, 72), (73, 90)],
    );
    let plan = plan_retention(&[g1.clone(), g2.clone()], now, keep);
    // g1 ends before the window's base (g2's snapshot at hour 70, the newest before the cutoff): all of it goes.
    assert_eq!(plan.generations, std::slice::from_ref(&g1.id));
    // g2 keeps 30 (the base), 50 and 70; 10 goes, with the segments that end at or before ts 30.
    assert_eq!(plan.snapshots, [(g2.id.clone(), snap(10, 50.0).name)]);
    assert_eq!(plan.segments, [(g2.id.clone(), 11, 25), (g2.id.clone(), 26, 30)]);

    // Everything inside the window: nothing to delete.
    assert!(plan_retention(std::slice::from_ref(&g2), 1_760_000_000_000.0 + 51.0 * 3_600_000.0, keep).is_empty());
    // A generation that started inside the window keeps the previous one's base and its later segments.
    let g3 = generation(90.0, vec![snap(5, 90.0)], &[(6, 9)]);
    let plan = plan_retention(&[g2.clone(), g3.clone()], now, keep);
    assert!(plan.generations.is_empty());
    assert_eq!(plan.snapshots, [(g2.id.clone(), snap(10, 50.0).name)]);
    assert_eq!(plan.segments, [(g2.id.clone(), 11, 25), (g2.id.clone(), 26, 30)]);
    // Only old snapshots: the newest one stays (with what follows it), whatever `keep` says.
    let plan = plan_retention(std::slice::from_ref(&g2), now + 1e9, Duration::ZERO);
    assert_eq!(plan.snapshots.len(), 3);
    assert_eq!(
        plan.segments.len(),
        3,
        "segments ending at or before ts 70 go; 56..72 (straddling the base) and 73..90 stay"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_runs_after_scheduled_snapshots() {
    let target = store_target(Arc::new(InMemory::new()), "keep");
    let mut r = options(target.clone());
    r.snapshot_every = Duration::from_secs(3600);
    r.keep = Duration::from_secs(2 * 3600);
    let t = replicated(r).await;
    rep(&t).sync_now(t.app()).await.unwrap();
    let mut snapshot_ts = vec![rep(&t).status(t.app()).await.shipped_ts];
    for hour in 1..=5 {
        for i in 0..3 {
            op(&t, "insert", None, hour, "x", i).await;
        }
        rep(&t).sync_now(t.app()).await.unwrap(); // ships; no snapshot due yet
        t.app().db().clock().advance(Duration::from_secs(3600));
        op(&t, "insert", None, hour, "y", 9).await;
        rep(&t).sync_now(t.app()).await.unwrap(); // ships, then the snapshot is due
        snapshot_ts.push(rep(&t).status(t.app()).await.last_snapshot.unwrap().ts);
    }
    let gens = replicate::list_generations(&target).await.unwrap();
    assert_eq!(gens.len(), 1);
    // Snapshots at hours 0..=5, keep 2h at hour 5: hours 3, 4, 5 are within, plus hour 2 as the base... the cutoff
    // is exactly hour 3, so hour 3 is the oldest kept one.
    let kept: Vec<u64> = gens[0].snapshots.iter().map(|s| s.ts).collect();
    assert_eq!(kept, snapshot_ts[3..].to_vec());
    assert!(gens[0].segments.iter().all(|s| s.to > snapshot_ts[3]), "{:?}", gens[0].segments);
    assert_eq!(gens[0].segments.first().unwrap().from, snapshot_ts[3] + 1, "segments ending at the base go");
    let (dir, info) = restore_to(&target, PointInTime { to_ts: Some(snapshot_ts[3] + 1), ..Default::default() }).await;
    assert_eq!(info.snapshot_ts, snapshot_ts[3]);
    assert_eq!(meta_ts(dir.path()), snapshot_ts[3] + 1);
}

// ---------------------------------------------------------------------------------------------------------------------
// Over HTTP
// ---------------------------------------------------------------------------------------------------------------------

struct Admin {
    t: TestApp,
    client: reqwest::Client,
    base: String,
    token: String,
}

impl Admin {
    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client.request(method, format!("{}{path}", self.base)).bearer_auth(&self.token)
    }
}

async fn admin_with(ops: OpsOptions) -> Admin {
    let options = AppOptions { dev: true, auth: AuthConfig::password().rate_limits(None), ops, ..Default::default() };
    let t = TestApp::with_options(options).await.unwrap();
    let base = format!("http://{}", t.serve().await.unwrap());
    let client = reqwest::Client::new();
    let app = t.app().clone();
    app.auth().create_admin(&app, "admin@example.com", "admin password".into()).await.unwrap();
    let r: Value = client
        .post(format!("{base}/api/auth/admin/signin"))
        .json(&json!({ "email": "admin@example.com", "password": "admin password" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    Admin { t, client, base, token: r["accessToken"].as_str().unwrap().into() }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to.join(e.file_name()));
        } else {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staged_point_in_time_restore_over_http() {
    use reqwest::Method;
    // Without replication: the endpoints say so.
    let plain = admin_with(OpsOptions::default()).await;
    let r: Value = plain.req(Method::GET, "/api/admin/replication").send().await.unwrap().json().await.unwrap();
    assert_eq!(r, json!({ "enabled": false }));
    let r = plain.req(Method::POST, "/api/admin/restore/point-in-time").json(&json!({})).send().await.unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(r.json::<Value>().await.unwrap()["errorData"]["code"], "replication_off");
    let listing: Value = plain.req(Method::GET, "/api/admin/backups").send().await.unwrap().json().await.unwrap();
    assert_eq!(listing["replication"], Value::Null);

    let target = store_target(Arc::new(InMemory::new()), "http");
    let a = admin_with(OpsOptions {
        replication: Some(options(target.clone())),
        metrics: rxferrix::ops::MetricsOptions { enabled: true, token: None },
        ..Default::default()
    })
    .await;
    rep(&a.t).sync_now(a.t.app()).await.unwrap();
    let (first, _) = op(&a.t, "insert", None, 1, "alpha kept", 1).await;
    let (_, target_ts) = op(&a.t, "insert", None, 2, "alpha also kept", 2).await;
    let wanted = model_of(&a.t.query::<pitr_all>(()).await.unwrap());
    op(&a.t, "delete", first, 0, "", 0).await;
    op(&a.t, "insert", None, 3, "alpha too late", 3).await;

    // Status: lag before shipping, none after; also in the backups listing and /metrics.
    let st: Value = a.req(Method::GET, "/api/admin/replication").send().await.unwrap().json().await.unwrap();
    assert_eq!(st["enabled"], true);
    assert!(st["lagCommits"].as_u64().unwrap() >= 4, "{st}");
    let metrics = a.client.get(format!("{}/metrics", a.base)).send().await.unwrap().text().await.unwrap();
    assert!(
        metrics.contains("rxferrix_replication_lag_commits ") && metrics.contains("rxferrix_replication_shipped_ts "),
        "{metrics}"
    );
    rep(&a.t).sync_now(a.t.app()).await.unwrap();
    let listing: Value = a.req(Method::GET, "/api/admin/backups").send().await.unwrap().json().await.unwrap();
    assert_eq!(listing["replication"]["lagCommits"], 0);
    assert_eq!(listing["replication"]["generations"].as_array().unwrap().len(), 1);
    let r = a.req(Method::POST, "/api/admin/replication/snapshot").send().await.unwrap();
    assert_eq!(r.status(), 201);
    assert!(r.json::<Value>().await.unwrap()["ts"].as_u64().unwrap() > target_ts);

    // Bad requests.
    let r = a
        .req(Method::POST, "/api/admin/restore/point-in-time")
        .json(&json!({ "toTime": "yesterday" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = a.req(Method::POST, "/api/admin/restore/point-in-time").json(&json!({ "toTs": 1 })).send().await.unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(r.json::<Value>().await.unwrap()["errorData"]["code"], "invalid_point_in_time");

    // Stage the point: checked, pending, the server keeps its data.
    let r = a
        .req(Method::POST, "/api/admin/restore/point-in-time")
        .json(&json!({ "toTs": target_ts }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let p = &body["pendingRestore"];
    assert_eq!(p["ts"], target_ts);
    assert_eq!(p["pointInTime"]["ts"], target_ts);
    assert_eq!(p["pointInTime"]["reached"], true);
    assert!(p["source"].as_str().unwrap().contains(&format!("ts {target_ts}")), "{p}");
    let r = a.req(Method::POST, "/api/admin/restore/point-in-time").json(&json!({})).send().await.unwrap();
    assert_eq!(r.status(), 409, "one staged restore at a time");
    assert_eq!(a.t.query::<pitr_all>(()).await.unwrap().len(), 2, "the running server is unaffected");

    // Applied at the next start (here: a copy of the data directory's staged files).
    let dest = tempfile::tempdir().unwrap();
    copy_dir(&a.t.dir().join(restore::STAGED_DIR), &dest.path().join(restore::STAGED_DIR));
    std::fs::copy(a.t.dir().join(restore::MARKER), dest.path().join(restore::MARKER)).unwrap();
    let report = restore::apply_pending(dest.path(), None).unwrap().expect("applied");
    assert_eq!(report.manifest.ts, target_ts);
    check_restored(dest.path(), target_ts, &wanted).await;

    // By time, through the same endpoint (after canceling).
    assert_eq!(a.req(Method::DELETE, "/api/admin/restore").send().await.unwrap().status(), 200);
    let now = chrono::Utc::now().to_rfc3339();
    let r =
        a.req(Method::POST, "/api/admin/restore/point-in-time").json(&json!({ "toTime": now })).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["pendingRestore"]["pointInTime"]["requestedTime"].as_f64().is_some());
}

// ---------------------------------------------------------------------------------------------------------------------
// Overhead
// ---------------------------------------------------------------------------------------------------------------------

async fn commits_per_second(replication: Option<ReplicationOptions>, n: u64) -> f64 {
    let options =
        AppOptions { dev: false, ops: OpsOptions { replication, ..Default::default() }, ..Default::default() };
    let t = TestApp::with_options(options).await.unwrap();
    if let Some(r) = t.app().replication() {
        r.sync_now(t.app()).await.unwrap();
    }
    for i in 0..200 {
        op(&t, "insert", None, i, "warm-up alpha beta", i).await;
    }
    let start = Instant::now();
    let mut id = None;
    for i in 0..n as i64 {
        let (new, _) =
            op(&t, if i % 3 == 2 { "patch" } else { "insert" }, id, i % 7, "a note with some alpha text in it", i)
                .await;
        id = new.or(id);
    }
    n as f64 / start.elapsed().as_secs_f64()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn bench_journal_overhead() {
    let n = std::env::var("BENCH_COMMITS").ok().and_then(|v| v.parse().ok()).unwrap_or(5000);
    let mut rows = Vec::new();
    for round in 0..3 {
        let off = commits_per_second(None, n).await;
        let dir = tempfile::tempdir().unwrap();
        let mut idle = options(dir_target(dir.path()));
        idle.every = Duration::from_secs(3600);
        let journal_only = commits_per_second(Some(idle), n).await;
        let dir = tempfile::tempdir().unwrap();
        let mut shipping = ReplicationOptions::with_target(dir_target(dir.path()));
        shipping.every = Duration::from_secs(1);
        let shipped = commits_per_second(Some(shipping), n).await;
        rows.push((off, journal_only, shipped));
        println!(
            "round {round}: off {off:.0}/s, journal {journal_only:.0}/s, journal + shipping every 1s {shipped:.0}/s"
        );
    }
    let med = |f: fn(&(f64, f64, f64)) -> f64| {
        let mut v: Vec<f64> = rows.iter().map(f).collect();
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let (off, on, ship) = (med(|r| r.0), med(|r| r.1), med(|r| r.2));
    println!(
        "median commits/s over {n} mutations: off {off:.0}, journal {on:.0} ({:+.1}%), journal + shipping {ship:.0} ({:+.1}%)",
        (on / off - 1.0) * 100.0,
        (ship / off - 1.0) * 100.0
    );
}

/// Deleted files are kept as long as point-in-time restores reach back (`StorageConfig::keep_deleted` defaults to
/// `--replicate-keep` when replicating): a restore to before a deletion finds the file's contents.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn point_in_time_restores_find_deleted_files() {
    use rxferrix::storage::{StorageBackend, StorageConfig};

    use crate::storage::app::{delete_file, read_text, store_text};

    let target_dir = tempfile::tempdir().unwrap();
    let target = dir_target(target_dir.path());
    let t = replicated(options(target.clone())).await;
    assert_eq!(t.app().storage().config().keep_deleted, Some(Duration::from_secs(7 * 86_400)));
    rep(&t).sync_now(t.app()).await.unwrap();
    let file = t.action::<store_text>(("hello".to_string(),)).await.unwrap();
    let before_delete = t.app().db().write_log().latest();
    t.mutation::<delete_file>((file, false)).await.unwrap();
    rep(&t).sync_now(t.app()).await.unwrap();

    // Past the orphan grace, the sweep keeps the deleted file's blob.
    t.advance_time(Duration::from_secs(2 * 3600)).await.unwrap();
    let stats = t.app().storage().sweep(t.app()).await.unwrap();
    assert_eq!((stats.deleted, stats.kept), (0, 1), "{stats:?}");
    let storage_dir = t.dir().join("storage");
    assert!(storage_dir.join(file.to_string()).exists());

    // Restored to before the deletion (blobs stay where they are: the restore only replaces the database), the file
    // is back, contents and all.
    let (dir, _) = restore_to(&target, PointInTime { to_ts: Some(before_delete), ..Default::default() }).await;
    assert_eq!(meta_ts(dir.path()), before_delete);
    let storage = StorageConfig { backend: StorageBackend::Local(Some(storage_dir)), ..Default::default() };
    let restored = App::open(dir.path(), AppOptions { dev: true, storage, ..Default::default() }).await.unwrap();
    assert_eq!(restored.action::<read_text>((file,)).await.unwrap().as_deref(), Some("hello"));
}
