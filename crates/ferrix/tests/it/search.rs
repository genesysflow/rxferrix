//! Full-text search (FTS5) and vector search (sqlite-vec): query semantics, escaping, transactional maintenance,
//! catalog changes across restarts, reactivity, and vector search against brute force.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ferrix::db::Database;
use ferrix::db::readset::ReadSet;
use ferrix::testing::TestApp;
use ferrix::writelog::CommitEntry;
use ferrix::{App, AppOptions, FunctionRef, Identity, SearchIndexDef, Table, TableDef, VectorIndexDef};
use futures::{SinkExt, StreamExt};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMessage;

pub mod msgs {
    use ferrix::prelude::*;

    #[table(name = "search_messages")]
    #[index(by_channel = [channel])]
    #[search_index(search_body, search = body, filter = [channel, pinned])]
    pub struct Message {
        pub channel: String,
        pub body: Option<String>,
        pub pinned: bool,
        pub note: Option<String>,
    }

    /// Search `text`, optionally in one channel / only pinned ones, optionally keeping only bodies longer than
    /// `min_len` (a post-filter).
    #[query]
    pub async fn search(
        ctx: &QueryCtx,
        text: String,
        channel: Option<String>,
        pinned: Option<bool>,
        limit: Option<u32>,
        min_len: Option<u32>,
    ) -> Result<Vec<Doc<Message>>> {
        let mut q = ctx.db.query::<Message>().with_search_index(Message::search_body, |q| {
            let mut q = q.search(Message::body, text);
            if let Some(c) = channel {
                q = q.eq(Message::channel, c);
            }
            if let Some(p) = pinned {
                q = q.eq(Message::pinned, p);
            }
            q
        });
        if let Some(n) = min_len {
            q = q.filter(move |d| d.body.as_deref().unwrap_or("").len() > n as usize);
        }
        match limit {
            Some(n) => q.take(n as usize).await,
            None => q.collect().await,
        }
    }

    /// Bodies of `search` results.
    #[query]
    pub async fn bodies(ctx: &QueryCtx, text: String, channel: Option<String>) -> Result<Vec<String>> {
        let docs = search(ctx, text, channel, None, None, None).await?;
        Ok(docs.into_iter().map(|d| d.value.body.unwrap_or_default()).collect())
    }

    #[query]
    pub async fn bad_field(ctx: &QueryCtx) -> Result<Vec<Doc<Message>>> {
        ctx.db.query::<Message>().with_search_index(Message::search_body, |q| q.search(Message::channel, "x")).collect().await
    }

    #[query]
    pub async fn bad_filter(ctx: &QueryCtx) -> Result<Vec<Doc<Message>>> {
        ctx.db
            .query::<Message>()
            .with_search_index(Message::search_body, |q| q.search(Message::body, "x").eq(Message::note, Some("n".into())))
            .collect()
            .await
    }

    #[query]
    pub async fn no_search(ctx: &QueryCtx) -> Result<Vec<Doc<Message>>> {
        ctx.db.query::<Message>().with_search_index(Message::search_body, |q| q.eq(Message::pinned, true)).collect().await
    }

    #[query]
    pub async fn after_order(ctx: &QueryCtx) -> Result<Vec<Doc<Message>>> {
        ctx.db
            .query::<Message>()
            .order(Order::Desc)
            .with_search_index(Message::search_body, |q| q.search(Message::body, "x"))
            .collect()
            .await
    }

    #[mutation]
    pub async fn add(ctx: &MutationCtx, channel: String, body: Option<String>) -> Result<Id<Message>> {
        ctx.db.insert(Message { channel, body, pinned: false, note: None }).await
    }

    #[mutation]
    pub async fn add_many(ctx: &MutationCtx, channel: String, bodies: Vec<String>) -> Result<()> {
        for body in bodies {
            ctx.db.insert(Message { channel: channel.clone(), body: Some(body), pinned: false, note: None }).await?;
        }
        Ok(())
    }

    /// Inserts, then fails: everything is rolled back.
    #[mutation]
    pub async fn add_then_fail(ctx: &MutationCtx, channel: String, body: String) -> Result<()> {
        ctx.db.insert(Message { channel, body: Some(body), pinned: false, note: None }).await?;
        Err(UserError::new("nope", "rolled back").into())
    }

    #[mutation]
    pub async fn set_body(ctx: &MutationCtx, id: Id<Message>, body: Option<String>) -> Result<()> {
        ctx.db.patch(id, json!({ "body": body })).await
    }

    #[mutation]
    pub async fn set_note(ctx: &MutationCtx, id: Id<Message>, note: String) -> Result<()> {
        ctx.db.patch(id, json!({ "note": note })).await
    }

    #[mutation]
    pub async fn pin(ctx: &MutationCtx, id: Id<Message>) -> Result<()> {
        ctx.db.patch(id, json!({ "pinned": true })).await
    }

    #[mutation]
    pub async fn replace(ctx: &MutationCtx, id: Id<Message>, channel: String, body: String) -> Result<()> {
        ctx.db.replace(id, Message { channel, body: Some(body), pinned: false, note: None }).await
    }

    #[mutation]
    pub async fn remove(ctx: &MutationCtx, id: Id<Message>) -> Result<()> {
        ctx.db.delete(id).await
    }

    /// The untyped (dashboard) write path.
    #[mutation]
    pub async fn raw_ops(ctx: &MutationCtx, body: String) -> Result<String> {
        let id = ctx.db.insert_raw("search_messages", json!({ "channel": "raw", "body": body, "pinned": false, "note": null })).await?;
        ctx.db.patch_raw(&id, json!({ "body": format!("{body} patched") })).await?;
        let other = ctx.db.insert_raw("search_messages", json!({ "channel": "raw", "body": "temporary raw", "pinned": false, "note": null })).await?;
        ctx.db.replace_raw(&other, json!({ "channel": "raw", "body": "replaced raw", "pinned": true, "note": null })).await?;
        ctx.db.delete_raw(&other).await?;
        Ok(id)
    }
}

pub mod vecs {
    use ferrix::prelude::*;

    pub const DIMS: usize = 8;

    #[table(name = "search_vectors")]
    #[vector_index(by_embedding, field = embedding, dims = 8, filter = [category, rank])]
    pub struct Item {
        pub category: String,
        pub rank: i64,
        pub embedding: Option<Vec<f32>>,
    }

    #[mutation]
    pub async fn add(ctx: &MutationCtx, category: String, rank: i64, embedding: Option<Vec<f32>>) -> Result<Id<Item>> {
        ctx.db.insert(Item { category, rank, embedding }).await
    }

    #[mutation]
    pub async fn add_many(ctx: &MutationCtx, items: Vec<(String, i64, Vec<f32>)>) -> Result<Vec<Id<Item>>> {
        let mut ids = Vec::new();
        for (category, rank, embedding) in items {
            ids.push(ctx.db.insert(Item { category, rank, embedding: Some(embedding) }).await?);
        }
        Ok(ids)
    }

    /// A wrong-dimension insert fails before anything is written; the mutation catches the error and goes on.
    #[mutation]
    pub async fn add_bad_then_good(ctx: &MutationCtx, embedding: Vec<f32>) -> Result<String> {
        let err = ctx.db.insert(Item { category: "bad".into(), rank: 0, embedding: Some(vec![1.0; 3]) }).await.unwrap_err();
        ctx.db.insert(Item { category: "good".into(), rank: 0, embedding: Some(embedding) }).await?;
        Ok(err.to_string())
    }

    #[mutation]
    pub async fn add_then_fail(ctx: &MutationCtx, embedding: Vec<f32>) -> Result<()> {
        ctx.db.insert(Item { category: "rolled_back".into(), rank: 0, embedding: Some(embedding) }).await?;
        Err(UserError::new("nope", "rolled back").into())
    }

    #[mutation]
    pub async fn set_embedding(ctx: &MutationCtx, id: Id<Item>, embedding: Option<Vec<f32>>) -> Result<()> {
        ctx.db.patch(id, json!({ "embedding": embedding })).await
    }

    #[mutation]
    pub async fn set_category(ctx: &MutationCtx, id: Id<Item>, category: String) -> Result<()> {
        ctx.db.patch(id, json!({ "category": category })).await
    }

    #[mutation]
    pub async fn remove(ctx: &MutationCtx, id: Id<Item>) -> Result<()> {
        ctx.db.delete(id).await
    }

    /// A search result (tuples can't be function outputs: ts-rs can't inline them for codegen).
    #[derive(serde::Serialize, serde::Deserialize, ts_rs::TS, Clone, Copy, Debug)]
    pub struct Hit {
        pub id: Id<Item>,
        pub score: f32,
    }

    #[action]
    pub async fn nearest(
        ctx: &ActionCtx,
        vector: Vec<f32>,
        limit: u32,
        categories: Option<Vec<String>>,
        rank: Option<i64>,
    ) -> Result<Vec<Hit>> {
        let hits = nearest_tuples(ctx, vector, limit, categories, rank).await?;
        Ok(hits.into_iter().map(|(id, score)| Hit { id, score }).collect())
    }

    pub async fn nearest_tuples(
        ctx: &ActionCtx,
        vector: Vec<f32>,
        limit: u32,
        categories: Option<Vec<String>>,
        rank: Option<i64>,
    ) -> Result<Vec<(Id<Item>, f32)>> {
        let mut opts = VectorSearchOpts::new(limit as usize);
        let mut filter = categories.map(|cs| VectorFilter::any_of(Item::category, cs));
        if let Some(r) = rank {
            let by_rank = VectorFilter::eq(Item::rank, r);
            filter = Some(match filter {
                Some(f) => f.and(by_rank),
                None => by_rank,
            });
        }
        if let Some(f) = filter {
            opts = opts.filter(f);
        }
        ctx.vector_search(Item::by_embedding, &vector, opts).await
    }

    /// Filtering on a field that is not a filter field of the index.
    #[action]
    pub async fn bad_filter(ctx: &ActionCtx) -> Result<usize> {
        let opts = VectorSearchOpts::new(3).filter(VectorFilter::eq(Item::embedding, None));
        Ok(ctx.vector_search(Item::by_embedding, [0.0f32; DIMS], opts).await?.len())
    }
}

use msgs::Message;
use vecs::Item;

async fn bodies(t: &TestApp, text: &str) -> Vec<String> {
    t.query::<msgs::bodies>((text.to_string(), Some("c".to_string()))).await.unwrap()
}

/// Every test uses its own channel / categories, since all tests in this binary share the table definitions (but
/// each `TestApp` has its own database).
#[tokio::test]
async fn ranking_prefix_filters_and_empty_text() {
    let t = TestApp::new().await.unwrap();
    let bodies_in = |c: &str, v: &[&str]| (c.to_string(), v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    t.mutation::<msgs::add_many>(bodies_in(
        "c",
        &[
            "the quick brown fox",
            "hello world",
            "hello hello hello wonderful world",
            "world of warcraft",
            "Say hello to the world",
            "unrelated text",
        ],
    ))
    .await
    .unwrap();
    t.mutation::<msgs::add_many>(bodies_in("other", &["hello world from another channel"])).await.unwrap();

    // AND of terms, last term as a prefix; ranked by bm25 (more occurrences in a short body rank higher).
    let got = bodies(&t, "hello wor").await;
    assert_eq!(got.len(), 3, "{got:?}");
    assert!(got.contains(&"hello world".to_string()) && got.contains(&"Say hello to the world".to_string()));
    assert!(got.contains(&"hello hello hello wonderful world".to_string()));
    // More occurrences rank higher (bm25 term frequency).
    assert_eq!(bodies(&t, "hello").await[0], "hello hello hello wonderful world");
    // Only the last term is a prefix.
    assert_eq!(bodies(&t, "hel world").await, Vec::<String>::new());
    assert_eq!(bodies(&t, "warc").await, vec!["world of warcraft"]);
    // Case-insensitive.
    assert_eq!(bodies(&t, "QUICK Fox").await, vec!["the quick brown fox"]);
    // Shorter documents rank higher for the same single occurrence.
    let got = bodies(&t, "world").await;
    assert_eq!(got.len(), 4);
    assert_eq!(got[0], "hello world");

    // Filters: channel (eq on a filter field), across channels, pinned.
    let all: Vec<Value> = t.app().run_query(&msgs::search::path(), json!({ "text": "hello" }), &Identity::default()).await.result.map(|v| v.as_array().unwrap().clone()).unwrap();
    assert_eq!(all.len(), 4);
    let other = t.query::<msgs::bodies>(("hello".to_string(), Some("other".to_string()))).await.unwrap();
    assert_eq!(other, vec!["hello world from another channel"]);
    let docs = t.query::<msgs::search>(("hello".to_string(), None, None, None, None)).await.unwrap();
    t.mutation::<msgs::pin>((docs[1].id,)).await.unwrap();
    let pinned = t.query::<msgs::search>(("hello".to_string(), None, Some(true), None, None)).await.unwrap();
    assert_eq!(pinned.iter().map(|d| d.id).collect::<Vec<_>>(), vec![docs[1].id]);
    let unpinned = t.query::<msgs::search>(("hello".to_string(), None, Some(false), None, None)).await.unwrap();
    assert_eq!(unpinned.len(), 3);

    // take(n) keeps the most relevant; a post-filter applies after ranking.
    let top = t.query::<msgs::search>(("world".to_string(), Some("c".to_string()), None, Some(2), None)).await.unwrap();
    assert_eq!(top.len(), 2);
    assert_eq!(top[0].body.as_deref(), Some("hello world"));
    let long = t.query::<msgs::search>(("world".to_string(), Some("c".to_string()), None, Some(1), Some(15))).await.unwrap();
    assert_eq!(long.len(), 1);
    assert!(long[0].body.as_deref().unwrap().len() > 15);

    // Empty text (or no terms at all) matches nothing.
    for empty in ["", "   ", "!!! ?? ...", "\"\"", "*"] {
        assert_eq!(bodies(&t, empty).await, Vec::<String>::new(), "{empty:?}");
    }

    // Misuse is reported.
    let err = |r: ferrix::Result<Vec<ferrix::Doc<Message>>>| format!("{:#}", r.unwrap_err());
    assert!(err(t.query::<msgs::bad_field>(()).await).contains("is not its search field"));
    assert!(err(t.query::<msgs::bad_filter>(()).await).contains("is not one of its filter fields"));
    assert!(err(t.query::<msgs::no_search>(()).await).contains("exactly once"));
    assert!(err(t.query::<msgs::after_order>(()).await).contains("cannot be combined"));
}

#[tokio::test]
async fn hostile_input_is_never_query_syntax() {
    let t = TestApp::new().await.unwrap();
    t.mutation::<msgs::add_many>((
        "c".to_string(),
        vec![
            "alpha beta".into(),
            "NEAR neighbours".into(),
            "say \"quoted\" words".into(),
            "t: column".into(),
            "a OR b".into(),
            "not this".into(),
            "star*power".into(),
        ],
    ))
    .await
    .unwrap();
    // None of these may error; they are matched as plain words.
    let hostile = [
        "\"", "\"\"\"", "alpha\"", "alpha\" OR \"beta", "*", "alpha*", "^alpha", "NEAR(alpha beta)", "NEAR", "near",
        "t:column", "t : column", "{t}: alpha", "-alpha", "(alpha", "alpha)", "((((", "))", "alpha AND", "OR", "NOT",
        "AND OR NOT", "+", ":", "alpha + beta", "rank", "rowid", "x'00'", "'; DROP TABLE documents; --", "\0", "\\",
        "a\u{0}b", "%", "_",
    ];
    for text in hostile {
        let r = t.query::<msgs::bodies>((text.to_string(), Some("c".to_string()))).await;
        assert!(r.is_ok(), "{text:?} failed: {:#}", r.unwrap_err());
    }
    // Operators are just words: "OR" finds the document containing the word "or", "NEAR" the one with "near".
    assert_eq!(bodies(&t, "a OR b").await, vec!["a OR b"]);
    assert_eq!(bodies(&t, "NEAR").await, vec!["NEAR neighbours"]);
    assert_eq!(bodies(&t, "NEAR(alpha beta)").await, Vec::<String>::new(), "needs all three words");
    assert_eq!(bodies(&t, "alpha\" OR \"beta").await, Vec::<String>::new(), "alpha AND or AND beta*");
    assert_eq!(bodies(&t, "\"quoted\"").await, vec!["say \"quoted\" words"]);
    assert_eq!(bodies(&t, "t:column").await, vec!["t: column"]);
    assert_eq!(bodies(&t, "star*").await, vec!["star*power"]);
    assert_eq!(bodies(&t, "not").await, vec!["not this"]);
    assert_eq!(bodies(&t, "-alpha").await, vec!["alpha beta"]);
    assert!(t.app().db().verify_indexes().await.unwrap().is_empty());
}

#[tokio::test]
async fn unicode() {
    let t = TestApp::new().await.unwrap();
    t.mutation::<msgs::add_many>((
        "c".to_string(),
        vec!["Café crème".into(), "naïve résumé".into(), "Ünïcödé ÄÖÜ".into(), "Привет мир".into(), "東京タワー".into(), "emoji 🎉 party".into()],
    ))
    .await
    .unwrap();
    // Diacritics folded and case-insensitive, both ways.
    assert_eq!(bodies(&t, "cafe").await, vec!["Café crème"]);
    assert_eq!(bodies(&t, "CAFÉ CREME").await, vec!["Café crème"]);
    assert_eq!(bodies(&t, "resume").await, vec!["naïve résumé"]);
    assert_eq!(bodies(&t, "unicode aou").await, vec!["Ünïcödé ÄÖÜ"]);
    assert_eq!(bodies(&t, "привет").await, vec!["Привет мир"]);
    assert_eq!(bodies(&t, "ми").await, vec!["Привет мир"]);
    // A run of CJK characters is one token: its prefixes match.
    assert_eq!(bodies(&t, "東京").await, vec!["東京タワー"]);
    assert_eq!(bodies(&t, "🎉 party").await, vec!["emoji 🎉 party"]);
}

#[tokio::test]
async fn result_cap() {
    let t = TestApp::new().await.unwrap();
    for chunk in 0..3 {
        let bodies: Vec<String> = (0..500).map(|i| format!("common word {chunk} {i}")).collect();
        t.mutation::<msgs::add_many>(("c".to_string(), bodies)).await.unwrap();
    }
    let all = t.query::<msgs::search>(("common".to_string(), None, None, None, None)).await.unwrap();
    assert_eq!(all.len(), ferrix::db::search::MAX_SEARCH_RESULTS);
    let many = t.query::<msgs::search>(("common".to_string(), None, None, Some(5000), None)).await.unwrap();
    assert_eq!(many.len(), ferrix::db::search::MAX_SEARCH_RESULTS, "take(n) is clamped");
    let few = t.query::<msgs::search>(("common".to_string(), None, None, Some(7), None)).await.unwrap();
    assert_eq!(few.len(), 7);
    // Deterministic order (equal ranks: insertion order).
    let again = t.query::<msgs::search>(("common".to_string(), None, None, Some(7), None)).await.unwrap();
    assert_eq!(few.iter().map(|d| d.id).collect::<Vec<_>>(), again.iter().map(|d| d.id).collect::<Vec<_>>());
}

#[tokio::test]
async fn writes_keep_fts_in_sync() {
    let t = TestApp::new().await.unwrap();
    let a = t.mutation::<msgs::add>(("c".to_string(), Some("first message".to_string()))).await.unwrap();
    let b = t.mutation::<msgs::add>(("c".to_string(), Some("second message".to_string()))).await.unwrap();
    let none = t.mutation::<msgs::add>(("c".to_string(), None)).await.unwrap();
    let verify = || async { assert_eq!(t.app().db().verify_indexes().await.unwrap(), Vec::<String>::new()) };
    verify().await;

    // A rolled-back mutation leaves FTS untouched.
    let err = t.mutation::<msgs::add_then_fail>(("c".to_string(), "ghost message".to_string())).await.unwrap_err();
    assert!(err.to_string().contains("rolled back"));
    assert_eq!(bodies(&t, "ghost").await, Vec::<String>::new());
    verify().await;

    // Patch (text), replace (text + channel), patch to null, patch from null, delete.
    t.mutation::<msgs::set_body>((a, Some("first edited".to_string()))).await.unwrap();
    assert_eq!(bodies(&t, "message").await, vec!["second message"]);
    assert_eq!(bodies(&t, "edited").await, vec!["first edited"]);
    t.mutation::<msgs::replace>((b, "elsewhere".to_string(), "second moved".to_string())).await.unwrap();
    assert_eq!(bodies(&t, "second").await, Vec::<String>::new());
    assert_eq!(t.query::<msgs::bodies>(("second".to_string(), Some("elsewhere".to_string()))).await.unwrap(), vec!["second moved"]);
    t.mutation::<msgs::set_body>((a, None)).await.unwrap();
    assert_eq!(bodies(&t, "first").await, Vec::<String>::new());
    t.mutation::<msgs::set_body>((none, Some("late text".to_string()))).await.unwrap();
    assert_eq!(bodies(&t, "late").await, vec!["late text"]);
    t.mutation::<msgs::set_note>((none, "a note".to_string())).await.unwrap();
    verify().await;
    t.mutation::<msgs::remove>((none,)).await.unwrap();
    assert_eq!(bodies(&t, "late").await, Vec::<String>::new());
    verify().await;

    // The untyped write path maintains the index too.
    t.mutation::<msgs::raw_ops>(("raw body".to_string(),)).await.unwrap();
    let raw = t.query::<msgs::bodies>(("raw".to_string(), Some("raw".to_string()))).await.unwrap();
    assert_eq!(raw, vec!["raw body patched"]);
    verify().await;

    // The checker notices tampering: a missing FTS row, and a leftover storage table.
    let n = t.app().db().catalog().table("search_messages").unwrap().search_indexes[0].id - ferrix::db::search::SPECIAL_INDEX_BASE;
    let conn = rusqlite::Connection::open(t.dir().join("data.db")).unwrap();
    conn.execute(&format!("DELETE FROM _search_{n} WHERE rowid = (SELECT MIN(rowid) FROM _search_{n})"), []).unwrap();
    conn.execute_batch("CREATE VIRTUAL TABLE _search_999 USING fts5(t)").unwrap();
    let problems = t.app().db().verify_indexes().await.unwrap();
    assert_eq!(problems.len(), 3, "{problems:?}");
    assert!(problems[0].contains("search index `search_body`") && problems[0].contains("1 differ"), "{problems:?}");
    assert!(problems[1].contains("rowid mappings"), "{problems:?}");
    assert!(problems[2].contains("`_search_999` belongs to no search or vector index"), "{problems:?}");
}

fn commit_at(t: &TestApp, ts: u64) -> Arc<CommitEntry> {
    let entries = t.app().db().write_log().entries_between(ts - 1, ts).unwrap();
    assert_eq!(entries.len(), 1);
    entries[0].clone()
}

async fn read_set(t: &TestApp, args: Value) -> ReadSet {
    let run = t.app().run_query(&msgs::search::path(), args, &Identity::default()).await;
    run.result.unwrap();
    run.read_set
}

#[tokio::test]
async fn search_read_sets() {
    let t = TestApp::new().await.unwrap();
    let a = t.mutation::<msgs::add>(("c".to_string(), Some("apple pie".to_string()))).await.unwrap();
    let b = t.mutation::<msgs::add>(("c".to_string(), Some("banana bread".to_string()))).await.unwrap();
    let rs = read_set(&t, json!({ "text": "apple" })).await;
    let search_id = t.app().db().catalog().table("search_messages").unwrap().search_indexes[0].id;
    assert!(search_id >= ferrix::db::search::SPECIAL_INDEX_BASE);
    assert!(rs.ranges().any(|(id, r)| id == search_id && r.start.is_empty() && r.end.is_none()), "{rs:?}");

    // A matching insert invalidates the query...
    let (_, ts) = t.mutation_ts::<msgs::add>(("c".to_string(), Some("apple tart".to_string()))).await.unwrap();
    assert!(rs.overlaps(&commit_at(&t, ts)));
    // ...as does any change of the search field (even of a non-matching document: coarse, but bm25 depends on it),
    let (_, ts) = t.mutation_ts::<msgs::set_body>((b, Some("banana split".to_string()))).await.unwrap();
    assert!(rs.overlaps(&commit_at(&t, ts)));
    // or of a filter field.
    let (_, ts) = t.mutation_ts::<msgs::pin>((b,)).await.unwrap();
    assert!(rs.overlaps(&commit_at(&t, ts)));
    // A change to another field of a returned document invalidates it (the result contains the document)...
    let (_, ts) = t.mutation_ts::<msgs::set_note>((a, "n".to_string())).await.unwrap();
    assert!(rs.overlaps(&commit_at(&t, ts)));
    // ...but not one of a document the query never examined.
    let (_, ts) = t.mutation_ts::<msgs::set_note>((b, "n".to_string())).await.unwrap();
    assert!(!rs.overlaps(&commit_at(&t, ts)));
    // A document without text is not indexed: inserting it changes nothing a search can see.
    let (_, ts) = t.mutation_ts::<msgs::add>(("c".to_string(), None)).await.unwrap();
    assert!(!rs.overlaps(&commit_at(&t, ts)));
    // Deletes of indexed documents count.
    let (_, ts) = t.mutation_ts::<msgs::remove>((b,)).await.unwrap();
    assert!(rs.overlaps(&commit_at(&t, ts)));

    // Empty text reads nothing.
    let rs = read_set(&t, json!({ "text": "  " })).await;
    assert!(rs.is_empty());
}

/// Minimal sync-protocol client: subscribe to one query and wait for its updates.
struct Ws(tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>);

impl Ws {
    async fn connect(addr: SocketAddr) -> Ws {
        let (stream, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/sync")).await.unwrap();
        let mut ws = Ws(stream);
        ws.send(json!({ "type": "Connect", "sessionId": "search-test", "connectionCount": 0, "lastCloseReason": null, "clientVersion": "0.1.0" })).await;
        ws
    }

    async fn send(&mut self, v: Value) {
        self.0.send(WsMessage::text(v.to_string())).await.unwrap();
    }

    /// The next query value pushed for query 0.
    async fn next_value(&mut self) -> Value {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), self.0.next()).await.expect("no update").unwrap().unwrap();
            let WsMessage::Text(text) = msg else { continue };
            let v: Value = serde_json::from_str(text.as_str()).unwrap();
            if v["type"] != "Transition" {
                continue;
            }
            if let Some(m) = v["modifications"].as_array().unwrap().iter().find(|m| m["queryId"] == 0) {
                assert_eq!(m["type"], "QueryUpdated", "{m}");
                return m["value"].clone();
            }
        }
    }
}

fn texts(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn subscriptions_follow_search_results() {
    let t = TestApp::new().await.unwrap();
    let addr = t.serve().await.unwrap();
    let id = t.mutation::<msgs::add>(("c".to_string(), Some("first note".to_string()))).await.unwrap();
    let mut ws = Ws::connect(addr).await;
    ws.send(json!({ "type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 1,
        "modifications": [{ "type": "Add", "queryId": 0, "udfPath": msgs::bodies::path(), "args": { "text": "hello wor", "channel": "c" } }] }))
    .await;
    assert_eq!(texts(&ws.next_value().await), Vec::<String>::new());

    // A matching insert is pushed.
    t.mutation::<msgs::add>(("c".to_string(), Some("hello world".to_string()))).await.unwrap();
    assert_eq!(texts(&ws.next_value().await), vec!["hello world"]);
    // Editing the search field of another document so that it matches is pushed too.
    t.mutation::<msgs::set_body>((id, Some("hello wonderful world".to_string()))).await.unwrap();
    let v = texts(&ws.next_value().await);
    assert_eq!(v.len(), 2);
    assert!(v.contains(&"hello wonderful world".to_string()));
    // And so is it no longer matching.
    t.mutation::<msgs::set_body>((id, Some("goodbye".to_string()))).await.unwrap();
    assert_eq!(texts(&ws.next_value().await), vec!["hello world"]);
}

#[tokio::test]
async fn dashboard_lists_search_and_vector_indexes() {
    let t = TestApp::new().await.unwrap();
    let admin = t.app().auth().create_admin(t.app(), "admin@example.com", "admin password".into()).await.unwrap();
    let run = t.app().run_query("_system/dashboard:tables", json!({}), &Identity::admin(admin, None)).await;
    let tables = run.result.unwrap();
    let find = |name: &str| tables.as_array().unwrap().iter().find(|t| t["name"] == name).unwrap()["searchIndexes"].clone();
    assert_eq!(
        find("search_messages"),
        json!([{ "name": "search_body", "kind": "search", "field": "body", "filterFields": ["channel", "pinned"], "dimensions": null }])
    );
    assert_eq!(
        find("search_vectors"),
        json!([{ "name": "by_embedding", "kind": "vector", "field": "embedding", "filterFields": ["category", "rank"], "dimensions": 8 }])
    );
    assert_eq!(find("todos"), json!([]));
}

// ------------------------------------------------------------------------------------------------ catalog

fn messages_def(search_indexes: Vec<SearchIndexDef>) -> TableDef {
    TableDef { search_indexes, ..Message::table_def() }
}

#[tokio::test]
async fn catalog_add_change_remove_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    {
        let app = App::open(dir.path(), AppOptions::default()).await.unwrap();
        app.mutation::<msgs::add_many>(("c".to_string(), vec!["one apple".into(), "two apples".into(), "three pears".into()]))
            .await
            .unwrap();
        app.mutation::<msgs::add>(("d".to_string(), None)).await.unwrap();
        assert!(app.db().verify_indexes().await.unwrap().is_empty());
    }
    let original = Message::table_def();
    let original_id = |db: &Database| db.catalog().table("search_messages").unwrap().search_indexes[0].id;

    // Restart with the same definition: nothing is rebuilt (same id), data intact.
    let db = Database::open(dir.path(), std::slice::from_ref(&original), 1).unwrap();
    let id0 = original_id(&db);
    assert!(db.verify_indexes().await.unwrap().is_empty());
    drop(db);

    // Change the filter fields (rebuild in place, same id), and add a second index on `note` (backfilled).
    let changed = messages_def(vec![
        SearchIndexDef { name: "search_body", search_field: "body", filter_fields: vec!["channel"] },
        SearchIndexDef { name: "search_note", search_field: "note", filter_fields: vec![] },
    ]);
    let db = Database::open(dir.path(), std::slice::from_ref(&changed), 1).unwrap();
    let t = db.catalog().table("search_messages").unwrap().clone();
    assert_eq!(t.search_indexes.len(), 2);
    assert_eq!(t.search_indexes[0].id, id0);
    assert_eq!(t.search_indexes[0].filter_fields(), ["channel".to_string()]);
    assert!(t.search_indexes[1].id > id0);
    assert_eq!(db.verify_indexes().await.unwrap(), Vec::<String>::new());
    drop(db);

    // Remove both: storage is dropped (verify checks for orphaned FTS tables).
    let removed = messages_def(vec![]);
    let db = Database::open(dir.path(), std::slice::from_ref(&removed), 1).unwrap();
    assert!(db.catalog().table("search_messages").unwrap().search_indexes.is_empty());
    assert_eq!(db.verify_indexes().await.unwrap(), Vec::<String>::new());
    drop(db);

    // Back to the code's definition: recreated and backfilled; searches work.
    let app = App::open(dir.path(), AppOptions::default()).await.unwrap();
    assert_eq!(app.db().verify_indexes().await.unwrap(), Vec::<String>::new());
    let found = app.query::<msgs::bodies>(("apple".to_string(), Some("c".to_string()))).await.unwrap();
    assert_eq!(found.len(), 2, "{found:?}");

    // Invalid definitions are refused.
    drop(app);
    let dup = messages_def(vec![
        SearchIndexDef { name: "by_channel", search_field: "body", filter_fields: vec![] },
    ]);
    assert!(Database::open(dir.path(), &[dup], 1).is_err());
    let sys = messages_def(vec![SearchIndexDef { name: "s", search_field: "_id", filter_fields: vec![] }]);
    assert!(Database::open(dir.path(), &[sys], 1).is_err());
    let zero = TableDef {
        vector_indexes: vec![VectorIndexDef { name: "v", vector_field: "x", dimensions: 0, filter_fields: vec![] }],
        ..original.clone()
    };
    assert!(Database::open(dir.path(), &[zero], 1).is_err());
}

// ------------------------------------------------------------------------------------------------ vectors

fn random_vector(rng: &mut StdRng) -> Vec<f32> {
    (0..vecs::DIMS).map(|_| rng.random_range(-1.0f32..1.0)).collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb)
}

#[tokio::test]
async fn vector_search_matches_brute_force() {
    let t = TestApp::new().await.unwrap();
    let mut rng = StdRng::seed_from_u64(7);
    let categories = ["a", "b", "c"];
    let mut items: Vec<(String, i64, Vec<f32>)> = Vec::new();
    for i in 0..600 {
        items.push((categories[i % 3].to_string(), (i % 5) as i64, random_vector(&mut rng)));
    }
    let ids = t.mutation::<vecs::add_many>((items.clone(),)).await.unwrap();
    // Documents without a vector are never returned.
    t.mutation::<vecs::add>(("a".to_string(), 0, None)).await.unwrap();

    for round in 0..20 {
        let q = random_vector(&mut rng);
        let cats: Option<Vec<String>> = match round % 4 {
            0 => None,
            1 => Some(vec!["a".into()]),
            2 => Some(vec!["a".into(), "c".into()]),
            _ => Some(vec![]),
        };
        let rank = if round % 3 == 0 { Some(2) } else { None };
        let limit = [1, 5, 10, 64][round % 4];
        let got = t.action::<vecs::nearest>((q.clone(), limit as u32, cats.clone(), rank)).await.unwrap();
        let mut expected: Vec<(usize, f32)> = items
            .iter()
            .enumerate()
            .filter(|(_, (c, r, _))| cats.as_ref().is_none_or(|cs| cs.contains(c)) && rank.is_none_or(|x| x == *r))
            .map(|(i, (_, _, v))| (i, cosine(&q, v)))
            .collect();
        expected.sort_by(|a, b| b.1.total_cmp(&a.1));
        expected.truncate(limit);
        assert_eq!(got.len(), expected.len(), "round {round}");
        for (vecs::Hit { id, score }, (i, s)) in got.iter().zip(&expected) {
            assert!((score - s).abs() < 1e-4, "round {round}: score {score} vs {s}");
            // Ties within float noise could swap neighbours; compare ids only when the scores are distinct.
            if expected.iter().filter(|(_, x)| (x - s).abs() < 1e-5).count() == 1 {
                assert_eq!(*id, ids[*i], "round {round}");
            }
        }
    }
}

#[tokio::test]
async fn vector_errors_and_transactional_maintenance() {
    let t = TestApp::new().await.unwrap();
    let mut rng = StdRng::seed_from_u64(1);
    let v = random_vector(&mut rng);
    let id = t.mutation::<vecs::add>(("x".to_string(), 1, Some(v.clone()))).await.unwrap();

    // Dimension checks: the query vector, and documents.
    let err = t.action::<vecs::nearest>((vec![1.0, 2.0], 3, None, None)).await.unwrap_err();
    assert!(format!("{err:#}").contains("has 2 dimensions"), "{err:#}");
    let err = t.mutation::<vecs::add>(("x".to_string(), 1, Some(vec![1.0; 3]))).await.unwrap_err();
    assert!(format!("{err:#}").contains("has 3 dimensions, but vector index `by_embedding` expects 8"), "{err:#}");
    // A failed write leaves nothing behind even when the mutation goes on and commits.
    let msg = t.mutation::<vecs::add_bad_then_good>((random_vector(&mut rng),)).await.unwrap();
    assert!(msg.contains("expects 8"));
    assert_eq!(t.app().db().verify_indexes().await.unwrap(), Vec::<String>::new());
    // Limits and filters.
    assert!(t.action::<vecs::nearest>((v.clone(), 0, None, None)).await.is_err());
    assert!(t.action::<vecs::nearest>((v.clone(), 257, None, None)).await.is_err());
    let err = t.action::<vecs::bad_filter>(()).await.unwrap_err();
    assert!(format!("{err:#}").contains("is not a filter field"), "{err:#}");

    // Rollback leaves the vec0 table untouched.
    t.mutation::<vecs::add_then_fail>((random_vector(&mut rng),)).await.unwrap_err();
    let got = t.action::<vecs::nearest>((v.clone(), 10, None, None)).await.unwrap();
    assert_eq!(got.len(), 2, "x and good");
    assert_eq!(got[0].id, id);
    assert!((got[0].score - 1.0).abs() < 1e-5, "identical vector: similarity 1");
    assert!(t.action::<vecs::nearest>((v.clone(), 10, Some(vec!["rolled_back".into()]), None)).await.unwrap().is_empty());

    // Patch the filter field, the vector, null it, delete.
    t.mutation::<vecs::set_category>((id, "y".to_string())).await.unwrap();
    assert!(t.action::<vecs::nearest>((v.clone(), 10, Some(vec!["x".into()]), None)).await.unwrap().is_empty());
    assert_eq!(t.action::<vecs::nearest>((v.clone(), 10, Some(vec!["y".into()]), None)).await.unwrap().len(), 1);
    let w: Vec<f32> = v.iter().map(|x| -x).collect();
    t.mutation::<vecs::set_embedding>((id, Some(w.clone()))).await.unwrap();
    let got = t.action::<vecs::nearest>((w.clone(), 1, Some(vec!["y".into()]), None)).await.unwrap();
    assert!((got[0].score - 1.0).abs() < 1e-5);
    t.mutation::<vecs::set_embedding>((id, None)).await.unwrap();
    assert!(t.action::<vecs::nearest>((w.clone(), 10, Some(vec!["y".into()]), None)).await.unwrap().is_empty());
    t.mutation::<vecs::set_embedding>((id, Some(v.clone()))).await.unwrap();
    assert_eq!(t.app().db().verify_indexes().await.unwrap(), Vec::<String>::new());
    t.mutation::<vecs::remove>((id,)).await.unwrap();
    assert!(t.action::<vecs::nearest>((v.clone(), 10, Some(vec!["y".into()]), None)).await.unwrap().is_empty());
    assert_eq!(t.app().db().verify_indexes().await.unwrap(), Vec::<String>::new());
}

#[tokio::test]
async fn vector_index_catalog_changes() {
    let dir = tempfile::tempdir().unwrap();
    let mut rng = StdRng::seed_from_u64(3);
    {
        let app = App::open(dir.path(), AppOptions::default()).await.unwrap();
        let items: Vec<_> = (0..50).map(|i| (format!("c{}", i % 2), i, random_vector(&mut rng))).collect();
        app.mutation::<vecs::add_many>((items,)).await.unwrap();
    }
    // Different dimensions: rebuilt; the existing 8-dim vectors don't fit and are skipped (with a warning).
    let four = TableDef {
        vector_indexes: vec![VectorIndexDef { name: "by_embedding", vector_field: "embedding", dimensions: 4, filter_fields: vec![] }],
        ..Item::table_def()
    };
    let db = Database::open(dir.path(), std::slice::from_ref(&four), 1).unwrap();
    assert_eq!(db.verify_indexes().await.unwrap(), Vec::<String>::new());
    drop(db);
    // Back to the definition in code: backfilled, searchable.
    let app = App::open(dir.path(), AppOptions::default()).await.unwrap();
    assert_eq!(app.db().verify_indexes().await.unwrap(), Vec::<String>::new());
    let ctx_check = app.action::<vecs::nearest>((random_vector(&mut rng), 100, Some(vec!["c0".into()]), None)).await.unwrap();
    assert_eq!(ctx_check.len(), 25);
}

/// Rough performance numbers: `cargo test --release -p ferrix --test it search::perf -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "benchmark; run with --ignored --nocapture (ideally --release)"]
async fn perf() {
    use std::time::Instant;
    const N: usize = 50_000;
    const DIMS: usize = 384;
    // A 384-dim table defined at runtime: open a Database directly and write through the untyped path of an App
    // would need a registered table, so measure FTS and vec0 through the registered tables and raw SQL instead.
    let t = TestApp::new().await.unwrap();
    let mut rng = StdRng::seed_from_u64(42);

    // Text: N documents of ~12 random words from a 5k-word vocabulary.
    let vocab: Vec<String> = (0..5000).map(|i| format!("w{i:04}x")).collect();
    let start = Instant::now();
    for _ in 0..N / 1000 {
        let bodies: Vec<String> = (0..1000)
            .map(|_| (0..12).map(|_| vocab[rng.random_range(0..vocab.len())].as_str()).collect::<Vec<_>>().join(" "))
            .collect();
        t.mutation::<msgs::add_many>(("c".to_string(), bodies)).await.unwrap();
    }
    println!("fts: inserted {N} docs in {:?}", start.elapsed());
    for q in ["w0001x", "w00", "w0001x w0002x", "w1"] {
        let start = Instant::now();
        let mut n = 0;
        for _ in 0..20 {
            n = t.query::<msgs::search>((q.to_string(), None, None, Some(10), None)).await.unwrap().len();
        }
        println!("fts: query {q:?} (take 10, {n} results): {:?} per query", start.elapsed() / 20);
    }

    // Vectors: N × 384 in a vec0 table, the same layout the index uses, queried with the same SQL.
    ferrix::db::search::vector::register_extension();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE v USING vec0(doc_id text primary key, v float[{DIMS}] distance_metric=cosine, f0 text)"
    ))
    .unwrap();
    let start = Instant::now();
    conn.execute_batch("BEGIN").unwrap();
    {
        let mut stmt = conn.prepare("INSERT INTO v (doc_id, v, f0) VALUES (?1, ?2, ?3)").unwrap();
        for i in 0..N {
            let vec: Vec<f32> = (0..DIMS).map(|_| rng.random_range(-1.0f32..1.0)).collect();
            let blob: Vec<u8> = vec.iter().flat_map(|f| f.to_le_bytes()).collect();
            stmt.execute(rusqlite::params![format!("{i:026}"), blob, format!("c{}", i % 10)]).unwrap();
        }
    }
    conn.execute_batch("COMMIT").unwrap();
    println!("vec0: inserted {N} x {DIMS} in {:?}", start.elapsed());
    for (label, filter) in [("no filter", ""), ("filter 1/10", " AND f0 = 'c3'")] {
        let mut stmt = conn
            .prepare(&format!("SELECT doc_id, distance FROM v WHERE v MATCH ?1 AND k = 10{filter} ORDER BY distance"))
            .unwrap();
        let start = Instant::now();
        for _ in 0..10 {
            let q: Vec<f32> = (0..DIMS).map(|_| rng.random_range(-1.0f32..1.0)).collect();
            let blob: Vec<u8> = q.iter().flat_map(|f| f.to_le_bytes()).collect();
            let n = stmt.query_map([blob], |_| Ok(())).unwrap().count();
            assert_eq!(n, 10);
        }
        println!("vec0: knn k=10 ({label}): {:?} per query", start.elapsed() / 10);
    }
}
