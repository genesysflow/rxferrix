//! Wire messages of the sync protocol (docs/PROTOCOL.md). JSON text frames, one message each, tagged by `"type"`.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::ClientError;

pub type QueryId = u64;
pub type RequestId = u64;

fn empty_object() -> Value {
    Value::Object(Default::default())
}

/// Client → server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum ClientMessage {
    Connect {
        session_id: String,
        #[serde(default)]
        connection_count: u64,
        #[serde(default)]
        last_close_reason: Option<String>,
        #[serde(default)]
        client_version: Option<String>,
        /// Optional protocol features the client supports: `"diffs"` (result diffs, see [`QueryValue::diff_from`]).
        #[serde(default)]
        features: Vec<String>,
    },
    ModifyQuerySet {
        base_version: u64,
        new_version: u64,
        modifications: Vec<QuerySetModification>,
    },
    Mutation {
        request_id: RequestId,
        udf_path: String,
        #[serde(default = "empty_object")]
        args: Value,
    },
    Action {
        request_id: RequestId,
        udf_path: String,
        #[serde(default = "empty_object")]
        args: Value,
    },
    Authenticate {
        base_version: u64,
        token: Option<String>,
    },
    Pong,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum QuerySetModification {
    Add {
        query_id: QueryId,
        udf_path: String,
        #[serde(default = "empty_object")]
        args: Value,
    },
    Remove {
        query_id: QueryId,
    },
}

/// The client's view of the world: query set version, identity version and ts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateVersion {
    pub query_set: u64,
    pub identity: u64,
    pub ts: u64,
}

/// A query result as sent to clients: the value and its JSON text, serialized once when the result is computed and
/// shared by every subscriber of the query (cloning is an `Arc` clone). Each client's `Transition` frame splices the
/// text in ([`ServerMessage::to_text`]) instead of serializing the value again per client, or splices a diff from the
/// result that client has ([`QueryValue::diff_from`]), computed once per pair of results.
#[derive(Clone)]
pub struct QueryValue(Arc<QueryValueInner>);

struct QueryValueInner {
    /// Unique per computed result: the key of `diffs`.
    id: u64,
    value: Value,
    json: Box<str>,
    /// Diffs to this result from earlier ones (by their id), as JSON text; `None` where sending the value is better.
    diffs: std::sync::Mutex<Vec<(u64, Option<Arc<str>>)>>,
}

/// How many diffs (from distinct earlier results) a result keeps: clients are almost always one result behind.
const DIFFS_KEPT: usize = 8;

impl QueryValue {
    pub fn new(value: Value) -> QueryValue {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let json = serde_json::to_string(&value).expect("a JSON value serializes").into_boxed_str();
        QueryValue(Arc::new(QueryValueInner { id, value, json, diffs: std::sync::Mutex::new(Vec::new()) }))
    }

    /// The diff ([`crate::sync::diff`]) that turns `base` into this result, as JSON text: computed by the first
    /// connection that needs it and shared by every other client that has the same `base`. `None` if sending the value
    /// is better (the diff isn't clearly smaller).
    pub fn diff_from(&self, base: &QueryValue) -> Option<Arc<str>> {
        let mut diffs = self.0.diffs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, cached)) = diffs.iter().find(|(id, _)| *id == base.0.id) {
            return cached.clone();
        }
        let text = crate::sync::diff::diff(&base.0.value, &self.0.value)
            .map(|d| serde_json::to_string(&d).expect("a JSON value serializes"))
            .filter(|d| d.len() * 10 < self.0.json.len() * 7)
            .map(Arc::<str>::from);
        if diffs.len() >= DIFFS_KEPT {
            diffs.remove(0);
        }
        diffs.push((base.0.id, text.clone()));
        text
    }

    pub fn value(&self) -> &Value {
        &self.0.value
    }

    /// The value as compact JSON.
    pub fn json(&self) -> &str {
        &self.0.json
    }
}

impl std::ops::Deref for QueryValue {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.0.value
    }
}

impl From<Value> for QueryValue {
    fn from(value: Value) -> QueryValue {
        QueryValue::new(value)
    }
}

impl PartialEq for QueryValue {
    fn eq(&self, other: &QueryValue) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || self.0.value == other.0.value
    }
}

impl std::fmt::Debug for QueryValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.value.fmt(f)
    }
}

/// `log_lines` are the function's captured log lines; the server fills them only in `--dev`.
#[derive(Debug, Clone, PartialEq)]
pub enum StateModification {
    /// `base`: the result the client has for this query, when it accepts diffs (set by the sync worker). The text form
    /// then carries a diff from it instead of the value where that is smaller; [`StateModification::to_json`] always
    /// carries the value.
    QueryUpdated { query_id: QueryId, value: QueryValue, base: Option<QueryValue>, log_lines: Vec<String> },
    QueryFailed { query_id: QueryId, error: ClientError, log_lines: Vec<String> },
    QueryRemoved { query_id: QueryId },
}

impl StateModification {
    pub fn query_id(&self) -> QueryId {
        match self {
            StateModification::QueryUpdated { query_id, .. }
            | StateModification::QueryFailed { query_id, .. }
            | StateModification::QueryRemoved { query_id } => *query_id,
        }
    }

    fn to_json(&self) -> Value {
        match self {
            StateModification::QueryUpdated { query_id, value, log_lines, .. } => {
                json!({ "type": "QueryUpdated", "queryId": query_id, "value": value.value(), "logLines": log_lines })
            }
            StateModification::QueryFailed { query_id, error, log_lines } => json!({
                "type": "QueryFailed", "queryId": query_id, "errorMessage": error.message,
                "errorData": error.data, "logLines": log_lines
            }),
            StateModification::QueryRemoved { query_id } => json!({ "type": "QueryRemoved", "queryId": query_id }),
        }
    }

    /// Append the JSON form ([`StateModification::to_json`]) to `out`, splicing in a result's shared text.
    fn write_json(&self, out: &mut String) {
        match self {
            StateModification::QueryUpdated { query_id, value, base, log_lines } => {
                out.push_str(r#"{"type":"QueryUpdated","queryId":"#);
                out.push_str(&query_id.to_string());
                match base.as_ref().and_then(|base| value.diff_from(base)) {
                    Some(diff) => {
                        out.push_str(r#","diff":"#);
                        out.push_str(&diff);
                    }
                    None => {
                        out.push_str(r#","value":"#);
                        out.push_str(value.json());
                    }
                }
                out.push_str(r#","logLines":"#);
                out.push_str(&serde_json::to_string(log_lines).expect("strings serialize"));
                out.push('}');
            }
            other => out.push_str(&other.to_json().to_string()),
        }
    }
}

/// Server → client.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    Transition { start_version: StateVersion, end_version: StateVersion, modifications: Vec<StateModification> },
    /// `Ok((result, commit ts))` or the error. `log_lines` only in `--dev`.
    MutationResponse { request_id: RequestId, result: Result<(Value, u64), ClientError>, log_lines: Vec<String> },
    ActionResponse { request_id: RequestId, result: Result<Value, ClientError>, log_lines: Vec<String> },
    FatalError { error: String },
    AuthError { error: String },
    Ping,
}

impl ServerMessage {
    /// The JSON form. Written by hand because success and failure responses have different shapes and a successful
    /// `result` of `null` must still be present.
    pub fn to_json(&self) -> Value {
        match self {
            ServerMessage::Transition { start_version, end_version, modifications } => json!({
                "type": "Transition",
                "startVersion": start_version,
                "endVersion": end_version,
                "modifications": modifications.iter().map(StateModification::to_json).collect::<Vec<_>>(),
            }),
            ServerMessage::MutationResponse { request_id, result: Ok((value, ts)), log_lines } => json!({
                "type": "MutationResponse", "requestId": request_id, "success": true, "result": value, "ts": ts,
                "logLines": log_lines
            }),
            ServerMessage::MutationResponse { request_id, result: Err(e), log_lines } => json!({
                "type": "MutationResponse", "requestId": request_id, "success": false, "errorMessage": e.message,
                "errorData": e.data, "logLines": log_lines
            }),
            ServerMessage::ActionResponse { request_id, result: Ok(value), log_lines } => json!({
                "type": "ActionResponse", "requestId": request_id, "success": true, "result": value,
                "logLines": log_lines
            }),
            ServerMessage::ActionResponse { request_id, result: Err(e), log_lines } => json!({
                "type": "ActionResponse", "requestId": request_id, "success": false, "errorMessage": e.message,
                "errorData": e.data, "logLines": log_lines
            }),
            ServerMessage::FatalError { error } => json!({ "type": "FatalError", "error": error }),
            ServerMessage::AuthError { error } => json!({ "type": "AuthError", "error": error }),
            ServerMessage::Ping => json!({ "type": "Ping" }),
        }
    }

    /// The JSON text of [`ServerMessage::to_json`]. A `Transition` is written directly, with each result's shared
    /// JSON spliced in: with many subscribers, serializing every result again per client was most of the server's
    /// work.
    pub fn to_text(&self) -> String {
        match self {
            ServerMessage::Transition { start_version, end_version, modifications } => {
                let values: usize = modifications
                    .iter()
                    .map(|m| match m {
                        StateModification::QueryUpdated { value, .. } => value.json().len() + 64,
                        _ => 64,
                    })
                    .sum();
                let mut out = String::with_capacity(160 + values);
                let version = |v: &StateVersion| serde_json::to_string(v).expect("a version serializes");
                out.push_str(r#"{"type":"Transition","startVersion":"#);
                out.push_str(&version(start_version));
                out.push_str(r#","endVersion":"#);
                out.push_str(&version(end_version));
                out.push_str(r#","modifications":["#);
                for (i, m) in modifications.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    m.write_json(&mut out);
                }
                out.push_str("]}");
                out
            }
            other => other.to_json().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_client_messages() {
        let m: ClientMessage = serde_json::from_str(
            r#"{ "type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 1, "modifications": [
                { "type": "Add", "queryId": 0, "udfPath": "todos:list", "args": {} },
                { "type": "Remove", "queryId": 3 } ] }"#,
        )
        .unwrap();
        assert_eq!(
            m,
            ClientMessage::ModifyQuerySet {
                base_version: 0,
                new_version: 1,
                modifications: vec![
                    QuerySetModification::Add { query_id: 0, udf_path: "todos:list".into(), args: json!({}) },
                    QuerySetModification::Remove { query_id: 3 },
                ],
            }
        );
        let m: ClientMessage = serde_json::from_str(
            r#"{ "type": "Connect", "sessionId": "x", "connectionCount": 0, "lastCloseReason": null, "clientVersion": "0.1.0" }"#,
        )
        .unwrap();
        assert!(matches!(m, ClientMessage::Connect { .. }));
        let m: ClientMessage = serde_json::from_str(r#"{ "type": "Authenticate", "baseVersion": 0, "token": null }"#).unwrap();
        assert_eq!(m, ClientMessage::Authenticate { base_version: 0, token: None });
        assert!(serde_json::from_str::<ClientMessage>(r#"{ "type": "Nope" }"#).is_err());
    }

    #[test]
    fn transition_text_splices_shared_results() {
        let text = "quote \" backslash \\ é ✓ \n";
        let value = QueryValue::new(json!([{ "_id": "a", "text": text, "n": 1.5, "nested": { "x": null } }]));
        let t = ServerMessage::Transition {
            start_version: StateVersion { query_set: 1, identity: 2, ts: 3 },
            end_version: StateVersion { query_set: 4, identity: 5, ts: 6 },
            modifications: vec![
                StateModification::QueryUpdated {
                    query_id: 7,
                    value: value.clone(),
                    base: None,
                    log_lines: vec!["[INFO] \"hi\"".into()],
                },
                StateModification::QueryUpdated { query_id: 8, value: json!(null).into(), base: None, log_lines: vec![] },
                StateModification::QueryFailed {
                    query_id: 9,
                    error: ClientError {
                        kind: crate::error::ErrorKind::User,
                        message: "boom".into(),
                        data: Some(json!({ "code": 1 })),
                    },
                    log_lines: vec![],
                },
                StateModification::QueryRemoved { query_id: 10 },
            ],
        };
        let parsed: Value = serde_json::from_str(&t.to_text()).unwrap();
        assert_eq!(parsed, t.to_json());
        let empty = ServerMessage::Transition {
            start_version: StateVersion::default(),
            end_version: StateVersion::default(),
            modifications: vec![],
        };
        assert_eq!(serde_json::from_str::<Value>(&empty.to_text()).unwrap(), empty.to_json());
        // Cloning shares the serialized text.
        assert!(std::ptr::eq(value.json(), value.clone().json()));
    }

    #[test]
    fn server_message_shapes() {
        let v = ServerMessage::MutationResponse { request_id: 1, result: Ok((Value::Null, 5)), log_lines: vec![] }
            .to_json();
        assert_eq!(v, json!({"type":"MutationResponse","requestId":1,"success":true,"result":null,"ts":5,"logLines":[]}));
        let log_lines = vec!["[INFO] x".to_string()];
        let m = StateModification::QueryUpdated { query_id: 2, value: json!(1).into(), base: None, log_lines };
        assert_eq!(m.to_json()["logLines"], json!(["[INFO] x"]));
        let t = ServerMessage::Transition {
            start_version: StateVersion::default(),
            end_version: StateVersion { query_set: 1, identity: 0, ts: 42 },
            modifications: vec![StateModification::QueryRemoved { query_id: 3 }],
        }
        .to_json();
        assert_eq!(t["endVersion"], json!({"querySet":1,"identity":0,"ts":42}));
        assert_eq!(t["modifications"][0], json!({"type":"QueryRemoved","queryId":3}));
    }
}
