//! Error types shared by the runtime and user code.
//!
//! User code returns `rxferrix::Result<T>` (an `anyhow::Result`). At the function boundary the runtime inspects the
//! error chain: a [`UserError`] is forwarded to the client with its code and data; everything else is reported as
//! `"Server Error"` unless the server runs with `--dev`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The error type used throughout RxFerrix (a re-export of `anyhow::Error`).
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

    /// The `UserError` in `err` (anywhere in its chain), if it is one: what a test asserts on.
    ///
    /// ```ignore
    /// let err = t.mutation::<send>((channel, String::new())).await.unwrap_err();
    /// assert_eq!(UserError::of(&err).map(|e| e.code.as_str()), Some("empty"));
    /// ```
    pub fn of(err: &Error) -> Option<&UserError> {
        err.chain().find_map(|e| e.downcast_ref::<UserError>())
    }
}

/// End the function with a [`UserError`] for the client: `bail_user!("not_found", "no channel {name}")` is
/// `return Err(UserError::new("not_found", format!("no channel {name}")).into())`.
#[macro_export]
macro_rules! bail_user {
    ($code:expr, $($message:tt)+) => {
        return ::core::result::Result::Err($crate::UserError::new($code, ::std::format!($($message)+)).into())
    };
}

/// [`bail_user!`] unless `cond` holds: `ensure_user!(!text.is_empty(), "empty", "a message needs some text")`.
#[macro_export]
macro_rules! ensure_user {
    ($cond:expr, $code:expr, $($message:tt)+) => {
        if !$cond {
            $crate::bail_user!($code, $($message)+);
        }
    };
}

/// Turn a missing value into a [`UserError`]: `ctx.db.get(id).await?.or_user("not_found", "no such channel")?`.
pub trait OrUserError<T> {
    fn or_user(self, code: &str, message: impl Into<String>) -> Result<T>;
}

impl<T> OrUserError<T> for Option<T> {
    fn or_user(self, code: &str, message: impl Into<String>) -> Result<T> {
        self.ok_or_else(|| UserError::new(code, message).into())
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

/// A mutation ran longer than `AppOptions::mutation_timeout` and was rolled back. A server error: clients see its
/// message only in `--dev`; the execution log always has it.
///
/// The timeout can only fire while the mutation is suspended at an `.await`: synchronous code (a busy loop, a
/// blocking call) inside a mutation is never preempted and keeps holding the writer lock until it yields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationTimeout {
    pub path: String,
    pub timeout: std::time::Duration,
}

impl std::fmt::Display for MutationTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "mutation `{}` timed out after {:?} and was rolled back (limit: AppOptions::mutation_timeout, \
             `--mutation-timeout` on the command line)",
            self.path, self.timeout
        )
    }
}

impl std::error::Error for MutationTimeout {}

/// A query ran longer than `AppOptions::query_timeout`. A server error. Like [`MutationTimeout`], it can only fire at
/// an `.await`. Without it, a stuck query would hold a reader connection, and the results its connections need, for
/// good.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryTimeout {
    pub path: String,
    pub timeout: std::time::Duration,
}

impl std::fmt::Display for QueryTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "query `{}` timed out after {:?} (limit: AppOptions::query_timeout, `--query-timeout` on the command line)",
            self.path, self.timeout
        )
    }
}

impl std::error::Error for QueryTimeout {}

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
