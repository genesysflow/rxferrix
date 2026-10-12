# Search

Full-text search uses SQLite FTS5 and vector search uses sqlite-vec, both compiled into the binary. The indexes are
updated in the mutation's transaction, so search results always agree with document reads, and search queries are
reactive like any other query (docs/CONTRACT.md §1.8).

```rust
#[table(name = "messages")]
#[search_index(search_body, search = body, filter = [channel], tokenizer = "porter")]
#[search_index(search_ro, search = body, tokenizer = "snowball", language = "romanian")]   // "orașelor" finds "orașul"
#[vector_index(by_embedding, field = embedding, dims = 1536, filter = [channel], kind = "hnsw")]
pub struct Message { pub channel: String, pub body: String, pub embedding: Option<Vec<f32>> }

#[query]   // most relevant first; the last word is a prefix ("hello wor" matches "hello world")
pub async fn search(ctx: &QueryCtx, channel: String, text: String) -> Result<Vec<Doc<Message>>> {
    ctx.db.query::<Message>()
        .with_search_index(Message::search_body, |q| q.search(Message::body, text).eq(Message::channel, channel))
        .take(20)
        .await
}

#[action]  // an embedding from an API: (id, cosine similarity) pairs, from the HNSW graph
pub async fn similar(ctx: &ActionCtx, embedding: Vec<f32>) -> Result<Vec<Id<Message>>> {
    let hits = ctx.vector_search(Message::by_embedding, &embedding, VectorSearchOpts::new(10)).await?;
    Ok(hits.into_iter().map(|(id, _score)| id).collect())
}

#[query]   // a vector the query already has: exact, deterministic and live (reruns when a nearer one is written)
pub async fn more_like(ctx: &QueryCtx, id: Id<Message>) -> Result<Vec<Id<Message>>> {
    let Some(msg) = ctx.db.get(id).await? else { return Ok(vec![]) };
    let Some(embedding) = &msg.embedding else { return Ok(vec![]) };
    let opts = VectorSearchOpts::new(5).filter(VectorFilter::eq(Message::channel, msg.channel.clone()));
    Ok(ctx.db.vector_search(Message::by_embedding, embedding, opts).await?.into_iter().map(|(id, _)| id).collect())
}
```

User text is split into words that are always matched literally (never parsed as FTS5 syntax); empty text matches
nothing; a query returns at most 1024 documents. Relevance is BM25 per document (term frequency and length, no
corpus statistics), so a document's rank depends only on itself and the query. That makes two things exact:

- **Pagination**: `.paginate(pagination_opts)` instead of `.take(n)` returns the usual `PaginationResult`, so
  `usePaginatedQuery` works as for index queries. Pages never duplicate or skip a match; when documents change, only
  those documents move.
- **Invalidation per filter value**: a search with `.eq(Message::channel, c)` reruns only after writes to messages
  whose channel is (or was) `c`; an unfiltered search reruns after any write to the index.

Tokenizers: `tokenizer = "porter"` (English stemming: "run" finds "running"), `tokenizer = "snowball", language =
"…"` (Snowball stemmers for 18 languages, e.g. `"romanian"`, `"french"`, `"german"`, `"spanish"`, `"russian"`: a
custom FTS5 tokenizer, so "chantait" finds "chanter" and "Häusern" finds "Haus"), `tokenizer = "trigram"` (substring
search: "ell" finds "hello"), default `"unicode61"`; `diacritics = "keep"` stops folding `café` into `cafe` (with
folding, a word typed without its accents still finds the accented forms). Changing the tokenizer or its language
rebuilds the index at startup.

Vector indexes are exact by default (sqlite-vec). For large sets, `kind = "hnsw"` adds an in-memory HNSW graph
(approximate, recall@10 ≈ 0.95 on embedding-like data) that follows commits through the write log, so a search sees
everything committed before it; filters are applied after the graph search with oversampling (exactly, when few
documents match), deletes become tombstones that a background rebuild drops, and graphs of 10k+ vectors are saved
next to `data.db` and validated against the table at startup. `VectorSearchOpts::exact()` bypasses the graph. Tune
it per index with `m`, `ef_construction` (changing either rebuilds the graph) and `ef_search`
(`#[vector_index(.., kind = "hnsw", m = 32, ef_construction = 256, ef_search = 64)]`), or per search with
`VectorSearchOpts::ef(n)`.

Which vector search: `ctx.db.vector_search` in a query (or mutation) is always the exact scan at the query's snapshot
(also on an HNSW index), so it is deterministic and reactive, with reruns narrowed per filter value like text search;
use it when the query vector is already in the database or comes from the client, and the index is small enough for
a scan per rerun. `ctx.vector_search` in an action is for embeddings from an API and for large indexes (the graph),
but is not reactive.

Rough numbers (release build, a laptop): 50k documents of 12 words: a one-word query takes 0.35 ms, a broad two-letter
prefix matching every document 80 ms. Vectors of 384 dimensions, k=10: exact, 18 ms at 50k and ~100 ms at 200k;
HNSW at 200k, ~0.4 ms (built in 9–25 s on 16 threads). The todo example has a search box; the dashboard's
`_system/dashboard:search_documents` backs a search box in the Data page (docs/CONTRACT.md §1.8, §4.1).
