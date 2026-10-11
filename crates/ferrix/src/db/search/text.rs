//! Full-text search queries: `db.query::<T>().with_search_index(T::idx, |q| q.search(T::body, "hello wor"))`.
//!
//! ## Query text
//!
//! The user's text is never passed to FTS5 as query syntax. It is split into terms at every character that is not
//! alphanumeric (`char::is_alphanumeric`), and each term becomes an FTS5 string literal (`"term"`). Terms are
//! combined with AND: a document matches when it contains every term. Quotes, `*`, `^`, `:`, parentheses, `NEAR`,
//! `AND`/`OR`/`NOT` and column filters in the input are therefore either separators or plain words. Text with no
//! terms at all (empty, only punctuation) matches nothing: the query returns no documents (and records no read).
//!
//! How terms match depends on the index's tokenizer (`#[search_index(.., tokenizer = ..)]`):
//! - `unicode61` (default): words, case-insensitive, diacritics folded (`café` = `cafe`) unless `diacritics =
//!   "keep"`. The **last term is a prefix** (`"wor" *`), for search-as-you-type. Scripts without spaces between words
//!   (e.g. Chinese) are indexed as whole runs, so only prefixes of runs match.
//! - `porter`: like `unicode61`, but words are reduced to their English stem first, so "run", "runs" and "running"
//!   match each other. The last term is a prefix (of the stem).
//! - `snowball`: like `porter`, for the index's `language` (Romanian, French, German, ...; see [`super::stem`]).
//! - `trigram`: substring search. Each term matches anywhere inside a word ("ell" finds "hello"); terms shorter than
//!   three characters are ignored (FTS5 can't match them), so text whose terms are all shorter matches nothing.
//!
//! Results are ordered by relevance ([`super::score`], best first); ties keep the order in which documents were first
//! indexed. At most [`MAX_SEARCH_RESULTS`] documents are examined per query (or per page, see
//! [`super::paginate`]).

use std::marker::PhantomData;
use std::sync::Arc;

use anyhow::{Context, bail};
use serde::Serialize;
use serde_json::Value;

use crate::db::catalog::TableInfo;
use crate::db::pagination::{PaginationOpts, PaginationResult};
use crate::db::query::{QueryDb, parse_doc, parse_value};
use crate::db::readset::KeyRange;
use crate::db::search::{SearchIndexInfo, SpecialIndexKind, encode_filter_value, filter_key_prefix};
use crate::keys;
use crate::schema::{Doc, Field, SearchIndex, Table, TokenizerKind};

/// The most documents one search query examines (and so returns). `take(n)` with a larger `n` is clamped.
pub const MAX_SEARCH_RESULTS: usize = 1024;
/// Terms beyond this many are ignored.
pub const MAX_SEARCH_TERMS: usize = 32;
/// Trigram indexes can only match terms of at least this many characters.
pub const MIN_TRIGRAM_TERM: usize = 3;

/// Split user text into terms and build an FTS5 query that only uses string literals: `"a" "b" "c" *` (`"a" "b"
/// "c"` for trigram indexes, without terms shorter than [`MIN_TRIGRAM_TERM`]). `None` when the text has no terms.
pub fn match_expression(text: &str, tokenizer: TokenizerKind) -> Option<String> {
    let trigram = tokenizer == TokenizerKind::Trigram;
    let terms: Vec<&str> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty() && (!trigram || t.chars().count() >= MIN_TRIGRAM_TERM))
        .take(MAX_SEARCH_TERMS)
        .collect();
    let (last, rest) = terms.split_last()?;
    let quote = |t: &str| format!("\"{}\"", t.replace('"', "\"\""));
    let mut out: Vec<String> = rest.iter().map(|t| quote(t)).collect();
    out.push(if trigram { quote(last) } else { format!("{} *", quote(last)) });
    Some(out.join(" "))
}

/// The constraints of a search query: exactly one `search` on the index's search field, and `eq` on any of its
/// filter fields. Checked when the query runs.
pub struct SearchFilter<T> {
    search: Vec<(&'static str, String)>,
    eqs: Vec<(&'static str, Result<Value, String>)>,
    _p: PhantomData<fn() -> T>,
}

impl<T> SearchFilter<T> {
    pub(crate) fn new() -> Self {
        SearchFilter { search: Vec::new(), eqs: Vec::new(), _p: PhantomData }
    }

    /// Search `field` (the index's search field) for `text`. See the module docs for how text is matched.
    pub fn search<V>(mut self, field: Field<T, V>, text: impl Into<String>) -> Self {
        self.search.push((field.name(), text.into()));
        self
    }

    /// Keep only documents whose `field` (one of the index's filter fields) equals `value`.
    pub fn eq<V: Serialize>(mut self, field: Field<T, V>, value: V) -> Self {
        self.eqs.push((field.name(), serde_json::to_value(value).map_err(|e| e.to_string())));
        self
    }
}

type Filter<'a, D> = Box<dyn Fn(&D) -> bool + Send + Sync + 'a>;

/// A full-text search over one search index of table `T`, created by `Query::with_search_index`. Results are
/// ordered by relevance; there is no `order`.
pub struct SearchQuery<'a, T> {
    db: &'a QueryDb,
    index: &'static str,
    constraints: SearchFilter<T>,
    filter: Option<Filter<'a, Doc<T>>>,
    /// `with_index` / `order(Desc)` was called before `with_search_index`, which can't be combined with it.
    conflict: bool,
}

impl<'a, T: Table> SearchQuery<'a, T> {
    pub(crate) fn new(
        db: &'a QueryDb,
        index: SearchIndex<T>,
        constraints: SearchFilter<T>,
        filter: Option<Filter<'a, Doc<T>>>,
        conflict: bool,
    ) -> Self {
        SearchQuery { db, index: index.name(), constraints, filter, conflict }
    }

    /// Keep only documents matching `pred`. Rejected documents still count as read (and toward the
    /// [`MAX_SEARCH_RESULTS`] examined).
    pub fn filter(mut self, pred: impl Fn(&Doc<T>) -> bool + Send + Sync + 'a) -> Self {
        self.filter = Some(Box::new(pred));
        self
    }

    /// The `n` most relevant matches (`n` is clamped to [`MAX_SEARCH_RESULTS`]).
    pub async fn take(self, n: usize) -> crate::Result<Vec<Doc<T>>> {
        self.run(n.min(MAX_SEARCH_RESULTS)).await
    }

    /// The most relevant match.
    pub async fn first(self) -> crate::Result<Option<Doc<T>>> {
        Ok(self.run(1).await?.into_iter().next())
    }

    /// Every match, most relevant first, up to [`MAX_SEARCH_RESULTS`].
    pub async fn collect(self) -> crate::Result<Vec<Doc<T>>> {
        self.run(MAX_SEARCH_RESULTS).await
    }

    /// One page of matches, most relevant first, with a cursor to continue from. Works with the clients'
    /// `usePaginatedQuery` like [`crate::Query::paginate`]; see [`super::paginate`] for the cursor and reactivity
    /// semantics.
    ///
    /// ```ignore
    /// #[query]
    /// pub async fn search(ctx: &QueryCtx, text: String, pagination_opts: PaginationOpts)
    ///     -> Result<PaginationResult<Doc<Message>>> {
    ///     ctx.db.query::<Message>()
    ///         .with_search_index(Message::search_body, |q| q.search(Message::body, text))
    ///         .paginate(pagination_opts)
    ///         .await
    /// }
    /// ```
    pub async fn paginate(self, opts: PaginationOpts) -> crate::Result<PaginationResult<Doc<T>>> {
        let (table, plan) = self.plan()?;
        super::paginate::paginate(self.db, &table, &plan, opts, |json| parse_doc::<T>(json), self.filter.as_deref())
            .await
    }

    fn plan(&self) -> anyhow::Result<(Arc<TableInfo>, SearchPlan)> {
        if self.conflict {
            bail!(
                "with_search_index cannot be combined with with_index or order: search results are ordered by relevance"
            );
        }
        let table = self.db.catalog().table(T::NAME)?.clone();
        let plan = SearchPlan::new(&table, self.index, &self.constraints.search, &self.constraints.eqs)?;
        Ok((table, plan))
    }

    async fn run(self, limit: usize) -> crate::Result<Vec<Doc<T>>> {
        let (table, plan) = self.plan()?;
        run_search(self.db, &table, &plan, limit, |json| parse_doc::<T>(json), self.filter.as_deref()).await
    }
}

/// An untyped full-text search (table, index and fields by name, documents as JSON), used by the dashboard.
/// Created by [`QueryDb::search_table`]; same semantics, read sets and cursors as [`SearchQuery`].
pub struct RawSearchQuery<'a> {
    db: &'a QueryDb,
    table: String,
    index: String,
    search: Vec<(String, String)>,
    eqs: Vec<(String, Result<Value, String>)>,
    filter: Option<Filter<'a, Value>>,
}

impl QueryDb {
    /// Untyped [`crate::Query::with_search_index`]: a search over the search index `index` of the table named
    /// `table`. Unknown tables, indexes and fields fail when the query runs.
    pub fn search_table(&self, table: &str, index: &str) -> RawSearchQuery<'_> {
        RawSearchQuery {
            db: self,
            table: table.to_string(),
            index: index.to_string(),
            search: Vec::new(),
            eqs: Vec::new(),
            filter: None,
        }
    }
}

impl<'a> RawSearchQuery<'a> {
    /// Search `field` (the index's search field, by JSON name) for `text`.
    pub fn search(mut self, field: &str, text: impl Into<String>) -> Self {
        self.search.push((field.to_string(), text.into()));
        self
    }

    /// Keep only documents whose filter field `field` equals `value`.
    pub fn eq(mut self, field: &str, value: Value) -> Self {
        self.eqs.push((field.to_string(), Ok(value)));
        self
    }

    /// Keep only documents matching `pred` (applied to the ranked candidates).
    pub fn filter(mut self, pred: impl Fn(&Value) -> bool + Send + Sync + 'a) -> Self {
        self.filter = Some(Box::new(pred));
        self
    }

    pub async fn take(self, n: usize) -> crate::Result<Vec<Value>> {
        let (table, plan) = self.plan()?;
        run_search(
            self.db,
            &table,
            &plan,
            n.min(MAX_SEARCH_RESULTS),
            |json| parse_value(&table, json),
            self.filter.as_deref(),
        )
        .await
    }

    pub async fn collect(self) -> crate::Result<Vec<Value>> {
        self.take(MAX_SEARCH_RESULTS).await
    }

    /// One page of matches; see [`SearchQuery::paginate`].
    pub async fn paginate(self, opts: PaginationOpts) -> crate::Result<PaginationResult<Value>> {
        let (table, plan) = self.plan()?;
        super::paginate::paginate(
            self.db,
            &table,
            &plan,
            opts,
            |json| parse_value(&table, json),
            self.filter.as_deref(),
        )
        .await
    }

    fn plan(&self) -> anyhow::Result<(Arc<TableInfo>, SearchPlan)> {
        let table = self.db.catalog().table(&self.table)?.clone();
        let plan = SearchPlan::new(&table, &self.index, &self.search, &self.eqs)?;
        Ok((table, plan))
    }
}

/// A resolved search: the index, the FTS5 match expression (`None` = no terms) and the encoded filter values.
pub(crate) struct SearchPlan {
    pub(crate) index: SearchIndexInfo,
    pub(crate) expression: Option<String>,
    /// `(filter column number, hex-encoded value)`, in the order given.
    pub(crate) eqs: Vec<(usize, String)>,
}

impl SearchPlan {
    pub(crate) fn new(
        table: &TableInfo,
        index: &str,
        search: &[(impl AsRef<str>, String)],
        eqs: &[(impl AsRef<str>, Result<Value, String>)],
    ) -> anyhow::Result<SearchPlan> {
        let info = table
            .search_indexes
            .iter()
            .find(|i| i.name == index)
            .with_context(|| format!("search index `{index}` not found on table `{}`", table.name))?;
        let SpecialIndexKind::Search { search_field, filter_fields, tokenizer, .. } = &info.kind else {
            bail!(
                "`{index}` on table `{}` is a vector index; use ctx.db.vector_search (or ctx.vector_search in an action)",
                table.name
            );
        };
        let ctx = || format!("invalid search on index `{index}` of table `{}`", table.name);
        let [(field, text)] = search else {
            bail!("{}: call `search(field, text)` exactly once", ctx());
        };
        if field.as_ref() != search_field {
            bail!("{}: `{}` is not its search field (`{search_field}`)", ctx(), field.as_ref());
        }
        let mut encoded = Vec::new();
        for (field, value) in eqs {
            let field = field.as_ref();
            let Some(pos) = filter_fields.iter().position(|f| f == field) else {
                bail!("{}: `{field}` is not one of its filter fields {filter_fields:?}", ctx());
            };
            let value = value.as_ref().map_err(|e| anyhow::anyhow!("{}: cannot serialize value: {e}", ctx()))?;
            encoded.push((pos, encode_filter_value(Some(value))));
        }
        Ok(SearchPlan { index: info.clone(), expression: match_expression(text, tokenizer.kind), eqs: encoded })
    }

    /// The synthetic key range this search depends on: the documents with the filter value of its `eq` on the lowest
    /// filter column, or the whole index without filters (see "Reactivity" in [`super`]).
    pub(crate) fn read_range(&self) -> KeyRange {
        match self.eqs.iter().min_by_key(|(col, _)| *col) {
            None => KeyRange { start: Vec::new(), end: None },
            Some((col, hex)) => {
                let start = filter_key_prefix(*col, hex);
                let end = keys::prefix_successor(&start);
                KeyRange { start, end }
            }
        }
    }
}

/// A position in relevance order: `(score, FTS rowid)`, ordered by score descending, then rowid ascending.
pub(crate) type Rank = (f64, i64);

/// One ranked match: its position, document id and stored JSON.
pub(crate) struct RankedRow {
    pub rank: Rank,
    pub doc_id: Vec<u8>,
    pub json: String,
}

/// The matches of `plan` in relevance order, strictly after `after` and up to and including `until` (`None`: from
/// the start / to the end), at most `limit` (`None`: all). Records nothing; callers record what they read.
pub(crate) fn fetch_ranked(
    db: &QueryDb,
    plan: &SearchPlan,
    expression: &str,
    after: Option<Rank>,
    until: Option<Rank>,
    limit: Option<usize>,
) -> anyhow::Result<Vec<RankedRow>> {
    let fts = plan.index.storage_table();
    let mut values: Vec<rusqlite::types::Value> = vec![expression.to_string().into()];
    let mut param = |v: rusqlite::types::Value| {
        values.push(v);
        format!("?{}", values.len())
    };
    let mut conds = String::new();
    for (col, v) in &plan.eqs {
        conds.push_str(&format!(" AND f{col} = {}", param(v.clone().into())));
    }
    if let Some((score, rowid)) = after {
        let (s, r) = (param(score.into()), param(rowid.into()));
        conds.push_str(&format!(" AND (sc < {s} OR (sc = {s} AND rowid > {r}))"));
    }
    if let Some((score, rowid)) = until {
        let (s, r) = (param(score.into()), param(rowid.into()));
        conds.push_str(&format!(" AND (sc > {s} OR (sc = {s} AND rowid <= {r}))"));
    }
    let limit = param(limit.map_or(-1, |l| l as i64).into());
    // Rank inside FTS5 first, then join only the rows that made the cut.
    let sql = format!(
        "SELECT s.sc, s.rowid, m.doc_id, d.json FROM (
             SELECT rowid, ferrix_score({fts}) AS sc FROM {fts} WHERE {fts} MATCH ?1{conds}
             ORDER BY sc DESC, rowid LIMIT {limit}
         ) s
         JOIN {docs} m ON m.rowid = s.rowid
         JOIN documents d ON d.id = m.doc_id
         ORDER BY s.sc DESC, s.rowid",
        docs = plan.index.docs_table(),
    );
    db.with_conn(|c| {
        let mut stmt = c.prepare_cached(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(values), |r| {
                Ok(RankedRow { rank: (r.get(0)?, r.get(1)?), doc_id: r.get(2)?, json: r.get(3)? })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

/// Run a search: up to `limit` documents passing `keep`, most relevant first, examining at most
/// [`MAX_SEARCH_RESULTS`] matches. Records the search's synthetic key range and a `by_id` point per examined document.
pub(crate) async fn run_search<D>(
    db: &QueryDb,
    table: &Arc<TableInfo>,
    plan: &SearchPlan,
    limit: usize,
    decode: impl Fn(&str) -> anyhow::Result<D>,
    keep: Option<&(dyn Fn(&D) -> bool + Send + Sync + '_)>,
) -> crate::Result<Vec<D>> {
    let Some(expression) = &plan.expression else { return Ok(Vec::new()) };
    if limit == 0 {
        return Ok(Vec::new());
    }
    db.record(plan.index.id, plan.read_range());
    // Without a post-filter, only `limit` rows are needed; with one, examine up to the cap.
    let fetch = if keep.is_none() { limit.min(MAX_SEARCH_RESULTS) } else { MAX_SEARCH_RESULTS };
    let rows = fetch_ranked(db, plan, expression, None, None, Some(fetch))?;
    let by_id = table.by_id().id;
    let mut out = Vec::new();
    for row in rows {
        db.record(by_id, KeyRange::point(&row.doc_id));
        let doc = decode(&row.json)?;
        if keep.is_none_or(|k| k(&doc)) {
            out.push(doc);
            if out.len() == limit {
                break;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{TokenizerKind, match_expression};

    #[test]
    fn terms_become_string_literals() {
        let m = |t: &str| match_expression(t, TokenizerKind::Unicode61);
        assert_eq!(m("hello wor").as_deref(), Some("\"hello\" \"wor\" *"));
        assert_eq!(m("  Hello,   WORLD!! ").as_deref(), Some("\"Hello\" \"WORLD\" *"));
        assert_eq!(m("a\"b OR c*").as_deref(), Some("\"a\" \"b\" \"OR\" \"c\" *"));
        assert_eq!(m("NEAR(x y, 2)").as_deref(), Some("\"NEAR\" \"x\" \"y\" \"2\" *"));
        assert_eq!(m("t:col ^start -neg").as_deref(), Some("\"t\" \"col\" \"start\" \"neg\" *"));
        assert_eq!(m("café 東京").as_deref(), Some("\"café\" \"東京\" *"));
        for empty in ["", "   ", "\"", "*", "()", "-- ;"] {
            assert_eq!(m(empty), None, "{empty:?}");
        }
        let many = (0..100).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
        assert_eq!(m(&many).unwrap().matches('"').count(), 2 * super::MAX_SEARCH_TERMS);
        assert_eq!(match_expression("run fa", TokenizerKind::Porter).as_deref(), Some("\"run\" \"fa\" *"));
    }

    #[test]
    fn trigram_terms_are_substrings_of_at_least_three_characters() {
        let m = |t: &str| match_expression(t, TokenizerKind::Trigram);
        assert_eq!(m("ell wor").as_deref(), Some("\"ell\" \"wor\""));
        assert_eq!(m("a ab abc").as_deref(), Some("\"abc\""));
        assert_eq!(m("é東京x").as_deref(), Some("\"é東京x\""));
        assert_eq!(m("ab cd"), None);
        assert_eq!(m("\"x\"y\"z"), None);
    }
}
