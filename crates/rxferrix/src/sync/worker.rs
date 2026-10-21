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
use crate::sync::protocol::{
    ClientMessage, QueryId, QuerySetModification, QueryValue, RequestId, ServerMessage, StateModification, StateVersion,
};
use crate::sync::subscriptions::{Connection, Mailbox, Update};

/// How often the server pings the client.
pub const PING_INTERVAL: Duration = Duration::from_secs(15);
/// A connection the client has sent nothing on for this long (it answers every ping) is closed.
pub const SILENCE_LIMIT: Duration = Duration::from_secs(45);
/// The most query subscriptions one connection may have.
pub const MAX_QUERIES: usize = 1024;
/// The most mutations and actions one connection may have running. Past it, the worker parks the next one and reads
/// nothing more from the client until a slot frees up (so its messages wait, in order, in the socket), while it goes
/// on sending query updates and pings and enforcing token expiry and revocation.
pub const MAX_IN_FLIGHT: usize = 64;

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
    /// The client accepts result diffs (`Connect` with `features: ["diffs"]`).
    diffs: bool,
    /// With `diffs`: the result each live query last got on this connection, i.e. what the client has. Updates are
    /// diffed against it ([`StateModification::QueryUpdated`]'s `base`).
    bases: std::collections::HashMap<QueryId, QueryValue>,
    conn: Connection,
    mailbox: Arc<Mailbox>,
    completions_tx: mpsc::UnboundedSender<Completion>,
    completions_rx: mpsc::UnboundedReceiver<Completion>,
    /// When the current access token expires; the socket then ends with `AuthError` unless the client has sent a
    /// fresh token by then.
    auth_expires_at: Option<Instant>,
    /// Set when the connection ends because of an auth failure: sent as `AuthError` instead of `FatalError`.
    auth_error: Option<String>,
    /// Fires when the authenticated session ends (sign-out, revocation, ...): the socket then ends with `AuthError`
    /// right away instead of at token expiry (`crate::auth::revocation`).
    session_watch: Option<crate::auth::SessionWatch>,
    /// When the client last sent anything ([`SILENCE_LIMIT`]).
    last_heard: Instant,
    /// One permit per running mutation or action ([`MAX_IN_FLIGHT`]).
    in_flight: Arc<tokio::sync::Semaphore>,
    /// A mutation or action that arrived while every slot was taken: it starts when one frees up, and nothing more is
    /// read from the client until then.
    parked: Option<ClientMessage>,
    /// The client's IP, which its mutations and actions are rate limited by (`crate::settings::rate_limits`).
    client_ip: Option<std::net::IpAddr>,
}

/// Spawn a worker and return its input and output channels (used by tests).
pub fn spawn(app: App) -> (mpsc::Sender<Inbound>, mpsc::Receiver<ServerMessage>) {
    spawn_from(app, None)
}

/// [`spawn`] for a client at `ip` (the WebSocket handler).
pub fn spawn_from(app: App, ip: Option<std::net::IpAddr>) -> (mpsc::Sender<Inbound>, mpsc::Receiver<ServerMessage>) {
    let (in_tx, in_rx) = mpsc::channel(64);
    let (out_tx, out_rx) = mpsc::channel(256);
    tokio::spawn(run_from(app, ip, in_rx, out_tx));
    (in_tx, out_rx)
}

/// Run a worker until the input closes, the output is dropped, or a fatal error occurs.
pub async fn run(app: App, inbound: mpsc::Receiver<Inbound>, out: mpsc::Sender<ServerMessage>) {
    run_from(app, None, inbound, out).await
}

async fn run_from(
    app: App,
    client_ip: Option<std::net::IpAddr>,
    inbound: mpsc::Receiver<Inbound>,
    out: mpsc::Sender<ServerMessage>,
) {
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
        diffs: false,
        bases: std::collections::HashMap::new(),
        conn,
        mailbox,
        completions_tx,
        completions_rx,
        auth_expires_at: None,
        auth_error: None,
        session_watch: None,
        last_heard: Instant::now(),
        in_flight: Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT)),
        parked: None,
        client_ip,
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
                let session_ended = self.session_watch.as_ref().map(|w| w.ended());
                tokio::select! {
                    msg = inbound.recv(), if self.parked.is_none() => match msg {
                        None => return Ok(()),
                        Some(Inbound::Malformed(e)) => return Err(Fatal(format!("malformed message: {e}"))),
                        Some(Inbound::Message(m)) => {
                            self.last_heard = Instant::now();
                            self.handle_message(m).await?
                        }
                    },
                    Some(done) = self.completions_rx.recv() => self.handle_completion(done).await?,
                    permit = self.in_flight.clone().acquire_owned(), if self.parked.is_some() => {
                        let permit = permit.map_err(|_| Fatal("connection closing".into()))?;
                        if let Some(msg) = self.parked.take() {
                            self.start(msg, permit);
                        }
                    }
                    Some(()) = doorbell.recv() => self.deliver().await?,
                    _ = ping.tick() => {
                        // While a message is parked the client's answers wait unread behind it: not silence.
                        if self.parked.is_none() && self.last_heard.elapsed() >= SILENCE_LIMIT {
                            return Err(Fatal("connection closed: the client has been silent too long".into()));
                        }
                        self.send(ServerMessage::Ping).await?
                    }
                    _ = sleep_until_opt(auth_expiry) => {
                        self.auth_error = Some("access token expired".into());
                        return Err(Fatal("access token expired".into()));
                    }
                    _ = wait_opt(session_ended) => {
                        self.auth_error = Some(crate::auth::revocation::SESSION_ENDED.into());
                        return Err(Fatal(crate::auth::revocation::SESSION_ENDED.into()));
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
        let mut modifications = update.into_modifications();
        if self.diffs {
            // Every modification reaches the client in order on this connection, so after this Transition the client
            // has exactly the values recorded here.
            for m in &mut modifications {
                match m {
                    StateModification::QueryUpdated { query_id, value, base, .. } => {
                        *base = self.bases.insert(*query_id, value.clone());
                    }
                    StateModification::QueryFailed { query_id, .. } | StateModification::QueryRemoved { query_id } => {
                        self.bases.remove(query_id);
                    }
                }
            }
        }
        self.send(ServerMessage::Transition { start_version: start, end_version: end, modifications }).await
    }

    async fn handle_message(&mut self, msg: ClientMessage) -> Result<(), Fatal> {
        match msg {
            ClientMessage::Connect { session_id, connection_count, features, .. } => {
                tracing::debug!(%session_id, connection_count, ?features, "sync client connected");
                self.session_id = Some(session_id);
                self.diffs = features.iter().any(|f| f == "diffs");
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
                            if self.live.len() >= MAX_QUERIES && !self.live.contains(query_id) {
                                let error = format!("too many queries on one connection (at most {MAX_QUERIES})");
                                return Err(Fatal(error));
                            }
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
                let (identity, expires_at, watch) = match token {
                    None => (Identity::default(), None, None),
                    Some(token) => match self.app.auth().authenticate_watched(&self.app, &token).await {
                        Ok((a, watch)) => (a.identity, Some(deadline(a.expires_at)), watch),
                        Err(e) => {
                            self.auth_error = Some(e.to_string());
                            return Err(Fatal(e.to_string()));
                        }
                    },
                };
                self.identity = identity.clone();
                self.auth_expires_at = expires_at;
                self.session_watch = watch;
                self.identity_version = base_version + 1;
                self.conn.authenticate(self.identity_version, identity).await?;
            }
            msg @ (ClientMessage::Mutation { .. } | ClientMessage::Action { .. }) => {
                if self.rate_limited(&msg) {
                    return Ok(());
                }
                match self.in_flight.clone().try_acquire_owned() {
                    Ok(permit) => self.start(msg, permit),
                    Err(_) => self.parked = Some(msg),
                }
            }
            ClientMessage::Pong => {}
        }
        Ok(())
    }

    /// Count a mutation or action against the function rate limits; over the limit, answer it with `rate_limited`
    /// (through the completions, so responses keep their usual path) and return true.
    fn rate_limited(&self, msg: &ClientMessage) -> bool {
        let (ClientMessage::Mutation { request_id, udf_path, .. } | ClientMessage::Action { request_id, udf_path, .. }) =
            msg
        else {
            return false;
        };
        let Err(limited) =
            crate::settings::rate_limits::check_function(&self.app, udf_path, &self.identity, || self.client_ip)
        else {
            return false;
        };
        let (request_id, error, log_lines) = (*request_id, limited.client_error(), Vec::new());
        let _ = self.completions_tx.send(match msg {
            ClientMessage::Mutation { .. } => Completion::Mutation { request_id, result: Err(error), log_lines },
            _ => Completion::Action { request_id, result: Err(error), log_lines },
        });
        true
    }

    /// Run a mutation or action off the worker task (so a slow one doesn't hold up query updates for this socket),
    /// holding `permit` (one of [`MAX_IN_FLIGHT`]) until it ends.
    fn start(&self, msg: ClientMessage, permit: tokio::sync::OwnedSemaphorePermit) {
        let (app, identity, tx) = (self.app.clone(), self.identity.clone(), self.completions_tx.clone());
        match msg {
            ClientMessage::Mutation { request_id, udf_path, args } => {
                // A client resending after a reconnect reuses (sessionId, requestId): the runtime then replays the
                // recorded response instead of running the mutation twice.
                let key = self.session_id.clone().map(|session_id| IdempotencyKey { session_id, request_id });
                tokio::spawn(async move {
                    let _permit = permit;
                    let run = app.execute_mutation(&udf_path, args, &identity, key.as_ref()).await;
                    let result = run.result.map_err(|e| ClientError::from_anyhow(&e, app.options().dev));
                    let log_lines = client_log_lines(&app, run.log_lines);
                    let _ = tx.send(Completion::Mutation { request_id, result, log_lines });
                });
            }
            ClientMessage::Action { request_id, udf_path, args } => {
                tokio::spawn(async move {
                    let _permit = permit;
                    let run = app.execute_action(&udf_path, args, &identity).await;
                    let result = run.result.map_err(|e| ClientError::from_anyhow(&e, app.options().dev));
                    let log_lines = client_log_lines(&app, run.log_lines);
                    let _ = tx.send(Completion::Action { request_id, result, log_lines });
                });
            }
            _ => unreachable!("only mutations and actions are started"),
        }
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

async fn wait_opt(f: Option<impl std::future::Future<Output = ()>>) {
    match f {
        Some(f) => f.await,
        None => std::future::pending().await,
    }
}

async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}
