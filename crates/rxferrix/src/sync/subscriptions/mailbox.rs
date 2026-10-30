//! Updates and the per-connection mailboxes they wait in.

use super::*;

/// What a connection is sent: its query set and identity versions, the ts every one of its results is valid at, and
/// the results that changed. The worker turns each into one `Transition`.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub query_set: u64,
    pub identity: u64,
    pub ts: u64,
    /// Queries removed from the set (sent first).
    pub removed: Vec<QueryId>,
    /// New results by query id.
    pub changed: BTreeMap<QueryId, StateModification>,
}

impl Update {
    pub fn into_modifications(self) -> Vec<StateModification> {
        let mut mods: Vec<StateModification> =
            self.removed.into_iter().map(|query_id| StateModification::QueryRemoved { query_id }).collect();
        mods.extend(self.changed.into_values());
        mods
    }

    /// Fold a later update that changes no version into this one.
    pub(super) fn absorb(&mut self, later: Update) {
        debug_assert!(later.removed.is_empty() && later.ts >= self.ts);
        debug_assert_eq!((later.query_set, later.identity), (self.query_set, self.identity));
        self.ts = later.ts;
        self.changed.extend(later.changed);
    }
}

/// The ordered, coalescing queue of updates from the manager to one connection's worker. See the module docs.
#[derive(Debug)]
pub struct Mailbox {
    state: Mutex<MailboxState>,
    doorbell: mpsc::Sender<()>,
}

#[derive(Debug, Default)]
struct MailboxState {
    queue: VecDeque<Update>,
    overflowed: bool,
}

impl Mailbox {
    /// A mailbox and the doorbell that rings when something is put in it.
    pub fn new() -> (Arc<Mailbox>, mpsc::Receiver<()>) {
        let (doorbell, rx) = mpsc::channel(1);
        (Arc::new(Mailbox { state: Mutex::default(), doorbell }), rx)
    }

    /// Queue `update`. `step` says it changes a version and must stay a Transition of its own; otherwise it is merged
    /// into the last queued update. Returns false (and marks the mailbox overflowed) when the limit is reached.
    pub(super) fn push(&self, update: Update, step: bool) -> bool {
        let ok = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let len = state.queue.len();
            match state.queue.back_mut() {
                Some(last) if !step => {
                    last.absorb(update);
                    true
                }
                _ if len >= MAILBOX_LIMIT => {
                    state.overflowed = true;
                    false
                }
                _ => {
                    state.queue.push_back(update);
                    true
                }
            }
        };
        let _ = self.doorbell.try_send(());
        ok
    }

    pub(super) fn overflow(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).overflowed = true;
        let _ = self.doorbell.try_send(());
    }

    /// Take every queued update, and whether the connection was dropped for being too slow.
    pub fn take(&self) -> (Vec<Update>, bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.queue.drain(..).collect(), state.overflowed)
    }

    /// Take the oldest queued update; also whether more are queued, and whether the connection was dropped for being
    /// too slow.
    pub fn take_one(&self) -> (Option<Update>, bool, bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let update = state.queue.pop_front();
        (update, !state.queue.is_empty(), state.overflowed)
    }

    /// Number of queued updates (for tests).
    pub fn len(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
