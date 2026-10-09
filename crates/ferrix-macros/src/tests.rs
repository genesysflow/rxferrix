//! Snapshot tests of the expansions (reviewed against CONTRACT.md §3). Type-checking and runtime behaviour are
//! covered by `tests/expand.rs` (against the mock runtime in `tests/mock`) and `tests/ui` (compile-fail).

use proc_macro2::TokenStream;
use quote::quote;

use crate::function::{FnKind, expand_function};
use crate::object::expand_object;
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

#[test]
fn table_search_and_vector_indexes() {
    insta::assert_snapshot!(table(
        quote!(name = "messages"),
        quote! {
            #[index(by_channel = [channel])]
            #[search_index(search_body, search = body, filter = [channel, author_name])]
            #[vector_index(by_embedding, field = embedding, dims = 1536, filter = [channel])]
            #[search_index(search_title, search = title)]
            #[cfg(feature = "x")]
            pub struct Message {
                pub channel: String,
                pub body: String,
                pub title: Option<String>,
                #[serde(rename = "author")]
                pub author_name: Option<String>,
                pub embedding: Option<Vec<f32>>,
            }
        }
    ));
}

#[test]
fn table_search_tokenizers_and_vector_kinds() {
    insta::assert_snapshot!(table(
        quote!(),
        quote! {
            #[search_index(s_porter, search = body, tokenizer = "porter", language = "english")]
            #[search_index(s_porter_keep, search = body, tokenizer = "porter", diacritics = "keep")]
            #[search_index(s_trigram, search = body, filter = [channel], tokenizer = "trigram")]
            #[search_index(s_plain, search = title, tokenizer = "unicode61", diacritics = "remove")]
            #[search_index(s_ro, search = body, tokenizer = "snowball", language = "romanian")]
            #[search_index(s_fr_keep, search = body, language = "french", tokenizer = "snowball", diacritics = "keep")]
            #[vector_index(v_hnsw, field = embedding, dims = 4, kind = "hnsw")]
            #[vector_index(v_tuned, field = embedding, dims = 4, kind = "hnsw", m = 32, ef_construction = 400, ef_search = 64)]
            #[vector_index(v_ef, field = embedding, dims = 4, ef_search = 20, kind = "hnsw")]
            #[vector_index(v_exact, field = embedding, dims = 4, filter = [channel], kind = "exact")]
            struct Doc { channel: String, body: String, title: Option<String>, embedding: Vec<f32> }
        }
    ));
}

#[test]
fn table_search_tokenizer_errors() {
    insta::assert_snapshot!(
        "table_err_search_tokenizers",
        table_err(
            quote!(),
            quote! {
                #[search_index(s1, search = text, tokenizer = "snowball")]
                #[search_index(s1b, search = text, tokenizer = "snowball", language = "klingon")]
                #[search_index(s1c, search = text, tokenizer = "trigram", language = "french")]
                #[search_index(s2, search = text, language = "english")]
                #[search_index(s3, search = text, tokenizer = "porter", language = "french")]
                #[search_index(s4, search = text, diacritics = "strip")]
                #[search_index(s5, search = text, tokenizer = "trigram", tokenizer = "porter", language = "english")]
                #[search_index(s6, search = text, tokenizer = porter)]
                #[search_index(s7, search = text, kind = "hnsw")]
                #[vector_index(v1, field = emb, dims = 2, kind = "ivf")]
                #[vector_index(v2, field = emb, dims = 2, tokenizer = "porter")]
                #[vector_index(v3, field = emb, dims = 2, kind = "hnsw", kind = "exact")]
                #[vector_index(v4, field = emb, dims = 2, m = 16)]
                #[vector_index(v5, field = emb, dims = 2, kind = "exact", ef_search = 10, ef_construction = 100)]
                #[vector_index(v6, field = emb, dims = 2, kind = "hnsw", m = 2, ef_construction = 9000, ef_search = 0)]
                #[vector_index(v7, field = emb, dims = 2, kind = "hnsw", m = 8, m = 9)]
                #[vector_index(v8, field = emb, dims = 2, kind = "hnsw", m = "8")]
                struct T { text: String, emb: Vec<f32> }
            }
        )
    );
}

#[test]
fn table_search_index_errors() {
    insta::assert_snapshot!(
        "table_err_search_indexes",
        table_err(
            quote!(),
            quote! {
                #[index(by_x = [x])]
                #[search_index(by_x, search = text)]
                #[search_index(text, search = text)]
                #[search_index(by_id, search = text)]
                #[search_index(s_missing, search = nope, filter = [x, x, gone, text, _id])]
                #[search_index(s_no_field, filter = [x])]
                #[search_index(s_wrong_type, search = x)]
                #[search_index(s_dup)]
                #[search_index(s_dup, search = text, search = text, filter = [x], filter = [x])]
                #[vector_index(v_wrong_type, field = text, dims = 3)]
                #[vector_index(v_f64, field = wide, dims = 3)]
                #[vector_index(v_no_dims, field = emb)]
                #[vector_index(v_zero, field = emb, dims = 0)]
                #[vector_index(v_huge, field = emb, dims = 100000)]
                #[vector_index(v_ok, field = emb, dims = 2, filter = [emb])]
                struct T { x: u32, text: String, emb: Vec<f32>, wide: Vec<f64> }
            }
        )
    );
    insta::assert_snapshot!(
        "table_err_search_index_syntax",
        table_err(
            quote!(),
            quote! {
                #[search_index(name = s, search = text)]
                struct T { text: String }
            }
        )
    );
    insta::assert_snapshot!(
        "table_err_search_index_unknown_arg",
        table_err(
            quote!(),
            quote! {
                #[search_index(s, search = text, dims = 3)]
                #[vector_index(v, field = emb, dims = 3, search = text)]
                #[search_index]
                struct T { text: String, emb: Vec<f32> }
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
fn dev_only_action() {
    insta::assert_snapshot!(pretty(
        expand_function(
            FnKind::Action,
            false,
            quote!(dev_only),
            quote! {
                /// The newest code emailed to `email` (dev only).
                pub async fn last_code(ctx: &ActionCtx, email: String) -> Result<Option<String>> { todo!() }
            }
        )
        .expect("expansion succeeds")
    ));
}

#[test]
fn dev_only_errors() {
    insta::assert_snapshot!(
        "fn_err_dev_only",
        [quote!(dev_only, dev_only), quote!(dev_only = true), quote!(internal)]
            .into_iter()
            .map(|attr| func_err(FnKind::Mutation, attr, quote!(async fn f(ctx: &MutationCtx) -> Result<()> {})))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn object(attr: TokenStream, item: TokenStream) -> String {
    pretty(expand_object(attr, item).expect("expansion succeeds"))
}

fn object_err(attr: TokenStream, item: TokenStream) -> String {
    let err = expand_object(attr, item).expect_err("expansion fails");
    err.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n")
}

#[test]
fn object_struct_with_flattened_doc() {
    insta::assert_snapshot!(object(
        quote!(),
        quote! {
            /// A message as the app shows it.
            #[derive(Clone, Debug)]
            pub struct MessageView {
                #[serde(flatten)]
                pub message: Doc<Message>,
                /// The author's display name.
                pub author_name: String,
                pub reactions: Vec<ReactionGroup>,
            }
        }
    ));
}

#[test]
fn object_enum_and_own_rename_all() {
    insta::assert_snapshot!(
        "object_enum",
        object(quote!(), quote! { pub enum Status { Open, InProgress, Done { at: f64 } } })
    );
    // The type's own rename_all wins; nothing else is added.
    insta::assert_snapshot!(
        "object_own_rename_all",
        object(
            quote!(),
            quote! {
                #[serde(rename_all = "snake_case", deny_unknown_fields)]
                pub struct Raw<T> { pub some_field: T }
            }
        )
    );
}

#[test]
fn object_errors() {
    insta::assert_snapshot!(
        "object_err",
        [
            object_err(quote!(x), quote!(pub struct A {})),
            object_err(quote!(), quote!(pub fn f() {})),
            object_err(
                quote!(),
                quote! {
                    #[derive(Debug, serde::Serialize, TS)]
                    #[serde(crate = "serde")]
                    #[ts(crate = "ts_rs", export)]
                    pub struct A { pub a: u32 }
                }
            ),
        ]
        .join("\n")
    );
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

// ----------------------------------------------------------------------------------------------- #[migration]

fn migration(attr: TokenStream, item: TokenStream) -> String {
    pretty(crate::migration::expand_migration(attr, item).expect("expansion succeeds"))
}

fn migration_err(attr: TokenStream, item: TokenStream) -> String {
    let err = crate::migration::expand_migration(attr, item).expect_err("expansion fails");
    err.into_iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn migration_contract_example() {
    insta::assert_snapshot!(migration(
        quote!(table = Todo, batch = 100),
        quote! {
            /// Adds a default priority.
            pub async fn add_priority(ctx: &MutationCtx, doc: serde_json::Value) -> Result<Option<serde_json::Value>> {
                Ok(doc.get("priority").is_none().then(|| json!({ "priority": 0 })))
            }
        }
    ));
}

#[test]
fn migration_typed_named_table_by_name() {
    insta::assert_snapshot!(migration(
        quote!(table = "todos", name = "0002_split-name", batch = 7, after = ["0001_add_priority", "todos:fix"]),
        quote! {
            #[cfg(feature = "x")]
            async fn split(_ctx: &ferrix::MutationCtx, doc: Doc<OldTodo>) -> anyhow::Result<DocChange> {
                Ok(DocChange::Keep)
            }
        }
    ));
}

#[test]
fn migration_errors() {
    insta::assert_snapshot!(
        "migration_err_args",
        migration_err(
            quote!(batch = 0, name = "has space", tables = X, name = "x", after = "0001", after = ["a"]),
            quote!(
                async fn m(ctx: &MutationCtx, doc: Value) -> Result<()> {}
            )
        )
    );
    insta::assert_snapshot!(
        "migration_err_after",
        migration_err(
            quote!(table = Todo, name = "0003_x", after = ["0001_ok", "bad name", 3, "0001_ok", "0003_x"]),
            quote!(
                async fn m(ctx: &MutationCtx, doc: Value) -> Result<()> {}
            )
        )
    );
    insta::assert_snapshot!(
        "migration_err_syntax",
        migration_err(
            quote!(table),
            quote!(
                async fn m(ctx: &MutationCtx, doc: Value) -> Result<()> {}
            )
        )
    );
    insta::assert_snapshot!(
        "migration_err_signature",
        migration_err(
            quote!(table = Todo),
            quote!(
                fn m<T>(ctx: &QueryCtx, doc: &Value, extra: u32) {}
            )
        )
    );
    insta::assert_snapshot!(
        "migration_err_params",
        migration_err(
            quote!(table = Todo),
            quote!(
                async fn m(ctx: MutationCtx) -> Result<()> {}
            )
        )
    );
    insta::assert_snapshot!(
        "migration_err_values",
        migration_err(
            quote!(table = 5, batch = "big", name = x),
            quote!(
                async fn m(ctx: &MutationCtx, (a, b): (u32, u32)) -> Option<()> {}
            )
        )
    );
}
