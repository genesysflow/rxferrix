//! Cursor-based pagination: [`Query::paginate`](crate::Query::paginate), [`PaginationOpts`] and
//! [`PaginationResult`].
//!
//! ## Cursors
//!
//! A cursor is an opaque string naming a *position* in the key space of the query's index range, in the query's
//! scan direction: "the start" (no cursor), "just past key K", or "the end of the range". It is the URL-safe base64
//! (no padding) of:
//!
//! ```text
//! [version = 1][fingerprint: 8 bytes][kind: 0 = after key, 1 = end][key bytes (kind 0 only)]
//! ```
//!
//! The fingerprint is a hash of the index id, the order and the range bounds, so a cursor handed to a query over a
//! different index, range or order is rejected with a clear error instead of returning a wrong page.
//!
//! ## Pages and pinning
//!
//! A page `[cursor, endCursor)` holds the matching documents whose keys lie after `cursor` and up to and including
//! the key `endCursor` points past. Without `endCursor` the scan stops after `numItems` matches (or after
//! [`MAX_PAGE_SCAN`] scanned documents), and `continueCursor` points just past the last *scanned* key. With
//! `endCursor`, every match up to `endCursor` is returned, whatever `numItems` says: clients re-subscribe a page with
//! `endCursor` pinned to its `continueCursor` once a next page follows it, so page boundaries don't shift when
//! documents are inserted or deleted.
//!
//! ## Read sets
//!
//! The read set covers exactly the scanned span, so a write in a later page never invalidates an earlier one.

use anyhow::{anyhow, bail};
use data_encoding::BASE64URL_NOPAD;
use serde::{Deserialize, Serialize};

use crate::db::catalog::IndexId;
use crate::db::query::{Decode, SCAN_BATCH, ScanPlan};
use crate::db::readset::KeyRange;
use crate::db::storage;
use crate::keys;
use crate::schema::Order;

/// The most documents one page scans (matching the filter or not) when no `endCursor` is given. A page that hits it
/// is returned early, possibly with fewer than `numItems` documents, and `isDone = false`.
pub const MAX_PAGE_SCAN: usize = 10_000;

/// Which page to read. Usually received from the client as a query argument named `paginationOpts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct PaginationOpts {
    /// How many documents to return (ignored when `endCursor` is set). Must be at least 1.
    pub num_items: usize,
    /// Where the page starts: `null` for the start of the range, otherwise a previous page's `continueCursor`.
    pub cursor: Option<String>,
    /// Where the page ends. When set, the page holds every matching document between `cursor` and `endCursor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub end_cursor: Option<String>,
}

impl PaginationOpts {
    /// The first `num_items` documents.
    pub fn first(num_items: usize) -> Self {
        PaginationOpts { num_items, cursor: None, end_cursor: None }
    }

    /// The next `num_items` documents after `cursor`.
    pub fn after(cursor: impl Into<String>, num_items: usize) -> Self {
        PaginationOpts { num_items, cursor: Some(cursor.into()), end_cursor: None }
    }

    /// The same page with its end pinned to `end_cursor`.
    pub fn with_end_cursor(mut self, end_cursor: impl Into<String>) -> Self {
        self.end_cursor = Some(end_cursor.into());
        self
    }
}

/// One page of results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct PaginationResult<T> {
    /// The documents of this page, in query order.
    pub page: Vec<T>,
    /// True when there is nothing after this page (`continueCursor` is then the end of the range).
    pub is_done: bool,
    /// Where the next page starts: just past the last scanned document (or `endCursor`, when given).
    pub continue_cursor: String,
}

impl<T> PaginationResult<T> {
    /// Transforms the documents of the page, keeping the cursors.
    pub fn map<U>(self, f: impl FnMut(T) -> U) -> PaginationResult<U> {
        PaginationResult { page: self.page.into_iter().map(f).collect(), is_done: self.is_done, continue_cursor: self.continue_cursor }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Cursors
// ---------------------------------------------------------------------------------------------------------------------

const CURSOR_VERSION: u8 = 1;
const KIND_AFTER: u8 = 0;
const KIND_END: u8 = 1;
const FINGERPRINT_LEN: usize = 8;

/// A position in the scan direction of a query.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Position {
    /// Just past this key (after it when ascending, before it when descending).
    After(Vec<u8>),
    /// The end of the range.
    End,
}

/// Identifies the (index, order, range) a cursor belongs to.
fn fingerprint(index_id: IndexId, order: Order, range: &KeyRange) -> [u8; FINGERPRINT_LEN] {
    let mut h = blake3::Hasher::new();
    h.update(b"ferrix-cursor");
    h.update(&index_id.to_be_bytes());
    h.update(&[matches!(order, Order::Desc) as u8]);
    h.update(&(range.start.len() as u64).to_be_bytes());
    h.update(&range.start);
    match &range.end {
        Some(end) => {
            h.update(&[1]);
            h.update(end);
        }
        None => {
            h.update(&[0]);
        }
    }
    let mut out = [0; FINGERPRINT_LEN];
    out.copy_from_slice(&h.finalize().as_bytes()[..FINGERPRINT_LEN]);
    out
}

fn encode_cursor(fp: &[u8; FINGERPRINT_LEN], pos: &Position) -> String {
    let mut bytes = Vec::with_capacity(2 + FINGERPRINT_LEN + 32);
    bytes.push(CURSOR_VERSION);
    bytes.extend_from_slice(fp);
    match pos {
        Position::After(key) => {
            bytes.push(KIND_AFTER);
            bytes.extend_from_slice(key);
        }
        Position::End => bytes.push(KIND_END),
    }
    BASE64URL_NOPAD.encode(&bytes)
}

/// Decodes and validates a cursor for the query identified by `fp` over `range`. `what` names the argument
/// (`cursor` / `endCursor`) in errors.
fn decode_cursor(s: &str, what: &str, fp: &[u8; FINGERPRINT_LEN], range: &KeyRange) -> anyhow::Result<Position> {
    let malformed = || anyhow!("paginate: malformed {what} {s:?}: not a cursor returned by paginate()");
    let bytes = BASE64URL_NOPAD.decode(s.as_bytes()).map_err(|_| malformed())?;
    let [version, rest @ ..] = bytes.as_slice() else { return Err(malformed()) };
    if *version != CURSOR_VERSION || rest.len() < FINGERPRINT_LEN + 1 {
        return Err(malformed());
    }
    let (got_fp, rest) = rest.split_at(FINGERPRINT_LEN);
    let foreign = || {
        anyhow!(
            "paginate: {what} does not belong to this query: it was returned for a different index, range or order \
             (did the query's arguments change?)"
        )
    };
    if got_fp != fp {
        return Err(foreign());
    }
    match rest {
        [KIND_END] => Ok(Position::End),
        [KIND_AFTER, key @ ..] if !key.is_empty() => {
            if !range.contains(key) {
                return Err(foreign());
            }
            Ok(Position::After(key.to_vec()))
        }
        _ => Err(malformed()),
    }
}

/// Is `a` strictly further along than `b` in the scan direction?
fn is_past(a: &Position, b: &Position, order: Order) -> bool {
    match (a, b) {
        (Position::End, Position::End) => false,
        (Position::End, _) => true,
        (_, Position::End) => false,
        (Position::After(a), Position::After(b)) => match order {
            Order::Asc => a > b,
            Order::Desc => a < b,
        },
    }
}

/// The key range between two positions of a scan over `range` in `order` (`start = None`: the start of the range).
fn span(range: &KeyRange, order: Order, start: Option<&Position>, end: Option<&Position>) -> KeyRange {
    let empty = || KeyRange { start: range.start.clone(), end: Some(range.start.clone()) };
    match order {
        Order::Asc => {
            let lo = match start {
                None => range.start.clone(),
                Some(Position::After(k)) => keys::immediate_successor(k),
                Some(Position::End) => return empty(),
            };
            let hi = match end {
                None | Some(Position::End) => range.end.clone(),
                Some(Position::After(k)) => Some(keys::immediate_successor(k)),
            };
            KeyRange { start: lo, end: hi }
        }
        Order::Desc => {
            let hi = match start {
                None => range.end.clone(),
                Some(Position::After(k)) => Some(k.clone()),
                Some(Position::End) => return empty(),
            };
            let lo = match end {
                None | Some(Position::End) => range.start.clone(),
                Some(Position::After(k)) => k.clone(),
            };
            KeyRange { start: lo, end: hi }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// paginate
// ---------------------------------------------------------------------------------------------------------------------

/// Read one page of `plan` (typed or untyped: `decode` turns a stored row into a `D`).
pub(super) async fn paginate<D: Send>(
    plan: ScanPlan<'_>,
    opts: PaginationOpts,
    decode: Decode<D>,
    filter: Option<&(dyn Fn(&D) -> bool + Send + Sync + '_)>,
) -> crate::Result<PaginationResult<D>> {
    let ScanPlan { db, table, index, range, order } = plan;
    let fp = fingerprint(index.id, order, &range);
    let start = opts.cursor.as_deref().map(|c| decode_cursor(c, "cursor", &fp, &range)).transpose()?;
    let end = opts.end_cursor.as_deref().map(|c| decode_cursor(c, "endCursor", &fp, &range)).transpose()?;
    if let (Some(s), Some(e)) = (&start, &end)
        && is_past(s, e, order)
    {
        bail!("paginate: endCursor is before cursor");
    }
    if end.is_none() && opts.num_items == 0 {
        bail!("paginate: numItems must be at least 1");
    }
    // With an end cursor every match up to it is returned; otherwise stop after `num_items` matches.
    let limit = if end.is_some() { None } else { Some(opts.num_items) };
    let scan = span(&range, order, start.as_ref(), end.as_ref());

    let mut page = Vec::new();
    let mut last: Option<Vec<u8>> = None;
    let mut scanned = 0usize;
    let mut stopped_early = false;
    let result: anyhow::Result<()> = async {
        if scan.is_empty() {
            return Ok(());
        }
        loop {
            let mut batch = SCAN_BATCH;
            if let Some(n) = limit {
                batch = batch.min(MAX_PAGE_SCAN - scanned);
                if filter.is_none() {
                    batch = batch.min(n - page.len());
                }
            }
            let rows = db.with_conn(|c| storage::scan_index_batch(c, index.id, &scan, order, last.as_deref(), batch))?;
            let exhausted = rows.len() < batch;
            for (key, json) in rows {
                let doc = decode(&table, &json)?;
                last = Some(key);
                scanned += 1;
                if filter.is_none_or(|f| f(&doc)) {
                    page.push(doc);
                }
                if let Some(n) = limit
                    && (page.len() == n || scanned == MAX_PAGE_SCAN)
                {
                    stopped_early = true;
                    return Ok(());
                }
            }
            if exhausted {
                return Ok(());
            }
            tokio::task::yield_now().await;
        }
    }
    .await;

    // What was read: the whole span, unless the scan stopped early, in which case only up to the last scanned key.
    // Writes past it cannot change this page (they belong to the next one).
    let recorded = match (&last, stopped_early && result.is_ok()) {
        (Some(last), true) => span(&range, order, start.as_ref(), Some(&Position::After(last.clone()))),
        _ => scan,
    };
    db.record(index.id, recorded);
    result?;

    let (is_done, continue_pos) = match (end, stopped_early, last) {
        (Some(end), _, _) => (end == Position::End, end),
        (None, true, Some(last)) => (false, Position::After(last)),
        _ => (true, Position::End),
    };
    Ok(PaginationResult { page, is_done, continue_cursor: encode_cursor(&fp, &continue_pos) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range() -> KeyRange {
        KeyRange { start: vec![3], end: Some(vec![7]) }
    }

    #[test]
    fn cursor_round_trip_and_validation() {
        let fp = fingerprint(4, Order::Asc, &range());
        for pos in [Position::After(vec![3, 1]), Position::After(vec![6, 255, 0]), Position::End] {
            let s = encode_cursor(&fp, &pos);
            assert!(s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'), "{s}");
            assert_eq!(decode_cursor(&s, "cursor", &fp, &range()).unwrap(), pos);
        }
        let err = |s: &str, fp: &[u8; 8]| decode_cursor(s, "cursor", fp, &range()).unwrap_err().to_string();
        assert!(err("not a cursor!", &fp).contains("malformed cursor"));
        assert!(err("", &fp).contains("malformed"));
        assert!(err(&BASE64URL_NOPAD.encode(&[1, 2, 3]), &fp).contains("malformed"));
        let mut bad_kind = vec![CURSOR_VERSION];
        bad_kind.extend_from_slice(&fp);
        bad_kind.push(9);
        assert!(err(&BASE64URL_NOPAD.encode(&bad_kind), &fp).contains("malformed"));

        // Another index, order or range: rejected.
        let s = encode_cursor(&fp, &Position::After(vec![4]));
        for other in [
            fingerprint(5, Order::Asc, &range()),
            fingerprint(4, Order::Desc, &range()),
            fingerprint(4, Order::Asc, &KeyRange { start: vec![3], end: None }),
        ] {
            assert!(err(&s, &other).contains("does not belong to this query"));
        }
        // A key outside the range (forged): rejected.
        let s = encode_cursor(&fp, &Position::After(vec![9]));
        assert!(err(&s, &fp).contains("does not belong"));
    }

    #[test]
    fn spans() {
        let r = range();
        let after = |k: u8| Position::After(vec![k]);
        assert_eq!(span(&r, Order::Asc, None, None), r);
        assert_eq!(span(&r, Order::Asc, Some(&after(4)), Some(&after(5))), KeyRange { start: vec![4, 0], end: Some(vec![5, 0]) });
        assert_eq!(span(&r, Order::Asc, Some(&after(4)), Some(&Position::End)), KeyRange { start: vec![4, 0], end: Some(vec![7]) });
        assert_eq!(span(&r, Order::Desc, None, Some(&after(4))), KeyRange { start: vec![4], end: Some(vec![7]) });
        assert_eq!(span(&r, Order::Desc, Some(&after(5)), None), KeyRange { start: vec![3], end: Some(vec![5]) });
        assert!(span(&r, Order::Asc, Some(&Position::End), None).is_empty());
        assert!(span(&r, Order::Desc, Some(&Position::End), Some(&Position::End)).is_empty());
        assert!(is_past(&after(5), &after(4), Order::Asc));
        assert!(is_past(&after(4), &after(5), Order::Desc));
        assert!(is_past(&Position::End, &after(5), Order::Desc));
        assert!(!is_past(&after(5), &after(5), Order::Asc));
    }
}
