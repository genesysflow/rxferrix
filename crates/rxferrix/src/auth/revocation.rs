//! Live revocation: connections that authenticated with a session (or, for external JWTs, as a provisioned user) are
//! told the moment that session or user document is deleted, instead of keeping their identity until the access
//! token expires.
//!
//! **Mechanism.** Long-lived connections (WebSockets in the sync worker, the admin SSE log stream) register the
//! document their identity hangs on, a `_sessions` id (native tokens, admin tokens, impersonation tokens) or a `_users`
//! id (external JWTs, which have no session row), in the app's [`Revocations`] registry and get a [`SessionWatch`].
//! A single watcher task per app follows the write log: for every commit that wrote the `by_id` key of a watched
//! document (a write log entry names the document id, so no extra bookkeeping is needed in the write paths), it checks
//! in a fresh snapshot whether the document still exists and fires the watches of the ones that don't. So every way a
//! session ends is covered without each code path having to remember to notify: sign-out, `revoke_session`,
//! `delete_user`, refresh-token reuse detection, a password reset ending other sessions, expiry clean-up, and any
//! future path. Rotating a refresh token rewrites the session document; the existence check then finds it and nothing
//! fires. Commits that touch no watched document cost one hash lookup per written `by_id` key.
//!
//! **Ordering.** A connection registers *before* the existence check that validates its token
//! ([`super::AuthService::authenticate_watched`]): a delete committed before that check fails the authentication, and
//! one committed after it is seen by the watcher, which only looks the registry up after the commit was appended. If
//! the watcher falls behind the write log's retention window, it re-checks every watched document.
//!
//! What a connection does when its watch fires is up to it: the sync worker sends `AuthError` ("session has ended")
//! and closes the socket, and the SSE log stream sends `event: revoked` and ends.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::watch;

use crate::id::DocId;
use crate::runtime::App;

/// Why a watch fired; shown to clients.
pub const SESSION_ENDED: &str = "session has ended";

/// The per-app registry of watched session (or user) documents.
#[derive(Default)]
pub struct Revocations {
    inner: Mutex<Inner>,
    watcher_started: AtomicBool,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    /// Document id → the watches on it.
    by_doc: HashMap<String, Vec<(u64, watch::Sender<bool>)>>,
}

/// A registration in [`Revocations`]; unregisters on drop. [`SessionWatch::ended`] resolves once the watched document
/// is deleted.
pub struct SessionWatch {
    registry: Weak<Revocations>,
    doc: String,
    id: u64,
    rx: watch::Receiver<bool>,
}

impl std::fmt::Debug for SessionWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionWatch").field("doc", &self.doc).field("ended", &*self.rx.borrow()).finish()
    }
}

impl SessionWatch {
    /// The watched document id (a `_sessions` or `_users` id).
    pub fn doc(&self) -> &str {
        &self.doc
    }

    /// Whether the document has been deleted.
    pub fn has_ended(&self) -> bool {
        *self.rx.borrow()
    }

    /// A future that resolves once the document is deleted. It owns what it needs (`'static`), so callers can build
    /// it before a `select!` without borrowing the watch. It never resolves if the app shuts down first.
    pub fn ended(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut rx = self.rx.clone();
        async move {
            if rx.wait_for(|ended| *ended).await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

impl Drop for SessionWatch {
    fn drop(&mut self) {
        let Some(registry) = self.registry.upgrade() else { return };
        let mut inner = registry.lock();
        if let Some(watches) = inner.by_doc.get_mut(&self.doc) {
            watches.retain(|(id, _)| *id != self.id);
            if watches.is_empty() {
                inner.by_doc.remove(&self.doc);
            }
        }
    }
}

impl Revocations {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Watch document `doc` (string id). Starts the app's watcher task on first use. The caller must check that the
    /// document exists *after* this returns (see the module docs).
    pub(crate) fn watch(self: &Arc<Self>, app: &App, doc: String) -> SessionWatch {
        self.ensure_watcher(app);
        let (tx, rx) = watch::channel(false);
        let mut inner = self.lock();
        inner.next_id += 1;
        let id = inner.next_id;
        inner.by_doc.entry(doc.clone()).or_default().push((id, tx));
        SessionWatch { registry: Arc::downgrade(self), doc, id, rx }
    }

    /// How many documents are watched (for tests and stats).
    pub fn watched(&self) -> usize {
        self.lock().by_doc.len()
    }

    /// Fire and drop every watch on `doc`.
    pub(crate) fn end(&self, doc: &str) -> usize {
        let watches = self.lock().by_doc.remove(doc).unwrap_or_default();
        for (_, tx) in &watches {
            tx.send_replace(true);
        }
        watches.len()
    }

    fn ensure_watcher(self: &Arc<Self>, app: &App) {
        if self.watcher_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let registry = Arc::downgrade(self);
        let weak = app.downgrade();
        let log = app.db().write_log().clone();
        tokio::spawn(async move {
            let mut rx = log.subscribe();
            let mut last = *rx.borrow_and_update();
            while rx.changed().await.is_ok() {
                let latest = *rx.borrow_and_update();
                let (Some(app), Some(registry)) = (weak.upgrade(), registry.upgrade()) else { return };
                let candidates = registry.candidates(&app, log.entries_between(last, latest));
                last = latest;
                if !candidates.is_empty() {
                    registry.check(&app, candidates).await;
                }
            }
        });
    }

    /// The watched documents that the commits in `entries` wrote (`None`: the log was trimmed; check everything).
    fn candidates(&self, app: &App, entries: Option<Vec<Arc<crate::writelog::CommitEntry>>>) -> Vec<String> {
        let inner = self.lock();
        if inner.by_doc.is_empty() {
            return Vec::new();
        }
        let Some(entries) = entries else { return inner.by_doc.keys().cloned().collect() };
        let catalog = app.db().catalog();
        let mut out = Vec::new();
        for entry in &entries {
            for (index_id, key) in &entry.writes {
                let Ok(raw) = <[u8; 16]>::try_from(key.as_slice()) else { continue };
                let doc = DocId(raw);
                let is_by_id = catalog.table_by_number(doc.table_number()).is_some_and(|t| t.by_id().id == *index_id);
                if !is_by_id {
                    continue;
                }
                let id = doc.encode();
                if inner.by_doc.contains_key(&id) && !out.contains(&id) {
                    out.push(id);
                }
            }
        }
        out
    }

    /// Fire the watches of the `docs` that no longer exist.
    async fn check(&self, app: &App, docs: Vec<String>) {
        let gone = super::store::read(app, |db| async move {
            let mut gone = Vec::new();
            for doc in docs {
                if db.get_raw(&doc).await?.is_none() {
                    gone.push(doc);
                }
            }
            Ok(gone)
        })
        .await;
        match gone {
            Ok(gone) => {
                for doc in gone {
                    let n = self.end(&doc);
                    tracing::debug!(doc, connections = n, "auth: session ended; notifying its connections");
                }
            }
            Err(e) => tracing::error!("auth: checking revoked sessions failed: {e:#}"),
        }
    }
}
