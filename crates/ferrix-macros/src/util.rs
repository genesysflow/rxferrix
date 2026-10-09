use proc_macro2::{Span, TokenStream, TokenTree};
use quote::ToTokens;
use syn::visit::Visit;

/// Collects several errors so that one expansion can report all of them at once.
#[derive(Default)]
pub struct Errors(Option<syn::Error>);

impl Errors {
    pub fn push(&mut self, err: syn::Error) {
        match &mut self.0 {
            Some(e) => e.combine(err),
            None => self.0 = Some(err),
        }
    }

    pub fn error(&mut self, tokens: impl ToTokens, msg: impl std::fmt::Display) {
        self.push(syn::Error::new_spanned(tokens, msg));
    }

    pub fn error_at(&mut self, span: Span, msg: impl std::fmt::Display) {
        self.push(syn::Error::new(span, msg));
    }

    pub fn finish(self) -> syn::Result<()> {
        match self.0 {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// serde's `rename_all = "camelCase"` rule for fields (`serde_derive::internals::case`), which ts-rs mirrors:
/// drop underscores, uppercase the character following one, then lowercase the first character.
/// Other characters are left unchanged. `created_by` → `createdBy`, `_foo` → `foo`, `foo_1` → `foo1`.
pub fn camel_case(field: &str) -> String {
    let mut pascal = String::with_capacity(field.len());
    let mut capitalize = true;
    for ch in field.chars() {
        if ch == '_' {
            capitalize = true;
        } else if capitalize {
            pascal.push(ch.to_ascii_uppercase());
            capitalize = false;
        } else {
            pascal.push(ch);
        }
    }
    match pascal.chars().next() {
        Some(first) => first.to_ascii_lowercase().to_string() + &pascal[first.len_utf8()..],
        None => field.to_owned(),
    }
}

/// One top-level `key ...` item of an attribute like `#[serde(a, b = "x", c(d))]`.
pub struct AttrItem {
    pub tokens: TokenStream,
}

impl AttrItem {
    /// The leading identifier (`rename_all`, `crate`, ...), if any.
    pub fn key(&self) -> Option<proc_macro2::Ident> {
        match self.tokens.clone().into_iter().next() {
            Some(TokenTree::Ident(i)) => Some(i),
            _ => None,
        }
    }

    /// For `key = "lit"`, the string literal.
    pub fn str_value(&self) -> Option<syn::LitStr> {
        let mut it = self.tokens.clone().into_iter().skip(1);
        match (it.next(), it.next(), it.next()) {
            (Some(TokenTree::Punct(p)), Some(TokenTree::Literal(l)), None)
                if p.as_char() == '=' =>
            {
                syn::parse2::<syn::LitStr>(TokenTree::Literal(l).into()).ok()
            }
            _ => None,
        }
    }
}

/// Splits the arguments of a list attribute (`#[name(...)]`) at top-level commas. Returns `None` for attributes
/// that are not in list form.
pub fn attr_items(attr: &syn::Attribute) -> Option<Vec<AttrItem>> {
    let syn::Meta::List(list) = &attr.meta else {
        return None;
    };
    let mut items = Vec::new();
    let mut cur = TokenStream::new();
    for tt in list.tokens.clone() {
        match &tt {
            TokenTree::Punct(p) if p.as_char() == ',' => {
                if !cur.is_empty() {
                    items.push(AttrItem {
                        tokens: std::mem::take(&mut cur),
                    });
                }
            }
            _ => cur.extend([tt]),
        }
    }
    if !cur.is_empty() {
        items.push(AttrItem { tokens: cur });
    }
    Some(items)
}

/// Rebuilds `#[name(items...)]` from the given items, or returns `None` if no items remain.
pub fn rebuild_attr(attr: &syn::Attribute, items: &[AttrItem]) -> Option<syn::Attribute> {
    if items.is_empty() {
        return None;
    }
    let mut attr = attr.clone();
    if let syn::Meta::List(list) = &mut attr.meta {
        let mut tokens = TokenStream::new();
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                tokens.extend(quote::quote!(,));
            }
            tokens.extend(item.tokens.clone());
        }
        list.tokens = tokens;
    }
    Some(attr)
}

pub fn is_cfg(attr: &syn::Attribute) -> bool {
    attr.path().is_ident("cfg")
}

/// Rejects types that cannot be deserialized into an owned `'static` value: references, non-`'static`
/// lifetimes, `impl Trait` and `_`.
pub fn check_owned_type(ty: &syn::Type, what: &str, errors: &mut Errors) {
    struct V<'a> {
        what: &'a str,
        errors: &'a mut Errors,
    }
    impl<'ast> Visit<'ast> for V<'_> {
        fn visit_type_reference(&mut self, r: &'ast syn::TypeReference) {
            self.errors.error(
                r,
                format_args!("{} types must be owned: references are not allowed (use e.g. `String` instead of `&str`)", self.what),
            );
        }
        fn visit_lifetime(&mut self, l: &'ast syn::Lifetime) {
            if l.ident != "static" {
                self.errors.error(
                    l,
                    format_args!(
                        "{} types must be owned and `'static`: lifetimes are not allowed",
                        self.what
                    ),
                );
            }
        }
        fn visit_type_impl_trait(&mut self, t: &'ast syn::TypeImplTrait) {
            self.errors.error(
                t,
                format_args!("{} types cannot use `impl Trait`", self.what),
            );
        }
        fn visit_type_infer(&mut self, t: &'ast syn::TypeInfer) {
            self.errors.error(
                t,
                format_args!(
                    "{} types must be spelled out; `_` is not allowed",
                    self.what
                ),
            );
        }
    }
    V { what, errors }.visit_type(ty);
}

#[cfg(test)]
mod tests {
    use super::camel_case;

    #[test]
    fn camel_case_matches_serde() {
        assert_eq!(camel_case("created_by"), "createdBy");
        assert_eq!(camel_case("text"), "text");
        assert_eq!(camel_case("_foo"), "foo");
        assert_eq!(camel_case("foo_1"), "foo1");
        assert_eq!(camel_case("foo__bar"), "fooBar");
        assert_eq!(camel_case("fooBar"), "fooBar");
        assert_eq!(camel_case("a_b_c"), "aBC");
        assert_eq!(camel_case("_"), "_");
    }
}
