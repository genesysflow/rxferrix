//! The per-connection sync worker.
//!
//! One task per WebSocket. It owns the connection's protocol state: the versions it validates (`ModifyQuerySet`,
//! `Authenticate`), the StateVersion the client holds, the socket's identity and token expiry, mutations and actions,
//! and pings. Queries are not run here: the worker forwards query-set changes and identity changes to the app's
//! shared subscription manager ([`super::subscriptions`]), which runs each distinct query once for every socket that
//! watches it, and receives the results back through the connection's ordered mailbox. Each update from the mailbox
//! becomes one `Transition`, so all of its results are valid at its `endVersion.ts` (see the manager's docs for why).
//!
//! Read-your-writes: when a mutation commits at ts M and the client has not seen M yet, the worker asks the manager to
//! cover M; the manager then sends an update (possibly empty) at a ts >= M.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};

use crate::db::dedup::IdempotencyKey;
use crate::error::ClientError;
use crate::runtime::{App, Identity};
use crate::sync::protocol::{ClientMessage, QueryId, QuerySetModification, RequestId, ServerMessage, StateVersion};
use crate::sync::subscriptions::{Connection, Mailbox, Update};

/// How often the server pings the client.
pub const PING_INTERVAL: Duration = Duration::from_secs(15);

/// Input to the worker from the socket reader.
#[derive(Debug)]
pub enum Inbound {
    Message(ClientMessage),
    /// A frame that could not be parsed; the worker answers with `FatalError` and stops.
    Malformed(String),
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
    /// The latest query set version the client asked for (may be ahead of `sent` while the manager answers).
    query_set: u64,
    /// The latest identity version the client asked for.
    identity_version: u64,
    /// The client's live query ids (to reject duplicates and ignore removals of unknown ids).
    live: BTreeSet<QueryId>,
    identity: Identity,
    /// From `Connect`; with each mutation's requestId it makes the idempotency key.
    session_id: Option<String>,
    conn: Connection,
    mailbox: Arc<Mailbox>,
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
    let (conn, mailbox, doorbell) = match app.subscriptions().connect().await {
        Ok(c) => c,
        Err(e) => {
            let _ = out.send(ServerMessage::FatalError { error: e.to_string() }).await;
            return;
        }
    };
    let (completions_tx, completions_rx) = mpsc::unbounded_channel();
    let worker = SyncWorker {
        app,
        out,
        sent: StateVersion::default(),
        query_set: 0,
        identity_version: 0,
        live: BTreeSet::new(),
        identity: Identity::default(),
        session_id: None,
        conn,
        mailbox,
        completions_tx,
        completions_rx,
        auth_expires_at: None,
        auth_error: None,
    };
    worker.run(inbound, doorbell).await
}

impl SyncWorker {
    async fn run(mut self, mut inbound: mpsc::Receiver<Inbound>, mut doorbell: mpsc::Receiver<()>) {
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
                    Some(()) = doorbell.recv() => self.deliver().await?,
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
        // Dropping `self.conn` unsubscribes everything.
    }

    async fn send(&self, msg: ServerMessage) -> Result<(), Fatal> {
        self.out.send(msg).await.map_err(|_| Fatal("connection closed".into()))
    }

    /// Send every update the manager queued, one Transition each.
    async fn deliver(&mut self) -> Result<(), Fatal> {
        let (updates, overflowed) = self.mailbox.take();
        for update in updates {
            self.send_transition(update).await?;
        }
        if overflowed {
            return Err(Fatal("connection dropped: the client is not keeping up with updates".into()));
        }
        Ok(())
    }

    async fn send_transition(&mut self, update: Update) -> Result<(), Fatal> {
        let end = StateVersion { query_set: update.query_set, identity: update.identity, ts: update.ts };
        debug_assert!(end.ts >= self.sent.ts, "transitions must not go back in time");
        let start = std::mem::replace(&mut self.sent, end);
        let modifications = update.into_modifications();
        self.send(ServerMessage::Transition { start_version: start, end_version: end, modifications }).await
    }

    async fn handle_message(&mut self, msg: ClientMessage) -> Result<(), Fatal> {
        match msg {
            ClientMessage::Connect { session_id, connection_count, .. } => {
                tracing::debug!(%session_id, connection_count, "sync client connected");
                self.session_id = Some(session_id);
            }
            ClientMessage::ModifyQuerySet { base_version, new_version, modifications } => {
                if base_version != self.query_set || new_version != base_version + 1 {
                    return Err(Fatal(format!(
                        "invalid ModifyQuerySet: server query set version is {}, got baseVersion {base_version} and \
                         newVersion {new_version}",
                        self.query_set
                    )));
                }
                let mut forwarded = Vec::with_capacity(modifications.len());
                for m in modifications {
                    match &m {
                        QuerySetModification::Add { query_id, .. } => {
                            if !self.live.insert(*query_id) {
                                return Err(Fatal(format!("duplicate queryId {query_id}")));
                            }
                            forwarded.push(m);
                        }
                        QuerySetModification::Remove { query_id } => {
                            // Removing an id that isn't live is harmless (e.g. a client racing a reconnect); ignore it.
                            if self.live.remove(query_id) {
                                forwarded.push(m);
                            }
                        }
                    }
                }
                self.query_set = new_version;
                // The manager answers with an update at querySet = new_version carrying the first results.
                self.conn.modify(new_version, forwarded).await?;
            }
            ClientMessage::Authenticate { base_version, token } => {
                if base_version != self.identity_version {
                    return Err(Fatal(format!(
                        "invalid Authenticate: server identity version is {}, got baseVersion {base_version}",
                        self.identity_version
                    )));
                }
                // An invalid, expired or revoked token ends the socket with AuthError (the client refreshes and
                // reconnects); a valid one becomes this socket's identity until it expires. Query results may depend
                // on who is asking: the manager moves every subscription to the shared query for the new identity.
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
                self.identity = identity.clone();
                self.auth_expires_at = expires_at;
                self.identity_version = base_version + 1;
                self.conn.authenticate(self.identity_version, identity).await?;
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
                    self.conn.cover(ts).await?;
                }
            }
            Completion::Action { request_id, result, log_lines } => {
                self.send(ServerMessage::ActionResponse { request_id, result, log_lines }).await?;
            }
        }
        Ok(())
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
