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

/// Field types `#[search_index(search = f)]` accepts. `#[table]` emits `const _: () = assert_search_field::<Ty>();`.
#[diagnostic::on_unimplemented(
    message = "the search field of a #[search_index] must be a `String` or `Option<String>`, not `{Self}`",
    label = "not a string field"
)]
pub trait SearchFieldType {}
impl SearchFieldType for String {}
impl SearchFieldType for Option<String> {}
pub const fn assert_search_field<T: SearchFieldType + ?Sized>() {}

/// Field types `#[vector_index(field = f)]` accepts. `#[table]` emits `const _: () = assert_vector_field::<Ty>();`.
#[diagnostic::on_unimplemented(
    message = "the field of a #[vector_index] must be a `Vec<f32>` or `Option<Vec<f32>>`, not `{Self}`",
    label = "not a `Vec<f32>` field"
)]
pub trait VectorFieldType {}
impl VectorFieldType for Vec<f32> {}
impl VectorFieldType for Option<Vec<f32>> {}
pub const fn assert_vector_field<T: VectorFieldType + ?Sized>() {}

/// Every `#[table]` registers its `table_def` here.
#[linkme::distributed_slice]
pub static TABLES: [fn() -> TableDef];

/// Every `#[query]` / `#[mutation]` / `#[action]` (and `internal_*`) registers a definition here.
#[linkme::distributed_slice]
pub static FUNCTIONS: [FunctionDef];

/// Every `#[migration]` registers a definition here (`crate::migrations`).
#[linkme::distributed_slice]
pub static MIGRATIONS: [MigrationDef];

/// Runs a migration on one document (the stored JSON, system fields included), inside a batch's transaction.
pub type MigrationHandler = for<'a> fn(
    &'a MutationCtx,
    serde_json::Value,
) -> BoxFuture<'a, crate::Result<crate::migrations::DocChange>>;

/// A `#[migration]` (docs/CONTRACT.md §3.3).
pub struct MigrationDef {
    /// `module_path!()` at the definition site.
    pub module_path: &'static str,
    /// The Rust fn name.
    pub fn_name: &'static str,
    /// `#[migration(name = "...")]`; without it the name is the function path (`function_path`).
    pub name: Option<&'static str>,
    /// The migrated table's name (`<T as Table>::NAME` or the string given).
    pub table: &'static str,
    /// Documents per batch (one transaction each).
    pub batch_size: usize,
    /// `#[migration(after = ["0001_x", ...])]`: the names of the migrations that must be done before this one runs.
    pub after: &'static [&'static str],
    pub handler: MigrationHandler,
}

impl MigrationDef {
    /// The migration's stable id.
    pub fn name(&self) -> String {
        match self.name {
            Some(n) => n.to_string(),
            None => function_path(self.module_path, self.fn_name),
        }
    }
}

impl std::fmt::Debug for MigrationDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MigrationDef")
            .field("name", &self.name())
            .field("table", &self.table)
            .field("batch_size", &self.batch_size)
            .field("after", &self.after)
            .finish()
    }
}

/// Deserialize a document for a `#[migration]` whose parameter is not `serde_json::Value` (e.g. `Doc<OldTodo>`).
pub fn decode_migration_doc<T: DeserializeOwned>(doc: serde_json::Value) -> crate::Result<T> {
    serde_json::from_value(doc).map_err(|e| {
        anyhow::anyhow!("the document does not deserialize as `{}`: {e}", std::any::type_name::<T>())
    })
}

/// Turn a `#[migration]`'s return value into the change to apply.
pub fn migration_output<O: crate::migrations::MigrationOutput>(out: O) -> crate::Result<crate::migrations::DocChange> {
    Ok(out.into_change())
}

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
    /// `#[query(dev_only)]` (etc.): refused unless the app runs in dev mode (`--dev`), and generated into `devApi`
    /// instead of `api`.
    pub dev_only: bool,
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
            .field("dev_only", &self.dev_only)
            .finish()
    }
}

/// `"todo_server::todos"` + `"list"` → `"todos:list"`; `"todo_server"` + `"list"` → `"list"`;
/// `"a::b::c"` + `"f"` → `"b/c:f"`. The first segment (the crate name) is dropped.
///
/// Functions defined inside the `ferrix` crate itself (auth, dashboard, ...) are namespaced under `_system`:
/// `"ferrix::auth::api"` + `"sign_in"` → `"_system/auth/api:sign_in"`. User crates can't produce `_`-prefixed module
/// names, so these never collide with app functions.
pub fn function_path(module_path: &str, name: &str) -> String {
    let mut segments = module_path.split("::");
    let krate = segments.next().unwrap_or_default();
    let mut modules: Vec<&str> = segments.collect();
    if krate == "ferrix" {
        modules.insert(0, SYSTEM_PREFIX);
    }
    if modules.is_empty() { name.to_string() } else { format!("{}:{name}", modules.join("/")) }
}

/// Path prefix (first module segment) of ferrix's built-in functions.
pub const SYSTEM_PREFIX: &str = "_system";

/// True for paths of ferrix's built-in functions (see [`function_path`]).
pub fn is_system_path(path: &str) -> bool {
    path.split(['/', ':']).next() == Some(SYSTEM_PREFIX)
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

/// `TableDef::validate` for table `T`: the document must deserialize as `Doc<T>`.
pub fn validate_doc<T: crate::Table>(doc: &serde_json::Value) -> Result<(), String> {
    <crate::Doc<T> as serde::Deserialize>::deserialize(doc).map(drop).map_err(|e| e.to_string())
}

/// The inline TS type of `T`, rendered with [`ts_config`]: the body of a table struct (`DataModel`) or of a
/// function's generated args struct.
///
/// While a codegen session is active on this thread (see [`crate::codegen`]), the named types `T` refers to are
/// also recorded, recursively, so that codegen can declare them.
///
/// A *generic* named type (a `#[derive(TS)] struct Page<T>`, e.g. [`crate::PaginationResult`]) is the exception: it
/// is referred to by name (`PaginationResult<Doc<"todos">>`) and declared, which reads better than its inlined body
/// and lets client-side types match on it.
pub fn ts_inline<T: ts_rs::TS + ?Sized + 'static>() -> String {
    let cfg = ts_config();
    if T::output_path().is_some() && T::name(cfg) != T::ident(cfg) {
        crate::codegen::collect_type::<T>();
        return T::name(cfg);
    }
    crate::codegen::collect_dependencies::<T>();
    T::inline(cfg)
}

/// The TS type of a function's output, rendered with [`ts_config`]: named types by name, also inside wrappers
/// (`Array<MessageView>`, `ChannelSummary | null`, `PaginationResult<MessageView>`), and declared in `dataModel.ts`
/// while a codegen session is active. Types without a name (`string`, tuples, maps) are written out.
pub fn ts_output<T: ts_rs::TS + ?Sized + 'static>() -> String {
    crate::codegen::collect_named::<T>();
    T::name(ts_config())
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

#[cfg(test)]
mod system_path_tests {
    use super::*;

    #[test]
    fn paths_of_app_and_system_functions() {
        assert_eq!(function_path("app::todos", "list"), "todos:list");
        assert_eq!(function_path("app", "list"), "list");
        assert_eq!(function_path("app::a::b", "f"), "a/b:f");
        assert_eq!(function_path("ferrix::auth::api", "sign_in"), "_system/auth/api:sign_in");
        assert_eq!(function_path("ferrix", "ping"), "_system:ping");
        assert!(is_system_path("_system/auth/api:sign_in"));
        assert!(is_system_path("_system:ping"));
        assert!(!is_system_path("todos:list"));
        assert!(!is_system_path("_systemx:list"));
    }

    /// The macros work inside the ferrix crate (via `extern crate self as ferrix`), may use `_` table names here,
    /// and produce `_system` paths.
    mod system_smoke {
        use crate::prelude::*;

        #[table(name = "_smoke")]
        pub struct Smoke {
            pub n: i64,
        }

        #[internal_query]
        pub async fn ping(_ctx: &QueryCtx) -> Result<String> {
            Ok("pong".into())
        }

        #[test]
        fn system_items_register() {
            use crate::{FunctionRef, Table};
            assert_eq!(Smoke::NAME, "_smoke");
            assert_eq!(ping::path(), "_system/private/system_path_tests/system_smoke:ping");
        }
    }
}
