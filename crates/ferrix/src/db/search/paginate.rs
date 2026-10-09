//! Search pagination: [`SearchQuery::paginate`](super::SearchQuery::paginate) (and the untyped
//! [`RawSearchQuery::paginate`](super::RawSearchQuery::paginate)), with the same [`PaginationOpts`] /
//! [`PaginationResult`] as index pagination, so the clients' `usePaginatedQuery` works unchanged.
//!
//! ## Ordering keys
//!
//! Matches are ordered by `(score descending, FTS rowid ascending)`. The score ([`super::score`]) depends only on the
//! document's text and the query, never on other documents, and the rowid of a document is fixed while it stays
//! indexed. So this key is a *stable ordering key*: a document's position changes only when that document is written.
//! That is what makes relevance pagination behave like index pagination.
//!
//! ## Cursors
//!
//! A cursor names a position in that order: "just past `(score, rowid)`" or "the end". It is the URL-safe base64 (no
//! padding) of
//!
//! ```text
//! [version = 2][fingerprint: 8 bytes][0][score: f64 bits, 8 bytes BE][rowid: i64, 8 bytes BE]    (after a match)
//! [version = 2][fingerprint: 8 bytes][1]                                                         (the end)
//! ```
//!
//! The fingerprint is a hash of the index id, the FTS5 match expression (i.e. the normalized search text) and the
//! `eq` filters, so a cursor handed to a search with other text or filters is rejected
//! (`paginate: cursor does not belong to this query`), as is an index-pagination cursor.
//!
//! ## Pages
//!
//! Exactly like index pagination (docs/CONTRACT.md §1.2.1):
//! - without `endCursor`, a page holds the first `numItems` matches (clamped to [`MAX_SEARCH_RESULTS`]) after
//!   `cursor` that pass the `.filter(..)` predicate, examining at most [`MAX_SEARCH_RESULTS`] matches;
//!   `continueCursor` points past the last examined match. Unlike index pagination, `isDone` is exact: one extra
//!   match is looked up, so a page that ends exactly at the last match is done (`continueCursor` = the end).
//! - with `endCursor` (pinned by the client once a next page follows), a page holds every match in
//!   `(cursor, endCursor]`, whatever `numItems` says, and `continueCursor = endCursor`.
//!
//! **Within one snapshot, the pages of a pinned chain partition the matches**: consecutive pages share a boundary
//! position, every match lies in exactly one interval `(c_k, c_k+1]`, so nothing is duplicated or skipped. The
//! client's transitions apply all of its pages at one ts, so this holds on screen too.
//!
//! **Across changes**: a written document is re-placed by its new score. A new or newly matching document appears in
//! the page whose interval contains its position (pinned pages grow, the last page grows up to `numItems`); a deleted
//! or no longer matching one disappears; an edited one may move to another page. No other document moves, so the
//! pages never shift as a whole.
//!
//! ## Read sets
//!
//! A page's result depends on every match in its partition (the documents with its `eq` value, or all of them), so a
//! page records the same synthetic key range as [`SearchQuery::take`](super::SearchQuery::take) (see "Reactivity" in
//! [`super`]), plus a `by_id` point per examined document. A write to a matching document therefore reruns every page
//! of the search (cheap: each page fetches only its own interval), and a write to another filter value reruns none.

use anyhow::{anyhow, bail};
use data_encoding::BASE64URL_NOPAD;

use crate::db::catalog::TableInfo;
use crate::db::pagination::{PaginationOpts, PaginationResult};
use crate::db::query::QueryDb;
use crate::db::readset::KeyRange;
use crate::db::search::text::{MAX_SEARCH_RESULTS, Rank, SearchPlan, fetch_ranked};

const CURSOR_VERSION: u8 = 2;
const KIND_AFTER: u8 = 0;
const KIND_END: u8 = 1;
const FINGERPRINT_LEN: usize = 8;

/// A position in relevance order.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Position {
    /// Just past this match.
    After(Rank),
    /// After every match.
    End,
}

fn fingerprint(plan: &SearchPlan, expression: &str) -> [u8; FINGERPRINT_LEN] {
    let mut h = blake3::Hasher::new();
    h.update(b"ferrix-search-cursor");
    h.update(&plan.index.id.to_be_bytes());
    h.update(&(expression.len() as u64).to_be_bytes());
    h.update(expression.as_bytes());
    let mut eqs = plan.eqs.clone();
    eqs.sort();
    for (col, value) in &eqs {
        h.update(&(*col as u64).to_be_bytes());
        h.update(&(value.len() as u64).to_be_bytes());
        h.update(value.as_bytes());
    }
    let mut out = [0; FINGERPRINT_LEN];
    out.copy_from_slice(&h.finalize().as_bytes()[..FINGERPRINT_LEN]);
    out
}

fn encode_cursor(fp: &[u8; FINGERPRINT_LEN], pos: Position) -> String {
    let mut bytes = Vec::with_capacity(2 + FINGERPRINT_LEN + 16);
    bytes.push(CURSOR_VERSION);
    bytes.extend_from_slice(fp);
    match pos {
        Position::After((score, rowid)) => {
            bytes.push(KIND_AFTER);
            bytes.extend_from_slice(&score.to_bits().to_be_bytes());
            bytes.extend_from_slice(&rowid.to_be_bytes());
        }
        Position::End => bytes.push(KIND_END),
    }
    BASE64URL_NOPAD.encode(&bytes)
}

fn decode_cursor(s: &str, what: &str, fp: &[u8; FINGERPRINT_LEN]) -> anyhow::Result<Position> {
    let malformed = || anyhow!("paginate: malformed {what} {s:?}: not a cursor returned by paginate()");
    let bytes = BASE64URL_NOPAD.decode(s.as_bytes()).map_err(|_| malformed())?;
    let [version, rest @ ..] = bytes.as_slice() else { return Err(malformed()) };
    let foreign = || {
        anyhow!(
            "paginate: {what} does not belong to this query: it was returned for a different search (text, filters \
             or index), or by another kind of query"
        )
    };
    if *version != CURSOR_VERSION {
        // An index-pagination cursor (version 1) is well-formed, just not for this query.
        return Err(if *version == 1 { foreign() } else { malformed() });
    }
    if rest.len() < FINGERPRINT_LEN + 1 {
        return Err(malformed());
    }
    let (got_fp, rest) = rest.split_at(FINGERPRINT_LEN);
    if got_fp != fp {
        return Err(foreign());
    }
    match rest {
        [KIND_END] => Ok(Position::End),
        [KIND_AFTER, s @ ..] if s.len() == 16 => {
            let score = f64::from_bits(u64::from_be_bytes(s[..8].try_into().expect("8 bytes")));
            let rowid = i64::from_be_bytes(s[8..].try_into().expect("8 bytes"));
            if !score.is_finite() {
                return Err(malformed());
            }
            Ok(Position::After((score, rowid)))
        }
        _ => Err(malformed()),
    }
}

/// Is `a` strictly further along than `b`?
fn is_past(a: Position, b: Position) -> bool {
    match (a, b) {
        (Position::End, Position::End) => false,
        (Position::End, _) => true,
        (_, Position::End) => false,
        (Position::After((sa, ra)), Position::After((sb, rb))) => sa < sb || (sa == sb && ra > rb),
    }
}

/// Read one page of a search (typed or untyped: `decode` turns stored JSON into a `D`).
pub(crate) async fn paginate<D>(
    db: &QueryDb,
    table: &TableInfo,
    plan: &SearchPlan,
    opts: PaginationOpts,
    decode: impl Fn(&str) -> anyhow::Result<D>,
    keep: Option<&(dyn Fn(&D) -> bool + Send + Sync + '_)>,
) -> crate::Result<PaginationResult<D>> {
    // Text without terms matches nothing: one empty, done page (and no read), whatever the cursors say.
    let expression = plan.expression.clone().unwrap_or_default();
    let fp = fingerprint(plan, &expression);
    let start = opts.cursor.as_deref().map(|c| decode_cursor(c, "cursor", &fp)).transpose()?;
    let end = opts.end_cursor.as_deref().map(|c| decode_cursor(c, "endCursor", &fp)).transpose()?;
    if let (Some(s), Some(e)) = (start, end)
        && is_past(s, e)
    {
        bail!("paginate: endCursor is before cursor");
    }
    if end.is_none() && opts.num_items == 0 {
        bail!("paginate: numItems must be at least 1");
    }
    let done = |page| PaginationResult { page, is_done: true, continue_cursor: encode_cursor(&fp, Position::End) };
    if plan.expression.is_none() || start == Some(Position::End) {
        return Ok(done(Vec::new()));
    }
    db.record(plan.index.id, plan.read_range());

    let after = match start {
        Some(Position::After(r)) => Some(r),
        _ => None,
    };
    let until = match end {
        Some(Position::After(r)) => Some(r),
        _ => None,
    };
    // Pinned: everything up to the end cursor. Otherwise the page plus one match to know whether more follow (or,
    // with a post-filter, up to the examination cap plus one).
    let num_items = opts.num_items.min(MAX_SEARCH_RESULTS);
    let limit = match (end, keep) {
        (Some(_), _) => None,
        (None, None) => Some(num_items + 1),
        (None, Some(_)) => Some(MAX_SEARCH_RESULTS + 1),
    };
    let rows = fetch_ranked(db, plan, &expression, after, until, limit)?;

    let by_id = table.by_id().id;
    let mut page = Vec::new();
    let mut last: Option<Rank> = None;
    let mut stopped = false;
    for (examined, row) in rows.iter().enumerate() {
        if end.is_none() && (page.len() == num_items || examined == MAX_SEARCH_RESULTS) {
            stopped = true;
            break;
        }
        db.record(by_id, KeyRange::point(&row.doc_id));
        last = Some(row.rank);
        let doc = decode(&row.json)?;
        if keep.is_none_or(|k| k(&doc)) {
            page.push(doc);
        }
    }
    let (is_done, continue_pos) = match (end, stopped, last) {
        (Some(end), _, _) => (end == Position::End, end),
        (None, true, Some(last)) => (false, Position::After(last)),
        _ => (true, Position::End),
    };
    Ok(PaginationResult { page, is_done, continue_cursor: encode_cursor(&fp, continue_pos) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_are_checked() {
        let fp = [7u8; 8];
        for pos in [Position::After((1.25, 3)), Position::After((0.0, i64::MAX)), Position::End] {
            let s = encode_cursor(&fp, pos);
            assert!(s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'), "{s}");
            assert_eq!(decode_cursor(&s, "cursor", &fp).unwrap(), pos);
        }
        let err = |s: &str| decode_cursor(s, "cursor", &fp).unwrap_err().to_string();
        assert!(err("not a cursor!").contains("malformed"));
        assert!(err("").contains("malformed"));
        assert!(err(&encode_cursor(&[8u8; 8], Position::End)).contains("does not belong"));
        // An index-pagination cursor (version 1).
        let mut v1 = vec![1u8];
        v1.extend_from_slice(&fp);
        v1.push(1);
        assert!(err(&BASE64URL_NOPAD.encode(&v1)).contains("does not belong"));
        let mut nan = vec![CURSOR_VERSION];
        nan.extend_from_slice(&fp);
        nan.push(KIND_AFTER);
        nan.extend_from_slice(&f64::NAN.to_bits().to_be_bytes());
        nan.extend_from_slice(&1i64.to_be_bytes());
        assert!(err(&BASE64URL_NOPAD.encode(&nan)).contains("malformed"));
    }

    #[test]
    fn order_is_score_descending_then_rowid() {
        let a = |s: f64, r: i64| Position::After((s, r));
        assert!(is_past(a(1.0, 1), a(2.0, 1)));
        assert!(is_past(a(2.0, 5), a(2.0, 4)));
        assert!(!is_past(a(2.0, 4), a(2.0, 4)));
        assert!(!is_past(a(3.0, 1), a(2.0, 9)));
        assert!(is_past(Position::End, a(0.0, 1)));
        assert!(!is_past(a(0.0, 1), Position::End));
    }
}
