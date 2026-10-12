//! The invalidation index: which subscriptions read a range containing a given index key.
//!
//! One interval tree per `index_id`. Each tree is a treap ordered by `(start, end, item)` whose nodes carry the
//! largest `end` in their subtree, so a stabbing query ("which ranges contain `key`?") skips every subtree whose
//! ranges all end at or before `key`, and every right subtree whose ranges all start after it. Finding the `k`
//! ranges that contain a key takes about `O(log n + k)`; inserting and removing a range `O(log n)` (expected).
//!
//! The subscription manager inserts every range of a query's read set, tagged with the query, and asks the index
//! for each key a commit wrote. [`crate::db::readset::ReadSet::overlaps`] is the linear reference it must agree
//! with (see the property test below).

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::db::catalog::IndexId;
use crate::db::readset::KeyRange;

/// The exclusive end of a range; `Unbounded` sorts after every key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum End {
    Key(Vec<u8>),
    Unbounded,
}

impl End {
    fn from_range(range: &KeyRange) -> End {
        match &range.end {
            Some(e) => End::Key(e.clone()),
            None => End::Unbounded,
        }
    }

    /// Does the range end after `key` (so `key` is below the end)?
    fn above(&self, key: &[u8]) -> bool {
        match self {
            End::Key(e) => e.as_slice() > key,
            End::Unbounded => true,
        }
    }
}

/// One stored range, as returned by [`IntervalIndex::entries_for`] and used to remove it later.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Interval {
    pub start: Vec<u8>,
    pub end: End,
}

impl Interval {
    pub fn from_range(range: &KeyRange) -> Interval {
        Interval { start: range.start.clone(), end: End::from_range(range) }
    }

    /// The bytes its bounds hold (for memory estimates).
    pub fn size_bytes(&self) -> usize {
        self.start.len()
            + match &self.end {
                End::Key(e) => e.len(),
                End::Unbounded => 0,
            }
    }

    /// Does this range contain `key`?
    pub fn contains(&self, key: &[u8]) -> bool {
        self.start.as_slice() <= key && self.end.above(key)
    }
}

struct Node<T> {
    interval: Interval,
    item: T,
    priority: u64,
    /// The largest `end` in this subtree.
    max_end: End,
    left: Option<Box<Node<T>>>,
    right: Option<Box<Node<T>>>,
}

impl<T: Ord + Copy> Node<T> {
    fn cmp_key(&self, interval: &Interval, item: T) -> Ordering {
        (&self.interval, self.item).cmp(&(interval, item))
    }

    fn update(&mut self) {
        let mut max = &self.interval.end;
        for child in [&self.left, &self.right].into_iter().flatten() {
            if child.max_end > *max {
                max = &child.max_end;
            }
        }
        self.max_end = max.clone();
    }
}

type Link<T> = Option<Box<Node<T>>>;

/// Split `t` into the nodes ordered before `(interval, item)` and the rest.
fn split<T: Ord + Copy>(t: Link<T>, interval: &Interval, item: T) -> (Link<T>, Link<T>) {
    match t {
        None => (None, None),
        Some(mut n) => {
            if n.cmp_key(interval, item) == Ordering::Less {
                let (l, r) = split(n.right.take(), interval, item);
                n.right = l;
                n.update();
                (Some(n), r)
            } else {
                let (l, r) = split(n.left.take(), interval, item);
                n.left = r;
                n.update();
                (l, Some(n))
            }
        }
    }
}

/// Join two treaps where every node of `a` is ordered before every node of `b`.
fn merge<T: Ord + Copy>(a: Link<T>, b: Link<T>) -> Link<T> {
    match (a, b) {
        (None, b) => b,
        (a, None) => a,
        (Some(mut a), Some(mut b)) => {
            if a.priority > b.priority {
                a.right = merge(a.right.take(), Some(b));
                a.update();
                Some(a)
            } else {
                b.left = merge(Some(a), b.left.take());
                b.update();
                Some(b)
            }
        }
    }
}

fn insert<T: Ord + Copy>(t: Link<T>, mut node: Box<Node<T>>) -> Box<Node<T>> {
    match t {
        None => node,
        Some(mut n) => {
            if node.priority > n.priority {
                let (l, r) = split(Some(n), &node.interval, node.item);
                node.left = l;
                node.right = r;
                node.update();
                node
            } else {
                if node.cmp_key(&n.interval, n.item) == Ordering::Less {
                    n.left = Some(insert(n.left.take(), node));
                } else {
                    n.right = Some(insert(n.right.take(), node));
                }
                n.update();
                n
            }
        }
    }
}

/// Remove `(interval, item)`; returns the new subtree and whether it was found.
fn remove<T: Ord + Copy>(t: Link<T>, interval: &Interval, item: T) -> (Link<T>, bool) {
    match t {
        None => (None, false),
        Some(mut n) => match n.cmp_key(interval, item) {
            Ordering::Equal => (merge(n.left.take(), n.right.take()), true),
            Ordering::Greater => {
                let (l, found) = remove(n.left.take(), interval, item);
                n.left = l;
                n.update();
                (Some(n), found)
            }
            Ordering::Less => {
                let (r, found) = remove(n.right.take(), interval, item);
                n.right = r;
                n.update();
                (Some(n), found)
            }
        },
    }
}

fn stab<T: Ord + Copy>(t: Option<&Node<T>>, key: &[u8], out: &mut impl FnMut(T)) {
    let Some(n) = t else { return };
    if !n.max_end.above(key) {
        return; // every range here ends at or before `key`
    }
    stab(n.left.as_deref(), key, out);
    if n.interval.start.as_slice() <= key {
        if n.interval.end.above(key) {
            out(n.item);
        }
        // Right subtree starts at or after this node's start: only worth visiting when that can be <= key.
        stab(n.right.as_deref(), key, out);
    }
}

/// Ranges per index, each tagged with an item (a shared query id). The same `(range, item)` is stored once.
pub(crate) struct IntervalIndex<T> {
    trees: HashMap<IndexId, Box<Node<T>>>,
    len: usize,
    rng: u64,
}

impl<T: Ord + Copy> Default for IntervalIndex<T> {
    fn default() -> Self {
        IntervalIndex { trees: HashMap::new(), len: 0, rng: 0x9E37_79B9_7F4A_7C15 }
    }
}

impl<T: Ord + Copy> IntervalIndex<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored ranges.
    pub fn len(&self) -> usize {
        self.len
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn next_priority(&mut self) -> u64 {
        // splitmix64
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Store `interval` on `index_id` for `item`. The caller must not insert the same triple twice.
    pub fn insert(&mut self, index_id: IndexId, interval: Interval, item: T) {
        let priority = self.next_priority();
        let max_end = interval.end.clone();
        let node = Box::new(Node { interval, item, priority, max_end, left: None, right: None });
        let root = self.trees.remove(&index_id);
        self.trees.insert(index_id, insert(root, node));
        self.len += 1;
    }

    /// Remove a triple stored with [`insert`](Self::insert). Returns whether it was present.
    pub fn remove(&mut self, index_id: IndexId, interval: &Interval, item: T) -> bool {
        let Some(root) = self.trees.remove(&index_id) else { return false };
        let (root, found) = remove(Some(root), interval, item);
        if let Some(root) = root {
            self.trees.insert(index_id, root);
        }
        if found {
            self.len -= 1;
        }
        found
    }

    /// Call `out` with the item of every stored range on `index_id` that contains `key` (once per stored range).
    pub fn stab(&self, index_id: IndexId, key: &[u8], mut out: impl FnMut(T)) {
        if let Some(root) = self.trees.get(&index_id) {
            stab(Some(root), key, &mut out);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;
    use crate::db::readset::ReadSet;
    use crate::writelog::CommitEntry;

    fn range(start: &[u8], end: Option<&[u8]>) -> KeyRange {
        KeyRange { start: start.to_vec(), end: end.map(|e| e.to_vec()) }
    }

    #[test]
    fn basic_stab_insert_remove() {
        let mut idx = IntervalIndex::new();
        idx.insert(1, Interval::from_range(&range(b"b", Some(b"d"))), 10u64);
        idx.insert(1, Interval::from_range(&range(b"a", None)), 11);
        idx.insert(2, Interval::from_range(&range(b"b", Some(b"c"))), 12);
        let hits = |idx: &IntervalIndex<u64>, i, k: &[u8]| {
            let mut v = Vec::new();
            idx.stab(i, k, |x| v.push(x));
            v.sort();
            v
        };
        assert_eq!(hits(&idx, 1, b"b"), vec![10, 11]);
        assert_eq!(hits(&idx, 1, b"d"), vec![11], "end is exclusive");
        assert_eq!(hits(&idx, 1, b"0"), Vec::<u64>::new());
        assert_eq!(hits(&idx, 2, b"b"), vec![12]);
        assert_eq!(hits(&idx, 3, b"b"), Vec::<u64>::new());
        assert!(idx.remove(1, &Interval::from_range(&range(b"a", None)), 11));
        assert!(!idx.remove(1, &Interval::from_range(&range(b"a", None)), 11));
        assert_eq!(hits(&idx, 1, b"z"), Vec::<u64>::new());
        assert_eq!(idx.len(), 2);
        idx.remove(1, &Interval::from_range(&range(b"b", Some(b"d"))), 10);
        idx.remove(2, &Interval::from_range(&range(b"b", Some(b"c"))), 12);
        assert!(idx.is_empty());
        assert!(idx.trees.is_empty(), "empty trees are dropped");
    }

    fn arb_key() -> impl Strategy<Value = Vec<u8>> {
        prop::collection::vec(0u8..4, 0..3)
    }

    fn arb_range() -> impl Strategy<Value = KeyRange> {
        (arb_key(), prop::option::weighted(0.8, arb_key())).prop_map(|(start, end)| KeyRange { start, end })
    }

    #[derive(Debug, Clone)]
    enum Op {
        Insert(IndexId, KeyRange, u64),
        Remove(usize),
        Stab(IndexId, Vec<u8>),
    }

    fn arb_op() -> impl Strategy<Value = Op> {
        prop_oneof![
            4 => (0u32..3, arb_range(), 0u64..20).prop_map(|(i, r, x)| Op::Insert(i, r, x)),
            2 => any::<usize>().prop_map(Op::Remove),
            3 => (0u32..3, arb_key()).prop_map(|(i, k)| Op::Stab(i, k)),
        ]
    }

    proptest! {
        /// The tree returns exactly what a linear scan over the stored ranges returns, through any sequence of
        /// inserts and removes.
        #[test]
        fn interval_index_matches_linear_scan(ops in prop::collection::vec(arb_op(), 1..200)) {
            let mut idx = IntervalIndex::new();
            let mut linear: Vec<(IndexId, KeyRange, u64)> = Vec::new();
            for op in ops {
                match op {
                    Op::Insert(i, r, x) => {
                        if r.is_empty() || linear.iter().any(|(j, s, y)| *j == i && *s == r && *y == x) {
                            continue;
                        }
                        idx.insert(i, Interval::from_range(&r), x);
                        linear.push((i, r, x));
                    }
                    Op::Remove(n) => {
                        if linear.is_empty() {
                            continue;
                        }
                        let (i, r, x) = linear.swap_remove(n % linear.len());
                        prop_assert!(idx.remove(i, &Interval::from_range(&r), x));
                    }
                    Op::Stab(i, k) => {
                        let mut got = Vec::new();
                        idx.stab(i, &k, |x| got.push(x));
                        got.sort();
                        let mut want: Vec<u64> =
                            linear.iter().filter(|(j, r, _)| *j == i && r.contains(&k)).map(|(_, _, x)| *x).collect();
                        want.sort();
                        prop_assert_eq!(got, want);
                    }
                }
                prop_assert_eq!(idx.len(), linear.len());
            }
        }

        /// Invalidation through the index finds exactly the read sets that the linear `ReadSet::overlaps` finds.
        #[test]
        fn index_invalidation_matches_read_set_overlaps(
            read_sets in prop::collection::vec(prop::collection::vec((0u32..3, arb_range()), 0..6), 0..25),
            writes in prop::collection::vec((0u32..3, arb_key()), 0..8),
        ) {
            let mut idx = IntervalIndex::new();
            let mut sets = Vec::new();
            for (q, ranges) in read_sets.iter().enumerate() {
                let mut rs = ReadSet::new();
                let mut stored = BTreeSet::new();
                for (i, r) in ranges {
                    rs.record(*i, r.clone());
                    if !r.is_empty() && stored.insert((*i, Interval::from_range(r))) {
                        idx.insert(*i, Interval::from_range(r), q);
                    }
                }
                sets.push(rs);
            }
            let commit = CommitEntry { ts: 1, writes: writes.clone() };
            let want: BTreeSet<usize> = sets.iter().enumerate().filter(|(_, rs)| rs.overlaps(&commit)).map(|(q, _)| q).collect();
            let mut got = BTreeSet::new();
            for (i, key) in &writes {
                idx.stab(*i, key, |q| { got.insert(q); });
            }
            prop_assert_eq!(got, want);
        }
    }
}
