//! Items used only by the code that `ferrix-macros` generates. Mounted as `ferrix::__private`; see
//! docs/CONTRACT.md §2. Not a stable API for application code.

pub use ::futures::future::BoxFuture;
pub use ::linkme;
pub use ::serde;
pub use ::serde_json;
pub use ::ts_rs;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::InvalidArgs;
use crate::runtime::{ActionCtx, MutationCtx, QueryCtx};
use crate::schema::TableDef;

/// Every `#[table]` registers its `table_def` here.
#[linkme::distributed_slice]
pub static TABLES: [fn() -> TableDef];

/// Every `#[query]` / `#[mutation]` / `#[action]` (and `internal_*`) registers a definition here.
#[linkme::distributed_slice]
pub static FUNCTIONS: [FunctionDef];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FunctionKind {
    Query,
    Mutation,
    Action,
}

impl FunctionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FunctionKind::Query => "query",
            FunctionKind::Mutation => "mutation",
            FunctionKind::Action => "action",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Visibility {
    Public,
    Internal,
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Internal => "internal",
        }
    }
}

pub type QueryHandler = fn(QueryCtx, serde_json::Value) -> BoxFuture<'static, crate::Result<serde_json::Value>>;
pub type MutationHandler = fn(MutationCtx, serde_json::Value) -> BoxFuture<'static, crate::Result<serde_json::Value>>;
pub type ActionHandler = fn(ActionCtx, serde_json::Value) -> BoxFuture<'static, crate::Result<serde_json::Value>>;

#[derive(Clone, Copy)]
pub enum Handler {
    Query(QueryHandler),
    Mutation(MutationHandler),
    Action(ActionHandler),
}

impl Handler {
    pub fn kind(&self) -> FunctionKind {
        match self {
            Handler::Query(_) => FunctionKind::Query,
            Handler::Mutation(_) => FunctionKind::Mutation,
            Handler::Action(_) => FunctionKind::Action,
        }
    }
}

pub struct FunctionDef {
    /// `module_path!()` at the definition site.
    pub module_path: &'static str,
    /// The Rust fn name.
    pub name: &'static str,
    pub visibility: Visibility,
    pub handler: Handler,
    /// TS signature for codegen: (args TS type, output TS type).
    pub ts: fn() -> (String, String),
}

impl FunctionDef {
    pub fn path(&self) -> String {
        function_path(self.module_path, self.name)
    }
}

impl std::fmt::Debug for FunctionDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FunctionDef")
            .field("path", &self.path())
            .field("kind", &self.handler.kind())
            .field("visibility", &self.visibility)
            .finish()
    }
}

/// `"todo_server::todos"` + `"list"` → `"todos:list"`; `"todo_server"` + `"list"` → `"list"`;
/// `"a::b::c"` + `"f"` → `"b/c:f"`. The first segment (the crate name) is dropped.
pub fn function_path(module_path: &str, name: &str) -> String {
    let modules: Vec<&str> = module_path.split("::").skip(1).collect();
    if modules.is_empty() { name.to_string() } else { format!("{}:{name}", modules.join("/")) }
}

/// Deserialize args; on failure the error message starts with `"Invalid args for <name>: "`.
pub fn decode_args<A: DeserializeOwned>(name: &str, args: serde_json::Value) -> crate::Result<A> {
    // A missing body (null) means "no args".
    let args = if args.is_null() { serde_json::Value::Object(Default::default()) } else { args };
    serde_json::from_value(args).map_err(|e| InvalidArgs(format!("Invalid args for {name}: {e}")).into())
}

pub fn encode_output<O: Serialize>(out: O) -> crate::Result<serde_json::Value> {
    Ok(serde_json::to_value(out)?)
}

/// The inline TS type of `T`, rendered with [`ts_config`].
///
/// While a codegen session is active on this thread (see [`crate::codegen`]), the named types `T` refers to are
/// also recorded, recursively, so that codegen can declare them.
pub fn ts_inline<T: ts_rs::TS + ?Sized + 'static>() -> String {
    crate::codegen::collect_dependencies::<T>();
    T::inline(ts_config())
}

/// The one ts-rs configuration used for every generated TS type. Large integers (`u64`, `i64`, ...) are `number`,
/// because the wire format is JSON numbers, not bigints.
pub fn ts_config() -> &'static ts_rs::Config {
    static CONFIG: std::sync::LazyLock<ts_rs::Config> =
        std::sync::LazyLock::new(|| ts_rs::Config::default().with_large_int("number"));
    &CONFIG
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(function_path("todo_server::todos", "list"), "todos:list");
        assert_eq!(function_path("todo_server", "list"), "list");
        assert_eq!(function_path("a::b::c", "f"), "b/c:f");
    }

    #[test]
    fn decode_args_message() {
        #[derive(serde::Deserialize, Debug)]
        #[allow(dead_code)]
        struct A {
            x: u32,
        }
        let err = decode_args::<A>("list", serde_json::json!({"x": "no"})).unwrap_err();
        assert!(err.to_string().starts_with("Invalid args for list: "), "{err}");
        #[derive(serde::Deserialize, Debug)]
        struct Empty {}
        decode_args::<Empty>("f", serde_json::Value::Null).unwrap();
    }
}
