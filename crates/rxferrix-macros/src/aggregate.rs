//! `#[aggregate(name, group = [fields], sum = [fields])]` on a `#[table]` struct: a live aggregate (CONTRACT.md §1.12).

use syn::parse::{ParseStream, Parser};
use syn::punctuated::Punctuated;
use syn::{Ident, Token, bracketed};

/// A parsed `#[aggregate(...)]`.
pub struct AggregateSpec {
    pub name: Ident,
    pub group: Vec<Ident>,
    pub sum: Vec<Ident>,
    /// Problems found while parsing that don't stop it (duplicate keys).
    pub errors: Vec<syn::Error>,
}

impl AggregateSpec {
    pub fn parse(attr: &syn::Attribute) -> syn::Result<AggregateSpec> {
        let list = match &attr.meta {
            syn::Meta::List(list) => list,
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "expected #[aggregate(name, group = [field, ...], sum = [field, ...])]",
                ));
            }
        };
        let parser = |input: ParseStream| -> syn::Result<AggregateSpec> {
            let name: Ident = input.parse()?;
            if input.peek(Token![=]) {
                return Err(
                    input.error("the aggregate's name comes first: #[aggregate(name, group = [...], sum = [...])]")
                );
            }
            let mut spec = AggregateSpec { name, group: Vec::new(), sum: Vec::new(), errors: Vec::new() };
            let (mut seen_group, mut seen_sum) = (false, false);
            while !input.is_empty() {
                input.parse::<Token![,]>()?;
                if input.is_empty() {
                    break;
                }
                let key: Ident = input.parse()?;
                input.parse::<Token![=]>()?;
                let content;
                bracketed!(content in input);
                let fields: Vec<Ident> =
                    Punctuated::<Ident, Token![,]>::parse_terminated(&content)?.into_iter().collect();
                match key.to_string().as_str() {
                    "group" if seen_group => spec.errors.push(syn::Error::new(key.span(), "duplicate `group`")),
                    "sum" if seen_sum => spec.errors.push(syn::Error::new(key.span(), "duplicate `sum`")),
                    "group" => {
                        seen_group = true;
                        spec.group = fields;
                    }
                    "sum" => {
                        seen_sum = true;
                        spec.sum = fields;
                    }
                    other => {
                        return Err(syn::Error::new(
                            key.span(),
                            format!("unknown #[aggregate] key `{other}` (expected `group = [...]` or `sum = [...]`)"),
                        ));
                    }
                }
            }
            Ok(spec)
        };
        parser.parse2(list.tokens.clone())
    }
}
