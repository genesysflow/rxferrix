//! `#[query]`, `#[mutation]`, `#[action]` and the `internal_*` variants (CONTRACT.md §3.2).

use std::collections::HashMap;

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, quote_spanned};
use syn::ext::IdentExt;
use syn::spanned::Spanned;

use crate::util::{Errors, camel_case, check_owned_type, is_cfg};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FnKind {
    Query,
    Mutation,
    Action,
}

impl FnKind {
    fn ctx_name(self) -> &'static str {
        match self {
            FnKind::Query => "QueryCtx",
            FnKind::Mutation => "MutationCtx",
            FnKind::Action => "ActionCtx",
        }
    }

    fn macro_name(self, internal: bool) -> String {
        let base = match self {
            FnKind::Query => "query",
            FnKind::Mutation => "mutation",
            FnKind::Action => "action",
        };
        if internal {
            format!("internal_{base}")
        } else {
            base.to_owned()
        }
    }

    fn handler_variant(self) -> syn::Ident {
        let s = match self {
            FnKind::Query => "Query",
            FnKind::Mutation => "Mutation",
            FnKind::Action => "Action",
        };
        syn::Ident::new(s, Span::call_site())
    }

    fn trait_name(self) -> syn::Ident {
        let s = match self {
            FnKind::Query => "QueryFunction",
            FnKind::Mutation => "MutationFunction",
            FnKind::Action => "ActionFunction",
        };
        syn::Ident::new(s, Span::call_site())
    }
}

/// A non-context parameter, which becomes a field of the args struct.
struct Arg {
    ident: syn::Ident,
    ty: syn::Type,
    /// `#[serde(..)]` / `#[ts(..)]` / doc attributes moved from the parameter to the args struct field.
    field_attrs: Vec<syn::Attribute>,
}

/// Returns the `T` of a `...::Result<T, ...>` return type.
pub(crate) fn output_type(ty: &syn::Type) -> Option<&syn::Type> {
    let syn::Type::Path(tp) = ty else { return None };
    if tp.qself.is_some() {
        return None;
    }
    let last = tp.path.segments.last()?;
    if last.ident != "Result" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(ab) = &last.arguments else {
        return None;
    };
    match ab.args.first()? {
        syn::GenericArgument::Type(t) => Some(t),
        _ => None,
    }
}

/// The last path segment of a context type such as `ferrix::QueryCtx`.
fn ctx_type_name(ty: &syn::Type) -> Option<&syn::Ident> {
    match ty {
        syn::Type::Path(tp) if tp.qself.is_none() => {
            let last = tp.path.segments.last()?;
            last.arguments.is_none().then_some(&last.ident)
        }
        syn::Type::Paren(p) => ctx_type_name(&p.elem),
        syn::Type::Group(g) => ctx_type_name(&g.elem),
        _ => None,
    }
}

/// Parameter attributes that are moved to the args struct field (and stripped from the impl fn).
fn is_moved_param_attr(a: &syn::Attribute) -> bool {
    a.path().is_ident("serde") || a.path().is_ident("ts") || a.path().is_ident("doc")
}

/// Checks that the first parameter's type is `&<Kind>Ctx` (matching on the last path segment).
pub(crate) fn check_ctx_type(
    ty: &syn::Type,
    kind: FnKind,
    macro_name: &str,
    no_generics: bool,
    errors: &mut Errors,
) {
    let ctx_name = kind.ctx_name();
    match ty {
        syn::Type::Reference(r) if r.mutability.is_none() => match ctx_type_name(&r.elem) {
            Some(id) if id == ctx_name => {
                // `&'a` is already reported as a generics error; `&'static` cannot work with `&ctx`.
                if r.lifetime.is_some() && no_generics {
                    errors.error(ty, "the context reference cannot have an explicit lifetime");
                }
            }
            Some(id) if ["QueryCtx", "MutationCtx", "ActionCtx"].iter().any(|c| id == c) => errors.error(
                ty,
                format_args!("#[{macro_name}] functions take `&{ctx_name}` as their first parameter, found `&{id}`"),
            ),
            _ => errors.error(ty, format_args!("#[{macro_name}] functions take the context as their first parameter: `ctx: &{ctx_name}`")),
        },
        _ => match ctx_type_name(ty) {
            Some(id) if id == ctx_name => {
                errors.error(ty, format_args!("#[{macro_name}] functions take the context by shared reference: `ctx: &{ctx_name}`"))
            }
            _ => errors.error(ty, format_args!("#[{macro_name}] functions take the context as their first parameter: `ctx: &{ctx_name}`")),
        },
    }
}

pub fn expand_function(
    kind: FnKind,
    internal: bool,
    attr: TokenStream,
    item: TokenStream,
) -> syn::Result<TokenStream> {
    let macro_name = kind.macro_name(internal);
    let mut errors = Errors::default();
    if !attr.is_empty() {
        errors.error(
            &attr,
            format_args!("#[{macro_name}] does not take arguments"),
        );
    }
    let mut func: syn::ItemFn = match syn::parse2(item) {
        Ok(f) => f,
        Err(e) => {
            errors.push(syn::Error::new(
                e.span(),
                format_args!("#[{macro_name}] can only be used on an `async fn`"),
            ));
            return Err(errors.finish().unwrap_err());
        }
    };
    let ctx_name = kind.ctx_name();
    let sig = &func.sig;
    let name = sig.ident.clone();

    if sig.asyncness.is_none() {
        errors.error_at(
            sig.fn_token.span,
            format_args!("#[{macro_name}] functions must be `async fn`"),
        );
    }
    if let Some(c) = &sig.constness {
        errors.error(
            c,
            format_args!("#[{macro_name}] functions cannot be `const`"),
        );
    }
    if let Some(u) = &sig.unsafety {
        errors.error(
            u,
            format_args!("#[{macro_name}] functions cannot be `unsafe`"),
        );
    }
    if let Some(abi) = &sig.abi {
        errors.error(
            abi,
            format_args!("#[{macro_name}] functions cannot have an ABI"),
        );
    }
    if !sig.generics.params.is_empty() || sig.generics.where_clause.is_some() {
        errors.error(&sig.generics, format_args!("#[{macro_name}] functions cannot be generic (no type, const or lifetime parameters)"));
    }
    if let Some(v) = &sig.variadic {
        errors.error(
            v,
            format_args!("#[{macro_name}] functions cannot be variadic"),
        );
    }

    // --- return type
    let output = match &sig.output {
        syn::ReturnType::Default => {
            errors.error_at(
                sig.paren_token.span.close(),
                format_args!("#[{macro_name}] functions must declare a return type `Result<T>`; use `Result<()>` for no value"),
            );
            None
        }
        syn::ReturnType::Type(_, ty) => match output_type(ty) {
            Some(t) => {
                check_owned_type(t, "return", &mut errors);
                Some(t.clone())
            }
            None => {
                errors.error(ty, "the return type must be `Result<T>` (`ferrix::Result<T>` or `anyhow::Result<T>`)");
                None
            }
        },
    };

    // --- parameters
    let mut typed = Vec::new();
    for input in &sig.inputs {
        match input {
            syn::FnArg::Receiver(r) => errors.error(
                r,
                format_args!("#[{macro_name}] functions cannot take `self`"),
            ),
            syn::FnArg::Typed(pt) => typed.push(pt),
        }
    }
    let mut inputs = typed.into_iter();
    match inputs.next() {
        None => errors.error_at(
            sig.paren_token.span.join(),
            format_args!("#[{macro_name}] functions take the context as their first parameter: `ctx: &{ctx_name}`"),
        ),
        Some(pt) => {
            match &*pt.pat {
                syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => {}
                syn::Pat::Wild(_) => {}
                other => errors.error(other, "parameter patterns must be plain identifiers"),
            }
            check_ctx_type(&pt.ty, kind, &macro_name, sig.generics.params.is_empty(), &mut errors);
        }
    }

    let mut args: Vec<Arg> = Vec::new();
    let mut seen_json: HashMap<String, syn::Ident> = HashMap::new();
    for pt in inputs {
        let ident = match &*pt.pat {
            syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => pi.ident.clone(),
            other => {
                errors.error(
                    other,
                    "parameter patterns must be plain identifiers (they become the argument names)",
                );
                continue;
            }
        };
        check_owned_type(&pt.ty, "argument", &mut errors);
        let field_attrs: Vec<_> = pt
            .attrs
            .iter()
            .filter(|a| is_moved_param_attr(a))
            .cloned()
            .collect();
        for a in pt
            .attrs
            .iter()
            .filter(|a| is_cfg(a) || a.path().is_ident("cfg_attr"))
        {
            errors.error(a, "`#[cfg]` is not supported on function parameters");
        }
        let json = camel_case(&ident.unraw().to_string());
        if let Some(prev) = seen_json.insert(json.clone(), ident.clone()) {
            errors.error(
                &ident,
                format_args!(
                    "parameters `{}` and `{}` both serialize as `{json}`",
                    prev.unraw(),
                    ident.unraw()
                ),
            );
        }
        args.push(Arg {
            ident,
            ty: (*pt.ty).clone(),
            field_attrs,
        });
    }

    errors.finish()?;
    let output = output.expect("checked above");

    // The user's fn is emitted unchanged, except that parameter-level `#[serde]` / `#[ts]` / doc attributes (which
    // rustc would reject on a fn parameter) are moved to the args struct fields.
    for input in func.sig.inputs.iter_mut().skip(1) {
        if let syn::FnArg::Typed(pt) = input {
            pt.attrs.retain(|a| !is_moved_param_attr(a));
        }
    }

    // --- names
    let name_str = name.unraw().to_string();
    let args_ident = format_ident!("__ferrix_args_{}", name, span = name.span());
    let static_ident = format_ident!("__FERRIX_FN_{}", name, span = name.span());
    let vis = func.vis.clone();
    // `#[cfg]` goes on every generated item.
    let cfgs: Vec<_> = func.attrs.iter().filter(|a| is_cfg(a)).cloned().collect();
    let ref_doc = format!(" Function reference for [`{name_str}`](fn@{name}).");

    // --- args struct
    let field_defs = args.iter().map(|a| {
        let Arg {
            ident,
            ty,
            field_attrs,
        } = a;
        // Spanned on the type so that trait errors from the derives (e.g. a missing `Serialize`) point at it.
        quote_spanned! {ty.span()=> #(#field_attrs)* pub #ident: #ty }
    });
    let arg_idents: Vec<_> = args.iter().map(|a| &a.ident).collect();
    let arg_types: Vec<_> = args.iter().map(|a| &a.ty).collect();
    let tuple_var = syn::Ident::new("__ferrix_tuple", Span::mixed_site());
    let from_param = if args.is_empty() {
        quote!(_)
    } else {
        quote!(#tuple_var)
    };
    let from_fields = args.iter().enumerate().map(|(i, a)| {
        let ident = &a.ident;
        let index = syn::Index::from(i);
        quote!(#ident: #tuple_var.#index)
    });

    // --- handler
    let ctx_var = syn::Ident::new("__ferrix_ctx", Span::mixed_site());
    let args_var = syn::Ident::new("__ferrix_args", Span::mixed_site());
    let decoded_var = syn::Ident::new("__ferrix_decoded", Span::mixed_site());
    let out_var = syn::Ident::new("__ferrix_out", Span::mixed_site());
    let call_args = arg_idents.iter().map(|ident| quote!(#decoded_var.#ident));
    let variant = kind.handler_variant();
    let trait_ident = kind.trait_name();
    // Mutations and actions can be scheduled (`ctx.scheduler.run_after::<f>(..)`, crons); queries can't.
    let schedulable = match kind {
        FnKind::Query => quote!(),
        FnKind::Mutation | FnKind::Action => quote! {
            #(#cfgs)*
            impl ::ferrix::SchedulableFunction for #name {}
        },
    };
    let visibility = if internal {
        quote!(Internal)
    } else {
        quote!(Public)
    };
    // Spanned on the fn name so that e.g. "future cannot be sent between threads safely" points at the function.
    let handler = quote_spanned! {name.span()=>
        ::ferrix::__private::Handler::#variant(|#ctx_var, #args_var| ::std::boxed::Box::pin(async move {
            let #decoded_var: #args_ident = ::ferrix::__private::decode_args(#name_str, #args_var)?;
            // Value namespace: the user's fn (the reference struct is type-namespace only).
            let #out_var = #name(&#ctx_var, #(#call_args),*).await?;
            ::ferrix::__private::encode_output(#out_var)
        }))
    };

    Ok(quote! {
        #func

        #(#cfgs)*
        #[allow(non_camel_case_types)]
        #[derive(
            ::ferrix::__private::serde::Serialize,
            ::ferrix::__private::serde::Deserialize,
            ::ferrix::__private::ts_rs::TS
        )]
        #[serde(crate = "::ferrix::__private::serde", rename_all = "camelCase")]
        #[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase", optional_fields = nullable)]
        pub struct #args_ident { #(#field_defs),* }

        #(#cfgs)*
        impl ::core::convert::From<(#(#arg_types,)*)> for #args_ident {
            fn from(#from_param: (#(#arg_types,)*)) -> Self {
                Self { #(#from_fields),* }
            }
        }

        #[doc = #ref_doc]
        #(#cfgs)*
        #[allow(non_camel_case_types, dead_code)]
        #vis struct #name {}

        #(#cfgs)*
        impl ::ferrix::FunctionRef for #name {
            type Args = #args_ident;
            type Output = #output;
            fn path() -> ::std::string::String {
                ::ferrix::__private::function_path(::core::module_path!(), #name_str)
            }
        }

        #(#cfgs)*
        impl ::ferrix::#trait_ident for #name {}
        #schedulable

        #(#cfgs)*
        #[::ferrix::__private::linkme::distributed_slice(::ferrix::__private::FUNCTIONS)]
        #[linkme(crate = ::ferrix::__private::linkme)]
        #[allow(non_upper_case_globals)]
        static #static_ident: ::ferrix::__private::FunctionDef = ::ferrix::__private::FunctionDef {
            module_path: ::core::module_path!(),
            name: #name_str,
            visibility: ::ferrix::__private::Visibility::#visibility,
            handler: #handler,
            ts: || (
                ::ferrix::__private::ts_inline::<#args_ident>(),
                ::ferrix::__private::ts_inline::<#output>(),
            ),
        };
    })
}
