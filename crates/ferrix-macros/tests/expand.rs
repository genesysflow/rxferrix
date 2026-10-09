//! Compiles real macro usages against the mock runtime in `tests/mock` (a stand-in for `crates/ferrix` that
//! defines exactly the CONTRACT.md §1–§2 items) and runs them: registration in the linkme slices, function paths,
//! TS signatures, handler round-trips and `run_*` with tuple args.

#![deny(warnings)]

use ferrix::__private::{FUNCTIONS, FunctionDef, Handler, TABLES, Visibility, function_path};
use ferrix::prelude::*;
use ferrix::{FunctionRef, IndexDef, Table, TableDef};
use futures::executor::block_on;
use serde_json::json;

pub mod todos {
    use ferrix::prelude::*;
    use ferrix_macros::{action, internal_mutation, internal_query, mutation, query, table};

    /// A todo item.
    #[table(name = "todos")]
    #[index(by_done = [done, created_by])]
    #[derive(PartialEq)]
    #[index(by_text = [text])]
    pub struct Todo {
        /// The text.
        pub text: String,
        pub done: bool,
        pub created_by: Option<String>,
        #[serde(rename = "prio")]
        pub priority_level: u8,
    }

    #[table]
    pub struct TodoItem {
        pub r#type: String,
    }

    /// Lists todos.
    #[query]
    pub async fn list(ctx: &QueryCtx, done: Option<bool>, limit: u32) -> Result<Vec<String>> {
        let _ = ctx.now();
        Ok(vec![format!("done={done:?}"), format!("limit={limit}")])
    }

    #[query]
    pub async fn count(_ctx: &ferrix::QueryCtx) -> ferrix::Result<u64> {
        Ok(42)
    }

    #[mutation]
    pub async fn add(ctx: &MutationCtx, text: String) -> anyhow::Result<String> {
        if text.is_empty() {
            return Err(UserError::new("empty", "text is empty").into());
        }
        Ok(format!("{}:{text}", ctx.now()))
    }

    #[action]
    pub async fn add_many(
        ctx: &ActionCtx,
        /// How many to add.
        n: u32,
        #[ts(type = "string | null")]
        #[serde(default)]
        prefix_text: Option<String>,
    ) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for i in 0..n {
            let p = prefix_text.clone().unwrap_or_default();
            out.push(ctx.run_mutation::<add>((format!("{p}{i}"),)).await?);
        }
        out.extend(ctx.run_query::<list>((Some(true), 3)).await?);
        out.push(ctx.run_query::<count>(()).await?.to_string());
        Ok(out)
    }

    #[internal_query]
    pub(crate) async fn secret(_ctx: &QueryCtx) -> Result<()> {
        Ok(())
    }

    #[internal_mutation]
    async fn bump(_ctx: &MutationCtx, mut x: i32) -> Result<i32> {
        x += 1;
        Ok(x)
    }

    #[ferrix_macros::internal_action]
    pub async fn get_doc(_ctx: &ActionCtx, id: Id<Todo>) -> Result<Option<Doc<Todo>>> {
        Ok(Some(Doc {
            id,
            creation_time: 1.5,
            value: Todo {
                text: "t".into(),
                done: true,
                created_by: None,
                priority_level: 2,
            },
        }))
    }

    /// Private function references are usable inside their module.
    pub fn bump_path() -> String {
        fn path_of<F: ferrix::MutationFunction>() -> String {
            F::path()
        }
        path_of::<bump>()
    }
}

/// `#[object]` types as outputs, and a dev-only function.
pub mod views {
    use ferrix::prelude::*;
    use ferrix_macros::{action, object};

    use super::todos::Todo;

    /// A todo as a list shows it.
    #[object]
    #[derive(Clone, Debug, PartialEq)]
    pub struct TodoView {
        pub id: Id<Todo>,
        /// The text.
        pub display_text: String,
        pub status: Status,
    }

    #[object]
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum Status {
        Open,
        InProgress,
    }

    #[object]
    #[serde(rename_all = "SCREAMING_SNAKE_CASE")]
    pub struct Shouty {
        pub some_field: u8,
    }

    #[action(dev_only)]
    pub async fn views(_ctx: &ActionCtx, status: Status) -> Result<Vec<TodoView>> {
        Ok(vec![TodoView { id: Id::from_raw([0; 16]), display_text: "t".into(), status }])
    }
}

/// Function paths from nested modules, and macros invoked through re-exported paths.
pub mod nested {
    pub mod deeper {
        use ferrix_macros as fm;

        #[fm::query]
        pub async fn deep(_ctx: &ferrix::QueryCtx) -> std::result::Result<bool, ferrix::Error> {
            Ok(true)
        }
    }
}

/// Function references live only in the type namespace, so bindings may share their names (the CONTRACT.md fix
/// for value-namespace shadowing).
pub mod shadowing {
    use ferrix::FunctionRef;
    use ferrix::prelude::*;
    use ferrix_macros::{mutation, query};

    #[query]
    pub async fn channel(_ctx: &QueryCtx) -> Result<u32> {
        Ok(1)
    }

    #[mutation]
    pub async fn post(_ctx: &MutationCtx, channel: String) -> Result<String> {
        let list = format!("{channel}!");
        Ok(list)
    }

    /// Imports both the fn and the reference struct named `list`.
    pub use super::todos::list;

    pub async fn uses_imported_list() -> Result<Vec<String>> {
        let channel = list(&QueryCtx::mock(), None, 1).await?;
        Ok(channel)
    }

    pub fn list_path() -> String {
        <list as FunctionRef>::path()
    }

    pub fn bindings() -> u32 {
        let list = 3;
        let channel = |list: u32| list + 1;
        let post = 0;
        channel(list) + post
    }
}

/// User items that shadow common names must not break the expansions (fully qualified paths, hygienic locals).
#[allow(dead_code, non_camel_case_types, unused_macros, unused_imports)]
pub mod hostile {
    use ferrix::prelude::*;
    use ferrix_macros::{query, table};

    struct Clone;
    struct Copy;
    struct Debug;
    struct Box;
    struct From;
    mod core {}
    // (`mod std {}` is not tested: the ts-rs derive itself uses unqualified `std::path::PathBuf`.)
    macro_rules! vec {
        ($($t:tt)*) => {
            compile_error!("user vec! used")
        };
    }
    macro_rules! module_path {
        () => {
            compile_error!("user module_path! used")
        };
    }
    type String = ::std::string::String;

    #[table]
    #[index(by_a = [a])]
    #[search_index(search_b, search = b, filter = [a], tokenizer = "porter", language = "english", diacritics = "keep")]
    #[search_index(search_b_sub, search = b, tokenizer = "trigram")]
    #[vector_index(by_v, field = v, dims = 2, filter = [a, b], kind = "hnsw")]
    pub struct Shadowed {
        pub a: u32,
        pub b: String,
        pub v: Option<Vec<f32>>,
    }

    // Function references named like the expansion's locals.
    #[query]
    pub async fn ctx(ctx: &QueryCtx) -> Result<u32> {
        let _ = ctx;
        Ok(1)
    }
    #[query]
    pub async fn args(_c: &QueryCtx, x: u32) -> Result<u32> {
        Ok(x)
    }
    #[query]
    pub async fn a(_c: &QueryCtx, x: u32) -> Result<u32> {
        Ok(x)
    }
    #[query]
    pub async fn out(_c: &QueryCtx, x: u32) -> Result<u32> {
        Ok(x)
    }
    #[query]
    pub async fn t(_c: &QueryCtx, x: u32, y: u32) -> Result<u32> {
        Ok(x + y)
    }
}

/// `#[migration]` in its forms: untyped (`Value`) or typed documents; patch, `DocChange` or `()` results; a table
/// by type or by name; default and explicit batch sizes and names.
pub mod data {
    use ferrix::prelude::*;
    use ferrix_macros::migration;
    use serde_json::{Value, json};

    use super::todos::Todo;

    /// Adds a default priority.
    #[migration(table = Todo)]
    pub async fn add_priority(_ctx: &MutationCtx, doc: Value) -> Result<Option<Value>> {
        Ok(doc.get("prio").is_none().then(|| json!({ "prio": 0 })))
    }

    #[derive(ferrix::__private::serde::Deserialize)]
    #[serde(crate = "ferrix::__private::serde")]
    pub struct OldTodo {
        pub title: String,
    }

    #[migration(table = "todos", batch = 500, name = "0002_rename-title")]
    pub(crate) async fn rename_title(_ctx: &MutationCtx, doc: Doc<OldTodo>) -> anyhow::Result<DocChange> {
        if doc.title.is_empty() {
            return Ok(DocChange::Delete);
        }
        Ok(DocChange::Replace(json!({ "text": doc.title, "done": false, "createdBy": null, "prio": 0 })))
    }

    #[migration(table = super::todos::TodoItem, batch = 1)]
    async fn noop(ctx: &MutationCtx, _: Value) -> ferrix::Result<()> {
        let _ = ctx.now();
        Ok(())
    }

    /// Private migration references are usable inside their module.
    pub fn noop_name() -> String {
        <noop as ferrix::migrations::MigrationRef>::name()
    }
}

fn def(name: &str) -> &'static FunctionDef {
    let mut it = FUNCTIONS.iter().filter(|f| f.name == name);
    let d = it.next().unwrap_or_else(|| panic!("{name} not registered"));
    assert!(it.next().is_none(), "{name} registered twice");
    d
}

fn run(name: &str, args: serde_json::Value) -> ferrix::Result<serde_json::Value> {
    match def(name).handler {
        Handler::Query(h) => block_on(h(ferrix::QueryCtx::mock(), args)),
        Handler::Mutation(h) => block_on(h(ferrix::MutationCtx::mock(), args)),
        Handler::Action(h) => block_on(h(ferrix::ActionCtx::mock(), args)),
    }
}

fn assert_send<T: Send>(_: &T) {}

#[test]
fn tables_are_registered() {
    let mut defs: Vec<TableDef> = TABLES.iter().map(|f| f()).collect();
    defs.sort_by_key(|d| d.name);
    let summary: Vec<(&str, Vec<IndexDef>, String)> = defs.iter().map(|d| (d.name, d.indexes.clone(), (d.ts)())).collect();
    assert_eq!(
        summary,
        vec![
            (
                "shadowed",
                vec![IndexDef { name: "by_a", fields: vec!["a"] }],
                "{ a: number, b: string, v: Array<number> | null, }".to_string(),
            ),
            ("todo_item", vec![], "{ type: string, }".to_string()),
            (
                "todos",
                vec![
                    IndexDef { name: "by_done", fields: vec!["done", "createdBy"] },
                    IndexDef { name: "by_text", fields: vec!["text"] },
                ],
                "{ \n/**\n * The text.\n */\ntext: string, done: boolean, createdBy: string | null, prio: number, }"
                    .to_string(),
            ),
        ]
    );
    // TableDef equality compares the rendered TS type, not the fn pointer.
    assert_eq!(defs[0], defs[0].clone());
    assert_ne!(defs[0], TableDef { ts: || String::new(), ..defs[0].clone() });
    assert_eq!(<todos::Todo as Table>::NAME, "todos");
    assert_eq!(<todos::TodoItem as Table>::NAME, "todo_item");
}

#[test]
fn table_validate_deserializes_a_doc() {
    let validate = todos::Todo::table_def().validate;
    let id = "00000001000000000000000000000000";
    let good = json!({ "_id": id, "_creationTime": 1.5, "text": "t", "done": false, "createdBy": null, "prio": 3 });
    assert_eq!(validate(&good), Ok(()));
    // Missing system fields, a missing user field, a wrong type.
    let mut no_id = good.clone();
    no_id.as_object_mut().unwrap().remove("_id");
    assert!(validate(&no_id).unwrap_err().contains("_id"));
    let mut no_done = good.clone();
    no_done.as_object_mut().unwrap().remove("done");
    assert!(validate(&no_done).unwrap_err().contains("done"));
    let mut bad_prio = good.clone();
    bad_prio["prio"] = json!(300);
    assert!(validate(&bad_prio).is_err());
}

#[test]
fn search_and_vector_indexes() {
    use ferrix::{SearchIndexDef, Tokenizer, TokenizerKind, VectorIndexDef, VectorIndexKind};
    use hostile::Shadowed;
    let def = Shadowed::table_def();
    assert_eq!(
        def.search_indexes,
        vec![
            SearchIndexDef {
                name: "search_b",
                search_field: "b",
                filter_fields: vec!["a"],
                tokenizer: Tokenizer { kind: TokenizerKind::Porter, language: Some("english"), remove_diacritics: false },
            },
            SearchIndexDef {
                name: "search_b_sub",
                search_field: "b",
                filter_fields: vec![],
                tokenizer: Tokenizer { kind: TokenizerKind::Trigram, language: None, remove_diacritics: true },
            },
        ]
    );
    assert_eq!(
        def.vector_indexes,
        vec![VectorIndexDef {
            name: "by_v",
            vector_field: "v",
            dimensions: 2,
            filter_fields: vec!["a", "b"],
            kind: VectorIndexKind::Hnsw,
        }]
    );
    let s: ferrix::SearchIndex<Shadowed> = Shadowed::search_b;
    assert_eq!(s.name(), "search_b");
    let v: ferrix::VectorIndex<Shadowed> = Shadowed::by_v;
    assert_eq!(v.name(), "by_v");
    // Equality covers the new fields.
    assert_ne!(def, TableDef { search_indexes: vec![], ..def.clone() });
    assert_ne!(def, TableDef { vector_indexes: vec![], ..def.clone() });
    assert_eq!(todos::Todo::table_def().search_indexes, vec![]);
}

#[test]
fn table_handles() {
    use todos::Todo;
    let f: ferrix::Field<Todo, Option<String>> = Todo::created_by;
    assert_eq!(f.name(), "createdBy");
    let f: ferrix::Field<Todo, u8> = Todo::priority_level;
    assert_eq!(f.name(), "prio");
    let f: ferrix::Field<Todo, ferrix::Id<Todo>> = Todo::_id;
    assert_eq!(f.name(), "_id");
    let f: ferrix::Field<Todo, f64> = Todo::_creation_time;
    assert_eq!(f.name(), "_creationTime");
    let i: ferrix::Index<Todo> = Todo::by_done;
    assert_eq!(i.name(), "by_done");
    assert_eq!(Todo::by_creation_time.name(), "by_creation_time");
    assert_eq!(todos::TodoItem::r#type.name(), "type");
}

#[test]
fn table_serde_ts_and_derives() {
    use todos::Todo;
    let t = Todo {
        text: "x".into(),
        done: false,
        created_by: Some("me".into()),
        priority_level: 3,
    };
    let v = serde_json::to_value(&t).unwrap();
    assert_eq!(
        v,
        json!({"text": "x", "done": false, "createdBy": "me", "prio": 3})
    );
    let back: Todo = serde_json::from_value(v).unwrap();
    assert_eq!(back, t.clone()); // PartialEq (user derive) + Clone (added)
    let _ = format!("{t:?}"); // Debug (added)
    assert_eq!(
        ferrix::__private::ts_inline::<Todo>(),
        "{ \n/**\n * The text.\n */\ntext: string, done: boolean, createdBy: string | null, prio: number, }"
    );
    // Doc comments are kept (ts-rs picks them up).
    assert_eq!(
        <Todo as ferrix::__private::ts_rs::TS>::docs().as_deref(),
        Some("/**\n * A todo item.\n */\n")
    );
}

#[test]
fn functions_are_registered() {
    let mut names: Vec<_> = FUNCTIONS.iter().map(|f| (f.module_path, f.name)).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            ("expand::hostile", "a"),
            ("expand::hostile", "args"),
            ("expand::hostile", "ctx"),
            ("expand::hostile", "out"),
            ("expand::hostile", "t"),
            ("expand::nested::deeper", "deep"),
            ("expand::shadowing", "channel"),
            ("expand::shadowing", "post"),
            ("expand::todos", "add"),
            ("expand::todos", "add_many"),
            ("expand::todos", "bump"),
            ("expand::todos", "count"),
            ("expand::todos", "get_doc"),
            ("expand::todos", "list"),
            ("expand::todos", "secret"),
            ("expand::views", "views"),
        ]
    );
    assert!(def("views").dev_only);
    assert!(FUNCTIONS.iter().filter(|f| f.name != "views").all(|f| !f.dev_only));
    use ferrix::__private::FunctionKind::*;
    for (name, kind, vis) in [
        ("list", Query, Visibility::Public),
        ("count", Query, Visibility::Public),
        ("add", Mutation, Visibility::Public),
        ("add_many", Action, Visibility::Public),
        ("secret", Query, Visibility::Internal),
        ("bump", Mutation, Visibility::Internal),
        ("get_doc", Action, Visibility::Internal),
    ] {
        let d = def(name);
        assert_eq!(d.handler.kind(), kind, "{name}");
        assert_eq!(d.visibility, vis, "{name}");
    }
}

#[test]
fn function_paths() {
    assert_eq!(todos::list::path(), "todos:list");
    assert_eq!(<todos::add_many as FunctionRef>::path(), "todos:add_many");
    assert_eq!(nested::deeper::deep::path(), "nested/deeper:deep");
    assert_eq!(
        function_path(def("deep").module_path, "deep"),
        "nested/deeper:deep"
    );
}

#[test]
fn ts_signatures() {
    let ts = |n| (def(n).ts)();
    assert_eq!(
        ts("list"),
        (
            "{ done?: boolean | null, limit: number, }".to_owned(),
            "Array<string>".to_owned()
        )
    );
    // ts-rs renders an empty struct as `Record<symbol, never>`.
    assert_eq!(
        ts("count"),
        ("Record<symbol, never>".to_owned(), "bigint".to_owned())
    );
    // Param doc comments and `#[ts]` / `#[serde]` param attributes move to the args struct fields.
    assert_eq!(
        ts("add_many").0,
        "{ \n/**\n * How many to add.\n */\nn: number, prefixText: string | null, }"
    );
    assert_eq!(ts("secret").1, "null");
    assert_eq!(
        ts("get_doc"),
        (
            "{ id: Id<\"todos\">, }".to_owned(),
            "Doc<\"todos\"> | null".to_owned()
        )
    );
}

#[test]
fn objects_serialize_camel_case_and_are_named_in_ts() {
    use ferrix::__private::ts_rs::TS;
    use views::{Shouty, Status, TodoView};
    let cfg = ferrix::__private::ts_rs::Config::default();
    // The output is referred to by name, also inside a Vec.
    assert_eq!((def("views").ts)().1, "Array<TodoView>");
    assert_eq!(
        TodoView::decl(&cfg),
        "type TodoView = { id: Id<\"todos\">, \n/**\n * The text.\n */\ndisplayText: string, status: Status, };"
    );
    assert_eq!(Status::decl(&cfg), "type Status = \"open\" | \"inProgress\";");
    assert_eq!(Shouty::inline(&cfg), "{ SOME_FIELD: number, }");
    let out = run("views", json!({"status": "inProgress"})).unwrap();
    assert_eq!(out[0]["displayText"], "t");
    assert_eq!(out[0]["status"], "inProgress");
    let back: Vec<TodoView> = serde_json::from_value(out).unwrap();
    assert_eq!(back[0].status, Status::InProgress);
    assert_eq!(serde_json::to_value(Shouty { some_field: 1 }).unwrap(), json!({"SOME_FIELD": 1}));
}

#[test]
fn handlers_round_trip() {
    assert_eq!(
        run("list", json!({"done": true, "limit": 2})).unwrap(),
        json!(["done=Some(true)", "limit=2"])
    );
    // Missing Option arg = None.
    assert_eq!(
        run("list", json!({"limit": 2})).unwrap(),
        json!(["done=None", "limit=2"])
    );
    assert_eq!(run("count", json!({})).unwrap(), json!(42));
    assert_eq!(run("bump", json!({"x": 41})).unwrap(), json!(42));
    assert_eq!(run("t", json!({"x": 1, "y": 2})).unwrap(), json!(3));
    let err = run("list", json!({"done": true})).unwrap_err();
    assert!(
        err.to_string().starts_with("Invalid args for list: "),
        "{err}"
    );
    let err = run("add", json!({"text": ""})).unwrap_err();
    assert_eq!(err.downcast_ref::<UserError>().unwrap().code, "empty");
    // camelCase arg names on the wire.
    let out = run("add_many", json!({"n": 2, "prefixText": "p"})).unwrap();
    assert_eq!(
        out,
        json!(["2:p0", "2:p1", "done=Some(true)", "limit=3", "42"])
    );
    let id = "0000000700000000000000000000002a";
    let out = run("get_doc", json!({"id": id})).unwrap();
    assert_eq!(
        out,
        json!({"_id": id, "_creationTime": 1.5, "text": "t", "done": true, "createdBy": null, "prio": 2})
    );
}

#[test]
fn handler_futures_are_send_and_static() {
    let Handler::Query(h) = def("list").handler else {
        panic!()
    };
    let fut = h(ferrix::QueryCtx::mock(), json!({"limit": 1}));
    assert_send(&fut);
    fn is_static<T: 'static>(_: T) {}
    is_static(fut);
}

#[test]
fn args_structs_and_tuple_conversions() {
    type ListArgs = <todos::list as FunctionRef>::Args;
    let a: ListArgs = (Some(false), 7).into();
    assert_eq!(
        serde_json::to_value(&a).unwrap(),
        json!({"done": false, "limit": 7})
    );
    let a: <todos::add as FunctionRef>::Args = ("hi".to_owned(),).into();
    assert_eq!(a.text, "hi");
    let _: <todos::count as FunctionRef>::Args = ().into();
    assert_eq!(todos::bump_path(), "todos:bump");
}

#[test]
fn migrations_are_registered_and_run() {
    use ferrix::__private::MIGRATIONS;
    use ferrix::migrations::{DocChange, MigrationRef};

    let find = |fn_name: &str| {
        let mut it = MIGRATIONS.iter().filter(|m| m.fn_name == fn_name);
        let d = it.next().unwrap_or_else(|| panic!("{fn_name} not registered"));
        assert!(it.next().is_none());
        d
    };
    let add = find("add_priority");
    assert_eq!((add.name(), add.table, add.batch_size), ("data:add_priority".to_string(), "todos", 100));
    let rename = find("rename_title");
    assert_eq!((rename.name(), rename.table, rename.batch_size), ("0002_rename-title".to_string(), "todos", 500));
    let noop = find("noop");
    assert_eq!((noop.table, noop.batch_size), ("todo_item", 1));
    assert_eq!(MIGRATIONS.len(), 3);

    assert_eq!(<data::add_priority as MigrationRef>::name(), "data:add_priority");
    assert_eq!(<data::rename_title as MigrationRef>::table(), "todos");
    assert_eq!(data::noop_name(), "data:noop");

    let ctx = ferrix::MutationCtx::mock();
    let doc = json!({ "_id": "00".repeat(16), "_creationTime": 1.0, "title": "hi" });
    assert_eq!(block_on((add.handler)(&ctx, doc.clone())).unwrap(), DocChange::Patch(json!({ "prio": 0 })));
    assert_eq!(block_on((add.handler)(&ctx, json!({ "prio": 3 }))).unwrap(), DocChange::Keep);
    assert_eq!(
        block_on((rename.handler)(&ctx, doc.clone())).unwrap(),
        DocChange::Replace(json!({ "text": "hi", "done": false, "createdBy": null, "prio": 0 }))
    );
    let empty = json!({ "_id": "00".repeat(16), "_creationTime": 1.0, "title": "" });
    assert_eq!(block_on((rename.handler)(&ctx, empty)).unwrap(), DocChange::Delete);
    // A document that doesn't deserialize as the typed parameter is an error (the runtime fails the migration).
    let err = block_on((rename.handler)(&ctx, json!({ "text": "no title" }))).unwrap_err();
    assert!(err.to_string().contains("does not deserialize"), "{err}");
    assert_eq!(block_on((noop.handler)(&ctx, json!({}))).unwrap(), DocChange::Keep);
    // The user's fn stays callable.
    assert_eq!(block_on(data::add_priority(&ctx, json!({}))).unwrap(), Some(json!({ "prio": 0 })));
    assert_send(&(add.handler)(&ctx, json!({})));
}

#[test]
fn user_fns_stay_callable_and_shadowing_works() {
    // The user's fn keeps its name, visibility and signature.
    let out = block_on(todos::list(&ferrix::QueryCtx::mock(), Some(true), 3)).unwrap();
    assert_eq!(out, vec!["done=Some(true)", "limit=3"]);
    assert_eq!(
        block_on(shadowing::channel(&ferrix::QueryCtx::mock())).unwrap(),
        1
    );
    assert_eq!(
        block_on(shadowing::post(&ferrix::MutationCtx::mock(), "c".into())).unwrap(),
        "c!"
    );
    assert_eq!(shadowing::bindings(), 4);
    // ... and the type-namespace reference sits beside it.
    assert_eq!(<shadowing::post as FunctionRef>::path(), "shadowing:post");
    assert_eq!(run("post", json!({"channel": "x"})).unwrap(), json!("x!"));
    assert_eq!(shadowing::list_path(), "todos:list");
    assert_eq!(
        block_on(shadowing::uses_imported_list()).unwrap(),
        vec!["done=None", "limit=1"]
    );
    assert_eq!(
        block_on(ferrix::ActionCtx::mock().run_query::<shadowing::channel>(())).unwrap(),
        1
    );
}
