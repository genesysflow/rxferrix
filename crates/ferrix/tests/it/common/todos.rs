//! The contract's example module. The table and `list` / `add` / `add_many` are expanded literally as in §3.

use ferrix::prelude::*;

// ---- #[table(name = "todos")] #[index(by_done = [done, created_by])] #[index(by_text = [text])] ----

#[derive(::ferrix::__private::serde::Serialize, ::ferrix::__private::serde::Deserialize,
         ::ferrix::__private::ts_rs::TS, ::core::clone::Clone, ::core::fmt::Debug)]
#[serde(crate = "::ferrix::__private::serde", rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct Todo { pub text: String, pub done: bool, pub created_by: Option<String> }

impl ::ferrix::Table for Todo {
    const NAME: &'static str = "todos";
    fn table_def() -> ::ferrix::TableDef {
        ::ferrix::TableDef { name: "todos", indexes: vec![
            ::ferrix::IndexDef { name: "by_done", fields: vec!["done", "createdBy"] },
            ::ferrix::IndexDef { name: "by_text", fields: vec!["text"] },
        ], search_indexes: vec![], vector_indexes: vec![],
        ts: || ::ferrix::__private::ts_inline::<Todo>(),
        validate: ::ferrix::__private::validate_doc::<Todo>,
        shape: || {
            let mut s = ::ferrix::shape::StructShape::default();
            s.field("text", ::ferrix::shape::Shape::Text);
            s.field("done", ::ferrix::shape::Shape::Bool);
            s.field("createdBy", ::ferrix::shape::Shape::Text.optional());
            ::ferrix::shape::Shape::Struct(s)
        },
        aggregates: vec![] }
    }
}

#[allow(non_upper_case_globals)]
impl Todo {
    pub const text: ::ferrix::Field<Todo, String> = ::ferrix::Field::new("text");
    pub const done: ::ferrix::Field<Todo, bool> = ::ferrix::Field::new("done");
    pub const created_by: ::ferrix::Field<Todo, Option<String>> = ::ferrix::Field::new("createdBy");
    pub const _id: ::ferrix::Field<Todo, ::ferrix::Id<Todo>> = ::ferrix::Field::new("_id");
    pub const _creation_time: ::ferrix::Field<Todo, f64> = ::ferrix::Field::new("_creationTime");
    pub const by_done: ::ferrix::Index<Todo> = ::ferrix::Index::new("by_done");
    pub const by_text: ::ferrix::Index<Todo> = ::ferrix::Index::new("by_text");
    pub const by_creation_time: ::ferrix::Index<Todo> = ::ferrix::Index::new("by_creation_time");
}

#[::ferrix::__private::linkme::distributed_slice(::ferrix::__private::TABLES)]
#[linkme(crate = ::ferrix::__private::linkme)]
static __FERRIX_TABLE_Todo: fn() -> ::ferrix::TableDef = <Todo as ::ferrix::Table>::table_def;

// ---- #[query] list ----

/// Lists todos.
pub async fn list(ctx: &QueryCtx, done: Option<bool>) -> Result<Vec<Doc<Todo>>> {
    match done {
        Some(d) => ctx.db.query::<Todo>().with_index(Todo::by_done, |q| q.eq(Todo::done, d)).collect().await,
        None => ctx.db.query::<Todo>().order(Order::Desc).take(100).await,
    }
}

#[allow(non_camel_case_types)]
#[derive(::ferrix::__private::serde::Serialize, ::ferrix::__private::serde::Deserialize, ::ferrix::__private::ts_rs::TS)]
#[serde(crate = "::ferrix::__private::serde", rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase", optional_fields = nullable)]
pub struct __ferrix_args_list { pub done: Option<bool> }

impl ::core::convert::From<(Option<bool>,)> for __ferrix_args_list {
    fn from((done,): (Option<bool>,)) -> Self { Self { done } }
}

/// Function reference for [`list`](fn@list).
#[allow(non_camel_case_types, dead_code)]
pub struct list {}

impl ::ferrix::FunctionRef for list {
    type Args = __ferrix_args_list;
    type Output = Vec<Doc<Todo>>;
    fn path() -> String { ::ferrix::__private::function_path(module_path!(), "list") }
}
impl ::ferrix::QueryFunction for list {}

#[::ferrix::__private::linkme::distributed_slice(::ferrix::__private::FUNCTIONS)]
#[linkme(crate = ::ferrix::__private::linkme)]
static __FERRIX_FN_list: ::ferrix::__private::FunctionDef = ::ferrix::__private::FunctionDef {
    module_path: module_path!(),
    name: "list",
    visibility: ::ferrix::__private::Visibility::Public,
    dev_only: false,
    handler: ::ferrix::__private::Handler::Query(|ctx, args| ::std::boxed::Box::pin(async move {
        let a: __ferrix_args_list = ::ferrix::__private::decode_args("list", args)?;
        let out = list(&ctx, a.done).await?;
        ::ferrix::__private::encode_output(out)
    })),
    ts: || (::ferrix::__private::ts_inline::<__ferrix_args_list>(),
            ::ferrix::__private::ts_output::<Vec<Doc<Todo>>>()),
};

// ---- #[mutation] add ----

pub async fn add(ctx: &MutationCtx, text: String) -> Result<Id<Todo>> {
    if text.is_empty() {
        return Err(UserError::new("empty", "text is empty").with_data(json!({ "min": 1 })).into());
    }
    ctx.db.insert(Todo { text, done: false, created_by: None }).await
}

#[allow(non_camel_case_types)]
#[derive(::ferrix::__private::serde::Serialize, ::ferrix::__private::serde::Deserialize, ::ferrix::__private::ts_rs::TS)]
#[serde(crate = "::ferrix::__private::serde", rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase", optional_fields = nullable)]
pub struct __ferrix_args_add { pub text: String }

impl ::core::convert::From<(String,)> for __ferrix_args_add {
    fn from((text,): (String,)) -> Self { Self { text } }
}

#[allow(non_camel_case_types, dead_code)]
pub struct add {}

impl ::ferrix::FunctionRef for add {
    type Args = __ferrix_args_add;
    type Output = Id<Todo>;
    fn path() -> String { ::ferrix::__private::function_path(module_path!(), "add") }
}
impl ::ferrix::MutationFunction for add {}
impl ::ferrix::SchedulableFunction for add {}

#[::ferrix::__private::linkme::distributed_slice(::ferrix::__private::FUNCTIONS)]
#[linkme(crate = ::ferrix::__private::linkme)]
static __FERRIX_FN_add: ::ferrix::__private::FunctionDef = ::ferrix::__private::FunctionDef {
    module_path: module_path!(),
    name: "add",
    visibility: ::ferrix::__private::Visibility::Public,
    dev_only: false,
    handler: ::ferrix::__private::Handler::Mutation(|ctx, args| ::std::boxed::Box::pin(async move {
        let a: __ferrix_args_add = ::ferrix::__private::decode_args("add", args)?;
        let out = add(&ctx, a.text).await?;
        ::ferrix::__private::encode_output(out)
    })),
    ts: || (::ferrix::__private::ts_inline::<__ferrix_args_add>(), ::ferrix::__private::ts_output::<Id<Todo>>()),
};

// ---- #[action] add_many ----

pub async fn add_many(ctx: &ActionCtx, n: u32) -> Result<()> {
    for i in 0..n {
        ctx.run_mutation::<add>((format!("item {i}"),)).await?;
    }
    Ok(())
}

#[allow(non_camel_case_types)]
#[derive(::ferrix::__private::serde::Serialize, ::ferrix::__private::serde::Deserialize, ::ferrix::__private::ts_rs::TS)]
#[serde(crate = "::ferrix::__private::serde", rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase", optional_fields = nullable)]
pub struct __ferrix_args_add_many { pub n: u32 }

impl ::core::convert::From<(u32,)> for __ferrix_args_add_many {
    fn from((n,): (u32,)) -> Self { Self { n } }
}

#[allow(non_camel_case_types, dead_code)]
pub struct add_many {}

impl ::ferrix::FunctionRef for add_many {
    type Args = __ferrix_args_add_many;
    type Output = ();
    fn path() -> String { ::ferrix::__private::function_path(module_path!(), "add_many") }
}
impl ::ferrix::ActionFunction for add_many {}
impl ::ferrix::SchedulableFunction for add_many {}

#[::ferrix::__private::linkme::distributed_slice(::ferrix::__private::FUNCTIONS)]
#[linkme(crate = ::ferrix::__private::linkme)]
static __FERRIX_FN_add_many: ::ferrix::__private::FunctionDef = ::ferrix::__private::FunctionDef {
    module_path: module_path!(),
    name: "add_many",
    visibility: ::ferrix::__private::Visibility::Public,
    dev_only: false,
    handler: ::ferrix::__private::Handler::Action(|ctx, args| ::std::boxed::Box::pin(async move {
        let a: __ferrix_args_add_many = ::ferrix::__private::decode_args("add_many", args)?;
        let out = add_many(&ctx, a.n).await?;
        ::ferrix::__private::encode_output(out)
    })),
    ts: || (::ferrix::__private::ts_inline::<__ferrix_args_add_many>(), ::ferrix::__private::ts_output::<()>()),
};

// ---- More functions, via the macro_rules stand-in ----

/// Latest `n` todos (by_creation_time, descending, take n).
pub async fn latest(ctx: &QueryCtx, n: u32) -> Result<Vec<Doc<Todo>>> {
    ctx.db.query::<Todo>().order(Order::Desc).take(n as usize).await
}
crate::expand_fn!(Query, QueryFunction, Public, latest, __ferrix_args_latest, __FERRIX_FN_latest, (n: u32) -> Vec<Doc<Todo>>);

pub async fn get(ctx: &QueryCtx, id: Id<Todo>) -> Result<Option<Doc<Todo>>> {
    ctx.db.get(id).await
}
crate::expand_fn!(Query, QueryFunction, Public, get, __ferrix_args_get, __FERRIX_FN_get, (id: Id<Todo>) -> Option<Doc<Todo>>);

/// Fails with a user error until a todo with this text exists.
pub async fn find(ctx: &QueryCtx, text: String) -> Result<Doc<Todo>> {
    let found = ctx.db.query::<Todo>().with_index(Todo::by_text, |q| q.eq(Todo::text, text.clone())).unique().await?;
    found.ok_or_else(|| UserError::new("not_found", format!("no todo {text:?}")).into())
}
crate::expand_fn!(Query, QueryFunction, Public, find, __ferrix_args_find, __FERRIX_FN_find, (text: String) -> Doc<Todo>);

pub async fn set_done(ctx: &MutationCtx, id: Id<Todo>, done: bool) -> Result<()> {
    ctx.db.patch(id, json!({ "done": done })).await
}
crate::expand_fn!(Mutation, MutationFunction, Public, set_done, __ferrix_args_set_done, __FERRIX_FN_set_done, (id: Id<Todo>, done: bool) -> ());

pub async fn rename(ctx: &MutationCtx, id: Id<Todo>, text: String) -> Result<()> {
    let old = ctx.db.get(id).await?.ok_or_else(|| UserError::new("not_found", "no such todo"))?;
    ctx.db.replace(id, Todo { text, ..old.value }).await
}
crate::expand_fn!(Mutation, MutationFunction, Public, rename, __ferrix_args_rename, __FERRIX_FN_rename, (id: Id<Todo>, text: String) -> ());

pub async fn remove(ctx: &MutationCtx, id: Id<Todo>) -> Result<()> {
    ctx.db.delete(id).await
}
crate::expand_fn!(Mutation, MutationFunction, Public, remove, __ferrix_args_remove, __FERRIX_FN_remove, (id: Id<Todo>) -> ());

/// Inserts, reads its own write, then fails: everything must be rolled back.
pub async fn add_then_fail(ctx: &MutationCtx, text: String) -> Result<()> {
    let id = ctx.db.insert(Todo { text, done: false, created_by: None }).await?;
    anyhow::ensure!(ctx.db.get(id).await?.is_some(), "own write not visible");
    Err(UserError::new("nope", "failed on purpose").into())
}
crate::expand_fn!(Mutation, MutationFunction, Public, add_then_fail, __ferrix_args_add_then_fail, __FERRIX_FN_add_then_fail, (text: String) -> ());

pub async fn add_then_panic(ctx: &MutationCtx, text: String) -> Result<()> {
    ctx.db.insert(Todo { text, done: false, created_by: None }).await?;
    panic!("boom");
}
crate::expand_fn!(Mutation, MutationFunction, Public, add_then_panic, __ferrix_args_add_then_panic, __FERRIX_FN_add_then_panic, (text: String) -> ());

/// Inserts a few todos and returns what a query inside the same mutation sees.
pub async fn add_and_list(ctx: &MutationCtx, texts: Vec<String>) -> Result<Vec<String>> {
    for text in texts {
        ctx.db.insert(Todo { text, done: false, created_by: None }).await?;
    }
    let all = ctx.db.query::<Todo>().collect().await?;
    Ok(all.into_iter().map(|d| d.value.text).collect())
}
crate::expand_fn!(Mutation, MutationFunction, Public, add_and_list, __ferrix_args_add_and_list, __FERRIX_FN_add_and_list, (texts: Vec<String>) -> Vec<String>);

/// A mutation that writes nothing.
pub async fn noop(_ctx: &MutationCtx) -> Result<u32> {
    Ok(7)
}
crate::expand_fn!(Mutation, MutationFunction, Public, noop, __ferrix_args_noop, __FERRIX_FN_noop, () -> u32);

pub async fn count(ctx: &QueryCtx) -> Result<usize> {
    Ok(ctx.db.query::<Todo>().collect().await?.len())
}
crate::expand_fn!(Query, QueryFunction, Internal, count, __ferrix_args_count, __FERRIX_FN_count, () -> usize);

/// A public action calling an internal query.
pub async fn count_via_action(ctx: &ActionCtx) -> Result<usize> {
    ctx.run_query::<count>(()).await
}
crate::expand_fn!(Action, ActionFunction, Public, count_via_action, __ferrix_args_count_via_action, __FERRIX_FN_count_via_action, () -> usize);

pub async fn server_error(_ctx: &QueryCtx) -> Result<()> {
    anyhow::bail!("secret internal detail")
}
crate::expand_fn!(Query, QueryFunction, Public, server_error, __ferrix_args_server_error, __FERRIX_FN_server_error, () -> ());
