//! `#[object]`: a non-table type used in function signatures (CONTRACT.md §3.4).
//!
//! Adds `Serialize`, `Deserialize` and `TS` through ferrix's re-exported crates, with `rename_all = "camelCase"`, so
//! an app needs no direct dependency on serde or ts-rs.

use proc_macro2::TokenStream;
use quote::quote;
use syn::Token;
use syn::punctuated::Punctuated;

use crate::util::{Errors, attr_items, rebuild_attr};

/// Derives `#[object]` adds itself; users must not derive them again.
const ADDED_DERIVES: &[&str] = &["Serialize", "Deserialize", "TS"];

pub fn expand_object(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let mut errors = Errors::default();
    if !attr.is_empty() {
        errors.error(&attr, "#[object] does not take arguments");
    }
    let mut item: syn::Item = syn::parse2(item)?;
    let attrs = match &mut item {
        syn::Item::Struct(s) => &mut s.attrs,
        syn::Item::Enum(e) => &mut e.attrs,
        other => {
            return Err(syn::Error::new_spanned(other, "#[object] can only be used on a struct or an enum"));
        }
    };

    // A `rename_all` the user chose (in #[serde] or #[ts]) replaces the camelCase default.
    let mut custom_rename_all = false;
    let mut kept = Vec::new();
    for attr in std::mem::take(attrs) {
        let path = attr.path();
        if path.is_ident("object") {
            errors.error(&attr, "duplicate #[object] attribute");
        } else if path.is_ident("derive") {
            match attr.parse_args_with(Punctuated::<syn::Path, Token![,]>::parse_terminated) {
                Ok(paths) => {
                    for p in &paths {
                        let last = p.segments.last().map(|s| s.ident.to_string()).unwrap_or_default();
                        if ADDED_DERIVES.contains(&last.as_str()) {
                            errors.error(
                                p,
                                format_args!("#[object] already derives `{last}`; remove it from this #[derive]"),
                            );
                        }
                    }
                }
                Err(e) => errors.push(e),
            }
            kept.push(attr);
        } else if path.is_ident("serde") || path.is_ident("ts") {
            let which = if path.is_ident("serde") { "serde" } else { "ts" };
            let Some(items) = attr_items(&attr) else {
                kept.push(attr);
                continue;
            };
            let mut kept_items = Vec::new();
            for item in items {
                match item.key().map(|k| k.to_string()).as_deref() {
                    Some("crate") => errors.error(
                        &item.tokens,
                        format_args!("#[object] sets `#[{which}(crate = ...)]` itself; remove this"),
                    ),
                    Some("rename_all") => {
                        custom_rename_all = true;
                        kept_items.push(item);
                    }
                    _ => kept_items.push(item),
                }
            }
            kept.extend(rebuild_attr(&attr, &kept_items));
        } else {
            kept.push(attr);
        }
    }
    *attrs = kept;
    errors.finish()?;

    let rename_all = if custom_rename_all {
        quote!()
    } else {
        quote! {
            #[serde(rename_all = "camelCase")]
            #[ts(rename_all = "camelCase")]
        }
    };
    let shape_impl = crate::shape::object_impl(&item);
    Ok(quote! {
        #[derive(
            ::ferrix::__private::serde::Serialize,
            ::ferrix::__private::serde::Deserialize,
            ::ferrix::__private::ts_rs::TS
        )]
        #[serde(crate = "::ferrix::__private::serde")]
        #[ts(crate = "::ferrix::__private::ts_rs")]
        #rename_all
        #item

        #shape_impl
    })
}
