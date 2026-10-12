//! Read sets: the key ranges a query read, used to decide whether a commit can change its result.
//!
//! Every read the query engine performs is a byte range over one index (see `keys.rs`). A commit writes a set of
//! `(index_id, key)` pairs (the old AND new key of every written document, for every index of its table). If none
//! of those keys falls inside a recorded range, the query reads exactly the same rows at the new timestamp, so its
//! result is unchanged and it need not rerun.

use std::collections::BTreeMap;

use crate::db::catalog::IndexId;
use crate::writelog::CommitEntry;

/// A half-open byte range `[start, end)`; `end = None` means unbounded above.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRange {
    pub start: Vec<u8>,
    pub end: Option<Vec<u8>>,
}

impl KeyRange {
    pub fn contains(&self, key: &[u8]) -> bool {
        key >= self.start.as_slice() && self.end.as_deref().is_none_or(|end| key < end)
    }

    /// The point range containing exactly `key`.
    pub fn point(key: &[u8]) -> Self {
        KeyRange { start: key.to_vec(), end: Some(crate::keys::immediate_successor(key)) }
    }

    pub fn is_empty(&self) -> bool {
        self.end.as_deref().is_some_and(|end| end <= self.start.as_slice())
    }
}

/// The ranges read by one query execution, grouped by index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadSet {
    ranges: BTreeMap<IndexId, Vec<KeyRange>>,
    /// The execution read the time (`ctx.now()`, or a storage URL that expires): its result depends on more than the
    /// database, so it is never kept in the query cache (`sync::subscriptions`, Query cache).
    clock: bool,
}

impl ReadSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, index_id: IndexId, range: KeyRange) {
        if range.is_empty() {
            return;
        }
        self.ranges.entry(index_id).or_default().push(range);
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub fn record_clock(&mut self) {
        self.clock = true;
    }

    /// Did the execution read the time?
    pub fn reads_clock(&self) -> bool {
        self.clock
    }

    pub fn ranges(&self) -> impl Iterator<Item = (IndexId, &KeyRange)> {
        self.ranges.iter().flat_map(|(id, rs)| rs.iter().map(move |r| (*id, r)))
    }

    /// Does writing `key` in index `index_id` intersect this read set?
    pub fn contains(&self, index_id: IndexId, key: &[u8]) -> bool {
        self.ranges.get(&index_id).is_some_and(|rs| rs.iter().any(|r| r.contains(key)))
    }

    /// Does `commit` invalidate this read set?
    pub fn overlaps(&self, commit: &CommitEntry) -> bool {
        // Linear scan over this one read set. The subscription manager finds the affected queries among all of them
        // with an interval index instead (`sync::interval`), which is tested against this.
        commit.writes.iter().any(|(index_id, key)| self.contains(*index_id, key))
    }

    pub fn merge(&mut self, other: ReadSet) {
        self.clock |= other.clock;
        for (id, rs) in other.ranges {
            self.ranges.entry(id).or_default().extend(rs);
        }
    }
}
