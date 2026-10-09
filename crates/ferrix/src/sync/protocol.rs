//! Wire messages of the sync protocol (docs/PROTOCOL.md). JSON text frames, one message each, tagged by `"type"`.

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

#[derive(Debug, Clone, PartialEq)]
pub enum StateModification {
    QueryUpdated { query_id: QueryId, value: Value },
    QueryFailed { query_id: QueryId, error: ClientError },
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
            StateModification::QueryUpdated { query_id, value } => {
                json!({ "type": "QueryUpdated", "queryId": query_id, "value": value, "logLines": [] })
            }
            StateModification::QueryFailed { query_id, error } => json!({
                "type": "QueryFailed", "queryId": query_id, "errorMessage": error.message,
                "errorData": error.data, "logLines": []
            }),
            StateModification::QueryRemoved { query_id } => json!({ "type": "QueryRemoved", "queryId": query_id }),
        }
    }
}

/// Server → client.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    Transition { start_version: StateVersion, end_version: StateVersion, modifications: Vec<StateModification> },
    /// `Ok((result, commit ts))` or the error.
    MutationResponse { request_id: RequestId, result: Result<(Value, u64), ClientError> },
    ActionResponse { request_id: RequestId, result: Result<Value, ClientError> },
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
            ServerMessage::MutationResponse { request_id, result: Ok((value, ts)) } => json!({
                "type": "MutationResponse", "requestId": request_id, "success": true, "result": value, "ts": ts,
                "logLines": []
            }),
            ServerMessage::MutationResponse { request_id, result: Err(e) } => json!({
                "type": "MutationResponse", "requestId": request_id, "success": false, "errorMessage": e.message,
                "errorData": e.data, "logLines": []
            }),
            ServerMessage::ActionResponse { request_id, result: Ok(value) } => json!({
                "type": "ActionResponse", "requestId": request_id, "success": true, "result": value, "logLines": []
            }),
            ServerMessage::ActionResponse { request_id, result: Err(e) } => json!({
                "type": "ActionResponse", "requestId": request_id, "success": false, "errorMessage": e.message,
                "errorData": e.data, "logLines": []
            }),
            ServerMessage::FatalError { error } => json!({ "type": "FatalError", "error": error }),
            ServerMessage::AuthError { error } => json!({ "type": "AuthError", "error": error }),
            ServerMessage::Ping => json!({ "type": "Ping" }),
        }
    }

    pub fn to_text(&self) -> String {
        self.to_json().to_string()
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
    fn server_message_shapes() {
        let v = ServerMessage::MutationResponse { request_id: 1, result: Ok((Value::Null, 5)) }.to_json();
        assert_eq!(v, json!({"type":"MutationResponse","requestId":1,"success":true,"result":null,"ts":5,"logLines":[]}));
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
