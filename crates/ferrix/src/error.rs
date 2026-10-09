//! Error types shared by the runtime and user code.
//!
//! User code returns `ferrix::Result<T>` (an `anyhow::Result`). At the function boundary the runtime inspects the
//! error chain: a [`UserError`] is forwarded to the client with its code and data; everything else is reported as
//! `"Server Error"` unless the server runs with `--dev`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The error type used throughout Ferrix (a re-export of `anyhow::Error`).
pub type Error = anyhow::Error;

/// `Result` with [`Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// An error meant for the client: it reaches the caller as `errorData = { code, data }` with `message` as the error
/// message, even outside `--dev`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserError {
    pub code: String,
    pub message: String,
    pub data: Value,
}

impl UserError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), data: Value::Null }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = data;
        self
    }
}

impl std::fmt::Display for UserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for UserError {}

/// Args failed to deserialize. Produced by `__private::decode_args`; maps to HTTP 400 and its message is always
/// shown to the client.
#[derive(Debug)]
pub(crate) struct InvalidArgs(pub String);

impl std::fmt::Display for InvalidArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for InvalidArgs {}

/// No (callable) function at this path, or one of the wrong kind. Maps to HTTP 404; the message is always shown.
#[derive(Debug)]
pub(crate) struct FunctionNotFound(pub String);

impl std::fmt::Display for FunctionNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for FunctionNotFound {}

/// How an error is classified at the client boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    User,
    InvalidArgs,
    NotFound,
    Server,
}

/// An error as presented to a client (WebSocket or HTTP).
#[derive(Debug, Clone, PartialEq)]
pub struct ClientError {
    pub kind: ErrorKind,
    pub message: String,
    /// `{ code, data }` for user errors, `None` (JSON null) otherwise.
    pub data: Option<Value>,
}

impl ClientError {
    /// Classify `err` for a client. In `dev` mode server errors carry their full message (including context).
    pub fn from_anyhow(err: &Error, dev: bool) -> Self {
        for cause in err.chain() {
            if let Some(u) = cause.downcast_ref::<UserError>() {
                return ClientError {
                    kind: ErrorKind::User,
                    message: u.message.clone(),
                    data: Some(serde_json::json!({ "code": u.code, "data": u.data })),
                };
            }
            if let Some(a) = cause.downcast_ref::<InvalidArgs>() {
                return ClientError { kind: ErrorKind::InvalidArgs, message: a.0.clone(), data: None };
            }
            if let Some(nf) = cause.downcast_ref::<FunctionNotFound>() {
                return ClientError { kind: ErrorKind::NotFound, message: nf.to_string(), data: None };
            }
        }
        let message = if dev { format!("{err:#}") } else { "Server Error".to_string() };
        ClientError { kind: ErrorKind::Server, message, data: None }
    }

    /// HTTP status code for the HTTP API (see PROTOCOL.md).
    pub fn http_status(&self) -> u16 {
        match self.kind {
            ErrorKind::User | ErrorKind::InvalidArgs => 400,
            ErrorKind::NotFound => 404,
            ErrorKind::Server => 500,
        }
    }
}
