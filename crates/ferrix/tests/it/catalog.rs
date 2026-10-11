//! Catalog reconciliation across restarts and schema changes.

use ferrix::db::Database;
use ferrix::{App, AppOptions, IndexDef, TableDef};

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
        ts: || ferrix::__private::ts_inline::<items::Item>(),
        validate: ferrix::__private::validate_doc::<items::Item>,
        shape: ferrix::shape::Shape::unknown,
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
    let bad = TableDef { name: "items", indexes: vec![IndexDef { name: "by_id", fields: vec!["x"] }], ..items_v2.clone() };
    assert!(Database::open(dir.path(), &[bad], 1).is_err());
    let dup = TableDef { name: "items", indexes: vec![], ..items_v2.clone() };
    assert!(Database::open(dir.path(), &[items_v2, dup], 1).is_err());
}
