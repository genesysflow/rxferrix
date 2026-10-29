# Search

Use a full-text index to search words in documents, or a vector index to find nearby embeddings. Both are maintained with document writes. You supply embeddings; the framework does not call an embedding provider for you.

## Add full-text search

```rust
use rxferrix::prelude::*;

#[table(name = "messages")]
#[search_index(search_body, search = body, filter = [channel], tokenizer = "porter")]
pub struct Message {
    pub channel: String,
    pub body: String,
}

#[query]
pub async fn search(ctx: &QueryCtx, channel: String, text: String)
    -> Result<Vec<Doc<Message>>>
{
    ctx.db.query::<Message>()
        .with_search_index(Message::search_body, |q| {
            q.search(Message::body, text).eq(Message::channel, channel)
        })
        .take(20)
        .await
}
```

This returns the most relevant matches in one channel. In a private app, check that the caller may read that channel before searching it.

Search text is treated as literal words, not FTS5 query syntax. All words must match; the last word is a prefix, so `hello wor` can match `hello world`. Empty text matches nothing. A `take` search returns at most 1024 documents.

Results use a per-document BM25 score without corpus statistics. A subscribed search filtered to one channel is invalidated by changes in that channel, rather than every message write.

## Choose a tokenizer

| Setting | Use |
| --- | --- |
| Default `unicode61` | Word matching with case and diacritic folding. |
| `tokenizer = "porter"` | English stemming, such as `run` matching `running`. |
| `tokenizer = "snowball", language = "romanian"` | Language-specific stemming; 18 languages are supported. |
| `tokenizer = "trigram"` | Substring search using three-character tokens. |
| `diacritics = "keep"` | Preserve accents instead of folding them. |

Changing a tokenizer or its language rebuilds the index at startup. Declare only the indexes your app uses, since each adds write and storage cost.

## Paginate search results

Replace `.take(20)` with `.paginate(pagination_opts)` and return `PaginationResult<Doc<Message>>`. The client can then use `usePaginatedQuery`, as for ordinary index queries. Loaded pages stay live, and their boundaries use stable search positions.

See [Functions and data](/guide/functions#paginate-a-growing-list) for the function argument and client convention.

## Add a vector index

Extend the table with an optional vector and an index whose dimensions match your embedding model:

```rust
#[table(name = "messages")]
#[vector_index(by_embedding, field = embedding, dims = 1536, filter = [channel])]
pub struct Message {
    pub channel: String,
    pub body: String,
    pub embedding: Option<Vec<f32>>,
}
```

Use `ctx.db.vector_search` inside a query when the vector comes from the database or client:

```rust
#[query]
pub async fn similar(ctx: &QueryCtx, channel: String, embedding: Vec<f32>)
    -> Result<Vec<Id<Message>>>
{
    let opts = VectorSearchOpts::new(10)
        .filter(VectorFilter::eq(Message::channel, channel));
    let hits = ctx.db.vector_search(Message::by_embedding, &embedding, opts).await?;
    Ok(hits.into_iter().map(|(id, _score)| id).collect())
}
```

This is an exact scan at the query's snapshot, so it is deterministic and reactive. Returned pairs contain an ID and cosine similarity; read the documents if the client needs their fields. Add access checks before exposing private results.

## Use approximate search for larger indexes

Add `kind = "hnsw"` to the vector index and call `ctx.vector_search(...)` from an action to use its approximate graph. This is useful when generating an embedding through an external API or searching a large collection. Action results are not subscriptions.

| API | Search behavior |
| --- | --- |
| `ctx.db.vector_search` in a query or mutation | Exact at that transaction's snapshot, including on an HNSW index. |
| `ctx.vector_search` in an action | Uses the configured index, including an HNSW graph when enabled. |

`VectorSearchOpts::exact()` bypasses the graph. Index parameters `m` and `ef_construction` affect graph construction; `ef_search` and per-call `.ef(n)` affect search effort. Increasing effort trades latency for recall. Filters use oversampling, with exact fallback for small matching sets.

For supported languages, index limits, and HNSW persistence, see the [search reference](/CONTRACT#_1-8-search-full-text-fts5-and-vector-sqlite-vec). The todo and chat examples both include text search.
