//! The sessions that access tokens were recently checked against, so a call with a native access token doesn't read
//! its `_sessions` row (a snapshot of its own, before the call's) every time.
//!
//! **Staying exact.** A cached session is only trusted while no commit since it was read wrote its document. Each
//! check first catches up with the write log: every commit after the cache's cursor drops the sessions it wrote (their
//! `by_id` key, which a write log entry names). The writer appends a commit to the log before the mutation that made
//! it returns, so a sign-out, a revocation or a user deletion that has returned is seen by the next check, exactly as
//! with the read it replaces. Commits that wrote a cached session's document (a refresh rotating it, too) drop it, and
//! the next call reads it again. If the log no longer has the commits since the cursor (trimmed while nothing
//! checked), the whole cache is dropped. Expiry is checked on every hit.
//!
//! An entry records the log's latest ts from *before* its read: commits up to it are in the snapshot the session was
//! read from, and later ones are checked against the entry (at worst dropping it for nothing).

use std::collections::HashMap;
use std::sync::RwLock;

use super::store::Principal;
use crate::db::catalog::IndexId;
use crate::writelog::WriteLog;

/// The most sessions kept; past it the cache starts over.
const MAX_SESSIONS: usize = 100_000;

#[derive(Default)]
pub(crate) struct SessionCache {
    inner: RwLock<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Every commit up to this ts has been applied.
    cursor: u64,
    /// Session id → who it belongs to and when it expires (ms).
    sessions: HashMap<[u8; 16], (Principal, f64)>,
}

impl SessionCache {
    /// Whether `session` is known to exist, belong to `principal` and be unexpired at `now`, as of the latest commit.
    /// `false` means unknown: read it.
    pub(crate) fn is_active(
        &self,
        log: &WriteLog,
        by_id: IndexId,
        session: &[u8; 16],
        principal: Principal,
        now: f64,
    ) -> bool {
        let valid = |inner: &Inner| inner.sessions.get(session).is_some_and(|&(p, exp)| p == principal && exp > now);
        let latest = log.latest();
        {
            let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
            if inner.cursor == latest {
                return valid(&inner);
            }
        }
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        // Read again: another check may have moved the cursor past `latest` while this one waited for the lock.
        inner.catch_up(log, by_id, log.latest());
        valid(&inner)
    }

    /// Remember that `session` was read active (owned by `principal`, expiring at `expires_at`) from a snapshot that
    /// had every commit up to `read_from`.
    pub(crate) fn insert(
        &self,
        log: &WriteLog,
        by_id: IndexId,
        session: [u8; 16],
        principal: Principal,
        expires_at: f64,
        read_from: u64,
    ) {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        inner.catch_up(log, by_id, log.latest());
        // Commits the cursor passed while the session was being read were applied without it: check them now.
        if read_from < inner.cursor {
            match log.entries_between(read_from, inner.cursor) {
                Some(entries)
                    if !entries.iter().any(|e| e.writes.iter().any(|(i, k)| *i == by_id && k[..] == session[..])) => {}
                _ => return,
            }
        }
        if inner.sessions.len() >= MAX_SESSIONS {
            inner.sessions.clear();
        }
        inner.sessions.insert(session, (principal, expires_at));
    }
}

impl Inner {
    /// Apply the commits after the cursor, up to `latest` (nothing if the cursor is there already).
    fn catch_up(&mut self, log: &WriteLog, by_id: IndexId, latest: u64) {
        if latest <= self.cursor {
            return;
        }
        match log.entries_between(self.cursor, latest) {
            Some(entries) if !self.sessions.is_empty() => {
                for entry in entries {
                    for (index, key) in &entry.writes {
                        if *index == by_id
                            && let Ok(id) = <[u8; 16]>::try_from(&key[..])
                        {
                            self.sessions.remove(&id);
                        }
                    }
                }
            }
            Some(_) => {}
            // Trimmed: anything may have changed.
            None => self.sessions.clear(),
        }
        self.cursor = latest;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::tables::User;
    use crate::id::{DocId, Id};
    use crate::writelog::CommitEntry;

    const BY_ID: IndexId = 7;

    fn principal() -> Principal {
        Principal::User(Id::<User>::from_doc_id(DocId([9; 16])))
    }

    fn write(log: &WriteLog, session: [u8; 16]) {
        log.append(CommitEntry { ts: log.latest() + 1, writes: vec![(BY_ID, session.to_vec())] });
    }

    #[test]
    fn a_write_to_the_session_drops_it() {
        let (log, cache, s, other) = (WriteLog::new(0), SessionCache::default(), [1; 16], [2; 16]);
        cache.insert(&log, BY_ID, s, principal(), 1e15, log.latest());
        assert!(cache.is_active(&log, BY_ID, &s, principal(), 1.0));
        write(&log, other);
        assert!(cache.is_active(&log, BY_ID, &s, principal(), 1.0), "other sessions' writes leave it");
        log.append(CommitEntry { ts: log.latest() + 1, writes: vec![(BY_ID + 1, s.to_vec())] });
        assert!(cache.is_active(&log, BY_ID, &s, principal(), 1.0), "the same bytes in another index leave it");
        write(&log, s);
        assert!(!cache.is_active(&log, BY_ID, &s, principal(), 1.0));
    }

    #[test]
    fn expiry_and_owner_are_checked_on_every_hit() {
        let (log, cache, s) = (WriteLog::new(0), SessionCache::default(), [1; 16]);
        cache.insert(&log, BY_ID, s, principal(), 100.0, 0);
        assert!(cache.is_active(&log, BY_ID, &s, principal(), 99.0));
        assert!(!cache.is_active(&log, BY_ID, &s, principal(), 100.0));
        let admin = Principal::Admin(Id::from_doc_id(DocId([9; 16])));
        assert!(!cache.is_active(&log, BY_ID, &s, admin, 99.0));
    }

    #[test]
    fn a_write_while_the_session_was_read_keeps_it_out() {
        let (log, cache, s, other) = (WriteLog::new(0), SessionCache::default(), [1; 16], [2; 16]);
        let read_from = log.latest();
        // While it is being read: the session is written, and another check moves the cursor past that commit.
        write(&log, s);
        assert!(!cache.is_active(&log, BY_ID, &other, principal(), 1.0));
        cache.insert(&log, BY_ID, s, principal(), 1e15, read_from);
        assert!(!cache.is_active(&log, BY_ID, &s, principal(), 1.0));
        // Unrelated commits in between don't.
        let read_from = log.latest();
        write(&log, other);
        assert!(!cache.is_active(&log, BY_ID, &other, principal(), 1.0));
        cache.insert(&log, BY_ID, s, principal(), 1e15, read_from);
        assert!(cache.is_active(&log, BY_ID, &s, principal(), 1.0));
    }

    #[test]
    fn a_check_behind_the_cursor_keeps_the_cache() {
        let (log, cache, s) = (WriteLog::new(0), SessionCache::default(), [1; 16]);
        cache.insert(&log, BY_ID, s, principal(), 1e15, 0);
        write(&log, [2; 16]);
        assert!(cache.is_active(&log, BY_ID, &s, principal(), 1.0));
        // A check that read an older latest ts before another moved the cursor past it.
        cache.inner.write().unwrap().catch_up(&log, BY_ID, 0);
        assert_eq!(cache.inner.read().unwrap().cursor, 1, "the cursor doesn't go back");
        assert!(cache.is_active(&log, BY_ID, &s, principal(), 1.0));
    }

    #[test]
    fn a_trimmed_log_drops_everything() {
        let (log, cache, s) = (WriteLog::new(0), SessionCache::default(), [1; 16]);
        cache.insert(&log, BY_ID, s, principal(), 1e15, 0);
        // The commits after the cursor are gone: what they wrote is unknown.
        let log = WriteLog::new(5);
        write(&log, [3; 16]);
        assert!(!cache.is_active(&log, BY_ID, &s, principal(), 1.0));
    }
}
