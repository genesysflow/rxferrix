//! Snapshot tests of the expansions (reviewed against CONTRACT.md §3). Type-checking and runtime behaviour are
//! covered by `tests/expand.rs` (against the mock runtime in `tests/mock`) and `tests/ui` (compile-fail).

use proc_macro2::TokenStream;
use quote::quote;

use crate::function::{FnKind, expand_function};
use crate::table::expand_table;

fn pretty(tokens: TokenStream) -> String {
    let file: syn::File = syn::parse2(tokens).expect("expansion parses as a file");
    prettyplease::unparse(&file)
}

fn table(attr: TokenStream, item: TokenStream) -> String {
    pretty(expand_table(attr, item).expect("expansion succeeds"))
}

fn func(kind: FnKind, internal: bool, item: TokenStream) -> String {
    pretty(expand_function(kind, internal, TokenStream::new(), item).expect("expansion succeeds"))
}

fn table_err(attr: TokenStream, item: TokenStream) -> String {
    let err = expand_table(attr, item).expect_err("expansion fails");
    err.into_iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn func_err(kind: FnKind, attr: TokenStream, item: TokenStream) -> String {
    let err = expand_function(kind, false, attr, item).expect_err("expansion fails");
    err.into_iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

// ----------------------------------------------------------------------------------------------- #[table]

#[test]
fn table_contract_example() {
    insta::assert_snapshot!(table(
        quote!(name = "todos"),
        quote! {
            #[index(by_done = [done, created_by])]
            pub struct Todo { pub text: String, pub done: bool, pub created_by: Option<String> }
        }
    ));
}

#[test]
fn table_default_name_docs_renames_and_extra_attrs() {
    insta::assert_snapshot!(table(
        quote!(),
        quote! {
            /// A todo item.
            ///
            /// Second paragraph.
            #[index(by_owner = [owner_id])]
            #[derive(PartialEq)]
            #[index(by_owner_and_due = [owner_id, due_at], by_label = [r#type])]
            #[serde(deny_unknown_fields)]
            pub(crate) struct TodoItem {
                /// The owner.
                #[serde(rename = "owner")]
                pub owner_id: String,
                pub due_at: Option<f64>,
                pub r#type: String,
                #[serde(default)]
                pub tags: Vec<String>,
            }
        }
    ));
}

#[test]
fn table_existing_camel_case_rename_all_is_deduplicated() {
    insta::assert_snapshot!(table(
        quote!(),
        quote! {
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            #[ts(rename_all = "camelCase")]
            #[cfg(feature = "x")]
            struct User { name: String }
        }
    ));
}

#[test]
fn table_errors() {
    insta::assert_snapshot!(
        "table_err_enum",
        table_err(
            quote!(),
            quote!(
                enum E {
                    A,
                }
            )
        ),
    );
    insta::assert_snapshot!(
        "table_err_tuple",
        table_err(
            quote!(),
            quote!(
                struct T(u32);
            )
        )
    );
    insta::assert_snapshot!(
        "table_err_unit",
        table_err(
            quote!(),
            quote!(
                struct T;
            )
        )
    );
    insta::assert_snapshot!(
        "table_err_generic",
        table_err(
            quote!(),
            quote!(
                struct T<X> {
                    x: X,
                }
            )
        )
    );
    insta::assert_snapshot!(
        "table_err_bad_arg",
        table_err(
            quote!(nme = "x"),
            quote!(
                struct T {
                    x: u32,
                }
            )
        )
    );
    insta::assert_snapshot!(
        "table_err_indexes",
        table_err(
            quote!(),
            quote! {
                #[index(by_x = [x], by_x = [x], by_id = [x], by_creation_time = [x])]
                #[index(by_missing = [nope], by_dup = [x, x], by_empty = [], by_sys = [_creation_time], x = [x])]
                struct T { x: u32 }
            }
        )
    );
    insta::assert_snapshot!(
        "table_err_serde",
        table_err(
            quote!(),
            quote! {
                #[derive(Serialize, serde::Deserialize, Clone, PartialEq)]
                #[serde(rename_all = "snake_case")]
                #[serde(crate = "serde")]
                struct T { _id: String, #[serde(rename = "_x")] a: u32, #[serde(rename = "b")] c: u32, b: u32 }
            }
        )
    );
}

// ----------------------------------------------------------------------------------------------- functions

#[test]
fn query_contract_example() {
    insta::assert_snapshot!(func(
        FnKind::Query,
        false,
        quote! {
            /// Lists todos.
            pub async fn list(ctx: &QueryCtx, done: Option<bool>, limit: u32) -> Result<Vec<Doc<Todo>>> { BODY }
        }
    ));
}

#[test]
fn query_zero_params_ferrix_result() {
    insta::assert_snapshot!(func(
        FnKind::Query,
        false,
        quote! {
            async fn count(_ctx: &ferrix::QueryCtx) -> ferrix::Result<u64> { Ok(0) }
        }
    ));
}

#[test]
fn mutation_one_param_anyhow_result() {
    insta::assert_snapshot!(func(
        FnKind::Mutation,
        false,
        quote! {
            /// Adds a todo.
            #[allow(unused_variables)]
            pub(crate) async fn add(ctx: &MutationCtx, mut text: String) -> anyhow::Result<Id<Todo>> { todo!() }
        }
    ));
}

#[test]
fn action_several_params_param_attrs() {
    insta::assert_snapshot!(func(
        FnKind::Action,
        false,
        quote! {
            pub async fn add_many(
                ctx: &ActionCtx,
                n: u32,
                prefix_text: Option<String>,
                #[serde(default)] dry_run: bool,
                r#type: String,
            ) -> Result<()> { Ok(()) }
        }
    ));
}

#[test]
fn internal_variants() {
    insta::assert_snapshot!(
        "internal_query",
        func(
            FnKind::Query,
            true,
            quote! { async fn iq(ctx: &QueryCtx) -> Result<()> { Ok(()) } }
        )
    );
    insta::assert_snapshot!(
        "internal_mutation",
        func(
            FnKind::Mutation,
            true,
            quote! { async fn im(ctx: &MutationCtx, x: i32) -> Result<i32> { Ok(x) } }
        )
    );
    insta::assert_snapshot!(
        "internal_action",
        func(
            FnKind::Action,
            true,
            quote! { #[cfg(feature = "x")] pub async fn ia(ctx: &ActionCtx) -> std::result::Result<String, MyError> { todo!() } }
        )
    );
}

#[test]
fn function_errors() {
    use FnKind::*;
    insta::assert_snapshot!(
        "fn_err_not_async",
        func_err(
            Query,
            quote!(),
            quote!(
                fn f(ctx: &QueryCtx) -> Result<()> {}
            )
        )
    );
    insta::assert_snapshot!(
        "fn_err_no_ctx",
        func_err(
            Query,
            quote!(),
            quote!(
                async fn f() -> Result<()> {}
            )
        )
    );
    insta::assert_snapshot!(
        "fn_err_wrong_ctx",
        func_err(
            Query,
            quote!(),
            quote!(
                async fn f(ctx: &MutationCtx) -> Result<()> {}
            )
        )
    );
    insta::assert_snapshot!(
        "fn_err_ctx_by_value",
        func_err(
            Mutation,
            quote!(),
            quote!(
                async fn f(ctx: MutationCtx) -> Result<()> {}
            )
        )
    );
    insta::assert_snapshot!(
        "fn_err_ctx_not_ctx",
        func_err(
            Action,
            quote!(),
            quote!(
                async fn f(n: u32) -> Result<()> {}
            )
        )
    );
    insta::assert_snapshot!(
        "fn_err_self",
        func_err(
            Query,
            quote!(),
            quote!(
                async fn f(&self, x: u32) -> Result<()> {}
            )
        )
    );
    insta::assert_snapshot!(
        "fn_err_patterns",
        func_err(
            Query,
            quote!(),
            quote!(
                async fn f(ctx: &QueryCtx, (a, b): (u32, u32), ref c: u32, _: u32) -> Result<()> {}
            )
        )
    );
    insta::assert_snapshot!(
        "fn_err_generics",
        func_err(
            Query,
            quote!(),
            quote!(
                async fn f<T: Clone>(ctx: &QueryCtx, x: T) -> Result<()>
                where
                    T: Copy,
                {
                }
            )
        )
    );
    insta::assert_snapshot!(
        "fn_err_no_return",
        func_err(
            Query,
            quote!(),
            quote!(
                async fn f(ctx: &QueryCtx) {}
            )
        )
    );
    insta::assert_snapshot!(
        "fn_err_bad_return",
        func_err(
            Query,
            quote!(),
            quote!(
                async fn f(ctx: &QueryCtx) -> Option<u32> {}
            )
        )
    );
    insta::assert_snapshot!(
        "fn_err_borrowed",
        func_err(
            Query,
            quote!(),
            quote!(
                async fn f(
                    ctx: &QueryCtx,
                    s: &str,
                    c: Cow<'a, str>,
                    k: Cow<'static, str>,
                    i: impl Into<u32>,
                ) -> Result<&'static str> {
                }
            )
        )
    );
    insta::assert_snapshot!(
        "fn_err_args",
        func_err(
            Query,
            quote!(x),
            quote!(
                async fn f(ctx: &QueryCtx) -> Result<()> {}
            )
        )
    );
    insta::assert_snapshot!(
        "fn_err_dup_json",
        func_err(
            Query,
            quote!(),
            quote!(
                async fn f(ctx: &QueryCtx, foo_bar: u32, fooBar: u32) -> Result<()> {}
            )
        )
    );
    insta::assert_snapshot!(
        "fn_err_not_fn",
        func_err(
            Query,
            quote!(),
            quote!(
                struct S;
            )
        )
    );
}
