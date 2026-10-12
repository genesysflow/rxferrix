//! Procedural macros for rxferrix: `#[table]`, `#[object]`, `#[query]`, `#[mutation]`, `#[action]`, the `internal_*`
//! variants, `#[workflow]`, and `#[migration]`.
//!
//! The expansions are specified normatively in `docs/CONTRACT.md` §3. Every path the expansions use is fully
//! qualified (`::core::..`, `::std::..`, `::rxferrix::..`), so `use` items in user code cannot change their meaning.
//!
//! Notes for users:
//! - `#[table]` adds `#[derive(Serialize, Deserialize, TS, Clone, Debug)]` itself. Deriving any of these again is
//!   a compile error. Put `#[table]` before any `#[derive]` and `#[index]` attributes.
//! - After `#[query]` (etc.), the fn is kept as is (still directly callable). A braced struct with the same name
//!   (`pub struct list {}`, type namespace only) is the function reference: `ctx.run_query::<list>(args)`.

use proc_macro::TokenStream;

mod aggregate;
mod function;
mod migration;
mod object;
mod search;
mod shape;
mod table;
mod util;

#[cfg(test)]
mod tests;

use function::{FnKind, expand_function};

fn into_tokens(result: syn::Result<proc_macro2::TokenStream>) -> TokenStream {
    result.unwrap_or_else(syn::Error::into_compile_error).into()
}

/// Declares a table document type. See `docs/CONTRACT.md` §3.1.
///
/// ```ignore
/// #[table(name = "todos")]                   // optional; defaults to snake_case(struct name)
/// #[index(by_done = [done, created_by])]
/// #[search_index(search_text, search = text, filter = [done])]                  // full-text (FTS5)
/// #[vector_index(by_embedding, field = embedding, dims = 384, filter = [done])]  // sqlite-vec
/// #[aggregate(done_count, group = [done])]                                     // live counts and sums per group
/// pub struct Todo { pub text: String, pub done: bool, pub created_by: Option<String>, pub embedding: Option<Vec<f32>> }
/// ```
#[proc_macro_attribute]
pub fn table(attr: TokenStream, item: TokenStream) -> TokenStream {
    into_tokens(table::expand_table(attr.into(), item.into()))
}

/// A type used in function signatures that is not a table: a function's output (a "view" struct), an argument, or
/// a type nested in either. See `docs/CONTRACT.md` §3.4.
///
/// Adds `Serialize`, `Deserialize` and `TS` (through rxferrix's re-exported serde and ts-rs, so the app needs no
/// direct dependency on them) and `rename_all = "camelCase"` (unless the type sets its own `#[serde(rename_all)]`).
/// Works on structs and enums; `#[serde(..)]` attributes keep working (ts-rs follows them).
///
/// ```ignore
/// #[rxferrix::object]
/// #[derive(Clone, Debug)]
/// pub struct MessageView {
///     #[serde(flatten)]
///     pub message: Doc<Message>,      // `_id`, `_creationTime` and the table's fields, at the top level
///     pub author_name: String,        // `authorName`
/// }
/// ```
#[proc_macro_attribute]
pub fn object(attr: TokenStream, item: TokenStream) -> TokenStream {
    into_tokens(object::expand_object(attr.into(), item.into()))
}

/// Declares a data migration over one table, run in batches. See `docs/CONTRACT.md` §1.9 and §3.3.
///
/// ```ignore
/// #[migration(table = Todo, batch = 100)]             // `name = "0002_priority"` sets a stable id (default: the path)
/// async fn add_priority(ctx: &MutationCtx, doc: serde_json::Value) -> Result<Option<serde_json::Value>> {
///     Ok(doc.get("priority").is_none().then(|| json!({ "priority": 0 })))   // a patch, or None to keep it
/// }
/// ```
#[proc_macro_attribute]
pub fn migration(attr: TokenStream, item: TokenStream) -> TokenStream {
    into_tokens(migration::expand_migration(attr.into(), item.into()))
}

/// Declares a public query function. See `docs/CONTRACT.md` §3.2. `#[query(dev_only)]`: refused unless the server
/// runs with `--dev`, and generated into `devApi` instead of `api`.
#[proc_macro_attribute]
pub fn query(attr: TokenStream, item: TokenStream) -> TokenStream {
    into_tokens(expand_function(FnKind::Query, false, attr.into(), item.into()))
}

/// Declares a public mutation function. See `docs/CONTRACT.md` §3.2. Takes `dev_only` like `#[query]`.
#[proc_macro_attribute]
pub fn mutation(attr: TokenStream, item: TokenStream) -> TokenStream {
    into_tokens(expand_function(FnKind::Mutation, false, attr.into(), item.into()))
}

/// Declares a public action function. See `docs/CONTRACT.md` §3.2. Takes `dev_only` like `#[query]`.
#[proc_macro_attribute]
pub fn action(attr: TokenStream, item: TokenStream) -> TokenStream {
    into_tokens(expand_function(FnKind::Action, false, attr.into(), item.into()))
}

/// Declares a durable workflow: an `async fn` taking `step: &WorkflowCtx` whose steps (`step.run_query`,
/// `run_mutation`, `run_action`, `sleep`, `wait_for_event`) are journaled, so it survives restarts and can run for
/// days. Started with `ctx.workflows.start::<name>(args)`. See `rxferrix::workflow`.
///
/// ```ignore
/// #[workflow]
/// pub async fn onboard(step: &WorkflowCtx, user: Id<User>) -> Result<()> {
///     step.run_action::<send_welcome>((user,)).with_retries(3).await?;
///     step.sleep(Duration::from_secs(86_400)).await?;
///     step.run_mutation::<mark_onboarded>((user,)).await
/// }
/// ```
#[proc_macro_attribute]
pub fn workflow(attr: TokenStream, item: TokenStream) -> TokenStream {
    into_tokens(expand_function(FnKind::Workflow, false, attr.into(), item.into()))
}

/// Declares a query that cannot be called by clients. See `docs/CONTRACT.md` §3.2.
#[proc_macro_attribute]
pub fn internal_query(attr: TokenStream, item: TokenStream) -> TokenStream {
    into_tokens(expand_function(FnKind::Query, true, attr.into(), item.into()))
}

/// Declares a mutation that cannot be called by clients. See `docs/CONTRACT.md` §3.2.
#[proc_macro_attribute]
pub fn internal_mutation(attr: TokenStream, item: TokenStream) -> TokenStream {
    into_tokens(expand_function(FnKind::Mutation, true, attr.into(), item.into()))
}

/// Declares an action that cannot be called by clients. See `docs/CONTRACT.md` §3.2.
#[proc_macro_attribute]
pub fn internal_action(attr: TokenStream, item: TokenStream) -> TokenStream {
    into_tokens(expand_function(FnKind::Action, true, attr.into(), item.into()))
}
