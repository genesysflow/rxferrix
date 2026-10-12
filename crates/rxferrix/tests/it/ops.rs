//! Operations (`rxferrix::ops`): online backups and offline restore, scheduled backups with rotation, export/import
//! round trips, CSV, `/metrics`, static frontend serving, TLS with certificate files, and the admin gating of the
//! `/api/admin/*` operations endpoints.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rxferrix::auth::AuthConfig;
use rxferrix::ops::backup::{self, BackupSpec, BackupTarget, RestoreOptions, ScheduledBackup};
use rxferrix::ops::export::{self, ExportFormat, ExportOptions, ImportMode, ImportOptions};
use rxferrix::ops::tls::{self, Listen};
use rxferrix::ops::{DirLock, MetricsOptions, OpsOptions, StaticFiles, WebOptions};
use rxferrix::testing::TestApp;
use rxferrix::{App, AppOptions, FunctionRef, Table};
use serde_json::{Value, json};

pub mod app {
    use rxferrix::prelude::*;
    use rxferrix::storage::StoredFile;

    /// Documents written in pairs by one mutation, so a torn snapshot would show an odd count for some group.
    #[table(name = "ops_pairs")]
    #[index(by_group = [group])]
    #[search_index(search_text, search = text, filter = [group])]
    pub struct Pair {
        pub group: i64,
        pub text: String,
    }

    #[mutation]
    pub async fn add_pair(ctx: &MutationCtx, group: i64, text: String) -> Result<()> {
        ctx.db.insert(Pair { group, text: format!("{text} a") }).await?;
        ctx.db.insert(Pair { group, text: format!("{text} b") }).await?;
        Ok(())
    }

    #[query]
    pub async fn all_pairs(ctx: &QueryCtx) -> Result<Vec<Doc<Pair>>> {
        ctx.db.query::<Pair>().collect().await
    }

    #[table(name = "ops_docs")]
    #[index(by_rank = [rank])]
    #[search_index(search_title, search = title, filter = [rank])]
    pub struct OpsDoc {
        pub title: String,
        pub rank: i64,
        pub file: Option<Id<StoredFile>>,
        pub parent: Option<Id<OpsDoc>>,
        pub tags: Vec<String>,
    }

    #[mutation]
    pub async fn add_doc(
        ctx: &MutationCtx,
        title: String,
        rank: i64,
        file: Option<Id<StoredFile>>,
        parent: Option<Id<OpsDoc>>,
    ) -> Result<Id<OpsDoc>> {
        ctx.db.insert(OpsDoc { title, rank, file, parent, tags: vec!["a,b".into(), "say \"hi\"".into()] }).await
    }

    #[query]
    pub async fn docs(ctx: &QueryCtx) -> Result<Vec<Doc<OpsDoc>>> {
        ctx.db.query::<OpsDoc>().collect().await
    }

    #[query]
    pub async fn search_docs(ctx: &QueryCtx, text: String) -> Result<Vec<Doc<OpsDoc>>> {
        ctx.db
            .query::<OpsDoc>()
            .with_search_index(OpsDoc::search_title, |q| q.search(OpsDoc::title, text))
            .take(50)
            .await
    }

    #[query]
    pub async fn by_rank(ctx: &QueryCtx, rank: i64) -> Result<Vec<Doc<OpsDoc>>> {
        ctx.db.query::<OpsDoc>().with_index(OpsDoc::by_rank, |q| q.eq(OpsDoc::rank, rank)).collect().await
    }

    #[object]
    #[derive(Debug, Clone, PartialEq)]
    pub enum Mood {
        Happy,
        Sad,
    }

    #[object]
    #[derive(Debug, Clone, PartialEq)]
    pub struct Meta {
        pub score: f64,
        pub label: Option<String>,
    }

    /// Every kind of value a CSV cell has to carry back (`csv_import_round_trips_mixed_types`).
    #[table(name = "ops_mixed")]
    pub struct Mixed {
        pub name: String,
        pub count: i64,
        pub ratio: f64,
        pub flag: bool,
        pub note: Option<String>,
        pub maybe: Option<bool>,
        pub tags: Vec<String>,
        pub meta: Meta,
        pub mood: Mood,
        pub data: Bytes,
        pub owner: Option<Id<OpsDoc>>,
    }

    /// Internal: its argument type would otherwise show up in the codegen fixture of `codegen.rs`.
    #[internal_mutation]
    pub async fn add_mixed(ctx: &MutationCtx, doc: Mixed) -> Result<Id<Mixed>> {
        ctx.db.insert(doc).await
    }

    #[query]
    pub async fn mixed(ctx: &QueryCtx) -> Result<Vec<Doc<Mixed>>> {
        ctx.db.query::<Mixed>().collect().await
    }

    #[action]
    pub async fn store_text(ctx: &ActionCtx, text: String) -> Result<Id<StoredFile>> {
        ctx.storage.store(text.into_bytes(), Some("text/plain")).await
    }

    #[action]
    pub async fn read_text(ctx: &ActionCtx, id: Id<StoredFile>) -> Result<Option<String>> {
        Ok(ctx.storage.get(id).await?.map(|b| String::from_utf8_lossy(&b).into_owned()))
    }
}

use app::*;

async fn verify(app: &App) {
    let problems = app.db().verify_indexes().await.unwrap();
    assert!(problems.is_empty(), "{problems:?}");
}

fn archive_path(target_dir: &Path, info: &rxferrix::ops::BackupInfo) -> PathBuf {
    target_dir.join(&info.name)
}

// ---------------------------------------------------------------------------------------------------------------------
// Backups
// ---------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backup_during_writes_restores_a_consistent_snapshot() {
    let t = TestApp::new().await.unwrap();
    // A fresh app may already have committed at startup (system crons such as `_storage_sweep`): count from here.
    let base_ts = t.app().db().write_log().latest();
    for g in 0..20 {
        t.mutation::<add_pair>((g, format!("seed {g}"))).await.unwrap();
    }
    let stop = Arc::new(AtomicBool::new(false));
    let mut writers = Vec::new();
    for w in 0..3 {
        let (app, stop) = (t.app().clone(), stop.clone());
        writers.push(tokio::spawn(async move {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                app.mutation::<add_pair>((w * 1000 + (n % 50) as i64, format!("writer {w} n{n}"))).await.unwrap();
                n += 1;
            }
            n
        }));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let target_dir = tempfile::tempdir().unwrap();
    let target = BackupTarget::parse(target_dir.path().to_str().unwrap(), t.dir()).unwrap();
    let ts_before = t.app().db().write_log().latest();
    let spec = BackupSpec { include_aux: true, include_files: true, include_secret: false };
    let info = backup::create(t.app(), &spec, &target, false).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stop.store(true, Ordering::Relaxed);
    let mut written = 0;
    for w in writers {
        written += w.await.unwrap();
    }
    let ts_after = t.app().db().write_log().latest();
    let snapshot_ts = info.ts.unwrap();
    assert!(
        written > 0 && snapshot_ts >= ts_before && snapshot_ts < ts_after,
        "{ts_before} {snapshot_ts} {ts_after} {written}"
    );
    let listed = &target.list().await.unwrap()[0];
    assert_eq!(listed.name, info.name);
    assert_eq!(listed.ts, Some(snapshot_ts), "listings report the snapshot ts");

    let restored = tempfile::tempdir().unwrap();
    let report = backup::restore(&archive_path(target_dir.path(), &info), restored.path(), &RestoreOptions::default())
        .await
        .unwrap();
    assert_eq!(report.manifest.ts, snapshot_ts);
    assert!(report.previous.is_none());
    assert!(restored.path().join("aux.db").exists() && !restored.path().join("auth_secret.key").exists());

    let app = App::open(restored.path(), AppOptions { dev: true, ..Default::default() }).await.unwrap();
    verify(&app).await;
    assert_eq!(app.db().write_log().latest(), snapshot_ts);
    let pairs = app.query::<all_pairs>(()).await.unwrap();
    // Every mutation wrote 2 documents atomically: the snapshot has whole mutations only, one per ts.
    assert_eq!(pairs.len() as u64, 2 * (snapshot_ts - base_ts), "one pair per commit");
    let mut by_text = std::collections::HashMap::<String, usize>::new();
    for p in &pairs {
        *by_text.entry(p.text.trim_end_matches(" a").trim_end_matches(" b").to_string()).or_default() += 1;
    }
    assert!(by_text.values().all(|n| *n == 2), "a torn mutation in the snapshot");
}

#[tokio::test]
async fn restore_is_refused_while_locked_and_validates_before_swapping() {
    let t = TestApp::new().await.unwrap();
    t.mutation::<add_pair>((1, "one".to_string())).await.unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let target = BackupTarget::parse(target_dir.path().to_str().unwrap(), t.dir()).unwrap();
    let info = backup::create(t.app(), &BackupSpec::default(), &target, false).await.unwrap();
    let archive = archive_path(target_dir.path(), &info);

    // A data dir with a database of its own and a "server" holding the lock.
    let dest = tempfile::tempdir().unwrap();
    let other = App::open(dest.path(), AppOptions::default()).await.unwrap();
    other.mutation::<add_pair>((7, "keep me".to_string())).await.unwrap();
    drop(other);
    let lock = DirLock::acquire(dest.path()).unwrap();
    let err = backup::restore(&archive, dest.path(), &RestoreOptions::default()).await.unwrap_err();
    assert!(format!("{err:#}").contains("is in use"), "{err:#}");
    drop(lock);

    // A corrupt archive is refused and the current database stays.
    let mut bytes = std::fs::read(&archive).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    let corrupt = target_dir.path().join("ferrix-corrupt.tar.zst");
    std::fs::write(&corrupt, &bytes).unwrap();
    assert!(backup::restore(&corrupt, dest.path(), &RestoreOptions::default()).await.is_err());
    let not_a_backup = target_dir.path().join("ferrix-junk.tar.zst");
    std::fs::write(&not_a_backup, b"junk").unwrap();
    assert!(backup::restore(&not_a_backup, dest.path(), &RestoreOptions::default()).await.is_err());
    {
        let app = App::open(dest.path(), AppOptions::default()).await.unwrap();
        let texts: Vec<String> = app.query::<all_pairs>(()).await.unwrap().into_iter().map(|p| p.value.text).collect();
        assert_eq!(texts, ["keep me a", "keep me b"]);
    }

    // Unlocked and valid: restored, and the previous files (persisted vector indexes included) are kept aside.
    std::fs::write(dest.path().join("vector_index_99.hnsw"), b"stale").unwrap();
    let report = backup::restore(&archive, dest.path(), &RestoreOptions::default()).await.unwrap();
    let previous = report.previous.unwrap();
    assert!(previous.join("data.db").exists() && previous.join("vector_index_99.hnsw").exists());
    assert!(!dest.path().join("vector_index_99.hnsw").exists());
    let app = App::open(dest.path(), AppOptions::default()).await.unwrap();
    let texts: Vec<String> = app.query::<all_pairs>(()).await.unwrap().into_iter().map(|p| p.value.text).collect();
    assert_eq!(texts, ["one a", "one b"]);
    verify(&app).await;
}

#[tokio::test]
async fn backups_include_files_and_the_secret_only_when_asked() {
    let t = TestApp::new().await.unwrap();
    let file = t.action::<store_text>(("blob content".to_string(),)).await.unwrap();
    t.mutation::<add_doc>(("with file".to_string(), 1, Some(file), None)).await.unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let target = BackupTarget::parse(target_dir.path().to_str().unwrap(), t.dir()).unwrap();

    let plain = backup::create(t.app(), &BackupSpec::default(), &target, false).await.unwrap();
    let full = BackupSpec { include_aux: false, include_files: true, include_secret: true };
    tokio::time::sleep(Duration::from_millis(5)).await;
    let with_all = backup::create(t.app(), &full, &target, false).await.unwrap();

    let dest = tempfile::tempdir().unwrap();
    let report = backup::restore(&archive_path(target_dir.path(), &plain), dest.path(), &RestoreOptions::default())
        .await
        .unwrap();
    assert_eq!(report.manifest.storage, "excluded");
    assert_eq!(report.manifest.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["data.db"]);

    let dest = tempfile::tempdir().unwrap();
    let report = backup::restore(&archive_path(target_dir.path(), &with_all), dest.path(), &RestoreOptions::default())
        .await
        .unwrap();
    assert_eq!(report.blobs_restored, 1);
    assert_eq!(
        std::fs::read(dest.path().join("auth_secret.key")).unwrap(),
        std::fs::read(t.dir().join("auth_secret.key")).unwrap()
    );
    let app = App::open(dest.path(), AppOptions::default()).await.unwrap();
    assert_eq!(app.action::<read_text>((file,)).await.unwrap().as_deref(), Some("blob content"));
}

#[tokio::test]
async fn scheduled_backups_rotate() {
    let target_dir = tempfile::tempdir().unwrap();
    let schedule = ScheduledBackup {
        to: Some(target_dir.path().to_str().unwrap().into()),
        keep: 2,
        include_aux: false,
        include_files: false,
    };
    let mut options = AppOptions { dev: true, ..Default::default() };
    options.scheduler.crons.push(schedule.cron(Duration::from_secs(3600)));
    let t = TestApp::with_options(options).await.unwrap();
    let target = BackupTarget::parse(target_dir.path().to_str().unwrap(), t.dir()).unwrap();
    // A manual backup in the same place is never rotated away.
    let manual = backup::create(t.app(), &BackupSpec::default(), &target, false).await.unwrap();
    for i in 0..4 {
        t.mutation::<add_pair>((i, format!("round {i}"))).await.unwrap();
        assert_eq!(t.advance_time(Duration::from_secs(3601)).await.unwrap(), 1, "round {i}");
    }
    let list = target.list().await.unwrap();
    let scheduled: Vec<_> = list.iter().filter(|b| b.scheduled).collect();
    assert_eq!(scheduled.len(), 2, "{list:?}");
    assert!(list.iter().any(|b| b.name == manual.name && !b.scheduled));
    // The newest two survive: the newest has all four rounds.
    let dest = tempfile::tempdir().unwrap();
    backup::restore(&target_dir.path().join(&scheduled[0].name), dest.path(), &RestoreOptions::default())
        .await
        .unwrap();
    let app = App::open(dest.path(), AppOptions::default()).await.unwrap();
    assert_eq!(app.query::<all_pairs>(()).await.unwrap().len(), 8);
    // The cron shows on the Scheduler page like any other.
    let crons =
        t.app().run_query("_system/scheduler:crons", json!({}), &admin_identity(&t).await).await.result.unwrap();
    assert!(crons.as_array().unwrap().iter().any(|c| c["name"] == backup::CRON_NAME), "{crons}");
}

async fn admin_identity(t: &TestApp) -> rxferrix::Identity {
    let id = t.app().auth().create_admin(t.app(), "ops-admin@example.com", "admin password".into()).await.unwrap();
    rxferrix::Identity::admin(id, None)
}

// ---------------------------------------------------------------------------------------------------------------------
// Export and import
// ---------------------------------------------------------------------------------------------------------------------

async fn seed_docs(t: &TestApp) -> Vec<rxferrix::Id<OpsDoc>> {
    let file = t.action::<store_text>(("attached".to_string(),)).await.unwrap();
    let root = t.mutation::<add_doc>(("root document, with comma".to_string(), 1, Some(file), None)).await.unwrap();
    let mut ids = vec![root];
    for i in 0..4 {
        ids.push(
            t.mutation::<add_doc>((format!("child number {i}\nsecond line"), i % 2, None, Some(root))).await.unwrap(),
        );
    }
    ids
}

fn strip(docs: &[rxferrix::Doc<OpsDoc>]) -> Vec<Value> {
    docs.iter().map(|d| serde_json::to_value(d).unwrap()).collect()
}

#[tokio::test]
async fn export_import_round_trip_keeps_ids_indexes_search_and_files() {
    let a = TestApp::new().await.unwrap();
    seed_docs(&a).await;
    let original = a.query::<docs>(()).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let options = ExportOptions {
        tables: Some(vec!["ops_docs".into(), "_storage".into()]),
        format: ExportFormat::Jsonl,
        include_files: true,
    };
    let manifest =
        export::export_to_dir(a.dir(), Some(export::BlobStore::of_app(a.app())), out.path(), &options).await.unwrap();
    assert_eq!(manifest.files, 1);
    assert_eq!(manifest.tables.iter().find(|t| t.name == "ops_docs").unwrap().documents, 5);
    let lines = std::fs::read_to_string(out.path().join("ops_docs.jsonl")).unwrap();
    assert_eq!(lines.lines().count(), 5);

    // Same table numbers: identical documents (ids and creation times included).
    let b = TestApp::new().await.unwrap();
    let ts = b.app().db().write_log().latest();
    let import = ImportOptions { mode: ImportMode::Insert, batch_size: 2, allow_system_tables: true, format: None };
    let report = export::import_path(b.app(), out.path(), None, &import).await.unwrap();
    assert!(!report.remapped_ids && report.files == 1);
    assert_eq!(
        report.tables.iter().map(|t| (t.table.as_str(), t.documents)).collect::<Vec<_>>(),
        [("_storage", 1), ("ops_docs", 5)]
    );
    // Batches of 2 through the normal write path: 1 + 3 commits, each in the write log (so subscriptions rerun).
    assert_eq!(b.app().db().write_log().latest(), ts + 4);
    let imported = b.query::<docs>(()).await.unwrap();
    assert_eq!(strip(&imported), strip(&original));
    verify(b.app()).await;
    assert_eq!(b.query::<search_docs>(("second line".to_string(),)).await.unwrap().len(), 4);
    assert_eq!(b.query::<by_rank>((1,)).await.unwrap().len(), 3);
    assert_eq!(b.action::<read_text>((imported[0].file.unwrap(),)).await.unwrap().as_deref(), Some("attached"));

    // Insert again: refused before anything is written. Upsert: replaced. Replace: extra documents go.
    let ts = b.app().db().write_log().latest();
    let err = export::import_path(b.app(), out.path(), Some("ops_docs"), &import).await.unwrap_err();
    assert!(format!("{err:#}").contains("exist in `ops_docs` already"), "{err:#}");
    assert_eq!(b.app().db().write_log().latest(), ts);
    let upsert = ImportOptions { mode: ImportMode::Upsert, ..import.clone() };
    let report = export::import_path(b.app(), out.path(), Some("ops_docs"), &upsert).await.unwrap();
    assert_eq!(report.tables[0].replaced, 5);
    b.mutation::<add_doc>(("extra".to_string(), 9, None, None)).await.unwrap();
    let replace = ImportOptions { mode: ImportMode::Replace, ..import.clone() };
    let report = export::import_path(b.app(), out.path(), Some("ops_docs"), &replace).await.unwrap();
    assert_eq!((report.tables[0].deleted, report.tables[0].documents), (6, 5));
    assert_eq!(strip(&b.query::<docs>(()).await.unwrap()), strip(&original));
    verify(b.app()).await;

    // A database whose tables were created in another order: ids are rewritten, references follow.
    let dir = tempfile::tempdir().unwrap();
    {
        let first = vec![OpsDoc::table_def()];
        let db = rxferrix::db::Database::open_with(dir.path(), &first, &Default::default()).unwrap();
        drop(db);
    }
    let c = App::open(dir.path(), AppOptions { dev: true, ..Default::default() }).await.unwrap();
    assert_ne!(
        c.db().catalog().table("ops_docs").unwrap().number,
        a.app().db().catalog().table("ops_docs").unwrap().number
    );
    let report = export::import_path(&c, out.path(), None, &import).await.unwrap();
    assert!(report.remapped_ids);
    verify(&c).await;
    let moved = c.query::<docs>(()).await.unwrap();
    assert_eq!(moved.len(), 5);
    let root = moved.iter().find(|d| d.parent.is_none()).unwrap();
    assert_eq!(root.id.to_string()[7..], original[0].id.to_string()[7..], "same random part");
    assert!(moved.iter().filter(|d| d.parent.is_some()).all(|d| d.parent == Some(root.id)));
    assert_eq!(c.action::<read_text>((root.file.unwrap(),)).await.unwrap().as_deref(), Some("attached"));
    assert_eq!(
        moved.iter().map(|d| d.creation_time).collect::<Vec<_>>(),
        original.iter().map(|d| d.creation_time).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn import_validates_every_line_first() {
    let t = TestApp::new().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("ops_docs.jsonl");
    std::fs::write(
        &file,
        "{\"title\":\"ok\",\"rank\":1,\"file\":null,\"parent\":null,\"tags\":[]}\n\n{\"title\":5,\"rank\":1,\"tags\":[]}\n",
    )
    .unwrap();
    let ts = t.app().db().write_log().latest();
    let err = export::import_path(t.app(), &file, None, &ImportOptions::default()).await.unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("ops_docs.jsonl:3") && msg.contains("does not match table `ops_docs`"), "{msg}");
    assert_eq!(t.app().db().write_log().latest(), ts, "nothing written");
    // Without `_id`, documents get new ids; system tables need the CLI's permission.
    std::fs::write(&file, "{\"title\":\"ok\",\"rank\":1,\"file\":null,\"parent\":null,\"tags\":[]}\n").unwrap();
    export::import_path(t.app(), &file, None, &ImportOptions::default()).await.unwrap();
    assert_eq!(t.query::<docs>(()).await.unwrap().len(), 1);
    let users = dir.path().join("_users.jsonl");
    std::fs::write(&users, "{}\n").unwrap();
    assert!(
        format!("{:#}", export::import_path(t.app(), &users, None, &ImportOptions::default()).await.unwrap_err())
            .contains("system table")
    );
}

#[tokio::test]
async fn csv_export_escapes_fields() {
    let t = TestApp::new().await.unwrap();
    seed_docs(&t).await;
    let out = tempfile::tempdir().unwrap();
    let options =
        ExportOptions { tables: Some(vec!["ops_docs".into()]), format: ExportFormat::Csv, include_files: false };
    export::export_to_dir(t.dir(), None, out.path(), &options).await.unwrap();
    let csv = std::fs::read_to_string(out.path().join("ops_docs.csv")).unwrap();
    let mut records = csv.split("\r\n");
    assert_eq!(records.next().unwrap(), "_id,_creationTime,file,parent,rank,tags,title");
    let docs = t.query::<docs>(()).await.unwrap();
    // Rows are in id order; find the root document's.
    let first = records.find(|r| r.starts_with(&format!("{},", docs[0].id))).unwrap();
    let expected_tags = "\"[\"\"a,b\"\",\"\"say \\\"\"hi\\\"\"\"\"]\"";
    assert!(first.contains(expected_tags), "{first}");
    assert!(first.ends_with(",\"root document, with comma\""), "{first}");
    // A newline inside a field stays inside its quotes.
    assert!(csv.contains("\"child number 0\nsecond line\""), "{csv}");
    assert_eq!(csv.matches("\r\n").count(), 6);
}

fn mixed_doc(name: &str, count: i64, note: Option<&str>, owner: Option<rxferrix::Id<OpsDoc>>) -> Mixed {
    Mixed {
        name: name.into(),
        count,
        ratio: count as f64 / 3.0,
        flag: count % 2 == 0,
        note: note.map(str::to_string),
        maybe: (count % 3 != 0).then_some(count % 3 == 1),
        tags: vec![name.into(), "a,b".into(), "say \"hi\"".into()],
        meta: Meta { score: -0.1 * count as f64, label: (count > 1).then(|| format!("label {count}")) },
        mood: if count % 2 == 0 { Mood::Happy } else { Mood::Sad },
        data: rxferrix::Bytes::new(vec![0, count as u8, 255]),
        owner,
    }
}

#[tokio::test]
async fn csv_import_round_trips_mixed_types() {
    let a = TestApp::new().await.unwrap();
    let owner = a.mutation::<add_doc>(("owner".to_string(), 1, None, None)).await.unwrap();
    // Strings that look like other types (only the table's shape tells them apart), quoting, line breaks, numbers
    // that need every digit, and nested values.
    let names = ["42", "true", "null", "", " padded ", "a,b\nc \"quoted\"", "{\"x\":1}", "[1]", "plain", "1e400"];
    for (i, name) in names.iter().enumerate() {
        let note = match i % 3 {
            0 => None,
            1 => Some("12"),
            _ => Some("false"),
        };
        a.mutation::<add_mixed>((mixed_doc(name, i as i64 - 3, note, (i % 2 == 0).then_some(owner)),)).await.unwrap();
    }
    let original = a.query::<mixed>(()).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let options =
        ExportOptions { tables: Some(vec!["ops_mixed".into()]), format: ExportFormat::Csv, include_files: false };
    let manifest = export::export_to_dir(a.dir(), None, out.path(), &options).await.unwrap();
    assert_eq!(manifest.tables[0].documents, names.len() as u64);

    // Into a fresh database: the export directory (CSV manifest), then a single .csv file with replace.
    let b = TestApp::new().await.unwrap();
    let import = ImportOptions { batch_size: 3, ..Default::default() };
    let report = export::import_path(b.app(), out.path(), None, &import).await.unwrap();
    assert_eq!(report.tables[0].documents, names.len() as u64);
    let as_json =
        |docs: &[rxferrix::Doc<Mixed>]| docs.iter().map(|d| serde_json::to_value(d).unwrap()).collect::<Vec<_>>();
    assert_eq!(as_json(&b.query::<mixed>(()).await.unwrap()), as_json(&original), "ids, creation times and values");
    let replace = ImportOptions { mode: ImportMode::Replace, ..import.clone() };
    let report = export::import_path(b.app(), &out.path().join("ops_mixed.csv"), None, &replace).await.unwrap();
    assert_eq!((report.tables[0].deleted, report.tables[0].documents), (names.len() as u64, names.len() as u64));
    assert_eq!(as_json(&b.query::<mixed>(()).await.unwrap()), as_json(&original));
    verify(b.app()).await;

    // Hand-written CSV: no `_id` / `_creationTime` columns (new ids), columns in any order, LF line ends.
    let file = out.path().join("hand.csv");
    std::fs::write(
        &file,
        "flag,name,count,ratio,note,maybe,tags,meta,mood,data,owner\n\
         false,007,7,0.5,,,[],\"{\"\"score\"\":1,\"\"label\"\":null}\",sad,\"{\"\"$bytes\"\":\"\"AQI=\"\"}\",\n",
    )
    .unwrap();
    let csv_insert = ImportOptions { format: Some(ExportFormat::Csv), ..Default::default() };
    export::import_path(b.app(), &file, Some("ops_mixed"), &csv_insert).await.unwrap();
    let added = b.query::<mixed>(()).await.unwrap().into_iter().find(|d| d.name == "007").unwrap();
    assert_eq!((added.count, added.note.clone(), added.maybe, added.data.0.clone()), (7, None, None, vec![1, 2]));

    // Bad cells are reported with their line, and nothing is written.
    let ts = b.app().db().write_log().latest();
    std::fs::write(
        &file,
        "name,count,ratio,flag,note,maybe,tags,meta,mood,data,owner\nx,many,1,true,,,[],{},happy,{},\nshort,1\n",
    )
    .unwrap();
    let err = format!("{:#}", export::import_path(b.app(), &file, Some("ops_mixed"), &csv_insert).await.unwrap_err());
    assert!(err.contains("hand.csv:2") && err.contains("does not match table `ops_mixed`"), "{err}");
    assert!(err.contains("hand.csv:3") && err.contains("but the header has 11 column(s)"), "{err}");
    assert_eq!(b.app().db().write_log().latest(), ts);
}

// ---------------------------------------------------------------------------------------------------------------------
// HTTP: metrics, admin endpoints, static files, TLS
// ---------------------------------------------------------------------------------------------------------------------

async fn serve(app: &App, web: &WebOptions) -> SocketAddr {
    let server =
        tls::start(rxferrix::ops::web::router(app.clone(), web), &Listen::Http("127.0.0.1:0".parse().unwrap()))
            .await
            .unwrap();
    let addr = server.addr;
    tokio::spawn(server.wait());
    addr
}

/// A minimal check of the Prometheus text format: every sample line is `name{labels} value`, and every metric family
/// is declared with `# TYPE` before its samples.
fn check_prometheus(text: &str) {
    let mut declared = std::collections::HashSet::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            let mut parts = rest.split(' ');
            declared.insert(parts.next().unwrap().to_string());
            assert!(matches!(parts.next(), Some("counter" | "gauge" | "histogram")), "{line}");
            continue;
        }
        if line.starts_with("# HELP ") {
            continue;
        }
        let name_end = line.find(['{', ' ']).unwrap_or_else(|| panic!("no value: {line}"));
        let name = &line[..name_end];
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':'), "{line}");
        let family =
            ["_bucket", "_sum", "_count"].iter().find_map(|s| name.strip_suffix(s)).filter(|f| declared.contains(*f));
        assert!(declared.contains(name) || family.is_some(), "undeclared {name}");
        let mut rest = &line[name_end..];
        if rest.starts_with('{') {
            let mut in_quotes = false;
            let mut escaped = false;
            let mut end = None;
            for (i, c) in rest.char_indices() {
                match c {
                    _ if escaped => escaped = false,
                    '\\' if in_quotes => escaped = true,
                    '"' => in_quotes = !in_quotes,
                    '}' if !in_quotes => {
                        end = Some(i);
                        break;
                    }
                    _ => {}
                }
            }
            rest = &rest[end.unwrap_or_else(|| panic!("unterminated labels: {line}")) + 1..];
        }
        let value = rest.strip_prefix(' ').unwrap_or_else(|| panic!("no value: {line}"));
        assert!(value.parse::<f64>().is_ok() || value == "+Inf" || value == "NaN", "bad value in {line}");
    }
}

#[tokio::test]
async fn metrics_endpoint_format_and_token() {
    let options = AppOptions {
        dev: true,
        ops: OpsOptions {
            metrics: MetricsOptions { enabled: true, token: Some("s3cret".into()) },
            ..Default::default()
        },
        ..Default::default()
    };
    let t = TestApp::with_options(options).await.unwrap();
    let addr = serve(t.app(), &WebOptions::default()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");
    let r = client
        .post(format!("{base}/api/mutation"))
        .json(&json!({ "path": add_pair::path(), "args": { "group": 1, "text": "x" } }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = client
        .post(format!("{base}/api/query"))
        .json(&json!({ "path": "nope:nope", "args": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    t.query::<all_pairs>(()).await.unwrap();

    assert_eq!(client.get(format!("{base}/metrics")).send().await.unwrap().status(), 401);
    assert_eq!(client.get(format!("{base}/metrics")).bearer_auth("wrong").send().await.unwrap().status(), 401);
    let r = client.get(format!("{base}/metrics")).bearer_auth("s3cret").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "text/plain; version=0.0.4; charset=utf-8");
    let text = r.text().await.unwrap();
    check_prometheus(&text);
    let pair = add_pair::path();
    for expected in [
        "rxferrix_http_requests_total{route=\"/api/mutation\",status=\"200\"} 1".to_string(),
        "rxferrix_http_requests_total{route=\"/api/query\",status=\"404\"} 1".to_string(),
        "rxferrix_http_requests_total{route=\"/metrics\",status=\"401\"} 2".to_string(),
        format!("rxferrix_function_executions_total{{path=\"{pair}\",kind=\"mutation\",outcome=\"success\"}} 1"),
        format!("rxferrix_function_duration_seconds_count{{path=\"{pair}\",kind=\"mutation\"}} 1"),
        format!("rxferrix_function_duration_seconds_bucket{{path=\"{pair}\",kind=\"mutation\",le=\"+Inf\"}} 1"),
        "rxferrix_commit_ts ".to_string(),
        "rxferrix_commits_total ".to_string(),
        "rxferrix_commits_per_second ".to_string(),
        "rxferrix_websocket_connections 0".to_string(),
        "rxferrix_shared_queries 0".to_string(),
        "rxferrix_query_reruns_total 0".to_string(),
        "rxferrix_query_cache_entries 0".to_string(),
        "rxferrix_query_cache_bytes 0".to_string(),
        "rxferrix_query_cache_hits_total 0".to_string(),
        "rxferrix_query_cache_misses_total ".to_string(),
        "rxferrix_query_cache_evictions_total 0".to_string(),
        "rxferrix_db_file_bytes{file=\"data.db\"} ".to_string(),
        "rxferrix_scheduler_queue_depth 0".to_string(),
        "rxferrix_exec_log_dropped_total 0".to_string(),
    ] {
        assert!(text.contains(&expected), "missing `{expected}` in\n{text}");
    }
    assert!(text.contains(&format!("path=\"{}\",kind=\"query\",outcome=\"success\"", all_pairs::path())));
    let ts: u64 = text.lines().find_map(|l| l.strip_prefix("rxferrix_commit_ts ")).unwrap().parse().unwrap();
    assert_eq!(ts, t.app().db().write_log().latest());

    // Off by default: 404 and nothing counted.
    let off = TestApp::new().await.unwrap();
    let addr = serve(off.app(), &WebOptions::default()).await;
    assert_eq!(client.get(format!("http://{addr}/metrics")).send().await.unwrap().status(), 404);
}

#[derive(rxferrix::rust_embed::Embed)]
#[folder = "tests/it/fixtures/web/"]
#[crate_path = "rxferrix::rust_embed"]
struct Fixture;

#[tokio::test]
async fn static_files_spa_fallback_caching_and_precompressed() {
    let t = TestApp::new().await.unwrap();
    let web = tempfile::tempdir().unwrap();
    let root = web.path();
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("index.html"), "<html>app index</html>").unwrap();
    std::fs::write(root.join("assets/app-1a2b3c4d.js"), "console.log('plain')").unwrap();
    std::fs::write(root.join("assets/app-1a2b3c4d.js.br"), "BROTLI").unwrap();
    std::fs::write(root.join("assets/app-1a2b3c4d.js.gz"), "GZIP").unwrap();
    std::fs::write(root.join("robots.txt"), "User-agent: *").unwrap();
    std::fs::write(root.join("docs/index.html"), "<html>docs</html>").unwrap();
    // A file that would shadow the API if static files came first.
    std::fs::create_dir_all(root.join("api")).unwrap();
    std::fs::write(root.join("api/health"), "shadowed").unwrap();
    let hook = axum::Router::new()
        .route("/hooks/echo", axum::routing::post(|body: String| async move { format!("hook:{body}") }));
    let options = WebOptions { static_files: Some(StaticFiles::Dir(root.to_path_buf())), routes: Some(hook) };
    let addr = serve(t.app(), &options).await;
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let get = |path: &str| client.get(format!("http://{addr}{path}"));

    let r = get("/").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["cache-control"], "no-cache");
    assert_eq!(r.headers()["content-type"], "text/html; charset=utf-8");
    assert_eq!(r.text().await.unwrap(), "<html>app index</html>");
    // SPA fallback for client-side routes, also with an extension when the browser asks for HTML.
    for path in ["/todos/42", "/deep/nested/route", "/user/john.doe"] {
        let r = get(path).header("accept", "text/html,*/*").send().await.unwrap();
        assert_eq!(
            (r.status().as_u16(), r.text().await.unwrap()),
            (200, "<html>app index</html>".to_string()),
            "{path}"
        );
    }
    let r = get("/todos/42").send().await.unwrap();
    assert_eq!(r.status(), 200, "no extension: a route");
    assert_eq!(get("/missing.png").send().await.unwrap().status(), 404);
    assert_eq!(get("/docs/").send().await.unwrap().text().await.unwrap(), "<html>docs</html>");

    // Hashed assets are immutable; other files revalidate.
    let r = get("/assets/app-1a2b3c4d.js").send().await.unwrap();
    assert_eq!(r.headers()["cache-control"], "public, max-age=31536000, immutable");
    assert_eq!(r.headers()["content-type"], "text/javascript; charset=utf-8");
    assert_eq!(r.headers()["vary"], "Accept-Encoding");
    assert!(r.headers().get("content-encoding").is_none());
    let etag = r.headers()["etag"].to_str().unwrap().to_string();
    assert_eq!(r.text().await.unwrap(), "console.log('plain')");
    let r = get("/assets/app-1a2b3c4d.js").header("if-none-match", &etag).send().await.unwrap();
    assert_eq!(r.status(), 304);
    assert_eq!(get("/robots.txt").send().await.unwrap().headers()["cache-control"], "no-cache");

    // Precompressed variants: brotli preferred, gzip otherwise.
    let r = get("/assets/app-1a2b3c4d.js").header("accept-encoding", "gzip, br").send().await.unwrap();
    assert_eq!(r.headers()["content-encoding"], "br");
    assert_eq!(r.headers()["content-type"], "text/javascript; charset=utf-8");
    assert_eq!(r.text().await.unwrap(), "BROTLI");
    let r = get("/assets/app-1a2b3c4d.js").header("accept-encoding", "gzip").send().await.unwrap();
    assert_eq!(r.headers()["content-encoding"], "gzip");
    assert_ne!(r.headers()["etag"].to_str().unwrap(), etag);
    assert_eq!(r.text().await.unwrap(), "GZIP");

    // `/api` and `/_` are never static: real routes answer, unknown ones are 404s, not the SPA.
    let r = get("/api/health").send().await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap(), json!({ "status": "ok" }));
    for path in ["/api/nope", "/api", "/api/storage/../health"] {
        let r = get(path).header("accept", "text/html").send().await.unwrap();
        assert_ne!(r.text().await.unwrap(), "<html>app index</html>", "{path}");
    }
    let r = get("/_/").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(!r.text().await.unwrap().contains("app index"), "the dashboard, not the app");
    let r = get("/_/data/todos").header("accept", "text/html").send().await.unwrap();
    assert!(!r.text().await.unwrap().contains("app index"));
    let r = client.post(format!("http://{addr}/api/unknown")).send().await.unwrap();
    assert_eq!(r.status(), 404);
    // Custom routes.
    let r = client.post(format!("http://{addr}/hooks/echo")).body("ping").send().await.unwrap();
    assert_eq!(r.text().await.unwrap(), "hook:ping");
    let r = client.head(format!("http://{addr}/")).send().await.unwrap();
    assert_eq!(r.status(), 200);

    // Embedded files behave the same.
    let options = WebOptions {
        static_files: Some(StaticFiles::Embedded(<Fixture as rxferrix::rust_embed::RustEmbed>::get)),
        routes: None,
    };
    let addr = serve(t.app(), &options).await;
    let r = client.get(format!("http://{addr}/some/route")).send().await.unwrap();
    assert!(r.text().await.unwrap().contains("embedded index"));
    let r = client.get(format!("http://{addr}/assets/app-1a2b3c4d.js")).send().await.unwrap();
    assert_eq!(r.headers()["cache-control"], "public, max-age=31536000, immutable");
    let etag = r.headers()["etag"].to_str().unwrap().to_string();
    let r =
        client.get(format!("http://{addr}/assets/app-1a2b3c4d.js")).header("if-none-match", etag).send().await.unwrap();
    assert_eq!(r.status(), 304);
}

#[tokio::test]
async fn tls_with_a_self_signed_certificate_and_the_redirect_listener() {
    let t = TestApp::new().await.unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap().self_signed(&key).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, key.serialize_pem()).unwrap();
    let listen = Listen::CertFiles {
        https: "127.0.0.1:0".parse().unwrap(),
        redirect: Some("127.0.0.1:0".parse().unwrap()),
        cert: cert_path.clone(),
        key: key_path.clone(),
        reload_every: None,
    };
    let server =
        tls::start(rxferrix::ops::web::router(t.app().clone(), &WebOptions::default()), &listen).await.unwrap();
    let (addr, redirect) = (server.addr, server.redirect_addr.unwrap());
    assert!(server.https);

    let trusted = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(cert.pem().as_bytes()).unwrap())
        .resolve("localhost", addr)
        .build()
        .unwrap();
    let r = trusted.get(format!("https://localhost:{}/api/health", addr.port())).send().await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap(), json!({ "status": "ok" }));
    // A client that doesn't trust the certificate can't connect.
    let untrusted = reqwest::Client::builder().resolve("localhost", addr).build().unwrap();
    assert!(untrusted.get(format!("https://localhost:{}/api/health", addr.port())).send().await.is_err());

    // The redirect listener sends everything to https, path and query kept.
    let plain = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let r = plain.get(format!("http://{redirect}/some/path?x=1")).header("host", "localhost").send().await.unwrap();
    assert_eq!(r.status(), 308);
    assert_eq!(r.headers()["location"], format!("https://localhost:{}/some/path?x=1", addr.port()).as_str());

    // A key that doesn't match the certificate is refused at startup.
    let other = rcgen::KeyPair::generate().unwrap();
    std::fs::write(&key_path, other.serialize_pem()).unwrap();
    let bad = Listen::CertFiles {
        https: "127.0.0.1:0".parse().unwrap(),
        redirect: None,
        cert: cert_path,
        key: key_path,
        reload_every: None,
    };
    assert!(tls::start(rxferrix::ops::web::router(t.app().clone(), &WebOptions::default()), &bad).await.is_err());

    server.shutdown();
    tokio::time::timeout(Duration::from_secs(15), server.wait()).await.unwrap().unwrap();
}

#[tokio::test]
async fn tls_certificate_files_are_reloaded_without_dropping_connections() {
    let t = TestApp::new().await.unwrap();
    let pair = || {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap().self_signed(&key).unwrap();
        (cert, key)
    };
    let (first, first_key) = pair();
    let (second, second_key) = pair();
    let dir = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert_path, first.pem()).unwrap();
    std::fs::write(&key_path, first_key.serialize_pem()).unwrap();
    let listen = Listen::CertFiles {
        https: "127.0.0.1:0".parse().unwrap(),
        redirect: None,
        cert: cert_path.clone(),
        key: key_path.clone(),
        reload_every: Some(Duration::from_millis(50)),
    };
    let server =
        tls::start(rxferrix::ops::web::router(t.app().clone(), &WebOptions::default()), &listen).await.unwrap();
    let addr = server.addr;
    let url = format!("https://localhost:{}/api/health", addr.port());
    // Trusts both certificates and reports the one the server presented.
    let client = || {
        reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(first.pem().as_bytes()).unwrap())
            .add_root_certificate(reqwest::Certificate::from_pem(second.pem().as_bytes()).unwrap())
            .resolve("localhost", addr)
            .tls_info(true)
            .build()
            .unwrap()
    };
    async fn presented(client: &reqwest::Client, url: &str) -> Vec<u8> {
        let r = client.get(url).send().await.unwrap();
        assert_eq!(r.status(), 200);
        r.extensions().get::<reqwest::tls::TlsInfo>().unwrap().peer_certificate().unwrap().to_vec()
    }
    let kept_alive = client();
    assert_eq!(presented(&kept_alive, &url).await, first.der().to_vec());

    // A half-written renewal (the new certificate, the old key) doesn't load: the current certificate stays.
    std::fs::write(&cert_path, second.pem()).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(presented(&client(), &url).await, first.der().to_vec());

    // Once the key is in place too, new connections get the new certificate; the open one keeps working.
    std::fs::write(&key_path, second_key.serialize_pem()).unwrap();
    let mut swapped = false;
    for _ in 0..100 {
        if presented(&client(), &url).await == second.der().to_vec() {
            swapped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(swapped, "the new certificate was never served");
    assert_eq!(presented(&kept_alive, &url).await, first.der().to_vec(), "the kept-alive connection was not dropped");

    // Garbage in the files: still the second certificate.
    std::fs::write(&cert_path, "not a certificate").unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(presented(&client(), &url).await, second.der().to_vec());

    server.shutdown();
    tokio::time::timeout(Duration::from_secs(15), server.wait()).await.unwrap().unwrap();
}

#[tokio::test]
async fn acme_redirect_targets_the_configured_domain() {
    let router = tls::redirect_router(443, vec!["example.com".into(), "www.example.com".into()]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let r = client.get(format!("http://{addr}/a?b=c")).header("host", "www.example.com").send().await.unwrap();
    assert_eq!(
        (r.status().as_u16(), r.headers()["location"].to_str().unwrap()),
        (308, "https://www.example.com/a?b=c")
    );
    let r = client.post(format!("http://{addr}/x")).header("host", "evil.test").send().await.unwrap();
    assert_eq!(r.headers()["location"], "https://example.com/x");
}

// ---------------------------------------------------------------------------------------------------------------------
// Admin endpoints
// ---------------------------------------------------------------------------------------------------------------------

struct Admin {
    t: TestApp,
    client: reqwest::Client,
    base: String,
    admin: String,
    user: String,
}

async fn admin_setup() -> Admin {
    admin_setup_with(AppOptions::default()).await
}

async fn admin_setup_with(options: AppOptions) -> Admin {
    let options = AppOptions { dev: true, auth: AuthConfig::password().rate_limits(None), ..options };
    let t = TestApp::with_options(options).await.unwrap();
    let base = format!("http://{}", t.serve().await.unwrap());
    let client = reqwest::Client::new();
    let app = t.app().clone();
    app.auth().create_admin(&app, "admin@example.com", "admin password".into()).await.unwrap();
    let admin: Value = client
        .post(format!("{base}/api/auth/admin/signin"))
        .json(&json!({ "email": "admin@example.com", "password": "admin password" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let user: Value = client
        .post(format!("{base}/api/auth/signup"))
        .json(&json!({ "email": "user@example.com", "password": "user password" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    Admin {
        t,
        client,
        base,
        admin: admin["accessToken"].as_str().unwrap().into(),
        user: user["accessToken"].as_str().unwrap().into(),
    }
}

impl Admin {
    fn req(&self, method: reqwest::Method, path: &str, token: Option<&str>) -> reqwest::RequestBuilder {
        let r = self.client.request(method, format!("{}{path}", self.base));
        match token {
            Some(t) => r.bearer_auth(t),
            None => r,
        }
    }
}

#[tokio::test]
async fn ops_admin_endpoints_are_admin_only() {
    let a = admin_setup().await;
    use reqwest::Method;
    let endpoints = [
        (Method::GET, "/api/admin/backups"),
        (Method::POST, "/api/admin/backups"),
        (Method::GET, "/api/admin/backups/ferrix-x.tar.zst"),
        (Method::DELETE, "/api/admin/backups/ferrix-x.tar.zst"),
        (Method::GET, "/api/admin/export/ops_docs"),
        (Method::POST, "/api/admin/import/ops_docs"),
        (Method::POST, "/api/admin/backups/ferrix-x.tar.zst/restore"),
        (Method::GET, "/api/admin/restore"),
        (Method::POST, "/api/admin/restore"),
        (Method::DELETE, "/api/admin/restore"),
        (Method::POST, "/api/admin/restore/restart"),
        (Method::POST, "/api/admin/restore/point-in-time"),
        (Method::GET, "/api/admin/replication"),
        (Method::POST, "/api/admin/replication/snapshot"),
    ];
    for (method, path) in &endpoints {
        let r = a.req(method.clone(), path, None).send().await.unwrap();
        let status = r.status().as_u16();
        let body: Value = r.json().await.unwrap_or(Value::Null);
        assert_eq!((status, body["errorData"]["code"].as_str()), (401, Some("unauthenticated")), "{method} {path}");
        let r = a.req(method.clone(), path, Some(&a.user)).send().await.unwrap();
        let status = r.status().as_u16();
        let body: Value = r.json().await.unwrap_or(Value::Null);
        assert_eq!((status, body["errorData"]["code"].as_str()), (403, Some("forbidden")), "{method} {path}");
        let r = a.req(method.clone(), path, Some("not.a.token")).send().await.unwrap();
        assert_eq!(r.status(), 401, "{method} {path}");
    }
    // Nothing was created by the refused calls.
    let r: Value = a.req(Method::GET, "/api/admin/backups", Some(&a.admin)).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["backups"], json!([]));
    assert_eq!(
        (r["pendingRestore"].clone(), r["restart"].clone()),
        (Value::Null, json!({ "available": false, "exitCode": null }))
    );
    assert!(!a.t.dir().join(rxferrix::ops::restore::MARKER).exists());
}

#[tokio::test]
async fn ops_admin_backup_export_and_import_flows() {
    let a = admin_setup().await;
    use reqwest::Method;
    a.t.mutation::<add_doc>(("exported, \"quoted\"".to_string(), 3, None, None)).await.unwrap();

    // Backups: create, list, download, delete.
    let r = a
        .req(Method::POST, "/api/admin/backups", Some(&a.admin))
        .json(&json!({ "includeAux": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let info: Value = r.json().await.unwrap();
    let name = info["name"].as_str().unwrap().to_string();
    assert!(name.starts_with("ferrix-") && name.ends_with(".tar.zst") && info["ts"].as_u64().unwrap() > 0, "{info}");
    let r = a
        .req(Method::POST, "/api/admin/backups", Some(&a.admin))
        .json(&json!({ "includeSecret": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "never the secret over HTTP");
    let list: Value =
        a.req(Method::GET, "/api/admin/backups", Some(&a.admin)).send().await.unwrap().json().await.unwrap();
    assert_eq!(list["backups"][0]["name"], name.as_str());
    assert!(list["target"].as_str().unwrap().ends_with("backups"));
    let r = a.req(Method::GET, &format!("/api/admin/backups/{name}"), Some(&a.admin)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "application/zstd");
    assert!(r.headers()["content-disposition"].to_str().unwrap().contains(&name));
    let bytes = r.bytes().await.unwrap();
    assert_eq!(bytes.len() as u64, info["bytes"].as_u64().unwrap());
    assert_eq!(&bytes[..4], &[0x28, 0xb5, 0x2f, 0xfd], "zstd magic");
    // The downloaded archive restores.
    let dl = tempfile::tempdir().unwrap();
    std::fs::write(dl.path().join(&name), &bytes).unwrap();
    let dest = tempfile::tempdir().unwrap();
    backup::restore(&dl.path().join(&name), dest.path(), &RestoreOptions::default()).await.unwrap();
    assert_eq!(
        a.req(Method::GET, "/api/admin/backups/..%2Fdata.db", Some(&a.admin)).send().await.unwrap().status(),
        400
    );
    let r = a.req(Method::DELETE, &format!("/api/admin/backups/{name}"), Some(&a.admin)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let r = a.req(Method::DELETE, &format!("/api/admin/backups/{name}"), Some(&a.admin)).send().await.unwrap();
    assert_eq!(r.status(), 404);

    // Export: JSONL and CSV, streamed; system tables masked; unknown tables 404.
    let r = a.req(Method::GET, "/api/admin/export/ops_docs", Some(&a.admin)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "application/x-ndjson; charset=utf-8");
    assert!(r.headers()["content-disposition"].to_str().unwrap().contains("ops_docs.jsonl"));
    let jsonl = r.text().await.unwrap();
    assert_eq!(jsonl.lines().count(), 1);
    let doc: Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    assert_eq!(doc["title"], "exported, \"quoted\"");
    let r = a.req(Method::GET, "/api/admin/export/ops_docs?format=csv", Some(&a.admin)).send().await.unwrap();
    assert_eq!(r.headers()["content-type"], "text/csv; charset=utf-8");
    let csv = r.text().await.unwrap();
    assert!(
        csv.starts_with("_id,_creationTime,file,parent,rank,tags,title\r\n")
            && csv.contains(",\"exported, \"\"quoted\"\"\"\r\n"),
        "{csv}"
    );
    let users = a
        .req(Method::GET, "/api/admin/export/_credentials", Some(&a.admin))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(users.contains("••••") && !users.contains("$argon2"), "{users}");
    assert_eq!(a.req(Method::GET, "/api/admin/export/nope", Some(&a.admin)).send().await.unwrap().status(), 404);
    assert_eq!(
        a.req(Method::GET, "/api/admin/export/ops_docs?format=xml", Some(&a.admin)).send().await.unwrap().status(),
        400
    );

    // Import: the exported JSONL goes back in with upsert; a system table is refused; bad input is a 400.
    let r = a
        .req(Method::POST, "/api/admin/import/ops_docs?mode=upsert", Some(&a.admin))
        .body(jsonl.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let report: Value = r.json().await.unwrap();
    assert_eq!(
        (report["tables"][0]["documents"].as_u64(), report["tables"][0]["replaced"].as_u64()),
        (Some(1), Some(1))
    );
    let r = a.req(Method::POST, "/api/admin/import/ops_docs", Some(&a.admin)).body(jsonl.clone()).send().await.unwrap();
    assert_eq!(r.status(), 400, "insert mode: the id exists");
    let new_doc = "{\"title\":\"new\",\"rank\":4,\"file\":null,\"parent\":null,\"tags\":[]}\n";
    let r = a.req(Method::POST, "/api/admin/import/ops_docs", Some(&a.admin)).body(new_doc).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(a.t.query::<docs>(()).await.unwrap().len(), 2);
    let r = a.req(Method::POST, "/api/admin/import/_users", Some(&a.admin)).body("{}\n").send().await.unwrap();
    assert_eq!(r.status(), 400);
    let r = a
        .req(Method::POST, "/api/admin/import/ops_docs?mode=bogus", Some(&a.admin))
        .body(new_doc)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    // CSV: the exported CSV goes back in (upsert keeps the ids); CSV sent as JSONL is refused.
    let r = a
        .req(Method::POST, "/api/admin/import/ops_docs?mode=upsert&format=csv", Some(&a.admin))
        .body(csv.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let report: Value = r.json().await.unwrap();
    assert_eq!(
        (report["tables"][0]["documents"].as_u64(), report["tables"][0]["replaced"].as_u64()),
        (Some(1), Some(1))
    );
    assert_eq!(
        a.t.query::<docs>(()).await.unwrap().iter().find(|d| d.rank == 3).unwrap().title,
        "exported, \"quoted\""
    );
    let r = a
        .req(Method::POST, "/api/admin/import/ops_docs?mode=upsert", Some(&a.admin))
        .body(csv.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = a
        .req(Method::POST, "/api/admin/import/ops_docs?format=xml", Some(&a.admin))
        .body(csv.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    verify(a.t.app()).await;
}

// ---------------------------------------------------------------------------------------------------------------------
// Staged restores
// ---------------------------------------------------------------------------------------------------------------------

/// A backup (with aux.db, a file blob and the secret) of a database with the pair "one", and a data directory
/// (closed) whose database has "keep me".
async fn backup_and_other_dir() -> (TestApp, tempfile::TempDir, PathBuf, tempfile::TempDir) {
    let t = TestApp::new().await.unwrap();
    t.mutation::<add_pair>((1, "one".to_string())).await.unwrap();
    let file = t.action::<store_text>(("restored blob".to_string(),)).await.unwrap();
    t.mutation::<add_doc>(("with file".to_string(), 1, Some(file), None)).await.unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let target = BackupTarget::parse(target_dir.path().to_str().unwrap(), t.dir()).unwrap();
    let spec = BackupSpec { include_aux: true, include_files: true, include_secret: true };
    let info = backup::create(t.app(), &spec, &target, false).await.unwrap();
    let archive = archive_path(target_dir.path(), &info);
    let dest = tempfile::tempdir().unwrap();
    let other = App::open(dest.path(), AppOptions::default()).await.unwrap();
    other.mutation::<add_pair>((7, "keep me".to_string())).await.unwrap();
    drop(other);
    (t, target_dir, archive, dest)
}

async fn pair_texts(dir: &Path) -> Vec<String> {
    let app = App::open(dir, AppOptions::default()).await.unwrap();
    let texts = app.query::<all_pairs>(()).await.unwrap().into_iter().map(|p| p.value.text).collect();
    verify(&app).await;
    texts
}

#[tokio::test]
async fn staged_restore_is_applied_at_the_next_start() {
    use rxferrix::ops::restore;
    let (_t, _target, archive, dest) = backup_and_other_dir().await;
    let key_before = std::fs::read(dest.path().join("auth_secret.key")).unwrap();
    {
        // Staged while a "server" holds the data directory: nothing it uses changes.
        let _lock = DirLock::acquire(dest.path()).unwrap();
        let pending = restore::stage(dest.path(), &archive, "the backup", Some("admin-1".into())).await.unwrap();
        assert!(
            pending.includes_aux && pending.blobs == 1 && pending.secret_ignored && pending.error.is_none(),
            "{pending:?}"
        );
        assert_eq!(restore::pending(dest.path()).unwrap().as_ref(), Some(&pending));
        assert!(dest.path().join(restore::STAGED_DIR).join("data.db").exists());
        assert!(!dest.path().join(restore::STAGED_DIR).join("auth_secret.key").exists(), "the key is never staged");
        // A second staging is refused until the first is canceled.
        let err = restore::stage(dest.path(), &archive, "again", None).await.unwrap_err();
        assert!(format!("{err:#}").contains("staged already"), "{err:#}");
    }
    assert_eq!(pair_texts(dest.path()).await, ["keep me a", "keep me b"], "opening the app doesn't apply it");

    // The next start (serve calls this with the lock held, before opening the database).
    let report = restore::apply_pending(dest.path(), None).unwrap().expect("applied");
    assert!(report.previous.as_ref().unwrap().join("data.db").exists() && report.blobs_restored == 1);
    assert!(!dest.path().join(restore::MARKER).exists() && !dest.path().join(restore::STAGED_DIR).exists());
    assert_eq!(pair_texts(dest.path()).await, ["one a", "one b"]);
    assert_eq!(std::fs::read(dest.path().join("auth_secret.key")).unwrap(), key_before, "the server's key stays");
    let app = App::open(dest.path(), AppOptions::default()).await.unwrap();
    let doc = app.query::<docs>(()).await.unwrap().remove(0);
    assert_eq!(app.action::<read_text>((doc.file.unwrap(),)).await.unwrap().as_deref(), Some("restored blob"));
    drop(app);
    assert!(restore::apply_pending(dest.path(), None).unwrap().is_none(), "nothing left to apply");
}

#[tokio::test]
async fn an_interrupted_staged_restore_is_finished_by_the_next_start() {
    use rxferrix::ops::restore;
    let (_t, _target, archive, dest) = backup_and_other_dir().await;
    let pending = restore::stage(dest.path(), &archive, "the backup", None).await.unwrap();
    // A crash after the old data.db went aside and the restored one moved in, before aux.db and the marker.
    let previous = dest.path().join(format!("pre-restore-{}", rxferrix::ops::file_timestamp(pending.staged_at)));
    std::fs::create_dir_all(&previous).unwrap();
    for f in ["data.db", "data.db-wal", "data.db-shm"] {
        if dest.path().join(f).exists() {
            std::fs::rename(dest.path().join(f), previous.join(f)).unwrap();
        }
    }
    std::fs::rename(dest.path().join(restore::STAGED_DIR).join("data.db"), dest.path().join("data.db")).unwrap();
    let report = restore::apply_pending(dest.path(), None).unwrap().expect("finished");
    assert_eq!(report.previous.as_deref(), Some(previous.as_path()));
    assert!(previous.join("aux.db").exists() && previous.join("data.db").exists());
    assert_eq!(pair_texts(dest.path()).await, ["one a", "one b"]);
}

#[tokio::test]
async fn staged_restores_cancel_and_refuse_bad_archives() {
    use rxferrix::ops::restore;
    let (_t, target, archive, dest) = backup_and_other_dir().await;
    // Canceled: the marker and the files go, the next start changes nothing.
    restore::stage(dest.path(), &archive, "the backup", None).await.unwrap();
    assert!(restore::cancel(dest.path()).unwrap());
    assert!(!restore::cancel(dest.path()).unwrap());
    assert!(!dest.path().join(restore::MARKER).exists() && !dest.path().join(restore::STAGED_DIR).exists());
    assert!(restore::apply_pending(dest.path(), None).unwrap().is_none());
    assert_eq!(pair_texts(dest.path()).await, ["keep me a", "keep me b"]);

    // A corrupt archive is refused when staging: nothing is staged, no temporary files stay.
    let mut bytes = std::fs::read(&archive).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    let corrupt = target.path().join("ferrix-corrupt.tar.zst");
    std::fs::write(&corrupt, &bytes).unwrap();
    let err = restore::stage(dest.path(), &corrupt, "corrupt", None).await.unwrap_err();
    let user = err.chain().find_map(|c| c.downcast_ref::<rxferrix::UserError>()).expect("a user error");
    assert_eq!(user.code, "invalid_backup");
    assert!(restore::pending(dest.path()).unwrap().is_none() && !dest.path().join(restore::STAGED_DIR).exists());
    let leftovers: Vec<String> = std::fs::read_dir(dest.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".ferrix-") || n.starts_with(".pending-restore"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // Staged files changed after staging: not applied, the server starts on its data, the marker says why.
    restore::stage(dest.path(), &archive, "the backup", None).await.unwrap();
    let staged_db = dest.path().join(restore::STAGED_DIR).join("data.db");
    let mut db = std::fs::read(&staged_db).unwrap();
    let last = db.len() - 1;
    db[last] ^= 1;
    std::fs::write(&staged_db, db).unwrap();
    assert!(restore::apply_pending(dest.path(), None).unwrap().is_none());
    let marker = restore::pending(dest.path()).unwrap().unwrap();
    assert!(marker.error.as_deref().unwrap().contains("changed since it was staged"), "{marker:?}");
    assert_eq!(pair_texts(dest.path()).await, ["keep me a", "keep me b"]);
    assert!(restore::apply_pending(dest.path(), None).unwrap().is_none(), "not retried");
    assert!(restore::cancel(dest.path()).unwrap());
}

#[tokio::test]
async fn staged_restore_over_http() {
    use reqwest::Method;
    use rxferrix::ops::restore::{self, RestartHandle};
    let restart = RestartHandle::new(Some(75));
    let options =
        AppOptions { ops: OpsOptions { restart: restart.clone(), ..Default::default() }, ..Default::default() };
    let a = admin_setup_with(options).await;
    a.t.mutation::<add_pair>((1, "from the backup".to_string())).await.unwrap();
    let r = a.req(Method::POST, "/api/admin/backups", Some(&a.admin)).json(&json!({})).send().await.unwrap();
    let name = r.json::<Value>().await.unwrap()["name"].as_str().unwrap().to_string();
    a.t.mutation::<add_pair>((2, "after the backup".to_string())).await.unwrap();

    // Restart needs a staged restore; unknown backups are 404.
    let r = a.req(Method::POST, "/api/admin/restore/restart", Some(&a.admin)).send().await.unwrap();
    assert_eq!(r.status(), 409);
    let r = a.req(Method::POST, "/api/admin/backups/ferrix-nope.tar.zst/restore", Some(&a.admin)).send().await.unwrap();
    assert_eq!(r.status(), 404);

    // Stage a backup of the target: validated, pending, shown in the listing; the server keeps serving its data.
    let r = a.req(Method::POST, &format!("/api/admin/backups/{name}/restore"), Some(&a.admin)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["pendingRestore"]["source"], name.as_str());
    assert_eq!(body["restart"], json!({ "available": true, "exitCode": 75 }));
    let list: Value =
        a.req(Method::GET, "/api/admin/backups", Some(&a.admin)).send().await.unwrap().json().await.unwrap();
    assert_eq!(list["pendingRestore"]["source"], name.as_str());
    assert_eq!(a.t.query::<all_pairs>(()).await.unwrap().len(), 4);
    let r = a.req(Method::POST, &format!("/api/admin/backups/{name}/restore"), Some(&a.admin)).send().await.unwrap();
    assert_eq!(r.status(), 409, "one staged restore at a time");

    // Cancel; canceling again is a 404.
    let r = a.req(Method::DELETE, "/api/admin/restore", Some(&a.admin)).send().await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap(), json!({ "canceled": true }));
    assert_eq!(a.req(Method::DELETE, "/api/admin/restore", Some(&a.admin)).send().await.unwrap().status(), 404);
    let state: Value =
        a.req(Method::GET, "/api/admin/restore", Some(&a.admin)).send().await.unwrap().json().await.unwrap();
    assert_eq!(state["pendingRestore"], Value::Null);

    // Upload: a corrupt archive is refused at once; the real one is staged.
    let bytes = a
        .req(Method::GET, &format!("/api/admin/backups/{name}"), Some(&a.admin))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let mut corrupt = bytes.to_vec();
    let mid = corrupt.len() / 2;
    corrupt[mid] ^= 0xff;
    let r =
        a.req(Method::POST, "/api/admin/restore?name=mine.tar.zst", Some(&a.admin)).body(corrupt).send().await.unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(r.json::<Value>().await.unwrap()["errorData"]["code"], "invalid_backup");
    let r = a.req(Method::POST, "/api/admin/restore", Some(&a.admin)).body(Vec::<u8>::new()).send().await.unwrap();
    assert_eq!(r.status(), 400);
    let r = a
        .req(Method::POST, "/api/admin/restore?name=../my%20backup.tar.zst", Some(&a.admin))
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["pendingRestore"]["source"], "..mybackup.tar.zst");
    assert!(body["pendingRestore"].get("stagedFiles").is_none(), "{body}");
    assert!(a.t.dir().join(restore::STAGED_DIR).join("data.db").exists());

    // Restart: accepted, then the handle fires (`serve` then shuts down and exits with 75).
    assert!(!restart.is_requested());
    let r = a.req(Method::POST, "/api/admin/restore/restart", Some(&a.admin)).send().await.unwrap();
    assert_eq!(r.status(), 202);
    assert_eq!(r.json::<Value>().await.unwrap(), json!({ "restarting": true, "exitCode": 75 }));
    tokio::time::timeout(Duration::from_secs(5), restart.requested()).await.unwrap();
    let args = json!({ "paginationOpts": { "numItems": 20, "cursor": null } });
    let audit = a.t.app().run_query("_system/audit:list", args, &admin_identity(&a.t).await).await;
    let actions: Vec<String> = audit.result.unwrap()["page"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap().to_string())
        .collect();
    for action in ["restore_stage", "restore_cancel", "restore_restart"] {
        assert!(actions.iter().any(|a| a == action), "{action} in {actions:?}");
    }
}

/// Without `--restart-exit-code`, restarting is refused.
#[tokio::test]
async fn restart_needs_an_exit_code() {
    use reqwest::Method;
    let a = admin_setup().await;
    let r = a.req(Method::POST, "/api/admin/restore/restart", Some(&a.admin)).send().await.unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(r.json::<Value>().await.unwrap()["errorData"]["code"], "restart_unavailable");
}
