//! The contract's example module. The table and `list` / `add` / `add_many` are expanded literally as in §3.

use rxferrix::prelude::*;

// ---- #[table(name = "todos")] #[index(by_done = [done, created_by])] #[index(by_text = [text])] ----

#[derive(
    ::rxferrix::__private::serde::Serialize,
    ::rxferrix::__private::serde::Deserialize,
    ::rxferrix::__private::ts_rs::TS,
    ::core::clone::Clone,
    ::core::fmt::Debug,
)]
#[serde(crate = "::rxferrix::__private::serde", rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct Todo {
    pub text: String,
    pub done: bool,
    pub created_by: Option<String>,
}

impl ::rxferrix::Table for Todo {
    const NAME: &'static str = "todos";
    fn table_def() -> ::rxferrix::TableDef {
        ::rxferrix::TableDef {
            name: "todos",
            indexes: vec![
                ::rxferrix::IndexDef { name: "by_done", fields: vec!["done", "createdBy"] },
                ::rxferrix::IndexDef { name: "by_text", fields: vec!["text"] },
            ],
            search_indexes: vec![],
            vector_indexes: vec![],
            ts: || ::rxferrix::__private::ts_inline::<Todo>(),
            validate: ::rxferrix::__private::validate_doc::<Todo>,
            shape: || {
                let mut s = ::rxferrix::shape::StructShape::default();
                s.field("text", ::rxferrix::shape::Shape::Text);
                s.field("done", ::rxferrix::shape::Shape::Bool);
                s.field("createdBy", ::rxferrix::shape::Shape::Text.optional());
                ::rxferrix::shape::Shape::Struct(s)
            },
            aggregates: vec![],
        }
    }
}

#[allow(non_upper_case_globals)]
impl Todo {
    pub const text: ::rxferrix::Field<Todo, String> = ::rxferrix::Field::new("text");
    pub const done: ::rxferrix::Field<Todo, bool> = ::rxferrix::Field::new("done");
    pub const created_by: ::rxferrix::Field<Todo, Option<String>> = ::rxferrix::Field::new("createdBy");
    pub const _id: ::rxferrix::Field<Todo, ::rxferrix::Id<Todo>> = ::rxferrix::Field::new("_id");
    pub const _creation_time: ::rxferrix::Field<Todo, f64> = ::rxferrix::Field::new("_creationTime");
    pub const by_done: ::rxferrix::Index<Todo> = ::rxferrix::Index::new("by_done");
    pub const by_text: ::rxferrix::Index<Todo> = ::rxferrix::Index::new("by_text");
    pub const by_creation_time: ::rxferrix::Index<Todo> = ::rxferrix::Index::new("by_creation_time");
}

#[::rxferrix::__private::linkme::distributed_slice(::rxferrix::__private::TABLES)]
#[linkme(crate = ::rxferrix::__private::linkme)]
static __RXFERRIX_TABLE_Todo: fn() -> ::rxferrix::TableDef = <Todo as ::rxferrix::Table>::table_def;

// ---- #[query] list ----

/// Lists todos.
pub async fn list(ctx: &QueryCtx, done: Option<bool>) -> Result<Vec<Doc<Todo>>> {
    match done {
        Some(d) => ctx.db.query::<Todo>().with_index(Todo::by_done, |q| q.eq(Todo::done, d)).collect().await,
        None => ctx.db.query::<Todo>().order(Order::Desc).take(100).await,
    }
}

#[allow(non_camel_case_types)]
#[derive(
    ::rxferrix::__private::serde::Serialize, ::rxferrix::__private::serde::Deserialize, ::rxferrix::__private::ts_rs::TS,
)]
#[serde(crate = "::rxferrix::__private::serde", rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase", optional_fields = nullable)]
pub struct __rxferrix_args_list {
    pub done: Option<bool>,
}

impl ::core::convert::From<(Option<bool>,)> for __rxferrix_args_list {
    fn from((done,): (Option<bool>,)) -> Self {
        Self { done }
    }
}

/// Function reference for [`list`](fn@list).
#[allow(non_camel_case_types, dead_code)]
pub struct list {}

impl ::rxferrix::FunctionRef for list {
    type Args = __rxferrix_args_list;
    type Output = Vec<Doc<Todo>>;
    fn path() -> String {
        ::rxferrix::__private::function_path(module_path!(), "list")
    }
}
impl ::rxferrix::QueryFunction for list {}

#[::rxferrix::__private::linkme::distributed_slice(::rxferrix::__private::FUNCTIONS)]
#[linkme(crate = ::rxferrix::__private::linkme)]
static __RXFERRIX_FN_list: ::rxferrix::__private::FunctionDef = ::rxferrix::__private::FunctionDef {
    module_path: module_path!(),
    name: "list",
    visibility: ::rxferrix::__private::Visibility::Public,
    dev_only: false,
    docs: "Lists todos.",
    handler: ::rxferrix::__private::Handler::Query(|ctx, args| {
        ::std::boxed::Box::pin(async move {
            let a: __rxferrix_args_list = ::rxferrix::__private::decode_args("list", args)?;
            let out = list(&ctx, a.done).await?;
            ::rxferrix::__private::encode_output(out)
        })
    }),
    ts: || {
        (
            ::rxferrix::__private::ts_inline::<__rxferrix_args_list>(),
            ::rxferrix::__private::ts_output::<Vec<Doc<Todo>>>(),
        )
    },
};

// ---- #[mutation] add ----

pub async fn add(ctx: &MutationCtx, text: String) -> Result<Id<Todo>> {
    if text.is_empty() {
        return Err(UserError::new("empty", "text is empty").with_data(json!({ "min": 1 })).into());
    }
    ctx.db.insert(Todo { text, done: false, created_by: None }).await
}

#[allow(non_camel_case_types)]
#[derive(
    ::rxferrix::__private::serde::Serialize, ::rxferrix::__private::serde::Deserialize, ::rxferrix::__private::ts_rs::TS,
)]
#[serde(crate = "::rxferrix::__private::serde", rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase", optional_fields = nullable)]
pub struct __rxferrix_args_add {
    pub text: String,
}

impl ::core::convert::From<(String,)> for __rxferrix_args_add {
    fn from((text,): (String,)) -> Self {
        Self { text }
    }
}

#[allow(non_camel_case_types, dead_code)]
pub struct add {}

impl ::rxferrix::FunctionRef for add {
    type Args = __rxferrix_args_add;
    type Output = Id<Todo>;
    fn path() -> String {
        ::rxferrix::__private::function_path(module_path!(), "add")
    }
}
impl ::rxferrix::MutationFunction for add {}
impl ::rxferrix::SchedulableFunction for add {}

#[::rxferrix::__private::linkme::distributed_slice(::rxferrix::__private::FUNCTIONS)]
#[linkme(crate = ::rxferrix::__private::linkme)]
static __RXFERRIX_FN_add: ::rxferrix::__private::FunctionDef = ::rxferrix::__private::FunctionDef {
    module_path: module_path!(),
    name: "add",
    visibility: ::rxferrix::__private::Visibility::Public,
    dev_only: false,
    docs: "",
    handler: ::rxferrix::__private::Handler::Mutation(|ctx, args| {
        ::std::boxed::Box::pin(async move {
            let a: __rxferrix_args_add = ::rxferrix::__private::decode_args("add", args)?;
            let out = add(&ctx, a.text).await?;
            ::rxferrix::__private::encode_output(out)
        })
    }),
    ts: || (::rxferrix::__private::ts_inline::<__rxferrix_args_add>(), ::rxferrix::__private::ts_output::<Id<Todo>>()),
};

// ---- #[action] add_many ----

pub async fn add_many(ctx: &ActionCtx, n: u32) -> Result<()> {
    for i in 0..n {
        ctx.run_mutation::<add>((format!("item {i}"),)).await?;
    }
    Ok(())
}

#[allow(non_camel_case_types)]
#[derive(
    ::rxferrix::__private::serde::Serialize, ::rxferrix::__private::serde::Deserialize, ::rxferrix::__private::ts_rs::TS,
)]
#[serde(crate = "::rxferrix::__private::serde", rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase", optional_fields = nullable)]
pub struct __rxferrix_args_add_many {
    pub n: u32,
}

impl ::core::convert::From<(u32,)> for __rxferrix_args_add_many {
    fn from((n,): (u32,)) -> Self {
        Self { n }
    }
}

#[allow(non_camel_case_types, dead_code)]
pub struct add_many {}

impl ::rxferrix::FunctionRef for add_many {
    type Args = __rxferrix_args_add_many;
    type Output = ();
    fn path() -> String {
        ::rxferrix::__private::function_path(module_path!(), "add_many")
    }
}
impl ::rxferrix::ActionFunction for add_many {}
impl ::rxferrix::SchedulableFunction for add_many {}

#[::rxferrix::__private::linkme::distributed_slice(::rxferrix::__private::FUNCTIONS)]
#[linkme(crate = ::rxferrix::__private::linkme)]
static __RXFERRIX_FN_add_many: ::rxferrix::__private::FunctionDef = ::rxferrix::__private::FunctionDef {
    module_path: module_path!(),
    name: "add_many",
    visibility: ::rxferrix::__private::Visibility::Public,
    dev_only: false,
    docs: "",
    handler: ::rxferrix::__private::Handler::Action(|ctx, args| {
        ::std::boxed::Box::pin(async move {
            let a: __rxferrix_args_add_many = ::rxferrix::__private::decode_args("add_many", args)?;
            let out = add_many(&ctx, a.n).await?;
            ::rxferrix::__private::encode_output(out)
        })
    }),
    ts: || (::rxferrix::__private::ts_inline::<__rxferrix_args_add_many>(), ::rxferrix::__private::ts_output::<()>()),
};

// ---- More functions, via the macro_rules stand-in ----

/// Latest `n` todos (by_creation_time, descending, take n).
pub async fn latest(ctx: &QueryCtx, n: u32) -> Result<Vec<Doc<Todo>>> {
    ctx.db.query::<Todo>().order(Order::Desc).take(n as usize).await
}
crate::expand_fn!(Query, QueryFunction, Public, latest, __rxferrix_args_latest, __RXFERRIX_FN_latest, (n: u32) -> Vec<Doc<Todo>>);

pub async fn get(ctx: &QueryCtx, id: Id<Todo>) -> Result<Option<Doc<Todo>>> {
    ctx.db.get(id).await
}
crate::expand_fn!(Query, QueryFunction, Public, get, __rxferrix_args_get, __RXFERRIX_FN_get, (id: Id<Todo>) -> Option<Doc<Todo>>);

/// Fails with a user error until a todo with this text exists.
pub async fn find(ctx: &QueryCtx, text: String) -> Result<Doc<Todo>> {
    let found = ctx.db.query::<Todo>().with_index(Todo::by_text, |q| q.eq(Todo::text, text.clone())).unique().await?;
    found.ok_or_else(|| UserError::new("not_found", format!("no todo {text:?}")).into())
}
crate::expand_fn!(Query, QueryFunction, Public, find, __rxferrix_args_find, __RXFERRIX_FN_find, (text: String) -> Doc<Todo>);

pub async fn set_done(ctx: &MutationCtx, id: Id<Todo>, done: bool) -> Result<()> {
    ctx.db.patch(id, json!({ "done": done })).await
}
crate::expand_fn!(Mutation, MutationFunction, Public, set_done, __rxferrix_args_set_done, __RXFERRIX_FN_set_done, (id: Id<Todo>, done: bool) -> ());

pub async fn rename(ctx: &MutationCtx, id: Id<Todo>, text: String) -> Result<()> {
    let old = ctx.db.get(id).await?.ok_or_else(|| UserError::new("not_found", "no such todo"))?;
    ctx.db.replace(id, Todo { text, ..old.value }).await
}
crate::expand_fn!(Mutation, MutationFunction, Public, rename, __rxferrix_args_rename, __RXFERRIX_FN_rename, (id: Id<Todo>, text: String) -> ());

pub async fn remove(ctx: &MutationCtx, id: Id<Todo>) -> Result<()> {
    ctx.db.delete(id).await
}
crate::expand_fn!(Mutation, MutationFunction, Public, remove, __rxferrix_args_remove, __RXFERRIX_FN_remove, (id: Id<Todo>) -> ());

/// Inserts, reads its own write, then fails: everything must be rolled back.
pub async fn add_then_fail(ctx: &MutationCtx, text: String) -> Result<()> {
    let id = ctx.db.insert(Todo { text, done: false, created_by: None }).await?;
    anyhow::ensure!(ctx.db.get(id).await?.is_some(), "own write not visible");
    Err(UserError::new("nope", "failed on purpose").into())
}
crate::expand_fn!(Mutation, MutationFunction, Public, add_then_fail, __rxferrix_args_add_then_fail, __RXFERRIX_FN_add_then_fail, (text: String) -> ());

pub async fn add_then_panic(ctx: &MutationCtx, text: String) -> Result<()> {
    ctx.db.insert(Todo { text, done: false, created_by: None }).await?;
    panic!("boom");
}
crate::expand_fn!(Mutation, MutationFunction, Public, add_then_panic, __rxferrix_args_add_then_panic, __RXFERRIX_FN_add_then_panic, (text: String) -> ());

/// Inserts a few todos and returns what a query inside the same mutation sees.
pub async fn add_and_list(ctx: &MutationCtx, texts: Vec<String>) -> Result<Vec<String>> {
    for text in texts {
        ctx.db.insert(Todo { text, done: false, created_by: None }).await?;
    }
    let all = ctx.db.query::<Todo>().collect().await?;
    Ok(all.into_iter().map(|d| d.value.text).collect())
}
crate::expand_fn!(Mutation, MutationFunction, Public, add_and_list, __rxferrix_args_add_and_list, __RXFERRIX_FN_add_and_list, (texts: Vec<String>) -> Vec<String>);

/// A mutation that writes nothing.
pub async fn noop(_ctx: &MutationCtx) -> Result<u32> {
    Ok(7)
}
crate::expand_fn!(Mutation, MutationFunction, Public, noop, __rxferrix_args_noop, __RXFERRIX_FN_noop, () -> u32);

pub async fn count(ctx: &QueryCtx) -> Result<usize> {
    Ok(ctx.db.query::<Todo>().collect().await?.len())
}
crate::expand_fn!(Query, QueryFunction, Internal, count, __rxferrix_args_count, __RXFERRIX_FN_count, () -> usize);

/// A public action calling an internal query.
pub async fn count_via_action(ctx: &ActionCtx) -> Result<usize> {
    ctx.run_query::<count>(()).await
}
crate::expand_fn!(Action, ActionFunction, Public, count_via_action, __rxferrix_args_count_via_action, __RXFERRIX_FN_count_via_action, () -> usize);

pub async fn server_error(_ctx: &QueryCtx) -> Result<()> {
    anyhow::bail!("secret internal detail")
}
crate::expand_fn!(Query, QueryFunction, Public, server_error, __rxferrix_args_server_error, __RXFERRIX_FN_server_error, () -> ());
