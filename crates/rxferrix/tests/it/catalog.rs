//! Catalog reconciliation across restarts and schema changes.

use std::sync::{Arc, Mutex};

use rxferrix::db::Database;
use rxferrix::{App, AppOptions, FunctionRef, IndexDef, LogFilter, Table, TableDef};

use crate::common::items;

#[tokio::test]
async fn reconcile_and_backfill() {
    let dir = tempfile::tempdir().unwrap();
    let (table_number, ids) = {
        let app = App::open(dir.path(), AppOptions::default()).await.unwrap();
        for v in 0..20 {
            app.mutation::<items::insert>((v % 3, v, format!("n{v}"))).await.unwrap();
        }
        let docs = app.query::<items::newest>((100u32,)).await.unwrap();
        let t = app.db().catalog().table("items").unwrap();
        (t.number, docs.iter().map(|d| d.id).collect::<Vec<_>>())
    };

    // Restart: same numbering, data intact, ids still resolve.
    {
        let app = App::open(dir.path(), AppOptions::default()).await.unwrap();
        assert_eq!(app.db().catalog().table("items").unwrap().number, table_number);
        for id in &ids {
            assert!(app.query::<items::get_item>((*id,)).await.unwrap().is_some());
        }
        assert_eq!(app.db().write_log().latest(), 20, "ts persists");
        assert!(app.db().verify_indexes().await.unwrap().is_empty());
    }

    // Change the schema: `by_name` changes fields, `by_group_score` is removed, `by_score` is new.
    let items_v2 = TableDef {
        name: "items",
        indexes: vec![
            IndexDef { name: "by_name", fields: vec!["name", "group"] },
            IndexDef { name: "by_score", fields: vec!["score"] },
        ],
        search_indexes: vec![],
        vector_indexes: vec![],
        ts: || rxferrix::__private::ts_inline::<items::Item>(),
        validate: rxferrix::__private::validate_doc::<items::Item>,
        shape: rxferrix::shape::Shape::unknown,
        aggregates: vec![],
    };
    let db = Database::open(dir.path(), std::slice::from_ref(&items_v2), 2).unwrap();
    let t = db.catalog().table("items").unwrap();
    assert_eq!(t.number, table_number);
    assert!(t.index("by_group_score").is_none());
    assert_eq!(t.index("by_name").unwrap().fields, vec!["name", "group"]);
    assert_eq!(t.index("by_score").unwrap().fields, vec!["score"]);
    assert!(t.index("by_id").is_some() && t.index("by_creation_time").is_some());
    // Backfilled entries match a recompute, and the removed index left no orphans.
    assert_eq!(db.verify_indexes().await.unwrap(), Vec::<String>::new());
    // Tables missing from code (todos) are kept.
    assert!(db.catalog().table("todos").is_ok());
    drop(db);

    // Invalid definitions are rejected.
    let bad =
        TableDef { name: "items", indexes: vec![IndexDef { name: "by_id", fields: vec!["x"] }], ..items_v2.clone() };
    assert!(Database::open(dir.path(), &[bad], 1).is_err());
    let dup = TableDef { name: "items", indexes: vec![], ..items_v2.clone() };
    assert!(Database::open(dir.path(), &[items_v2, dup], 1).is_err());
}

/// Index keys too long for a page of the index B-tree are reported: on the first such write (then at most once an
/// hour per index), and when a backfill writes some.
#[tokio::test]
async fn long_index_keys_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    let long = "x".repeat(rxferrix::db::WARN_KEY_BYTES);
    {
        let app = App::open(dir.path(), AppOptions::default()).await.unwrap();
        for i in 0..3 {
            app.mutation::<items::insert>((i, i, format!("{long}{i}"))).await.unwrap();
        }
        app.mutation::<items::insert>((9, 9, "short".to_string())).await.unwrap();
        app.logs().flush().await.unwrap();
        let filter = LogFilter { path_prefix: Some(items::insert::path()), ..Default::default() };
        let entries = app.logs().query(filter).await.unwrap();
        let warned: Vec<bool> = entries
            .iter()
            .rev()
            .map(|e| e.log_lines.iter().any(|l| l.starts_with("[WARN] a ") && l.contains("`items.by_name`")))
            .collect();
        assert_eq!(warned, [true, false, false, false], "reported once, not again within the hour");
    }

    // Rebuilding `by_name` (its fields changed) writes the three long keys again.
    let items_v2 = TableDef {
        indexes: vec![IndexDef { name: "by_name", fields: vec!["name", "group"] }],
        ..items::Item::table_def()
    };
    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let out = Buf::default();
    let writer = out.clone();
    let subscriber = tracing_subscriber::fmt().with_writer(move || writer.clone()).with_ansi(false).finish();
    let db = tracing::subscriber::with_default(subscriber, || Database::open(dir.path(), &[items_v2], 1)).unwrap();
    drop(db);
    let printed = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
    assert!(printed.contains("the index `items.by_name` has 3 keys longer than 960 bytes"), "{printed}");
}
