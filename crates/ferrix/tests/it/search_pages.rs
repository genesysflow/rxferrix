//! Search pagination (cursors, pinned pages under changes, read sets), configurable tokenizers, the dashboard's
//! `search_documents`, and approximate (HNSW) vector indexes: recall, consistency with commits, tombstones and
//! rebuilds, persistence and validation at startup.

use std::collections::HashSet;
use std::time::Duration;

use ferrix::db::Database;
use ferrix::db::readset::ReadSet;
use ferrix::testing::TestApp;
use ferrix::{App, AppOptions, FunctionRef, Identity, PaginationOpts, PaginationResult, Table, TableDef, Tokenizer};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use serde_json::{Value, json};

pub mod pages {
    use ferrix::prelude::*;

    #[table(name = "page_messages")]
    #[search_index(search_body, search = body, filter = [channel])]
    pub struct Msg {
        pub channel: String,
        pub body: String,
        pub note: Option<String>,
    }

    /// One page of a search, optionally in one channel, optionally keeping only bodies longer than `min_len`.
    #[query]
    pub async fn page(
        ctx: &QueryCtx,
        text: String,
        channel: Option<String>,
        min_len: Option<u32>,
        pagination_opts: PaginationOpts,
    ) -> Result<PaginationResult<Doc<Msg>>> {
        let mut q = ctx.db.query::<Msg>().with_search_index(Msg::search_body, |q| {
            let q = q.search(Msg::body, text);
            match channel {
                Some(c) => q.eq(Msg::channel, c),
                None => q,
            }
        });
        if let Some(n) = min_len {
            q = q.filter(move |d| d.body.len() > n as usize);
        }
        q.paginate(pagination_opts).await
    }

    /// Every match, in relevance order.
    #[query]
    pub async fn all(ctx: &QueryCtx, text: String, channel: Option<String>) -> Result<Vec<Doc<Msg>>> {
        ctx.db
            .query::<Msg>()
            .with_search_index(Msg::search_body, |q| {
                let q = q.search(Msg::body, text);
                match channel {
                    Some(c) => q.eq(Msg::channel, c),
                    None => q,
                }
            })
            .collect()
            .await
    }

    #[mutation]
    pub async fn add_many(ctx: &MutationCtx, channel: String, bodies: Vec<String>) -> Result<Vec<Id<Msg>>> {
        let mut ids = Vec::new();
        for body in bodies {
            ids.push(ctx.db.insert(Msg { channel: channel.clone(), body, note: None }).await?);
        }
        Ok(ids)
    }

    #[mutation]
    pub async fn set_body(ctx: &MutationCtx, id: Id<Msg>, body: String) -> Result<()> {
        ctx.db.patch(id, json!({ "body": body })).await
    }

    #[mutation]
    pub async fn set_channel(ctx: &MutationCtx, id: Id<Msg>, channel: String) -> Result<()> {
        ctx.db.patch(id, json!({ "channel": channel })).await
    }

    #[mutation]
    pub async fn set_note(ctx: &MutationCtx, id: Id<Msg>, note: String) -> Result<()> {
        ctx.db.patch(id, json!({ "note": note })).await
    }

    #[mutation]
    pub async fn remove(ctx: &MutationCtx, id: Id<Msg>) -> Result<()> {
        ctx.db.delete(id).await
    }
}

pub mod tok {
    use ferrix::prelude::*;

    #[table(name = "tok_docs")]
    #[search_index(stemmed, search = body, tokenizer = "porter", language = "english")]
    #[search_index(substring, search = body, tokenizer = "trigram")]
    #[search_index(accents, search = body, diacritics = "keep")]
    #[search_index(plain, search = body)]
    pub struct TokDoc {
        pub body: String,
    }

    /// Bodies matching `text` in the index named `index`, most relevant first.
    #[query]
    pub async fn find(ctx: &QueryCtx, index: String, text: String) -> Result<Vec<String>> {
        let idx = match index.as_str() {
            "stemmed" => TokDoc::stemmed,
            "substring" => TokDoc::substring,
            "accents" => TokDoc::accents,
            _ => TokDoc::plain,
        };
        let docs = ctx.db.query::<TokDoc>().with_search_index(idx, |q| q.search(TokDoc::body, text)).collect().await?;
        Ok(docs.into_iter().map(|d| d.value.body).collect())
    }

    #[mutation]
    pub async fn add(ctx: &MutationCtx, bodies: Vec<String>) -> Result<()> {
        for body in bodies {
            ctx.db.insert(TokDoc { body }).await?;
        }
        Ok(())
    }
}

pub mod ann {
    use ferrix::prelude::*;

    pub const DIMS: usize = 24;

    #[table(name = "ann_items")]
    #[vector_index(by_vec, field = vec, dims = 24, filter = [category], kind = "hnsw")]
    pub struct AnnItem {
        pub category: String,
        pub vec: Option<Vec<f32>>,
    }

    #[mutation]
    pub async fn add_many(ctx: &MutationCtx, items: Vec<(String, Vec<f32>)>) -> Result<Vec<Id<AnnItem>>> {
        let mut ids = Vec::new();
        for (category, vec) in items {
            ids.push(ctx.db.insert(AnnItem { category, vec: Some(vec) }).await?);
        }
        Ok(ids)
    }

    #[mutation]
    pub async fn add_then_fail(ctx: &MutationCtx, vec: Vec<f32>) -> Result<()> {
        ctx.db.insert(AnnItem { category: "ghost".into(), vec: Some(vec) }).await?;
        Err(UserError::new("nope", "rolled back").into())
    }

    #[mutation]
    pub async fn set_vec(ctx: &MutationCtx, id: Id<AnnItem>, vec: Option<Vec<f32>>) -> Result<()> {
        ctx.db.patch(id, json!({ "vec": vec })).await
    }

    #[mutation]
    pub async fn set_category(ctx: &MutationCtx, id: Id<AnnItem>, category: String) -> Result<()> {
        ctx.db.patch(id, json!({ "category": category })).await
    }

    #[mutation]
    pub async fn remove_many(ctx: &MutationCtx, ids: Vec<Id<AnnItem>>) -> Result<()> {
        for id in ids {
            ctx.db.delete(id).await?;
        }
        Ok(())
    }

    #[derive(serde::Serialize, serde::Deserialize, ts_rs::TS, Clone, Copy, Debug, PartialEq)]
    #[ts(rename = "PageHit")]
    pub struct Hit {
        pub id: Id<AnnItem>,
        pub score: f32,
    }

    #[action]
    pub async fn nearest(
        ctx: &ActionCtx,
        vector: Vec<f32>,
        limit: u32,
        category: Option<String>,
        exact: bool,
    ) -> Result<Vec<Hit>> {
        let mut opts = VectorSearchOpts::new(limit as usize);
        if let Some(c) = category {
            opts = opts.filter(VectorFilter::eq(AnnItem::category, c));
        }
        if exact {
            opts = opts.exact();
        }
        let hits = ctx.vector_search(AnnItem::by_vec, &vector, opts).await?;
        Ok(hits.into_iter().map(|(id, score)| Hit { id, score }).collect())
    }

    /// The graph search with an explicit search width.
    #[action]
    pub async fn nearest_ef(ctx: &ActionCtx, vector: Vec<f32>, limit: u32, ef: u32) -> Result<Vec<Hit>> {
        let opts = VectorSearchOpts::new(limit as usize).ef(ef as usize);
        let hits = ctx.vector_search(AnnItem::by_vec, &vector, opts).await?;
        Ok(hits.into_iter().map(|(id, score)| Hit { id, score }).collect())
    }
}

use pages::Msg;

fn first(n: usize) -> PaginationOpts {
    PaginationOpts::first(n)
}

async fn page(
    t: &TestApp,
    text: &str,
    channel: Option<&str>,
    opts: PaginationOpts,
) -> ferrix::Result<PaginationResult<ferrix::Doc<Msg>>> {
    t.query::<pages::page>((text.to_string(), channel.map(str::to_string), None, opts)).await
}

async fn all(t: &TestApp, text: &str, channel: Option<&str>) -> Vec<ferrix::Id<Msg>> {
    t.query::<pages::all>((text.to_string(), channel.map(str::to_string)))
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.id)
        .collect()
}

/// Bodies with varied relevance for "apple": 1-3 occurrences in bodies of different lengths, many ties.
fn bodies(n: usize, rng: &mut StdRng) -> Vec<String> {
    (0..n)
        .map(|i| {
            let hits = rng.random_range(1..4);
            let filler = rng.random_range(0..6);
            let mut words: Vec<String> = (0..hits).map(|_| "apple".to_string()).collect();
            words.extend((0..filler).map(|j| format!("w{i}x{j}")));
            words.join(" ")
        })
        .collect()
}

#[tokio::test]
async fn pages_walk_every_match_once_in_relevance_order() {
    let t = TestApp::new().await.unwrap();
    let mut rng = StdRng::seed_from_u64(1);
    t.mutation::<pages::add_many>(("c".to_string(), bodies(53, &mut rng))).await.unwrap();
    t.mutation::<pages::add_many>(("d".to_string(), bodies(9, &mut rng))).await.unwrap();
    let expected = all(&t, "appl", None).await;
    assert_eq!(expected.len(), 62);

    for size in [1, 7, 62, 100] {
        let mut got = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages_read = 0;
        loop {
            let opts = PaginationOpts { num_items: size, cursor: cursor.clone(), end_cursor: None };
            let p = page(&t, "appl", None, opts).await.unwrap();
            pages_read += 1;
            assert!(p.page.len() <= size);
            got.extend(p.page.iter().map(|d| d.id));
            cursor = Some(p.continue_cursor.clone());
            if p.is_done {
                break;
            }
            assert_eq!(p.page.len(), size, "a page that is not done is full");
        }
        assert_eq!(got, expected, "page size {size}");
        // isDone is exact: no trailing empty page.
        assert_eq!(pages_read, expected.len().div_ceil(size), "page size {size}");
        // Past the end: empty and done.
        let p = page(&t, "appl", None, PaginationOpts::after(cursor.unwrap(), 5)).await.unwrap();
        assert!(p.page.is_empty() && p.is_done);
    }

    // A filter by channel paginates its own matches.
    let in_d = all(&t, "apple", Some("d")).await;
    let p1 = page(&t, "apple", Some("d"), first(5)).await.unwrap();
    let p2 = page(&t, "apple", Some("d"), PaginationOpts::after(&p1.continue_cursor, 5)).await.unwrap();
    assert!(p2.is_done);
    assert_eq!(p1.page.iter().chain(&p2.page).map(|d| d.id).collect::<Vec<_>>(), in_d);

    // A post-filter: pages hold only passing documents, still in order, still covering everything.
    let long: Vec<_> = t
        .query::<pages::all>(("apple".to_string(), None))
        .await
        .unwrap()
        .into_iter()
        .filter(|d| d.body.len() > 20)
        .map(|d| d.id)
        .collect();
    let mut got = Vec::new();
    let mut cursor = None;
    loop {
        let opts = PaginationOpts { num_items: 4, cursor, end_cursor: None };
        let p = t.query::<pages::page>(("apple".to_string(), None, Some(20), opts)).await.unwrap();
        got.extend(p.page.iter().map(|d| d.id));
        cursor = Some(p.continue_cursor);
        if p.is_done {
            break;
        }
    }
    assert_eq!(got, long);

    // Cursors are checked: other text, other filters, malformed, reversed, an index-pagination cursor.
    let p = page(&t, "apple", None, first(3)).await.unwrap();
    let err = |r: ferrix::Result<PaginationResult<ferrix::Doc<Msg>>>| format!("{:#}", r.unwrap_err());
    assert!(
        err(page(&t, "pear", None, PaginationOpts::after(&p.continue_cursor, 3)).await).contains("does not belong")
    );
    assert!(
        err(page(&t, "apple", Some("c"), PaginationOpts::after(&p.continue_cursor, 3)).await)
            .contains("does not belong")
    );
    assert!(err(page(&t, "apple", None, PaginationOpts::after("bogus!", 3)).await).contains("malformed"));
    let p2 = page(&t, "apple", None, PaginationOpts::after(&p.continue_cursor, 3)).await.unwrap();
    let reversed = PaginationOpts::after(&p2.continue_cursor, 3).with_end_cursor(&p.continue_cursor);
    assert!(err(page(&t, "apple", None, reversed).await).contains("endCursor is before cursor"));
    assert!(err(page(&t, "apple", None, first(0)).await).contains("at least 1"));
    // Text with the same terms shares cursors (whitespace and punctuation don't matter).
    assert!(page(&t, "  apple!! ", None, PaginationOpts::after(&p.continue_cursor, 3)).await.is_ok());

    // Empty text: one empty, done page.
    let p = page(&t, " ", None, first(3)).await.unwrap();
    assert!(p.page.is_empty() && p.is_done);
}

/// The pinned-pages contract: within one snapshot, a chain of pinned pages (each from the previous one's
/// `continueCursor`, every page but the last pinned to its own) partitions the matches, through random inserts,
/// edits that change relevance, channel moves and deletes. A page whose read set a commit misses is unchanged.
#[tokio::test]
async fn pinned_pages_partition_matches_through_changes() {
    let t = TestApp::new().await.unwrap();
    let mut rng = StdRng::seed_from_u64(7);
    let mut ids = t.mutation::<pages::add_many>(("c".to_string(), bodies(30, &mut rng))).await.unwrap();
    ids.extend(t.mutation::<pages::add_many>(("d".to_string(), bodies(10, &mut rng))).await.unwrap());

    // Load a chain of pages of 6 for channel "c", as the client does: subscribe page k from page k-1's
    // continueCursor, then pin page k-1 to it.
    let mut chain: Vec<PaginationOpts> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..4 {
        let opts = PaginationOpts { num_items: 6, cursor: cursor.clone(), end_cursor: None };
        let p = page(&t, "apple", Some("c"), opts.clone()).await.unwrap();
        if let Some(prev) = chain.last_mut() {
            prev.end_cursor = cursor.clone();
        }
        chain.push(opts);
        cursor = Some(p.continue_cursor);
        if p.is_done {
            break;
        }
    }
    // Pin the last loaded one too, except that the client keeps the last page unpinned: keep it so.
    async fn run_chain(t: &TestApp, chain: &[PaginationOpts]) -> Vec<(Vec<ferrix::Id<Msg>>, ReadSet)> {
        let mut out = Vec::new();
        for opts in chain {
            let args = json!({ "text": "apple", "channel": "c", "paginationOpts": opts });
            let run = t.app().run_query(&pages::page::path(), args, &Identity::default()).await;
            let result: PaginationResult<ferrix::Doc<Msg>> = serde_json::from_value(run.result.unwrap()).unwrap();
            out.push((result.page.into_iter().map(|d| d.id).collect(), run.read_set));
        }
        out
    }
    let mut state = run_chain(&t, &chain).await;
    let mut skipped = 0;
    for step in 0..120 {
        let id = ids[rng.random_range(0..ids.len())];
        let res = match rng.random_range(0..6) {
            0 => {
                let (new, ts) = t.mutation_ts::<pages::add_many>(("c".to_string(), bodies(1, &mut rng))).await.unwrap();
                ids.extend(new);
                Ok(ts)
            }
            1 => t.mutation_ts::<pages::set_body>((id, bodies(1, &mut rng).remove(0))).await.map(|r| r.1),
            2 => t.mutation_ts::<pages::set_body>((id, "no match here".to_string())).await.map(|r| r.1),
            3 => {
                let c = if rng.random_bool(0.5) { "c" } else { "d" };
                t.mutation_ts::<pages::set_channel>((id, c.to_string())).await.map(|r| r.1)
            }
            4 => t.mutation_ts::<pages::set_note>((id, format!("n{step}"))).await.map(|r| r.1),
            _ => {
                let r = t.mutation_ts::<pages::remove>((id,)).await.map(|r| r.1);
                ids.retain(|x| *x != id);
                r
            }
        };
        let Ok(ts) = res else { continue }; // e.g. patching a deleted document
        let commit = t.app().db().write_log().entries_between(ts - 1, ts).unwrap().remove(0);
        let fresh = run_chain(&t, &chain).await;
        // No duplicates, no gaps: the concatenation of the pages is the full result, except what lies after the
        // last (unpinned) page's numItems.
        let concat: Vec<_> = fresh.iter().flat_map(|(p, _)| p.iter().copied()).collect();
        let unique: HashSet<_> = concat.iter().collect();
        assert_eq!(unique.len(), concat.len(), "step {step}: duplicate across pages");
        let everything = all(&t, "apple", Some("c")).await;
        assert_eq!(concat[..], everything[..concat.len()], "step {step}: pages are not a prefix of the result");
        let last_full = fresh.last().unwrap().0.len() == 6;
        assert!(last_full || concat.len() == everything.len(), "step {step}: a gap after the last page");
        for (k, ((old, rs), (new, _))) in state.iter().zip(&fresh).enumerate() {
            if !rs.overlaps(&commit) {
                skipped += 1;
                assert_eq!(old, new, "step {step}: page {k} changed without being invalidated");
            }
        }
        state = fresh;
    }
    assert!(skipped > 0, "writes in channel d spare the pages of channel c");
}

#[tokio::test]
async fn paginated_search_subscription_updates_live() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    let t = TestApp::new().await.unwrap();
    let addr = t.serve().await.unwrap();
    t.mutation::<pages::add_many>(("c".to_string(), vec!["apple".into(), "apple pie with cream".into()]))
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/sync")).await.unwrap();
    let send = |v: Value| WsMessage::text(v.to_string());
    ws.send(send(json!({ "type": "Connect", "sessionId": "pages", "connectionCount": 0, "lastCloseReason": null, "clientVersion": "0.1.0" }))).await.unwrap();
    ws.send(send(json!({ "type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 1, "modifications": [{
        "type": "Add", "queryId": 0, "udfPath": pages::page::path(),
        "args": { "text": "apple", "channel": "c", "paginationOpts": { "numItems": 2, "cursor": null } } }] })))
        .await
        .unwrap();
    let mut next = async || loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.expect("no update").unwrap().unwrap();
        let WsMessage::Text(text) = msg else { continue };
        let v: Value = serde_json::from_str(text.as_str()).unwrap();
        if v["type"] == "Transition"
            && let Some(m) = v["modifications"].as_array().unwrap().iter().find(|m| m["queryId"] == 0)
        {
            return m["value"].clone();
        }
    };
    let bodies = |v: &Value| {
        v["page"].as_array().unwrap().iter().map(|d| d["body"].as_str().unwrap().to_string()).collect::<Vec<_>>()
    };
    let v = next().await;
    assert_eq!(bodies(&v), vec!["apple", "apple pie with cream"]);
    assert_eq!(v["isDone"], true);
    // A more relevant match in the channel enters the first page, pushing the last one out.
    t.mutation::<pages::add_many>(("c".to_string(), vec!["apple apple".into()])).await.unwrap();
    let v = next().await;
    assert_eq!(bodies(&v), vec!["apple apple", "apple"]);
    assert_eq!(v["isDone"], false);
}

// ------------------------------------------------------------------------------------------------ tokenizers

async fn find(t: &TestApp, index: &str, text: &str) -> Vec<String> {
    t.query::<tok::find>((index.to_string(), text.to_string())).await.unwrap()
}

#[tokio::test]
async fn tokenizers_stem_find_substrings_and_keep_diacritics() {
    let t = TestApp::new().await.unwrap();
    t.mutation::<tok::add>((vec![
        "running every morning".to_string(),
        "she runs fast".to_string(),
        "a quick run".to_string(),
        "hello world".to_string(),
        "Café crème".to_string(),
        "cafe au lait".to_string(),
    ],))
        .await
        .unwrap();
    // Porter: "run" matches running and runs; "running" matches run (all stem to "run").
    let mut got = find(&t, "stemmed", "run").await;
    got.sort();
    assert_eq!(got, vec!["a quick run", "running every morning", "she runs fast"]);
    let mut got = find(&t, "stemmed", "running").await;
    got.sort();
    assert_eq!(got, vec!["a quick run", "running every morning", "she runs fast"]);
    // Without stemming, "runs" is only a prefix match of itself.
    assert_eq!(find(&t, "plain", "runs").await, vec!["she runs fast"]);
    assert_eq!(find(&t, "plain", "run").await.len(), 3, "prefix: run, runs, running");
    assert_eq!(find(&t, "plain", "running morning").await, vec!["running every morning"]);

    // Trigram: substrings anywhere in a word; terms under 3 characters are ignored.
    assert_eq!(find(&t, "substring", "ell").await, vec!["hello world"]);
    assert_eq!(find(&t, "substring", "orl ello").await, vec!["hello world"]);
    assert_eq!(find(&t, "substring", "unn").await, vec!["running every morning"]);
    assert_eq!(find(&t, "substring", "a ell").await, vec!["hello world"], "`a` is too short and ignored");
    assert!(find(&t, "substring", "el").await.is_empty(), "only short terms: matches nothing");
    assert!(find(&t, "plain", "ell").await.is_empty(), "word tokenizers don't match inside words");
    // Diacritics folded by default (and by trigram), kept when asked.
    let mut got = find(&t, "plain", "cafe").await;
    got.sort();
    assert_eq!(got, vec!["Café crème", "cafe au lait"]);
    assert_eq!(find(&t, "substring", "afé").await.len(), 2);
    assert_eq!(find(&t, "accents", "café").await, vec!["Café crème"]);
    assert_eq!(find(&t, "accents", "cafe").await, vec!["cafe au lait"]);
    assert!(t.app().db().verify_indexes().await.unwrap().is_empty());
}

#[tokio::test]
async fn changing_the_tokenizer_rebuilds_the_index() {
    let dir = tempfile::tempdir().unwrap();
    {
        let app = App::open(dir.path(), AppOptions::default()).await.unwrap();
        app.mutation::<tok::add>((vec!["running fast".to_string(), "hello".to_string()],)).await.unwrap();
    }
    let original = tok::TokDoc::table_def();
    let index_id = |db: &Database| {
        db.catalog().table("tok_docs").unwrap().search_indexes.iter().find(|i| i.name == "stemmed").unwrap().id
    };
    let ddl = |db: &Database| {
        let n = index_id(db) - ferrix::db::search::SPECIAL_INDEX_BASE;
        let conn = rusqlite::Connection::open(dir.path().join("data.db")).unwrap();
        conn.query_row(&format!("SELECT sql FROM sqlite_master WHERE name = '_search_{n}'"), [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap()
    };
    let db = Database::open(dir.path(), std::slice::from_ref(&original), 1).unwrap();
    let id = index_id(&db);
    assert!(ddl(&db).contains("porter unicode61 remove_diacritics 2"), "{}", ddl(&db));
    drop(db);

    // Switch `stemmed` to trigram: rebuilt in place (same id), backfilled, consistent.
    let mut changed = original.clone();
    changed.search_indexes.iter_mut().find(|s| s.name == "stemmed").unwrap().tokenizer = Tokenizer::trigram();
    let db = Database::open(dir.path(), std::slice::from_ref(&changed), 1).unwrap();
    assert_eq!(index_id(&db), id);
    assert!(ddl(&db).contains("trigram remove_diacritics 1"), "{}", ddl(&db));
    assert_eq!(db.verify_indexes().await.unwrap(), Vec::<String>::new());
    drop(db);

    // An invalid language is refused at startup.
    let mut bad = original.clone();
    bad.search_indexes[0].tokenizer = Tokenizer::porter("klingon");
    let err = Database::open(dir.path(), &[bad], 1).err().unwrap();
    assert!(format!("{err:#}").contains("unsupported stemming language"), "{err:#}");
    let mut bad = original.clone();
    bad.search_indexes[0].tokenizer = Tokenizer { language: Some("english"), ..Tokenizer::trigram() };
    assert!(Database::open(dir.path(), &[bad], 1).is_err());

    // Back to the code's definition (porter): rebuilt again, and stemming works.
    let app = App::open(dir.path(), AppOptions::default()).await.unwrap();
    assert_eq!(app.query::<tok::find>(("stemmed".to_string(), "run".to_string())).await.unwrap(), vec!["running fast"]);
    assert!(app.db().verify_indexes().await.unwrap().is_empty());
}

// ------------------------------------------------------------------------------------------------ dashboard

#[tokio::test]
async fn dashboard_search_documents() {
    let t = TestApp::new().await.unwrap();
    let admin = t.app().auth().create_admin(t.app(), "admin@example.com", "admin password".into()).await.unwrap();
    let admin = Identity::admin(admin, None);
    t.mutation::<pages::add_many>(("c".to_string(), vec!["apple pie".into(), "apple apple".into(), "pear".into()]))
        .await
        .unwrap();
    t.mutation::<pages::add_many>(("d".to_string(), vec!["apple tart".into()])).await.unwrap();
    let call = |args: Value, who: Identity| {
        let app = t.app();
        async move { app.run_query("_system/dashboard:search_documents", args, &who).await.result }
    };
    let args = |text: &str, filters: Value, opts: Value| json!({ "table": "page_messages", "index": "search_body", "text": text, "filters": filters, "paginationOpts": opts });
    let body = |v: &Value| {
        v["page"].as_array().unwrap().iter().map(|d| d["body"].as_str().unwrap().to_string()).collect::<Vec<_>>()
    };

    let p1 = call(args("apple", Value::Null, json!({ "numItems": 2, "cursor": null })), admin.clone()).await.unwrap();
    assert_eq!(body(&p1), vec!["apple apple", "apple pie"]);
    assert_eq!(p1["isDone"], false);
    let p2 = call(args("apple", Value::Null, json!({ "numItems": 2, "cursor": p1["continueCursor"] })), admin.clone())
        .await
        .unwrap();
    assert_eq!(body(&p2), vec!["apple tart"]);
    assert_eq!(p2["isDone"], true);
    let only_d = call(
        args("apple", json!([{ "field": "channel", "value": "d" }]), json!({ "numItems": 10, "cursor": null })),
        admin.clone(),
    )
    .await
    .unwrap();
    assert_eq!(body(&only_d), vec!["apple tart"]);
    assert!(only_d["page"][0]["_id"].is_string() && only_d["page"][0]["_creationTime"].is_number());

    // Errors are user errors with codes.
    let code =
        |r: ferrix::Result<Value>| r.unwrap_err().downcast_ref::<ferrix::UserError>().map(|e| e.code.clone()).unwrap();
    let opts = json!({ "numItems": 2, "cursor": null });
    assert_eq!(
        code(call(json!({ "table": "nope", "index": "x", "text": "a", "paginationOpts": opts }), admin.clone()).await),
        "unknown_table"
    );
    assert_eq!(
        code(
            call(json!({ "table": "page_messages", "index": "x", "text": "a", "paginationOpts": opts }), admin.clone())
                .await
        ),
        "invalid_query"
    );
    assert_eq!(
        code(
            call(
                json!({ "table": "ann_items", "index": "by_vec", "text": "a", "paginationOpts": opts }),
                admin.clone()
            )
            .await
        ),
        "invalid_query"
    );
    assert_eq!(
        code(call(args("a", json!([{ "field": "note", "value": "x" }]), opts.clone()), admin.clone()).await),
        "invalid_query"
    );
    assert_eq!(
        code(call(args("a", Value::Null, json!({ "numItems": 2, "cursor": "bad" })), admin.clone()).await),
        "invalid_query"
    );
    // Admins only.
    assert_eq!(code(call(args("apple", Value::Null, opts.clone()), Identity::anonymous()).await), "unauthenticated");
    let user = Identity { user_id: Some("someone".into()), ..Identity::anonymous() };
    assert_eq!(code(call(args("apple", Value::Null, opts.clone()), user).await), "forbidden");
}

// ------------------------------------------------------------------------------------------------ HNSW

fn random_vector(rng: &mut StdRng) -> Vec<f32> {
    (0..ann::DIMS).map(|_| rng.random_range(-1.0f32..1.0)).collect()
}

async fn ann_index(t: &TestApp) -> std::sync::Arc<ferrix::db::search::ann::AnnIndex> {
    let id = t.app().db().catalog().table("ann_items").unwrap().search_indexes[0].id;
    let index = t.app().db().ann().index(id).expect("an HNSW index").clone();
    assert!(index.wait_ready(Duration::from_secs(60)).await, "graph not ready");
    index
}

async fn nearest(t: &TestApp, q: &[f32], k: u32, category: Option<&str>, exact: bool) -> Vec<ann::Hit> {
    t.action::<ann::nearest>((q.to_vec(), k, category.map(str::to_string), exact)).await.unwrap()
}

/// Recall@10 of the graph against exact search, unfiltered and with a filter keeping half (post-filtering).
#[tokio::test]
async fn hnsw_recall_against_exact_search() {
    let t = TestApp::new().await.unwrap();
    let mut rng = StdRng::seed_from_u64(42);
    // Each category above BRUTE_FORCE_MAX, so filtered searches use the graph (post-filtering) too.
    let n = 10_000;
    assert!(n / 2 > ferrix::db::search::ann::BRUTE_FORCE_MAX);
    for chunk in 0..5 {
        let items: Vec<(String, Vec<f32>)> =
            (0..n / 5).map(|i| (format!("c{}", (chunk + i) % 2), random_vector(&mut rng))).collect();
        t.mutation::<ann::add_many>((items,)).await.unwrap();
    }
    let index = ann_index(&t).await;
    // The background thread catches up; a search would too.
    let queries: Vec<Vec<f32>> = (0..60).map(|_| random_vector(&mut rng)).collect();
    for category in [None, Some("c1")] {
        let mut hits = 0;
        for q in &queries {
            let truth: HashSet<_> = nearest(&t, q, 10, category, true).await.into_iter().map(|h| h.id).collect();
            assert_eq!(truth.len(), 10);
            let got = nearest(&t, q, 10, category, false).await;
            assert_eq!(got.len(), 10);
            assert!(got.windows(2).all(|w| w[0].score >= w[1].score));
            hits += got.iter().filter(|h| truth.contains(&h.id)).count();
        }
        let recall = hits as f64 / (queries.len() * 10) as f64;
        println!("HNSW recall@10 ({n} x {}, filter {category:?}): {recall:.3}", ann::DIMS);
        assert!(recall >= 0.9, "recall {recall} with filter {category:?}");
    }
    assert_eq!(index.status().live, n);
    assert!(t.app().db().verify_indexes().await.unwrap().is_empty());
}

/// The graph follows commits: a search sees every commit before it (inserts, vector and filter changes, deletes),
/// never a rolled-back write; tombstones are dropped by a rebuild.
#[tokio::test]
async fn hnsw_follows_commits_tombstones_and_rebuilds() {
    let t = TestApp::new().await.unwrap();
    let mut rng = StdRng::seed_from_u64(5);
    // Enough that the survivors of the deletes below are still above BRUTE_FORCE_MAX (searched in the graph).
    let items: Vec<(String, Vec<f32>)> = (0..8000).map(|i| (format!("k{}", i % 4), random_vector(&mut rng))).collect();
    let ids = t.mutation::<ann::add_many>((items.clone(),)).await.unwrap();
    let index = ann_index(&t).await;

    // Right after a commit, the search finds the new document (it catches up to its own snapshot).
    let q = random_vector(&mut rng);
    let new = t.mutation::<ann::add_many>((vec![("k9".to_string(), q.clone())],)).await.unwrap()[0];
    let hit = nearest(&t, &q, 1, None, false).await;
    assert_eq!(hit[0].id, new);
    assert!((hit[0].score - 1.0).abs() < 1e-4);
    assert_eq!(nearest(&t, &q, 3, Some("k9"), false).await.iter().map(|h| h.id).collect::<Vec<_>>(), vec![new]);
    // A rolled-back insert never shows up.
    let ghost = random_vector(&mut rng);
    t.mutation::<ann::add_then_fail>((ghost.clone(),)).await.unwrap_err();
    assert!(nearest(&t, &ghost, 5, Some("ghost"), false).await.is_empty());
    // Moving it to another category, changing its vector, removing the vector.
    t.mutation::<ann::set_category>((new, "k8".to_string())).await.unwrap();
    assert!(nearest(&t, &q, 3, Some("k9"), false).await.is_empty());
    assert_eq!(nearest(&t, &q, 1, Some("k8"), false).await[0].id, new);
    let w = random_vector(&mut rng);
    t.mutation::<ann::set_vec>((new, Some(w.clone()))).await.unwrap();
    assert_eq!(nearest(&t, &w, 1, None, false).await[0].id, new);
    assert_ne!(nearest(&t, &q, 1, None, false).await[0].id, new);
    t.mutation::<ann::set_vec>((new, None)).await.unwrap();
    assert!(nearest(&t, &w, 10, None, false).await.iter().all(|h| h.id != new));
    assert!(t.app().db().verify_indexes().await.unwrap().is_empty());

    // Delete 40%: tombstones are never returned, and searches stay exact for the survivors' top hit.
    let doomed: Vec<_> = ids.iter().copied().filter(|_| rng.random_bool(0.4)).collect();
    t.mutation::<ann::remove_many>((doomed.clone(),)).await.unwrap();
    let doomed: HashSet<_> = doomed.into_iter().collect();
    for (i, (_, v)) in items.iter().enumerate().take(40) {
        let got = nearest(&t, v, 5, None, false).await;
        assert!(got.iter().all(|h| !doomed.contains(&h.id)));
        if !doomed.contains(&ids[i]) {
            assert_eq!(got[0].id, ids[i]);
        }
    }
    // The background thread rebuilds once tombstones pass 25% (as here); also ask for it, then wait for either.
    index.rebuild();
    let start = std::time::Instant::now();
    while index.status().tombstones > 0 {
        assert!(start.elapsed() < Duration::from_secs(60), "no rebuild: {:?}", index.status());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let after = index.status();
    assert_eq!(after.live, ids.len() - doomed.len());
    assert!(t.app().db().verify_indexes().await.unwrap().is_empty());
    for (i, (_, v)) in items.iter().enumerate().take(40) {
        if !doomed.contains(&ids[i]) {
            assert_eq!(nearest(&t, v, 1, None, false).await[0].id, ids[i]);
        }
    }
}

/// Saved graphs are reloaded at startup and validated against the table: changes made after the save are applied,
/// and a corrupt file is ignored (rebuilt).
#[tokio::test]
async fn hnsw_persists_and_validates_at_startup() {
    ferrix::db::search::ann::set_persist_threshold(1);
    let t = TestApp::new().await.unwrap();
    let mut rng = StdRng::seed_from_u64(9);
    let items: Vec<(String, Vec<f32>)> = (0..1500).map(|i| (format!("p{}", i % 3), random_vector(&mut rng))).collect();
    let ids = t.mutation::<ann::add_many>((items.clone(),)).await.unwrap();
    let index = ann_index(&t).await;
    index.save().unwrap();
    let file = t.dir().join(format!("vector_index_{}.hnsw", index.id));
    assert!(file.exists());

    // Changes after the save: the file is stale.
    let moved = ids[0];
    t.mutation::<ann::set_category>((moved, "p9".to_string())).await.unwrap();
    t.mutation::<ann::remove_many>((vec![ids[1]],)).await.unwrap();
    let fresh = random_vector(&mut rng);
    let added = t.mutation::<ann::add_many>((vec![("p0".to_string(), fresh.clone())],)).await.unwrap()[0];
    drop(index);

    let t = t.restart().await.unwrap();
    let index = ann_index(&t).await;
    assert_eq!(index.status().live, ids.len());
    assert!(t.app().db().verify_indexes().await.unwrap().is_empty());
    assert_eq!(nearest(&t, &items[0].1, 1, Some("p9"), false).await[0].id, moved);
    assert!(nearest(&t, &items[1].1, 5, None, false).await.iter().all(|h| h.id != ids[1]));
    assert_eq!(nearest(&t, &fresh, 1, None, false).await[0].id, added);
    assert_eq!(nearest(&t, &items[2].1, 1, None, false).await[0].id, ids[2]);

    // A corrupt file is ignored: the graph is rebuilt from the table.
    index.save().unwrap();
    drop(index);
    let mut bytes = std::fs::read(&file).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    std::fs::write(&file, bytes).unwrap();
    let t = t.restart().await.unwrap();
    let index = ann_index(&t).await;
    assert_eq!(index.status().live, ids.len());
    assert_eq!(index.status().tombstones, 0, "rebuilt from scratch");
    assert!(t.app().db().verify_indexes().await.unwrap().is_empty());
    assert_eq!(nearest(&t, &items[2].1, 1, None, false).await[0].id, ids[2]);
}

/// Switching an index between exact and HNSW keeps its vec0 table (no rebuild of the stored vectors).
#[tokio::test]
async fn switching_vector_index_kind_keeps_the_vectors() {
    let dir = tempfile::tempdir().unwrap();
    {
        let app = App::open(dir.path(), AppOptions::default()).await.unwrap();
        let mut rng = StdRng::seed_from_u64(3);
        let items: Vec<_> = (0..20).map(|_| ("x".to_string(), random_vector(&mut rng))).collect();
        app.mutation::<ann::add_many>((items,)).await.unwrap();
    }
    let def = ann::AnnItem::table_def();
    let exact = TableDef {
        vector_indexes: vec![ferrix::VectorIndexDef {
            kind: ferrix::VectorIndexKind::Exact,
            ..def.vector_indexes[0].clone()
        }],
        ..def.clone()
    };
    let count = || {
        let conn = rusqlite::Connection::open(dir.path().join("data.db")).unwrap();
        let rowids: i64 = conn
            .query_row("SELECT COUNT(*) FROM sqlite_master WHERE name LIKE '\\_vector\\_%' ESCAPE '\\'", [], |r| {
                r.get(0)
            })
            .unwrap();
        rowids
    };
    let tables_before = count();
    let db = Database::open(dir.path(), std::slice::from_ref(&exact), 1).unwrap();
    assert!(db.ann().index(db.catalog().table("ann_items").unwrap().search_indexes[0].id).is_none());
    assert_eq!(count(), tables_before);
    assert!(db.verify_indexes().await.unwrap().is_empty());
    drop(db);
    let app = App::open(dir.path(), AppOptions::default()).await.unwrap();
    assert!(app.db().verify_indexes().await.unwrap().is_empty());
}
