//! Full-text search queries: `db.query::<T>().with_search_index(T::idx, |q| q.search(T::body, "hello wor"))`.
//!
//! ## Query text
//!
//! The user's text is never passed to FTS5 as query syntax. It is split into terms at every character that is not
//! alphanumeric (`char::is_alphanumeric`), and each term becomes an FTS5 string literal (`"term"`); the last one gets
//! a prefix match (`"wor" *`), for search-as-you-type. Terms are combined with AND: a document matches when it
//! contains every term (the last one as a prefix). Quotes, `*`, `^`, `:`, parentheses, `NEAR`, `AND`/`OR`/`NOT` and
//! column filters in the input are therefore either separators or plain words. Text with no terms at all (empty,
//! only punctuation) matches nothing: the query returns no documents (and records no read).
//!
//! The FTS5 tokenizer is `unicode61 remove_diacritics 2`: case-insensitive, diacritics folded (`café` = `cafe`).
//! Scripts without spaces between words (e.g. Chinese) are indexed as whole runs, so only prefixes of runs match.
//!
//! Results are ordered by bm25 relevance (best first); ties keep the order in which documents were first indexed.
//! At most [`MAX_SEARCH_RESULTS`] documents are examined per query.

use std::marker::PhantomData;
use std::sync::Arc;

use anyhow::{Context, bail};
use serde::Serialize;
use serde_json::Value;

use crate::db::catalog::TableInfo;
use crate::db::query::{QueryDb, parse_doc};
use crate::db::readset::KeyRange;
use crate::db::search::{SearchIndexInfo, SpecialIndexKind, encode_filter_value};
use crate::schema::{Doc, Field, SearchIndex, Table};

/// The most documents one search query examines (and so returns). `take(n)` with a larger `n` is clamped.
pub const MAX_SEARCH_RESULTS: usize = 1024;
/// Terms beyond this many are ignored.
pub const MAX_SEARCH_TERMS: usize = 32;

/// Split user text into terms and build an FTS5 query that only uses string literals: `"a" "b" "c" *`.
/// `None` when the text has no terms.
pub fn match_expression(text: &str) -> Option<String> {
    let terms: Vec<&str> =
        text.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).take(MAX_SEARCH_TERMS).collect();
    let (last, rest) = terms.split_last()?;
    let quote = |t: &str| format!("\"{}\"", t.replace('"', "\"\""));
    let mut out: Vec<String> = rest.iter().map(|t| quote(t)).collect();
    out.push(format!("{} *", quote(last)));
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

type Filter<'a, T> = Box<dyn Fn(&Doc<T>) -> bool + Send + Sync + 'a>;

/// A full-text search over one search index of table `T`, created by `Query::with_search_index`. Results are
/// ordered by relevance; there is no `order` or `paginate`.
pub struct SearchQuery<'a, T> {
    db: &'a QueryDb,
    index: &'static str,
    constraints: SearchFilter<T>,
    filter: Option<Filter<'a, T>>,
    /// `with_index` / `order(Desc)` was called before `with_search_index`, which can't be combined with it.
    conflict: bool,
}

impl<'a, T: Table> SearchQuery<'a, T> {
    pub(crate) fn new(
        db: &'a QueryDb,
        index: SearchIndex<T>,
        constraints: SearchFilter<T>,
        filter: Option<Filter<'a, T>>,
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

    async fn run(self, limit: usize) -> crate::Result<Vec<Doc<T>>> {
        if self.conflict {
            bail!("with_search_index cannot be combined with with_index or order: search results are ordered by relevance");
        }
        let table = self.db.catalog().table(T::NAME)?.clone();
        let plan = SearchPlan::new(&table, self.index, &self.constraints.search, &self.constraints.eqs)?;
        run_search(self.db, &table, plan, limit, |json| parse_doc::<T>(json), self.filter.as_deref()).await
    }
}

/// A resolved search: the index, the FTS5 match expression (`None` = no terms) and the encoded filter values.
pub(crate) struct SearchPlan {
    index: SearchIndexInfo,
    expression: Option<String>,
    /// `(filter column number, encoded value)`.
    eqs: Vec<(usize, String)>,
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
        let SpecialIndexKind::Search { search_field, filter_fields, .. } = &info.kind else {
            bail!("`{index}` on table `{}` is a vector index; use ctx.vector_search in an action", table.name);
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
        Ok(SearchPlan { index: info.clone(), expression: match_expression(text), eqs: encoded })
    }
}

/// Run a search: up to `limit` documents passing `keep`, most relevant first, examining at most
/// [`MAX_SEARCH_RESULTS`] matches. Records the coarse search-index read and a `by_id` point per examined document.
pub(crate) async fn run_search<D>(
    db: &QueryDb,
    table: &Arc<TableInfo>,
    plan: SearchPlan,
    limit: usize,
    decode: impl Fn(&str) -> anyhow::Result<D>,
    keep: Option<&(dyn Fn(&D) -> bool + Send + Sync + '_)>,
) -> crate::Result<Vec<D>> {
    let Some(expression) = plan.expression else { return Ok(Vec::new()) };
    if limit == 0 {
        return Ok(Vec::new());
    }
    let info = &plan.index;
    db.record(info.id, KeyRange { start: Vec::new(), end: None });
    let where_eqs: String = plan.eqs.iter().enumerate().map(|(i, (col, _))| format!(" AND s.f{col} = ?{}", i + 3)).collect();
    // Without a post-filter, only `limit` rows are needed; with one, examine up to the cap.
    let fetch = if keep.is_none() { limit.min(MAX_SEARCH_RESULTS) } else { MAX_SEARCH_RESULTS };
    let sql = format!(
        "SELECT m.doc_id, d.json FROM {fts} s
         JOIN {docs} m ON m.rowid = s.rowid
         JOIN documents d ON d.id = m.doc_id
         WHERE s.t MATCH ?1{where_eqs}
         ORDER BY s.rank, s.rowid LIMIT ?2",
        fts = info.storage_table(),
        docs = info.docs_table(),
    );
    let rows: Vec<(Vec<u8>, String)> = db.with_conn(|c| {
        let mut stmt = c.prepare_cached(&sql)?;
        let mut values: Vec<rusqlite::types::Value> = vec![expression.clone().into(), (fetch as i64).into()];
        values.extend(plan.eqs.iter().map(|(_, v)| v.clone().into()));
        let rows = stmt
            .query_map(rusqlite::params_from_iter(values), |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })?;
    let by_id = table.by_id().id;
    let mut out = Vec::new();
    for (id, json) in rows {
        db.record(by_id, KeyRange::point(&id));
        let doc = decode(&json)?;
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
    use super::match_expression;

    #[test]
    fn terms_become_string_literals() {
        assert_eq!(match_expression("hello wor").as_deref(), Some("\"hello\" \"wor\" *"));
        assert_eq!(match_expression("  Hello,   WORLD!! ").as_deref(), Some("\"Hello\" \"WORLD\" *"));
        assert_eq!(match_expression("a\"b OR c*").as_deref(), Some("\"a\" \"b\" \"OR\" \"c\" *"));
        assert_eq!(match_expression("NEAR(x y, 2)").as_deref(), Some("\"NEAR\" \"x\" \"y\" \"2\" *"));
        assert_eq!(match_expression("t:col ^start -neg").as_deref(), Some("\"t\" \"col\" \"start\" \"neg\" *"));
        assert_eq!(match_expression("café 東京").as_deref(), Some("\"café\" \"東京\" *"));
        for empty in ["", "   ", "\"", "*", "()", "-- ;"] {
            assert_eq!(match_expression(empty), None, "{empty:?}");
        }
        let many = (0..100).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
        assert_eq!(match_expression(&many).unwrap().matches('"').count(), 2 * super::MAX_SEARCH_TERMS);
    }
}
