//! Index keys end in `_creationTime, _id`: documents with equal declared fields are in creation order (both
//! directions, also when paginating), `_creationTime` can be bounded after the declared fields, and a database
//! written with the previous layout (v2: keys ended in the id alone) has its indexes rebuilt on open.

use ferrix::testing::TestApp;
use ferrix::{Doc, PaginationOpts, PaginationResult};

pub mod posts {
    use ferrix::prelude::*;

    #[table(name = "posts")]
    #[index(by_channel = [channel])]
    #[index(by_channel_kind = [channel, kind])]
    #[search_index(search_body, search = body, filter = [channel])]
    pub struct Post {
        pub channel: String,
        pub kind: String,
        pub body: String,
    }

    /// Inserts one post per body, in order, in one transaction.
    #[mutation]
    pub async fn post(ctx: &MutationCtx, channel: String, kind: String, bodies: Vec<String>) -> Result<Vec<Id<Post>>> {
        let mut ids = Vec::new();
        for body in bodies {
            ids.push(ctx.db.insert(Post { channel: channel.clone(), kind: kind.clone(), body }).await?);
        }
        Ok(ids)
    }

    /// A channel's posts (optionally of one kind, optionally created after `after`), as bodies.
    #[query]
    pub async fn bodies(
        ctx: &QueryCtx,
        channel: String,
        kind: Option<String>,
        after: Option<f64>,
        desc: bool,
    ) -> Result<Vec<String>> {
        let order = if desc { Order::Desc } else { Order::Asc };
        let q = ctx.db.query::<Post>();
        let q = match (kind, after) {
            (Some(kind), Some(t)) => q.with_index(Post::by_channel_kind, |r| {
                r.eq(Post::channel, channel).eq(Post::kind, kind).gt(Post::_creation_time, t)
            }),
            (Some(kind), None) => q.with_index(Post::by_channel_kind, |r| r.eq(Post::channel, channel).eq(Post::kind, kind)),
            (None, Some(t)) => q.with_index(Post::by_channel, |r| r.eq(Post::channel, channel).gt(Post::_creation_time, t)),
            (None, None) => q.with_index(Post::by_channel, |r| r.eq(Post::channel, channel)),
        };
        Ok(q.order(order).collect().await?.into_iter().map(|d| d.value.body).collect())
    }

    /// A range on `_creationTime` that skips a declared field: refused.
    #[query]
    pub async fn skipping(ctx: &QueryCtx, t: f64) -> Result<usize> {
        Ok(ctx.db.query::<Post>().with_index(Post::by_channel, |r| r.gt(Post::_creation_time, t)).collect().await?.len())
    }

    /// One page of a channel's posts.
    #[query]
    pub async fn page(
        ctx: &QueryCtx,
        channel: String,
        desc: bool,
        pagination_opts: PaginationOpts,
    ) -> Result<PaginationResult<Doc<Post>>> {
        let order = if desc { Order::Desc } else { Order::Asc };
        ctx.db
            .query::<Post>()
            .with_index(Post::by_channel, |r| r.eq(Post::channel, channel))
            .order(order)
            .paginate(pagination_opts)
            .await
    }

    #[query]
    pub async fn search(ctx: &QueryCtx, channel: String, text: String) -> Result<Vec<String>> {
        let hits = ctx
            .db
            .query::<Post>()
            .with_search_index(Post::search_body, |q| q.search(Post::body, text).eq(Post::channel, channel))
            .take(10)
            .await?;
        Ok(hits.into_iter().map(|d| d.value.body).collect())
    }
}

use posts::*;

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// Posts `n0..n<count>` alternating between channels `a` and `b` (one mutation each, so random ids would scramble
/// them), plus three more in `a` in one mutation; returns the bodies of `a` in creation order.
async fn seed(t: &TestApp, count: usize) -> Vec<String> {
    let mut a = Vec::new();
    for i in 0..count {
        let (channel, kind) = if i % 2 == 0 { ("a", if i % 4 == 0 { "x" } else { "y" }) } else { ("b", "x") };
        let body = format!("n{i}");
        t.mutation::<post>((channel.to_string(), kind.to_string(), vec![body.clone()])).await.unwrap();
        if channel == "a" {
            a.push(body);
        }
    }
    let batch = strings(&["batch0", "batch1", "batch2"]);
    t.mutation::<post>(("a".to_string(), "y".to_string(), batch.clone())).await.unwrap();
    a.extend(batch);
    a
}

async fn bodies_of(t: &TestApp, kind: Option<&str>, after: Option<f64>, desc: bool) -> Vec<String> {
    t.query::<bodies>(("a".to_string(), kind.map(String::from), after, desc)).await.unwrap()
}

#[tokio::test]
async fn equal_fields_are_in_creation_order_both_ways() {
    let t = TestApp::new().await.unwrap();
    let a = seed(&t, 30).await;
    assert_eq!(bodies_of(&t, None, None, false).await, a);
    let mut newest_first = a.clone();
    newest_first.reverse();
    assert_eq!(bodies_of(&t, None, None, true).await, newest_first);

    // Two declared fields: still creation order within them.
    let y: Vec<String> =
        a.iter().filter(|b| b.starts_with("batch") || b[1..].parse::<usize>().is_ok_and(|i| i % 4 == 2)).cloned().collect();
    assert_eq!(bodies_of(&t, Some("y"), None, false).await, y);
    let mut y_desc = y.clone();
    y_desc.reverse();
    assert_eq!(bodies_of(&t, Some("y"), None, true).await, y_desc);
}

#[tokio::test]
async fn creation_time_can_be_bounded_after_the_declared_fields() {
    let t = TestApp::new().await.unwrap();
    let a = seed(&t, 12).await;
    let page = t.query::<page>(("a".to_string(), false, PaginationOpts::first(100))).await.unwrap();
    assert_eq!(page.page.len(), a.len());
    // Strictly after the 4th post of `a`.
    let cut = page.page[3].creation_time;
    assert_eq!(bodies_of(&t, None, Some(cut), false).await, a[4..]);
    let mut desc = a[4..].to_vec();
    desc.reverse();
    assert_eq!(bodies_of(&t, None, Some(cut), true).await, desc);
    // With the two-field index too.
    let y_after: Vec<String> = page.page[4..].iter().filter(|d| d.kind == "y").map(|d| d.body.clone()).collect();
    assert!(!y_after.is_empty());
    assert_eq!(bodies_of(&t, Some("y"), Some(cut), false).await, y_after);

    // A bound on `_creationTime` must follow an eq on every declared field.
    let err = t.query::<skipping>((cut,)).await.unwrap_err().to_string();
    assert!(err.contains("range bound must be on `channel`"), "{err}");
}

#[tokio::test]
async fn pagination_walks_creation_order_in_both_directions() {
    let t = TestApp::new().await.unwrap();
    let a = seed(&t, 40).await;
    for desc in [false, true] {
        let mut expected = a.clone();
        if desc {
            expected.reverse();
        }
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let opts = PaginationOpts { num_items: 3, cursor: cursor.clone(), end_cursor: None };
            let p = t.query::<page>(("a".to_string(), desc, opts)).await.unwrap();
            seen.extend(p.page.iter().map(|d| d.body.clone()));
            if p.is_done {
                break;
            }
            cursor = Some(p.continue_cursor);
        }
        assert_eq!(seen, expected, "desc = {desc}");
    }

    // A newer post shows up on the first page of the newest-first walk, and not in a pinned later page.
    let first = t.query::<page>(("a".to_string(), true, PaginationOpts::first(3))).await.unwrap();
    let second_opts = PaginationOpts::after(first.continue_cursor.clone(), 3);
    let second = t.query::<page>(("a".to_string(), true, second_opts.clone())).await.unwrap();
    t.mutation::<post>(("a".to_string(), "x".to_string(), strings(&["newest"]))).await.unwrap();
    let pinned = PaginationOpts { end_cursor: Some(first.continue_cursor.clone()), ..PaginationOpts::first(3) };
    let first_again = t.query::<page>(("a".to_string(), true, pinned)).await.unwrap();
    assert_eq!(first_again.page[0].body, "newest");
    assert_eq!(first_again.page.len(), 4, "the pinned first page grew by the new post");
    let second_again = t.query::<page>(("a".to_string(), true, second_opts)).await.unwrap();
    let ids = |p: &PaginationResult<Doc<Post>>| p.page.iter().map(|d| d.id).collect::<Vec<_>>();
    assert_eq!(ids(&second_again), ids(&second));
}

#[tokio::test]
async fn indexes_written_with_the_v2_layout_are_rebuilt_on_open() {
    let t = TestApp::new().await.unwrap();
    let a = seed(&t, 20).await;
    let old_cursor = {
        let p = t.query::<page>(("a".to_string(), true, PaginationOpts::first(2))).await.unwrap();
        p.continue_cursor
    };

    // Rewrite the user indexes as v2 wrote them: the declared fields, then the id (no `_creationTime`).
    let conn = rusqlite::Connection::open(t.dir().join("data.db")).unwrap();
    let indexes: Vec<(u32, Vec<String>)> = conn
        .prepare("SELECT i.index_id, i.fields FROM _indexes i JOIN _tables t ON t.table_number = i.table_number \
             WHERE t.name = 'posts' AND i.name IN ('by_channel', 'by_channel_kind')")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, serde_json::from_str(&r.get::<_, String>(1)?).unwrap())))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(indexes.len(), 2);
    let docs: Vec<(Vec<u8>, String)> = conn
        .prepare("SELECT d.id, d.json FROM documents d JOIN _tables t ON t.table_number = d.table_number WHERE t.name = 'posts'")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for (index_id, fields) in &indexes {
        conn.execute("DELETE FROM index_entries WHERE index_id = ?1", [index_id]).unwrap();
        for (id, json) in &docs {
            let doc: serde_json::Value = serde_json::from_str(json).unwrap();
            let id16: [u8; 16] = id.as_slice().try_into().unwrap();
            let key = ferrix::keys::index_key(doc.as_object().unwrap(), fields, &id16);
            conn.execute(
                "INSERT INTO index_entries (index_id, key, doc_id) VALUES (?1, ?2, ?3)",
                rusqlite::params![index_id, key, id],
            )
            .unwrap();
        }
    }
    conn.execute("UPDATE _meta SET v = 2 WHERE k = 'key_encoding'", []).unwrap();
    drop(conn);

    let t = t.restart().await.unwrap();
    assert!(t.app().db().verify_indexes().await.unwrap().is_empty(), "the indexes were rebuilt");
    let conn = rusqlite::Connection::open(t.dir().join("data.db")).unwrap();
    let version: i64 = conn.query_row("SELECT v FROM _meta WHERE k = 'key_encoding'", [], |r| r.get(0)).unwrap();
    assert_eq!(version as u64, ferrix::keys::KEY_ENCODING_VERSION);
    assert_eq!(ferrix::keys::KEY_ENCODING_VERSION, 3);
    drop(conn);

    assert_eq!(bodies_of(&t, None, None, false).await, a);
    let mut desc = a.clone();
    desc.reverse();
    assert_eq!(bodies_of(&t, None, None, true).await, desc);
    // Search indexes (whose layout didn't change) still answer.
    assert_eq!(t.query::<search>(("a".to_string(), "batch1".to_string())).await.unwrap(), ["batch1"]);

    // A cursor of the current layout survives the rebuild (cursors of the v2 layout carry another fingerprint and
    // are rejected; see `db::pagination`'s unit tests).
    let p = t.query::<page>(("a".to_string(), true, PaginationOpts::after(old_cursor, 2))).await.unwrap();
    assert_eq!(p.page.iter().map(|d| d.body.clone()).collect::<Vec<_>>(), desc[2..4]);
}
