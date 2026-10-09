//! The per-connection sync worker.
//!
//! One task per WebSocket. It owns the connection's query set and decides what to push. Everything a client sees
//! comes from [`SyncWorker::refresh`]:
//!
//! 1. Open ONE read snapshot `S` (at the latest commit).
//! 2. For each query not yet known to be valid at `S.ts`, look at the commits in `(valid_at, S.ts]` from the write
//!    log. If any of them wrote a key inside the query's read set (or the log no longer covers that span), rerun the
//!    query in `S`. Reruns run concurrently, all on the same snapshot.
//! 3. Every query is now valid at `S.ts`.
//! 4. Results whose hash changed become modifications.
//!
//! Because every query is brought to the *same* ts before anything is sent, all results in a Transition are
//! consistent with each other ("no tearing"), and transitions move forward in ts.

use std::collections::BTreeMap;
use std::time::Duration;

use futures::future::join_all;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};

use crate::db::dedup::IdempotencyKey;
use crate::db::readset::ReadSet;
use crate::error::ClientError;
use crate::runtime::{App, Identity};
use crate::sync::protocol::{
    ClientMessage, QueryId, QuerySetModification, RequestId, ServerMessage, StateModification, StateVersion,
};

/// How often the server pings the client.
pub const PING_INTERVAL: Duration = Duration::from_secs(15);

/// Input to the worker from the socket reader.
#[derive(Debug)]
pub enum Inbound {
    Message(ClientMessage),
    /// A frame that could not be parsed; the worker answers with `FatalError` and stops.
    Malformed(String),
}

struct QueryState {
    path: String,
    args: Value,
    /// blake3 of the last result sent (value or error); `None` before the first run.
    hash: Option<[u8; 32]>,
    read_set: ReadSet,
    /// The result is known to be correct at every ts in `[ran_at, valid_at]`.
    valid_at: u64,
    /// Must be rerun regardless of the write log (newly added, or identity changed).
    force: bool,
}

/// A finished mutation or action. `log_lines` are already empty outside `--dev`.
enum Completion {
    Mutation { request_id: RequestId, result: Result<(Value, u64), ClientError>, log_lines: Vec<String> },
    Action { request_id: RequestId, result: Result<Value, ClientError>, log_lines: Vec<String> },
}

fn client_log_lines(app: &App, lines: Vec<String>) -> Vec<String> {
    crate::log::for_client(app.options().dev, lines)
}

/// A protocol violation that ends the connection with `FatalError`.
struct Fatal(String);

impl<E: std::fmt::Display> From<E> for Fatal {
    fn from(e: E) -> Self {
        Fatal(e.to_string())
    }
}

pub struct SyncWorker {
    app: App,
    out: mpsc::Sender<ServerMessage>,
    /// The version the client has (the end version of the last Transition sent).
    sent: StateVersion,
    identity: Identity,
    /// From `Connect`; with each mutation's requestId it makes the idempotency key.
    session_id: Option<String>,
    queries: BTreeMap<QueryId, QueryState>,
    /// Highest commit ts of a successful mutation response not yet covered by a sent Transition.
    pending_mutation_ts: Option<u64>,
    completions_tx: mpsc::UnboundedSender<Completion>,
    completions_rx: mpsc::UnboundedReceiver<Completion>,
    /// When the current access token expires; the socket then ends with `AuthError` unless the client has sent a
    /// fresh token by then.
    auth_expires_at: Option<Instant>,
    /// Set when the connection ends because of an auth failure: sent as `AuthError` instead of `FatalError`.
    auth_error: Option<String>,
}

/// Spawn a worker and return its input and output channels (used by the WebSocket handler and by tests).
pub fn spawn(app: App) -> (mpsc::Sender<Inbound>, mpsc::Receiver<ServerMessage>) {
    let (in_tx, in_rx) = mpsc::channel(64);
    let (out_tx, out_rx) = mpsc::channel(256);
    tokio::spawn(run(app, in_rx, out_tx));
    (in_tx, out_rx)
}

/// Run a worker until the input closes, the output is dropped, or a fatal error occurs.
pub async fn run(app: App, inbound: mpsc::Receiver<Inbound>, out: mpsc::Sender<ServerMessage>) {
    let (completions_tx, completions_rx) = mpsc::unbounded_channel();
    let worker = SyncWorker {
        app,
        out,
        sent: StateVersion::default(),
        identity: Identity::default(),
        session_id: None,
        queries: BTreeMap::new(),
        pending_mutation_ts: None,
        completions_tx,
        completions_rx,
        auth_expires_at: None,
        auth_error: None,
    };
    worker.run(inbound).await
}

impl SyncWorker {
    async fn run(mut self, mut inbound: mpsc::Receiver<Inbound>) {
        let mut commits = self.app.db().write_log().subscribe();
        let mut ping = tokio::time::interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL);
        ping.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let result: Result<(), Fatal> = async {
            loop {
                let auth_expiry = self.auth_expires_at;
                tokio::select! {
                    msg = inbound.recv() => match msg {
                        None => return Ok(()),
                        Some(Inbound::Malformed(e)) => return Err(Fatal(format!("malformed message: {e}"))),
                        Some(Inbound::Message(m)) => self.handle_message(m).await?,
                    },
                    Some(done) = self.completions_rx.recv() => self.handle_completion(done).await?,
                    changed = commits.changed() => {
                        if changed.is_err() {
                            return Ok(());
                        }
                        // Mark seen before refreshing: commits that land during the refresh trigger another round,
                        // so bursts of commits coalesce into few refreshes.
                        commits.borrow_and_update();
                        self.refresh_and_send().await?;
                    }
                    _ = ping.tick() => self.send(ServerMessage::Ping).await?,
                    _ = sleep_until_opt(auth_expiry) => {
                        self.auth_error = Some("access token expired".into());
                        return Err(Fatal("access token expired".into()));
                    }
                }
            }
        }
        .await;

        if let Err(Fatal(error)) = result {
            tracing::debug!("closing sync connection: {error}");
            let msg = match self.auth_error.take() {
                Some(error) => ServerMessage::AuthError { error },
                None => ServerMessage::FatalError { error },
            };
            let _ = self.out.send(msg).await;
        }
    }

    async fn send(&self, msg: ServerMessage) -> Result<(), Fatal> {
        self.out.send(msg).await.map_err(|_| Fatal("connection closed".into()))
    }

    fn client_error(&self, e: &anyhow::Error) -> ClientError {
        ClientError::from_anyhow(e, self.app.options().dev)
    }

    async fn handle_message(&mut self, msg: ClientMessage) -> Result<(), Fatal> {
        match msg {
            ClientMessage::Connect { session_id, connection_count, .. } => {
                tracing::debug!(%session_id, connection_count, "sync client connected");
                self.session_id = Some(session_id);
            }
            ClientMessage::ModifyQuerySet { base_version, new_version, modifications } => {
                if base_version != self.sent.query_set || new_version != base_version + 1 {
                    return Err(Fatal(format!(
                        "invalid ModifyQuerySet: server query set version is {}, got baseVersion {base_version} and \
                         newVersion {new_version}",
                        self.sent.query_set
                    )));
                }
                let mut removed = Vec::new();
                for m in modifications {
                    match m {
                        QuerySetModification::Add { query_id, udf_path, args } => {
                            if self.queries.contains_key(&query_id) {
                                return Err(Fatal(format!("duplicate queryId {query_id}")));
                            }
                            let state = QueryState {
                                path: udf_path,
                                args,
                                hash: None,
                                read_set: ReadSet::new(),
                                valid_at: 0,
                                force: true,
                            };
                            self.queries.insert(query_id, state);
                        }
                        QuerySetModification::Remove { query_id } => {
                            // Removing an id that isn't live is harmless (e.g. a client racing a reconnect); ignore it.
                            if self.queries.remove(&query_id).is_some() {
                                removed.push(StateModification::QueryRemoved { query_id });
                            }
                        }
                    }
                }
                // New queries run at a fresh snapshot S and every other query is aligned to S as well, so the
                // Transition that answers this ModifyQuerySet is consistent at one ts.
                let (ts, mut mods) = self.refresh().await?;
                removed.append(&mut mods);
                let end = StateVersion { query_set: new_version, ts, ..self.sent };
                self.send_transition(end, removed).await?;
            }
            ClientMessage::Authenticate { base_version, token } => {
                if base_version != self.sent.identity {
                    return Err(Fatal(format!(
                        "invalid Authenticate: server identity version is {}, got baseVersion {base_version}",
                        self.sent.identity
                    )));
                }
                // An invalid, expired or revoked token ends the socket with AuthError (the client refreshes and
                // reconnects); a valid one becomes this socket's identity until it expires. Everything reruns:
                // query results may depend on who is asking.
                let (identity, expires_at) = match token {
                    None => (Identity::default(), None),
                    Some(token) => match self.app.auth().authenticate(&self.app, &token).await {
                        Ok(a) => (a.identity, Some(deadline(a.expires_at))),
                        Err(e) => {
                            self.auth_error = Some(e.to_string());
                            return Err(Fatal(e.to_string()));
                        }
                    },
                };
                self.identity = identity;
                self.auth_expires_at = expires_at;
                for q in self.queries.values_mut() {
                    q.force = true;
                }
                let (ts, mods) = self.refresh().await?;
                let end = StateVersion { identity: base_version + 1, ts, ..self.sent };
                self.send_transition(end, mods).await?;
            }
            ClientMessage::Mutation { request_id, udf_path, args } => {
                // Run off the worker task so a slow mutation doesn't block query updates for this socket.
                let (app, identity, tx) = (self.app.clone(), self.identity.clone(), self.completions_tx.clone());
                // A client resending after a reconnect reuses (sessionId, requestId): the runtime then replays the
                // recorded response instead of running the mutation twice.
                let key = self.session_id.clone().map(|session_id| IdempotencyKey { session_id, request_id });
                tokio::spawn(async move {
                    let run = app.execute_mutation(&udf_path, args, &identity, key.as_ref()).await;
                    let result = run.result.map_err(|e| ClientError::from_anyhow(&e, app.options().dev));
                    let log_lines = client_log_lines(&app, run.log_lines);
                    let _ = tx.send(Completion::Mutation { request_id, result, log_lines });
                });
            }
            ClientMessage::Action { request_id, udf_path, args } => {
                let (app, identity, tx) = (self.app.clone(), self.identity.clone(), self.completions_tx.clone());
                tokio::spawn(async move {
                    let run = app.execute_action(&udf_path, args, &identity).await;
                    let result = run.result.map_err(|e| ClientError::from_anyhow(&e, app.options().dev));
                    let log_lines = client_log_lines(&app, run.log_lines);
                    let _ = tx.send(Completion::Action { request_id, result, log_lines });
                });
            }
            ClientMessage::Pong => {}
        }
        Ok(())
    }

    async fn handle_completion(&mut self, done: Completion) -> Result<(), Fatal> {
        match done {
            Completion::Mutation { request_id, result, log_lines } => {
                let committed_ts = result.as_ref().ok().map(|(_, ts)| *ts);
                self.send(ServerMessage::MutationResponse { request_id, result, log_lines }).await?;
                // Read-your-writes: the client resolves the mutation only once it has a Transition at ts >= the
                // commit ts, so make sure one follows promptly (even an empty one).
                if let Some(ts) = committed_ts
                    && ts > self.sent.ts
                {
                    self.pending_mutation_ts = Some(self.pending_mutation_ts.map_or(ts, |p| p.max(ts)));
                    self.refresh_and_send().await?;
                }
            }
            Completion::Action { request_id, result, log_lines } => {
                self.send(ServerMessage::ActionResponse { request_id, result, log_lines }).await?;
            }
        }
        Ok(())
    }

    /// Bring every query up to the latest ts; send a Transition if anything changed or a mutation response is
    /// waiting for one. Otherwise the queries' validity advances silently.
    async fn refresh_and_send(&mut self) -> Result<(), Fatal> {
        if self.queries.is_empty() && self.pending_mutation_ts.is_none() {
            return Ok(());
        }
        let (ts, mods) = self.refresh().await?;
        let covers_pending = self.pending_mutation_ts.is_some_and(|p| ts >= p);
        if !mods.is_empty() || covers_pending {
            self.send_transition(StateVersion { ts, ..self.sent }, mods).await?;
        }
        Ok(())
    }

    async fn send_transition(&mut self, end: StateVersion, modifications: Vec<StateModification>) -> Result<(), Fatal> {
        debug_assert!(end.ts >= self.sent.ts, "transitions must not go back in time");
        let start = self.sent;
        self.sent = end;
        if self.pending_mutation_ts.is_some_and(|p| end.ts >= p) {
            self.pending_mutation_ts = None;
        }
        self.send(ServerMessage::Transition { start_version: start, end_version: end, modifications }).await
    }

    /// The core algorithm; see the module docs. Returns the snapshot ts and the modifications to send.
    async fn refresh(&mut self) -> Result<(u64, Vec<StateModification>), Fatal> {
        let db = self.app.db();
        let snapshot = db.snapshot().await?;
        let ts = snapshot.ts;
        // The writer appends to the log right after COMMIT; the snapshot may have seen a commit whose entry isn't
        // appended yet. Wait so `entries_between` below sees every commit up to `ts`.
        db.write_log().wait_for(ts).await;

        let mut to_run: Vec<QueryId> = Vec::new();
        for (id, q) in &self.queries {
            if q.force {
                to_run.push(*id);
            } else if q.valid_at < ts {
                let invalidated = match db.write_log().entries_between(q.valid_at, ts) {
                    None => true, // trimmed: we can't tell, so assume it changed
                    Some(entries) => entries.iter().any(|e| q.read_set.overlaps(e)),
                };
                if invalidated {
                    to_run.push(*id);
                }
            }
        }

        let runs = to_run.iter().map(|id| {
            let q = &self.queries[id];
            self.app.query_in_snapshot(&snapshot, &q.path, q.args.clone(), &self.identity, false)
        });
        let results = join_all(runs).await;
        drop(snapshot);

        let mut mods = Vec::new();
        for (id, run) in to_run.into_iter().zip(results) {
            // Log lines don't count towards the hash: a rerun with the same result sends nothing.
            let log_lines = client_log_lines(&self.app, run.log_lines);
            let (hash, modification) = match run.result {
                Ok(value) => {
                    let hash = hash_result(b"ok", &value);
                    (hash, StateModification::QueryUpdated { query_id: id, value, log_lines })
                }
                Err(e) => {
                    let error = self.client_error(&e);
                    let hash = hash_result(b"err", &serde_json::json!([error.message, error.data]));
                    (hash, StateModification::QueryFailed { query_id: id, error, log_lines })
                }
            };
            let q = self.queries.get_mut(&id).expect("query exists");
            q.read_set = run.read_set;
            q.force = false;
            if q.hash != Some(hash) {
                q.hash = Some(hash);
                mods.push(modification);
            }
        }
        for q in self.queries.values_mut() {
            q.valid_at = q.valid_at.max(ts);
        }
        Ok((ts, mods))
    }
}

/// The tokio instant at which `at` (wall clock) is reached.
fn deadline(at: std::time::SystemTime) -> Instant {
    let left = at.duration_since(std::time::SystemTime::now()).unwrap_or_default();
    Instant::now() + left
}

async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

fn hash_result(tag: &[u8], value: &Value) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(tag);
    // serde_json maps are sorted (no `preserve_order`), so equal values serialize identically.
    h.update(&serde_json::to_vec(value).unwrap_or_default());
    *h.finalize().as_bytes()
}
