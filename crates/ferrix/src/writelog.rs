//! The in-memory write log: recent commits and the keys they wrote.
//!
//! The subscription manager uses it to decide which subscriptions a range of commits can affect. Entries are appended
//! by the writer right after `COMMIT` while it still holds the writer lock, so they arrive in ts order with no gaps. A
//! reader snapshot may observe a ts slightly before its entry is appended; [`WriteLog::wait_for`] closes that window.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::db::catalog::IndexId;

/// One committed mutation: its ts and, for every written document, the old and new key in every index of its table
/// (including `by_id`, whose key is the 16 id bytes, and `by_creation_time`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitEntry {
    pub ts: u64,
    pub writes: Vec<(IndexId, Vec<u8>)>,
}

/// Retention limits: the oldest entries go once any is passed. Readers that need trimmed entries treat everything as
/// changed (the subscription manager reruns, the HNSW indexes rescan, ...), so these trade memory for occasional extra
/// work: commits are normally consumed within milliseconds, or seconds behind a slow query.
const MAX_ENTRIES: usize = 100_000;
const MAX_AGE: Duration = Duration::from_secs(60);
/// Estimated memory of the retained entries (a bulk write can put megabytes of index keys in one entry).
pub const MAX_BYTES: usize = 32 << 20;

/// Estimated memory of an entry.
fn entry_bytes(entry: &CommitEntry) -> usize {
    128 + entry.writes.iter().map(|(_, key)| key.len() + 40).sum::<usize>()
}

struct Inner {
    entries: VecDeque<(Instant, Arc<CommitEntry>)>,
    /// [`entry_bytes`] of `entries`.
    bytes: usize,
    /// Every commit with ts > `complete_after` is in `entries` (or not yet appended). Commits at or before it may
    /// have been trimmed.
    complete_after: u64,
    latest: u64,
}

pub struct WriteLog {
    inner: Mutex<Inner>,
    latest_tx: watch::Sender<u64>,
}

impl WriteLog {
    /// A log starting at `ts` (the database ts at startup). Nothing before it is available.
    pub fn new(ts: u64) -> Self {
        let (latest_tx, _) = watch::channel(ts);
        WriteLog {
            inner: Mutex::new(Inner { entries: VecDeque::new(), bytes: 0, complete_after: ts, latest: ts }),
            latest_tx,
        }
    }

    /// Append a commit. Called by the writer, in ts order, while holding the writer lock.
    pub fn append(&self, entry: CommitEntry) {
        let ts = entry.ts;
        {
            let mut inner = self.inner.lock().unwrap();
            debug_assert!(ts > inner.latest, "write log entries must be appended in ts order");
            inner.bytes += entry_bytes(&entry);
            inner.entries.push_back((Instant::now(), Arc::new(entry)));
            inner.latest = ts;
            inner.trim();
        }
        self.latest_tx.send_replace(ts);
    }

    /// Drop entries past the retention limits. Appending does this too; this is for when commits stop (the
    /// subscription manager calls it periodically), so an idle server doesn't keep the last burst's entries.
    pub fn trim(&self) {
        self.inner.lock().unwrap().trim();
    }

    /// Estimated memory of the retained entries, and how many there are.
    pub fn retained(&self) -> (usize, usize) {
        let inner = self.inner.lock().unwrap();
        (inner.bytes, inner.entries.len())
    }

    /// The latest appended ts.
    pub fn latest(&self) -> u64 {
        *self.latest_tx.borrow()
    }

    /// How many commits were appended within the last `window` (at most the retained ones; for metrics).
    pub fn commits_within(&self, window: Duration) -> usize {
        let inner = self.inner.lock().unwrap();
        let now = Instant::now();
        inner.entries.iter().rev().take_while(|(at, _)| now.duration_since(*at) <= window).count()
    }

    /// A receiver that changes whenever a commit is appended.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.latest_tx.subscribe()
    }

    /// Wait until every commit up to `ts` has been appended.
    pub async fn wait_for(&self, ts: u64) {
        let mut rx = self.latest_tx.subscribe();
        // The sender lives as long as `self`, so this can only fail if the log is being dropped.
        let _ = rx.wait_for(|latest| *latest >= ts).await;
    }

    /// The commits with `after < ts <= up_to`, or `None` if some of them were trimmed or are not appended yet (the
    /// caller must then assume everything changed).
    pub fn entries_between(&self, after: u64, up_to: u64) -> Option<Vec<Arc<CommitEntry>>> {
        let inner = self.inner.lock().unwrap();
        if after < inner.complete_after || up_to > inner.latest {
            return None;
        }
        if up_to <= after {
            return Some(Vec::new());
        }
        // Entries are contiguous in ts, so binary search for the first one after `after`.
        let start = inner.entries.partition_point(|(_, e)| e.ts <= after);
        Some(inner.entries.iter().skip(start).take_while(|(_, e)| e.ts <= up_to).map(|(_, e)| e.clone()).collect())
    }
}

impl Inner {
    fn trim(&mut self) {
        let now = Instant::now();
        while let Some((at, front)) = self.entries.front() {
            let over = self.entries.len() > MAX_ENTRIES || self.bytes > MAX_BYTES || now.duration_since(*at) > MAX_AGE;
            if !over {
                break;
            }
            self.complete_after = front.ts;
            self.bytes -= entry_bytes(front);
            self.entries.pop_front();
        }
        // After a burst, give the queue's own memory back too.
        if self.entries.capacity() > 4 * self.entries.len() + 1024 {
            self.entries.shrink_to_fit();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn between_and_trimmed() {
        let log = WriteLog::new(10);
        assert_eq!(log.entries_between(10, 10), Some(vec![]));
        assert_eq!(log.entries_between(9, 10), None, "before the start is unknown");
        for ts in 11..=15 {
            log.append(CommitEntry { ts, writes: vec![(1, vec![ts as u8])] });
        }
        let got: Vec<u64> = log.entries_between(12, 14).unwrap().iter().map(|e| e.ts).collect();
        assert_eq!(got, vec![13, 14]);
        assert_eq!(log.entries_between(10, 15).unwrap().len(), 5);
        assert_eq!(log.entries_between(14, 16), None, "not yet appended");
        assert_eq!(log.latest(), 15);
    }

    #[test]
    fn retention_is_bounded_by_bytes() {
        let log = WriteLog::new(0);
        // 1 MB of keys per commit: the byte cap, not the entry count, trims.
        for ts in 1..=100 {
            log.append(CommitEntry { ts, writes: vec![(1, vec![0u8; 1 << 20])] });
        }
        let (bytes, entries) = log.retained();
        assert!(bytes <= MAX_BYTES && entries < 40, "{bytes} bytes in {entries} entries");
        assert_eq!(log.entries_between(0, 100), None, "trimmed entries are reported as unknown");
        assert_eq!(log.entries_between(99, 100).map(|e| e.len()), Some(1));
    }

    #[tokio::test]
    async fn wait_for_wakes() {
        let log = Arc::new(WriteLog::new(0));
        let l2 = log.clone();
        let waiter = tokio::spawn(async move { l2.wait_for(2).await });
        log.append(CommitEntry { ts: 1, writes: vec![] });
        log.append(CommitEntry { ts: 2, writes: vec![] });
        tokio::time::timeout(Duration::from_secs(1), waiter).await.unwrap().unwrap();
    }
}
