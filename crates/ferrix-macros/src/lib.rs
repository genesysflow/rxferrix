//! Procedural macros for ferrix: `#[table]`, `#[query]`, `#[mutation]`, `#[action]` and the `internal_*`
//! variants.
//!
//! The expansions are specified normatively in `docs/CONTRACT.md` §3. Every path the expansions use is fully
//! qualified (`::core::..`, `::std::..`, `::ferrix::..`), so `use` items in user code cannot change their meaning.
//!
//! Notes for users:
//! - `#[table]` adds `#[derive(Serialize, Deserialize, TS, Clone, Debug)]` itself. Deriving any of these again is
//!   a compile error. Put `#[table]` before any `#[derive]` and `#[index]` attributes.
//! - After `#[query]` (etc.), the fn is kept as is (still directly callable). A braced struct with the same name
//!   (`pub struct list {}`, type namespace only) is the function reference: `ctx.run_query::<list>(args)`.

use proc_macro::TokenStream;

mod function;
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
/// pub struct Todo { pub text: String, pub done: bool, pub created_by: Option<String> }
/// ```
#[proc_macro_attribute]
pub fn table(attr: TokenStream, item: TokenStream) -> TokenStream {
    into_tokens(table::expand_table(attr.into(), item.into()))
}

/// Declares a public query function. See `docs/CONTRACT.md` §3.2.
#[proc_macro_attribute]
pub fn query(attr: TokenStream, item: TokenStream) -> TokenStream {
    into_tokens(expand_function(
        FnKind::Query,
        false,
        attr.into(),
        item.into(),
    ))
}

/// Declares a public mutation function. See `docs/CONTRACT.md` §3.2.
#[proc_macro_attribute]
pub fn mutation(attr: TokenStream, item: TokenStream) -> TokenStream {
    into_tokens(expand_function(
        FnKind::Mutation,
        false,
        attr.into(),
        item.into(),
    ))
}

/// Declares a public action function. See `docs/CONTRACT.md` §3.2.
#[proc_macro_attribute]
pub fn action(attr: TokenStream, item: TokenStream) -> TokenStream {
    into_tokens(expand_function(
        FnKind::Action,
        false,
        attr.into(),
        item.into(),
    ))
}

/// Declares a query that cannot be called by clients. See `docs/CONTRACT.md` §3.2.
#[proc_macro_attribute]
pub fn internal_query(attr: TokenStream, item: TokenStream) -> TokenStream {
    into_tokens(expand_function(
        FnKind::Query,
        true,
        attr.into(),
        item.into(),
    ))
}

/// Declares a mutation that cannot be called by clients. See `docs/CONTRACT.md` §3.2.
#[proc_macro_attribute]
pub fn internal_mutation(attr: TokenStream, item: TokenStream) -> TokenStream {
    into_tokens(expand_function(
        FnKind::Mutation,
        true,
        attr.into(),
        item.into(),
    ))
}

/// Declares an action that cannot be called by clients. See `docs/CONTRACT.md` §3.2.
#[proc_macro_attribute]
pub fn internal_action(attr: TokenStream, item: TokenStream) -> TokenStream {
    into_tokens(expand_function(
        FnKind::Action,
        true,
        attr.into(),
        item.into(),
    ))
}
