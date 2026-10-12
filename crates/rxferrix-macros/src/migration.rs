//! `#[migration(table = T, batch = N, name = "...", after = ["...", ...], on_read)]` (CONTRACT.md §3.3).

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, quote_spanned};
use syn::ext::IdentExt;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;

use crate::function::{FnKind, check_ctx_type, output_type};
use crate::util::{Errors, check_owned_type, is_cfg};

/// Default and bounds of `batch` (the runtime's `migrations::DEFAULT_BATCH_SIZE` / `MAX_BATCH_SIZE`).
const DEFAULT_BATCH: u64 = 100;
const MAX_BATCH: u64 = 10_000;

enum TableArg {
    /// `table = Todo`: a type implementing `Table`.
    Type(syn::Path),
    /// `table = "todos"`.
    Name(syn::LitStr),
}

struct Args {
    table: Option<TableArg>,
    /// `table` was given (possibly invalid, already reported) or the arguments didn't parse.
    table_seen: bool,
    batch: u64,
    name: Option<syn::LitStr>,
    /// `after = ["0001_x", ...]`: names of migrations that must be done first.
    after: Vec<syn::LitStr>,
    /// `on_read`: a plain `fn(doc: &serde_json::Value)`, also applied in memory to documents read before it ran.
    on_read: bool,
}

/// The same rule as the runtime's (`migrations::check_name`): 1..=128 characters of `A-Z a-z 0-9 _ - . : /`.
fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 128 {
        return Err("a migration name must have 1 to 128 characters".into());
    }
    if let Some(c) = name.chars().find(|c| !(c.is_ascii_alphanumeric() || "_-.:/".contains(*c))) {
        return Err(format!("a migration name may only contain letters, digits and `_ - . : /`, not `{c}`"));
    }
    Ok(())
}

fn parse_args(attr: TokenStream, errors: &mut Errors) -> Args {
    let mut args =
        Args { table: None, table_seen: false, batch: DEFAULT_BATCH, name: None, after: Vec::new(), on_read: false };
    let parser = Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated;
    let items = match syn::parse::Parser::parse2(parser, attr) {
        Ok(items) => items,
        Err(e) => {
            errors.push(syn::Error::new(
                e.span(),
                "expected `#[migration(table = TableType, batch = 100, name = \"...\", after = [\"...\"], on_read)]` (all but `table` are optional)",
            ));
            args.table_seen = true;
            return args;
        }
    };
    let mut seen = std::collections::HashSet::new();
    for meta in items {
        let item = match meta {
            syn::Meta::Path(path) if path.is_ident("on_read") => {
                if !seen.insert("on_read".to_string()) {
                    errors.error(&path, "duplicate argument `on_read`");
                }
                args.on_read = true;
                continue;
            }
            syn::Meta::NameValue(item) => item,
            syn::Meta::Path(path)
                if path
                    .get_ident()
                    .is_some_and(|i| ["table", "batch", "name", "after"].contains(&i.to_string().as_str())) =>
            {
                let key = path.get_ident().map(|i| i.to_string()).unwrap_or_default();
                args.table_seen |= key == "table";
                let example = match key.as_str() {
                    "table" => "table = TableType",
                    "batch" => "batch = 100",
                    "name" => "name = \"0002_add_priority\"",
                    _ => "after = [\"0001_add_priority\"]",
                };
                errors.error(&path, format_args!("`{key}` needs a value: `{example}`"));
                continue;
            }
            other => {
                errors
                    .error(&other, "expected `key = value` (`table`, `batch`, `name`, `after`) or the flag `on_read`");
                continue;
            }
        };
        let key = item.path.get_ident().map(|i| i.to_string()).unwrap_or_default();
        if !seen.insert(key.clone()) {
            errors.error(&item.path, format_args!("duplicate argument `{key}`"));
            continue;
        }
        args.table_seen |= key == "table";
        match key.as_str() {
            "table" => match &item.value {
                syn::Expr::Path(p) if p.qself.is_none() => args.table = Some(TableArg::Type(p.path.clone())),
                syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) => {
                    if s.value().is_empty() {
                        errors.error(s, "the table name must not be empty");
                    }
                    args.table = Some(TableArg::Name(s.clone()));
                }
                other => errors.error(other, "`table` must be a table type (`table = Todo`) or a table name (`table = \"todos\"`)"),
            },
            "batch" => match &item.value {
                syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Int(n), .. }) => match n.base10_parse::<u64>() {
                    Ok(v) if (1..=MAX_BATCH).contains(&v) => args.batch = v,
                    _ => errors.error(n, format_args!("`batch` must be between 1 and {MAX_BATCH}")),
                },
                other => errors.error(other, "`batch` must be an integer literal, e.g. `batch = 100`"),
            },
            "name" => match &item.value {
                syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) => {
                    if let Err(e) = check_name(&s.value()) {
                        errors.error(s, e);
                    }
                    args.name = Some(s.clone());
                }
                other => errors.error(other, "`name` must be a string literal, e.g. `name = \"0002_add_priority\"`"),
            },
            "after" => match &item.value {
                syn::Expr::Array(array) => {
                    let mut seen_names = std::collections::HashSet::new();
                    for elem in &array.elems {
                        match elem {
                            syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) => {
                                if let Err(e) = check_name(&s.value()) {
                                    errors.error(s, e);
                                } else if !seen_names.insert(s.value()) {
                                    errors.error(s, format_args!("`{}` is listed twice in `after`", s.value()));
                                }
                                args.after.push(s.clone());
                            }
                            other => errors.error(
                                other,
                                "`after` lists migration names as string literals, e.g. `after = [\"0001_add_priority\"]`",
                            ),
                        }
                    }
                }
                other => errors.error(
                    other,
                    "`after` must be a list of migration names, e.g. `after = [\"0001_add_priority\", \"todos:split\"]`",
                ),
            },
            _ => errors.error(
                &item.path,
                format_args!("unknown argument `{key}` (expected `table`, `batch`, `name`, `after` or `on_read`)"),
            ),
        }
    }
    if let Some(name) = &args.name
        && let Some(own) = args.after.iter().find(|a| a.value() == name.value())
    {
        errors.error(own, "a migration can't run after itself");
    }
    args
}

pub fn expand_migration(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let mut errors = Errors::default();
    let attr_span = attr.span();
    let args = parse_args(attr, &mut errors);
    let func: syn::ItemFn = match syn::parse2(item) {
        Ok(f) => f,
        Err(e) => {
            errors.push(syn::Error::new(e.span(), "#[migration] can only be used on an `async fn`"));
            return Err(errors.finish().unwrap_err());
        }
    };
    if !args.table_seen {
        errors.error_at(attr_span, "#[migration] needs the table it migrates: `#[migration(table = TableType)]`");
    }
    let sig = &func.sig;
    let name = sig.ident.clone();
    if args.on_read {
        if let Some(a) = &sig.asyncness {
            errors.error(
                a,
                "an `on_read` migration is a plain `fn(doc: &serde_json::Value)`: it also runs on reads, without the database",
            );
        }
    } else if sig.asyncness.is_none() {
        errors.error_at(sig.fn_token.span, "#[migration] functions must be `async fn`");
    }
    if let Some(c) = &sig.constness {
        errors.error(c, "#[migration] functions cannot be `const`");
    }
    if let Some(u) = &sig.unsafety {
        errors.error(u, "#[migration] functions cannot be `unsafe`");
    }
    if let Some(abi) = &sig.abi {
        errors.error(abi, "#[migration] functions cannot have an ABI");
    }
    if !sig.generics.params.is_empty() || sig.generics.where_clause.is_some() {
        errors.error(&sig.generics, "#[migration] functions cannot be generic (no type, const or lifetime parameters)");
    }
    if let Some(v) = &sig.variadic {
        errors.error(v, "#[migration] functions cannot be variadic");
    }

    // --- return type
    let output = match &sig.output {
        syn::ReturnType::Default => {
            errors.error_at(
                sig.paren_token.span.close(),
                "#[migration] functions return `Result<Option<serde_json::Value>>` (a patch or None), `Result<DocChange>` or `Result<()>`",
            );
            None
        }
        syn::ReturnType::Type(_, ty) => match output_type(ty) {
            Some(t) => Some(t.clone()),
            None => {
                errors.error(ty, "the return type must be `Result<T>` (`rxferrix::Result<T>` or `anyhow::Result<T>`)");
                None
            }
        },
    };

    // --- parameters: (ctx: &MutationCtx, doc: T)
    const PARAMS: &str =
        "#[migration] functions take the context and the document: `(ctx: &MutationCtx, doc: serde_json::Value)`";
    let mut typed = Vec::new();
    for input in &sig.inputs {
        match input {
            syn::FnArg::Receiver(r) => errors.error(r, "#[migration] functions cannot take `self`"),
            syn::FnArg::Typed(pt) => typed.push(pt),
        }
    }
    let mut doc_ty = None;
    match typed.as_slice() {
        [doc] if args.on_read => {
            match &*doc.pat {
                syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => {}
                syn::Pat::Wild(_) => {}
                other => errors.error(other, "parameter patterns must be plain identifiers"),
            }
            match &*doc.ty {
                syn::Type::Reference(r) if r.mutability.is_none() => {}
                other => errors
                    .error(other, "an `on_read` migration takes the document by reference: `doc: &serde_json::Value`"),
            }
            doc_ty = Some((*doc.ty).clone());
        }
        _ if args.on_read => errors.error_at(
            sig.paren_token.span.join(),
            "an `on_read` migration takes only the document: `fn f(doc: &serde_json::Value) -> Result<…>`",
        ),
        [ctx, doc] => {
            for pt in [ctx, doc] {
                match &*pt.pat {
                    syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => {}
                    syn::Pat::Wild(_) => {}
                    other => errors.error(other, "parameter patterns must be plain identifiers"),
                }
                for a in pt.attrs.iter() {
                    errors.error(a, "#[migration] parameters cannot have attributes");
                }
            }
            check_ctx_type(&ctx.ty, FnKind::Mutation, "migration", sig.generics.params.is_empty(), &mut errors);
            check_owned_type(&doc.ty, "document", &mut errors);
            doc_ty = Some((*doc.ty).clone());
        }
        [] | [_] => errors.error_at(sig.paren_token.span.join(), PARAMS),
        [_, _, extra, ..] => errors.error(extra, PARAMS),
    }

    errors.finish()?;
    let output = output.expect("checked above");
    let doc_ty = doc_ty.expect("checked above");
    let table = args.table.expect("checked above");

    let name_str = name.unraw().to_string();
    let static_ident = format_ident!("__RXFERRIX_MIGRATION_{}", name, span = name.span());
    let vis = func.vis.clone();
    let cfgs: Vec<_> = func.attrs.iter().filter(|a| is_cfg(a)).cloned().collect();
    let ref_doc = format!(" Migration reference for [`{name_str}`](fn@{name}).");
    let name_opt = match &args.name {
        Some(n) => quote!(::core::option::Option::Some(#n)),
        None => quote!(::core::option::Option::None),
    };
    let table_expr = match &table {
        TableArg::Type(path) => quote_spanned!(path.span()=> <#path as ::rxferrix::Table>::NAME),
        TableArg::Name(s) => quote!(#s),
    };
    let batch = syn::LitInt::new(&format!("{}usize", args.batch), Span::call_site());
    let after = &args.after;
    let ctx_var = syn::Ident::new("__rxferrix_ctx", Span::mixed_site());
    let doc_var = syn::Ident::new("__rxferrix_doc", Span::mixed_site());
    let out_var = syn::Ident::new("__rxferrix_out", Span::mixed_site());
    let handler_ident = syn::Ident::new("__rxferrix_migrate", Span::mixed_site());
    // Spanned so that a type error points at the document parameter or the return type.
    let convert = quote_spanned!(output.span()=> ::rxferrix::__private::migration_output(#out_var));
    let (handler_body, on_read) = if args.on_read {
        let on_read_ident = syn::Ident::new("__rxferrix_on_read", Span::mixed_site());
        (
            quote! {
                let _ = #ctx_var;
                let #out_var: #output = #name(&#doc_var)?;
                #convert
            },
            quote! {{
                fn #on_read_ident(
                    #doc_var: &::rxferrix::__private::serde_json::Value,
                ) -> ::rxferrix::Result<::rxferrix::migrations::DocChange> {
                    let #out_var: #output = #name(#doc_var)?;
                    #convert
                }
                ::core::option::Option::Some(#on_read_ident)
            }},
        )
    } else {
        let decode = quote_spanned!(doc_ty.span()=> ::rxferrix::__private::decode_migration_doc::<#doc_ty>(#doc_var)?);
        (
            quote! {
                let #doc_var: #doc_ty = #decode;
                let #out_var: #output = #name(#ctx_var, #doc_var).await?;
                #convert
            },
            quote!(::core::option::Option::None),
        )
    };

    Ok(quote! {
        #func

        #[doc = #ref_doc]
        #(#cfgs)*
        #[allow(non_camel_case_types, dead_code)]
        #vis struct #name {}

        #(#cfgs)*
        impl ::rxferrix::migrations::MigrationRef for #name {
            fn name() -> ::std::string::String {
                ::rxferrix::__private::MigrationDef::name(&#static_ident)
            }
            fn table() -> &'static str {
                #static_ident.table
            }
        }

        #(#cfgs)*
        #[::rxferrix::__private::linkme::distributed_slice(::rxferrix::__private::MIGRATIONS)]
        #[linkme(crate = ::rxferrix::__private::linkme)]
        #[allow(non_upper_case_globals)]
        static #static_ident: ::rxferrix::__private::MigrationDef = ::rxferrix::__private::MigrationDef {
            module_path: ::core::module_path!(),
            fn_name: #name_str,
            name: #name_opt,
            table: #table_expr,
            batch_size: #batch,
            after: &[#(#after),*],
            handler: {
                fn #handler_ident<'a>(
                    #ctx_var: &'a ::rxferrix::MutationCtx,
                    #doc_var: ::rxferrix::__private::serde_json::Value,
                ) -> ::rxferrix::__private::BoxFuture<'a, ::rxferrix::Result<::rxferrix::migrations::DocChange>> {
                    ::std::boxed::Box::pin(async move { #handler_body })
                }
                #handler_ident
            },
            on_read: #on_read,
        };
    })
}
