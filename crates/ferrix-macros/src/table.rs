//! `#[table]` (CONTRACT.md §3.1).

use std::collections::{HashMap, HashSet};

use heck::ToSnakeCase;
use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, quote_spanned};
use syn::ext::IdentExt;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Token, bracketed};

use crate::util::{Errors, attr_items, camel_case, is_cfg, rebuild_attr};

/// Derives `#[table]` adds itself; users must not derive them again.
const ADDED_DERIVES: &[&str] = &["Serialize", "Deserialize", "TS", "Clone", "Debug"];
/// Index names reserved for the built-in indexes.
const RESERVED_INDEXES: &[&str] = &["by_id", "by_creation_time"];
/// Associated consts generated for system fields / built-in indexes; user fields and indexes must not reuse them.
const RESERVED_ITEMS: &[&str] = &["_id", "_creation_time", "by_creation_time", "by_id"];

/// `name = "..."` in `#[table(...)]`.
struct TableArgs {
    name: Option<syn::LitStr>,
}

/// True while expanding inside the `ferrix` crate itself, which may define `_`-prefixed system tables.
/// Cargo sets `CARGO_CRATE_NAME` for every rustc invocation, so proc macros can read it.
fn is_ferrix_crate() -> bool {
    std::env::var("CARGO_CRATE_NAME").is_ok_and(|name| name == "ferrix")
}

fn parse_table_args(attr: TokenStream) -> syn::Result<TableArgs> {
    let mut args = TableArgs { name: None };
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("name") {
            if args.name.is_some() {
                return Err(meta.error("duplicate `name` argument"));
            }
            let lit: syn::LitStr = meta.value()?.parse()?;
            if lit.value().is_empty() {
                return Err(syn::Error::new(lit.span(), "table name cannot be empty"));
            }
            if lit.value().starts_with('_') && !is_ferrix_crate() {
                return Err(syn::Error::new(
                    lit.span(),
                    "table names starting with `_` are reserved for ferrix's built-in system tables",
                ));
            }
            args.name = Some(lit);
            Ok(())
        } else {
            Err(meta.error("unknown #[table] argument; expected `name = \"...\"`"))
        }
    });
    syn::parse::Parser::parse2(parser, attr)?;
    Ok(args)
}

/// One `name = [field, ...]` entry of an `#[index(...)]` attribute.
struct IndexSpec {
    name: syn::Ident,
    fields: Vec<syn::Ident>,
    bracket_span: Span,
}

impl Parse for IndexSpec {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let name: syn::Ident = input.parse()?;
        input.parse::<Token![=]>()?;
        let content;
        let bracket = bracketed!(content in input);
        let fields = Punctuated::<syn::Ident, Token![,]>::parse_terminated(&content)?;
        Ok(IndexSpec {
            name,
            fields: fields.into_iter().collect(),
            bracket_span: bracket.span.join(),
        })
    }
}

struct IndexAttr(Vec<IndexSpec>);

impl Parse for IndexAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        if input.is_empty() {
            return Err(input.error("expected `#[index(name = [field, ...])]`"));
        }
        let specs = Punctuated::<IndexSpec, Token![,]>::parse_terminated(input)?;
        Ok(IndexAttr(specs.into_iter().collect()))
    }
}

struct FieldInfo<'a> {
    ident: &'a syn::Ident,
    ty: &'a syn::Type,
    json_name: String,
}

pub fn expand_table(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let args = parse_table_args(attr);
    let item: syn::Item = syn::parse2(item)?;
    let mut strukt = match item {
        syn::Item::Struct(s) => s,
        syn::Item::Enum(e) => {
            return Err(syn::Error::new(
                e.enum_token.span,
                "#[table] can only be used on a struct with named fields, not an enum",
            ));
        }
        syn::Item::Union(u) => {
            return Err(syn::Error::new(
                u.union_token.span,
                "#[table] can only be used on a struct with named fields, not a union",
            ));
        }
        other => {
            return Err(syn::Error::new_spanned(
                other,
                "#[table] can only be used on a struct with named fields",
            ));
        }
    };
    let mut errors = Errors::default();
    let args = match args {
        Ok(a) => a,
        Err(e) => {
            errors.push(e);
            TableArgs { name: None }
        }
    };

    if !strukt.generics.params.is_empty() || strukt.generics.where_clause.is_some() {
        errors.error(&strukt.generics, "#[table] structs cannot be generic");
    }
    let named = match &strukt.fields {
        syn::Fields::Named(n) => Some(n),
        syn::Fields::Unnamed(u) => {
            errors.error(
                u,
                "#[table] requires a struct with named fields, not a tuple struct",
            );
            None
        }
        syn::Fields::Unit => {
            errors.error_at(
                strukt.ident.span(),
                "#[table] requires a struct with named fields, not a unit struct",
            );
            None
        }
    };

    // --- struct-level attributes: consume #[index], check derives / serde / ts, keep the rest.
    let mut kept_attrs = Vec::new();
    let mut index_specs = Vec::new();
    for attr in std::mem::take(&mut strukt.attrs) {
        let path = attr.path();
        if path.is_ident("index") {
            match attr.parse_args::<IndexAttr>() {
                Ok(IndexAttr(specs)) => index_specs.extend(specs),
                Err(e) => errors.push(e),
            }
        } else if path.is_ident("table") {
            errors.error(&attr, "duplicate #[table] attribute");
        } else if path.is_ident("derive") {
            match attr.parse_args_with(Punctuated::<syn::Path, Token![,]>::parse_terminated) {
                Ok(paths) => {
                    for p in &paths {
                        let last = p
                            .segments
                            .last()
                            .map(|s| s.ident.to_string())
                            .unwrap_or_default();
                        if ADDED_DERIVES.contains(&last.as_str()) {
                            errors.error(p, format_args!("#[table] already derives `{last}`; remove it from this #[derive]"));
                        }
                    }
                }
                Err(e) => errors.push(e),
            }
            kept_attrs.push(attr);
        } else if path.is_ident("serde") || path.is_ident("ts") {
            let which = if path.is_ident("serde") {
                "serde"
            } else {
                "ts"
            };
            let Some(items) = attr_items(&attr) else {
                kept_attrs.push(attr);
                continue;
            };
            let mut kept_items = Vec::new();
            for item in items {
                match item.key().map(|k| k.to_string()).as_deref() {
                    Some("rename_all") => match item.str_value() {
                        // Already what #[table] sets: drop it to avoid a duplicate-attribute error.
                        Some(v) if v.value() == "camelCase" => {}
                        _ => errors.error(
                            &item.tokens,
                            format_args!("#[table] sets `#[{which}(rename_all = \"camelCase\")]`; other `rename_all` rules are not supported (use per-field `#[serde(rename = \"...\")]`)"),
                        ),
                    },
                    Some("crate") => errors.error(&item.tokens, format_args!("#[table] sets `#[{which}(crate = ...)]` itself; remove this")),
                    _ => kept_items.push(item),
                }
            }
            kept_attrs.extend(rebuild_attr(&attr, &kept_items));
        } else {
            kept_attrs.push(attr);
        }
    }
    strukt.attrs = kept_attrs;

    // --- fields
    let mut fields = Vec::new();
    if let Some(named) = named {
        for f in &named.named {
            let ident = f.ident.as_ref().expect("named field");
            let mut json_name = camel_case(&ident.unraw().to_string());
            for attr in f.attrs.iter().filter(|a| a.path().is_ident("serde")) {
                for item in attr_items(attr).unwrap_or_default() {
                    if item.key().is_some_and(|k| k == "rename") {
                        match item.str_value() {
                            Some(v) => json_name = v.value(),
                            None => errors.error(&item.tokens, "#[table] only supports the `rename = \"...\"` form of #[serde(rename)]"),
                        }
                    }
                }
            }
            let rust_name = ident.unraw().to_string();
            if RESERVED_ITEMS.contains(&rust_name.as_str()) {
                errors.error(
                    ident,
                    format_args!("`{rust_name}` is reserved for a system field or built-in index"),
                );
            }
            if json_name.starts_with('_') {
                errors.error(ident, format_args!("JSON field name `{json_name}` is reserved: names starting with `_` are system fields"));
            }
            fields.push(FieldInfo {
                ident,
                ty: &f.ty,
                json_name,
            });
        }
    }
    let mut seen_json = HashMap::new();
    for f in &fields {
        if let Some(prev) = seen_json.insert(f.json_name.clone(), f.ident) {
            errors.error(
                f.ident,
                format_args!(
                    "fields `{}` and `{}` both serialize as `{}`",
                    prev.unraw(),
                    f.ident.unraw(),
                    f.json_name
                ),
            );
        }
    }
    let field_by_name: HashMap<String, &FieldInfo> = fields
        .iter()
        .map(|f| (f.ident.unraw().to_string(), f))
        .collect();

    // --- indexes
    let mut index_defs = Vec::new();
    let mut seen_index = HashSet::new();
    for spec in &index_specs {
        let name = spec.name.unraw().to_string();
        if RESERVED_INDEXES.contains(&name.as_str()) {
            errors.error(
                &spec.name,
                format_args!("index name `{name}` is reserved for a built-in index"),
            );
        } else if !seen_index.insert(name.clone()) {
            errors.error(&spec.name, format_args!("duplicate index name `{name}`"));
        } else if field_by_name.contains_key(&name) {
            errors.error(
                &spec.name,
                format_args!(
                    "index name `{name}` conflicts with the field handle `{}::{name}`",
                    strukt.ident
                ),
            );
        }
        if spec.fields.is_empty() {
            errors.error_at(spec.bracket_span, "an index needs at least one field");
        }
        let mut json_fields = Vec::new();
        let mut seen_fields = HashSet::new();
        for field in &spec.fields {
            let fname = field.unraw().to_string();
            match field_by_name.get(&fname) {
                Some(info) => {
                    if !seen_fields.insert(fname.clone()) {
                        errors.error(field, format_args!("field `{fname}` appears twice in index `{name}`"));
                    }
                    json_fields.push(info.json_name.clone());
                }
                None if fname == "_id" || fname == "_creation_time" => errors.error(
                    field,
                    format_args!("system field `{fname}` cannot be listed in an index (every index implicitly ends with `_id`; use `by_creation_time` to order by creation time)"),
                ),
                None => errors.error(field, format_args!("unknown field `{fname}` in index `{name}`; struct `{}` has no such field", strukt.ident)),
            }
        }
        index_defs.push((spec, name, json_fields));
    }

    errors.finish()?;

    // --- output
    let ident = &strukt.ident;
    let table_name = match &args.name {
        Some(lit) => lit.value(),
        None => ident.unraw().to_string().to_snake_case(),
    };
    let cfgs: Vec<_> = strukt.attrs.iter().filter(|a| is_cfg(a)).cloned().collect();

    let index_def_exprs = index_defs.iter().map(|(_, name, json_fields)| {
        quote! { ::ferrix::IndexDef { name: #name, fields: ::std::vec![#(#json_fields),*] } }
    });
    let field_consts = fields.iter().map(|f| {
        let FieldInfo {
            ident: fident,
            ty,
            json_name,
        } = f;
        quote_spanned! {fident.span()=>
            pub const #fident: ::ferrix::Field<#ident, #ty> = ::ferrix::Field::new(#json_name);
        }
    });
    let index_consts = index_defs.iter().map(|(spec, name, _)| {
        let iident = &spec.name;
        quote_spanned! {iident.span()=>
            pub const #iident: ::ferrix::Index<#ident> = ::ferrix::Index::new(#name);
        }
    });
    let static_ident = format_ident!("__FERRIX_TABLE_{}", ident);

    Ok(quote! {
        #[derive(
            ::ferrix::__private::serde::Serialize,
            ::ferrix::__private::serde::Deserialize,
            ::ferrix::__private::ts_rs::TS,
            ::core::clone::Clone,
            ::core::fmt::Debug
        )]
        #[serde(crate = "::ferrix::__private::serde", rename_all = "camelCase")]
        #[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
        #strukt

        #(#cfgs)*
        impl ::ferrix::Table for #ident {
            const NAME: &'static str = #table_name;
            fn table_def() -> ::ferrix::TableDef {
                ::ferrix::TableDef {
                    name: #table_name,
                    indexes: ::std::vec![#(#index_def_exprs),*],
                    ts: || ::ferrix::__private::ts_inline::<#ident>(),
                    validate: ::ferrix::__private::validate_doc::<#ident>,
                }
            }
        }

        #(#cfgs)*
        #[allow(non_upper_case_globals)]
        impl #ident {
            #(#field_consts)*
            pub const _id: ::ferrix::Field<#ident, ::ferrix::Id<#ident>> = ::ferrix::Field::new("_id");
            pub const _creation_time: ::ferrix::Field<#ident, ::core::primitive::f64> = ::ferrix::Field::new("_creationTime");
            #(#index_consts)*
            pub const by_creation_time: ::ferrix::Index<#ident> = ::ferrix::Index::new("by_creation_time");
        }

        #(#cfgs)*
        #[::ferrix::__private::linkme::distributed_slice(::ferrix::__private::TABLES)]
        #[linkme(crate = ::ferrix::__private::linkme)]
        #[allow(non_upper_case_globals)]
        static #static_ident: fn() -> ::ferrix::TableDef = <#ident as ::ferrix::Table>::table_def;
    })
}
