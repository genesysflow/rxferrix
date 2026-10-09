//! `ferrix::Bytes` end to end: the `{"$bytes": "<base64>"}` form in storage, args and results, index ranges and
//! ordering (byte-wise, between strings and arrays), the dashboard's untyped paths, codegen, and the rebuild of every
//! index when a database was written with the previous key encoding.

use ferrix::testing::TestApp;
use ferrix::{Bytes, FunctionRef, Identity};
use serde_json::{Value, json};

pub mod blobs {
    use ferrix::prelude::*;

    #[table(name = "blobs")]
    #[index(by_data = [data])]
    #[index(by_tags = [tags])]
    pub struct Blob {
        pub name: String,
        pub data: Bytes,
        pub thumb: Option<Bytes>,
        pub tags: Vec<String>,
    }

    #[mutation]
    pub async fn add_blob(ctx: &MutationCtx, name: String, data: Bytes, tags: Vec<String>) -> Result<Id<Blob>> {
        ctx.db.insert(Blob { name, data, thumb: None, tags }).await
    }

    /// Names of the blobs whose data is in `[lo, hi)`, in data order.
    #[query]
    pub async fn in_range(ctx: &QueryCtx, lo: Bytes, hi: Bytes) -> Result<Vec<String>> {
        let docs = ctx.db.query::<Blob>().with_index(Blob::by_data, |q| q.gte(Blob::data, lo).lt(Blob::data, hi)).collect().await?;
        Ok(docs.into_iter().map(|d| d.value.name).collect())
    }

    /// Every blob's data, in index order.
    #[query]
    pub async fn ordered(ctx: &QueryCtx) -> Result<Vec<Bytes>> {
        let docs = ctx.db.query::<Blob>().with_index(Blob::by_data, |q| q).collect().await?;
        Ok(docs.into_iter().map(|d| d.value.data).collect())
    }

    #[query]
    pub async fn with_data(ctx: &QueryCtx, data: Bytes) -> Result<Option<Doc<Blob>>> {
        ctx.db.query::<Blob>().with_index(Blob::by_data, |q| q.eq(Blob::data, data)).unique().await
    }

    #[query]
    pub async fn with_tags(ctx: &QueryCtx, tags: Vec<String>) -> Result<Vec<String>> {
        let docs = ctx.db.query::<Blob>().with_index(Blob::by_tags, |q| q.eq(Blob::tags, tags)).collect().await?;
        Ok(docs.into_iter().map(|d| d.value.name).collect())
    }
}

use blobs::*;

fn b(v: &[u8]) -> Bytes {
    Bytes::from(v)
}

#[tokio::test]
async fn bytes_are_stored_tagged_indexed_and_ordered_bytewise() {
    let t = TestApp::new().await.unwrap();
    let values: [&[u8]; 6] = [&[0xFF], &[], &[0, 1], &[0], &[1, 2, 3], &[0, 0xFF, 0]];
    let mut ids = Vec::new();
    for (i, v) in values.iter().enumerate() {
        ids.push(t.mutation::<add_blob>((format!("b{i}"), b(v), vec![format!("t{}", i % 2)])).await.unwrap());
    }
    let ordered = t.query::<ordered>(()).await.unwrap();
    let mut expected: Vec<Vec<u8>> = values.iter().map(|v| v.to_vec()).collect();
    expected.sort();
    assert_eq!(ordered.into_iter().map(Bytes::into_vec).collect::<Vec<_>>(), expected);

    // Ranges and equality on a Bytes index.
    assert_eq!(t.query::<in_range>((b(&[0]), b(&[1]))).await.unwrap(), vec!["b3", "b2", "b5"]);
    assert_eq!(t.query::<in_range>((b(&[]), b(&[0]))).await.unwrap(), vec!["b1"]);
    let found = t.query::<with_data>((b(&[1, 2, 3]),)).await.unwrap().unwrap();
    assert_eq!((found.name.as_str(), found.id), ("b4", ids[4]));
    assert!(t.query::<with_data>((b(&[9]),)).await.unwrap().is_none());
    // Arrays still index (their tag moved after Bytes).
    let mut names = t.query::<with_tags>((vec!["t1".to_string()],)).await.unwrap();
    names.sort();
    assert_eq!(names, vec!["b1", "b3", "b5"]);
    assert!(t.app().db().verify_indexes().await.unwrap().is_empty());

    // On the wire (args and results) and in storage, Bytes is `{"$bytes": "<base64>"}`.
    let app = t.app();
    let out = app.run_query(&ordered::path(), json!({}), &Identity::default()).await.result.unwrap();
    assert_eq!(out[0], json!({ "$bytes": "" }));
    assert_eq!(out[5], json!({ "$bytes": "/w==" }));
    let out = app
        .run_query(&in_range::path(), json!({ "lo": { "$bytes": "AA==" }, "hi": { "$bytes": "AAE=" } }), &Identity::default())
        .await
        .result
        .unwrap();
    assert_eq!(out, json!(["b3"]));
    let err = app.run_query(&in_range::path(), json!({ "lo": "AA==", "hi": { "$bytes": "AAE=" } }), &Identity::default()).await.result.unwrap_err();
    assert!(format!("{err:#}").contains("Invalid args for in_range"), "{err:#}");

    // The dashboard's untyped paths show (and accept) the tagged JSON.
    let admin = Identity { admin_id: Some(ferrix::id::DocId::new_random(1).encode()), ..Default::default() };
    let doc = app.run_query("_system/dashboard:document", json!({ "id": ids[4].to_string() }), &admin).await.result.unwrap();
    assert_eq!(doc["document"]["data"], json!({ "$bytes": "AQID" }));
    assert_eq!(doc["document"]["thumb"], Value::Null);
    app.run_mutation(
        "_system/dashboard:patch_document",
        json!({ "id": ids[4].to_string(), "patch": { "thumb": { "$bytes": "qrs=" } } }),
        &admin,
    )
    .await
    .unwrap();
    let doc = t.query::<with_data>((b(&[1, 2, 3]),)).await.unwrap().unwrap();
    assert_eq!(doc.thumb, Some(b(&[0xAA, 0xBB])));
    let err = app
        .run_mutation("_system/dashboard:patch_document", json!({ "id": ids[4].to_string(), "patch": { "data": "AQID" } }), &admin)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("does not match table `blobs`"), "{err:#}");
    let page = app
        .run_query(
            "_system/dashboard:documents",
            json!({ "table": "blobs", "index": "by_data", "filters": [{ "field": "data", "op": "gte", "value": { "$bytes": "AQ==" } }], "paginationOpts": { "numItems": 10, "cursor": null } }),
            &admin,
        )
        .await
        .result
        .unwrap();
    let names: Vec<&str> = page["page"].as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["b4", "b0"]);
}

#[test]
fn codegen_types_bytes_as_tagged_objects() {
    let files = ferrix::codegen::generate(ferrix::registry().unwrap(), &ferrix::runtime::registered_tables()).unwrap();
    assert!(
        files.data_model_ts.contains(
            "  \"blobs\": { name: string, data: { $bytes: string }, thumb: { $bytes: string } | null, tags: Array<string>, };\n"
        ),
        "{}",
        files.data_model_ts
    );
}

#[tokio::test]
async fn indexes_are_rebuilt_when_the_key_encoding_changed() {
    let t = TestApp::new().await.unwrap();
    for (i, tags) in [vec!["x"], vec!["a", "b"], vec![]].into_iter().enumerate() {
        let tags = tags.into_iter().map(String::from).collect();
        t.mutation::<add_blob>((format!("b{i}"), b(&[i as u8]), tags)).await.unwrap();
    }
    let conn = rusqlite::Connection::open(t.dir().join("data.db")).unwrap();
    let version: i64 = conn.query_row("SELECT v FROM _meta WHERE k = 'key_encoding'", [], |r| r.get(0)).unwrap();
    assert_eq!(version as u64, ferrix::keys::KEY_ENCODING_VERSION);
    // A database from before the version was recorded (v1): its index entries used other tags. Simulate it by
    // dropping the version and the entries.
    conn.execute("DELETE FROM _meta WHERE k = 'key_encoding'", []).unwrap();
    conn.execute("DELETE FROM index_entries", []).unwrap();
    drop(conn);

    let t = t.restart().await.unwrap();
    assert!(t.app().db().verify_indexes().await.unwrap().is_empty(), "every index was rebuilt");
    assert_eq!(t.query::<with_tags>((vec!["a".to_string(), "b".to_string()],)).await.unwrap(), vec!["b1"]);
    assert_eq!(t.query::<in_range>((b(&[1]), b(&[3]))).await.unwrap(), vec!["b1", "b2"]);
    let conn = rusqlite::Connection::open(t.dir().join("data.db")).unwrap();
    let version: i64 = conn.query_row("SELECT v FROM _meta WHERE k = 'key_encoding'", [], |r| r.get(0)).unwrap();
    assert_eq!(version as u64, ferrix::keys::KEY_ENCODING_VERSION);
}
