//! External JWTs (`super::jwt`) recently verified and mapped to their users, so a call with one neither checks its
//! signature nor reads its user's account (a snapshot of its own, before the call's) every time.
//!
//! **Staying exact.** A cached token is only used while verifying it and mapping it again would give the same user:
//! - it hasn't expired (`ExternalClaims::expires_at`, the leeway included), checked on every hit;
//! - its issuer's keys are the set it was verified with (`JwtVerifier::key_generation`, checked by the caller): a
//!   refreshed JWKS, which may have dropped the key, has every token of the issuer verified again;
//! - no commit since it was read wrote its user's or its account's document. As in `super::session_cache`, each
//!   check first catches up with the write log, dropping the tokens of every user and account a commit wrote (their
//!   `by_id` key); a log that no longer has the commits since the cursor drops everything. So a deleted user or an
//!   account moved to another user is seen by the next call.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::jwt::ExternalClaims;
use super::tables::User;
use crate::db::catalog::IndexId;
use crate::id::Id;
use crate::writelog::WriteLog;

/// The most tokens kept; past it the cache starts over.
const MAX_TOKENS: usize = 100_000;

/// A verified token and its user.
#[derive(Debug)]
pub(crate) struct Verified {
    pub claims: ExternalClaims,
    /// The generation of the keys it was verified with.
    pub generation: u64,
    pub user: Id<User>,
    /// Its `_accounts` row.
    pub account: [u8; 16],
}

/// The `by_id` indexes of `_users` and `_accounts`: a write to either document of a cached token drops it.
pub(crate) type Watched = [IndexId; 2];

#[derive(Default)]
pub(crate) struct ExternalCache {
    inner: RwLock<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Every commit up to this ts has been applied.
    cursor: u64,
    /// BLAKE3 of the token → what it was verified as.
    tokens: HashMap<[u8; 32], Arc<Verified>>,
    /// User and account ids → the tokens that map to them.
    by_doc: HashMap<[u8; 16], Vec<[u8; 32]>>,
}

pub(crate) fn token_key(token: &str) -> [u8; 32] {
    *blake3::hash(token.as_bytes()).as_bytes()
}

impl ExternalCache {
    /// The token `key` ([`token_key`]) as last verified, if unexpired at `now_s` and no commit up to the latest wrote
    /// its user or account. The caller still checks its key generation.
    pub(crate) fn get(&self, log: &WriteLog, watched: Watched, key: &[u8; 32], now_s: u64) -> Option<Arc<Verified>> {
        let hit = |inner: &Inner| inner.tokens.get(key).filter(|v| v.claims.expires_at > now_s).cloned();
        let latest = log.latest();
        {
            let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
            if inner.cursor == latest {
                return hit(&inner);
            }
        }
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        inner.catch_up(log, watched, log.latest());
        hit(&inner)
    }

    /// Remember token `key` as `verified`, its user and account read from a snapshot that had every commit up to
    /// `read_from`.
    pub(crate) fn insert(&self, log: &WriteLog, watched: Watched, key: [u8; 32], verified: Verified, read_from: u64) {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        inner.catch_up(log, watched, log.latest());
        let docs = [*verified.user.as_bytes(), verified.account];
        // Commits the cursor passed while the user was being read were applied without it: check them now.
        if read_from < inner.cursor {
            match log.entries_between(read_from, inner.cursor) {
                Some(entries)
                    if !entries
                        .iter()
                        .flat_map(|e| &e.writes)
                        .any(|(i, k)| watched.contains(i) && docs.iter().any(|d| k[..] == d[..])) => {}
                _ => return,
            }
        }
        if inner.tokens.len() >= MAX_TOKENS {
            inner.tokens.clear();
            inner.by_doc.clear();
        }
        for doc in docs {
            inner.by_doc.entry(doc).or_default().push(key);
        }
        inner.tokens.insert(key, Arc::new(verified));
    }
}

impl Inner {
    /// Apply the commits after the cursor, up to `latest` (nothing if the cursor is there already).
    fn catch_up(&mut self, log: &WriteLog, watched: Watched, latest: u64) {
        if latest <= self.cursor {
            return;
        }
        match log.entries_between(self.cursor, latest) {
            Some(entries) if !self.tokens.is_empty() => {
                for (index, key) in entries.iter().flat_map(|e| &e.writes) {
                    if watched.contains(index)
                        && let Ok(doc) = <[u8; 16]>::try_from(&key[..])
                        && let Some(tokens) = self.by_doc.remove(&doc)
                    {
                        for token in tokens {
                            self.tokens.remove(&token);
                        }
                    }
                }
            }
            Some(_) => {}
            // Trimmed: anything may have changed.
            None => {
                self.tokens.clear();
                self.by_doc.clear();
            }
        }
        self.cursor = latest;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::DocId;
    use crate::writelog::CommitEntry;

    const WATCHED: Watched = [7, 8];
    const USER: [u8; 16] = [1; 16];
    const ACCOUNT: [u8; 16] = [2; 16];

    fn verified(expires_at: u64) -> Verified {
        let claims = ExternalClaims {
            issuer: "https://issuer".into(),
            subject: "s".into(),
            expires_at,
            email: None,
            email_verified: false,
            name: None,
            picture: None,
        };
        Verified { claims, generation: 3, user: Id::from_doc_id(DocId(USER)), account: ACCOUNT }
    }

    fn write(log: &WriteLog, index: IndexId, doc: [u8; 16]) {
        log.append(CommitEntry { ts: log.latest() + 1, writes: vec![(index, doc.to_vec())] });
    }

    #[test]
    fn a_write_to_the_user_or_the_account_drops_the_token() {
        for (index, doc) in [(7, USER), (8, ACCOUNT)] {
            let (log, cache, key) = (WriteLog::new(0), ExternalCache::default(), token_key("t"));
            cache.insert(&log, WATCHED, key, verified(100), log.latest());
            assert_eq!(cache.get(&log, WATCHED, &key, 1).unwrap().generation, 3);
            write(&log, index, [9; 16]);
            write(&log, 9, doc);
            assert!(cache.get(&log, WATCHED, &key, 1).is_some(), "other documents, other indexes: kept");
            write(&log, index, doc);
            assert!(cache.get(&log, WATCHED, &key, 1).is_none());
        }
    }

    #[test]
    fn expired_tokens_miss() {
        let (log, cache, key) = (WriteLog::new(0), ExternalCache::default(), token_key("t"));
        cache.insert(&log, WATCHED, key, verified(100), 0);
        assert!(cache.get(&log, WATCHED, &key, 99).is_some());
        assert!(cache.get(&log, WATCHED, &key, 100).is_none());
        assert!(cache.get(&log, WATCHED, &token_key("other"), 1).is_none());
    }

    #[test]
    fn a_write_while_the_user_was_read_keeps_it_out() {
        let (log, cache, key) = (WriteLog::new(0), ExternalCache::default(), token_key("t"));
        let read_from = log.latest();
        write(&log, 8, ACCOUNT);
        // Another check moves the cursor past that commit before the insert.
        assert!(cache.get(&log, WATCHED, &token_key("other"), 1).is_none());
        cache.insert(&log, WATCHED, key, verified(100), read_from);
        assert!(cache.get(&log, WATCHED, &key, 1).is_none());
        let read_from = log.latest();
        write(&log, 7, [9; 16]);
        cache.insert(&log, WATCHED, key, verified(100), read_from);
        assert!(cache.get(&log, WATCHED, &key, 1).is_some(), "an unrelated commit in between doesn't");
    }

    #[test]
    fn a_trimmed_log_drops_everything() {
        let (log, cache, key) = (WriteLog::new(0), ExternalCache::default(), token_key("t"));
        cache.insert(&log, WATCHED, key, verified(100), 0);
        let log = WriteLog::new(5);
        write(&log, 9, [3; 16]);
        assert!(cache.get(&log, WATCHED, &key, 1).is_none());
    }
}
