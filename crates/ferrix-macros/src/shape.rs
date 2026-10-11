//! The `shape` expressions of `#[table]` and `#[object]` (`ferrix::shape`): each field's JSON name, as serde writes
//! it, and its type's shape. Whatever can't be told from the attributes becomes `Shape::Json`, or makes the object
//! open: a shape is never wrong, at worst unspecific.

use proc_macro2::TokenStream;
use quote::quote;
use syn::ext::IdentExt;

use crate::util::{attr_items, camel_case};

/// A serde `rename_all` rule for field and variant names.
#[derive(Clone, Copy, PartialEq)]
pub enum RenameAll {
    Lower,
    Upper,
    Pascal,
    Camel,
    Snake,
    ScreamingSnake,
    Kebab,
    ScreamingKebab,
}

impl RenameAll {
    fn parse(rule: &str) -> Option<RenameAll> {
        Some(match rule {
            "lowercase" => RenameAll::Lower,
            "UPPERCASE" => RenameAll::Upper,
            "PascalCase" => RenameAll::Pascal,
            "camelCase" => RenameAll::Camel,
            "snake_case" => RenameAll::Snake,
            "SCREAMING_SNAKE_CASE" => RenameAll::ScreamingSnake,
            "kebab-case" => RenameAll::Kebab,
            "SCREAMING-KEBAB-CASE" => RenameAll::ScreamingKebab,
            _ => return None,
        })
    }

    /// serde's rule for a field (a snake_case Rust name).
    fn field(self, name: &str) -> String {
        match self {
            RenameAll::Snake | RenameAll::Lower => name.to_string(),
            RenameAll::Upper | RenameAll::ScreamingSnake => name.to_ascii_uppercase(),
            RenameAll::Camel => camel_case(name),
            RenameAll::Pascal => {
                let camel = camel_case(name);
                let mut chars = camel.chars();
                chars.next().map(|c| c.to_ascii_uppercase().to_string() + chars.as_str()).unwrap_or_default()
            }
            RenameAll::Kebab => name.replace('_', "-"),
            RenameAll::ScreamingKebab => name.replace('_', "-").to_ascii_uppercase(),
        }
    }
}

/// The container attributes that matter for the shape.
#[derive(Default)]
pub struct Container {
    /// `None` when `rename_all` has a form the macro doesn't read: field names are then unknown.
    pub rename_all: Option<RenameAll>,
    pub transparent: bool,
    /// `tag`, `content` or `untagged`: an enum's JSON form depends on its variants.
    pub tagged: bool,
}

impl Container {
    pub fn read(attrs: &[syn::Attribute], default_rule: RenameAll) -> Container {
        let mut c = Container { rename_all: Some(default_rule), ..Default::default() };
        for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
            for item in attr_items(attr).unwrap_or_default() {
                match item.key().map(|k| k.to_string()).as_deref() {
                    Some("rename_all") => c.rename_all = item.str_value().and_then(|v| RenameAll::parse(&v.value())),
                    Some("transparent") => c.transparent = true,
                    Some("tag" | "content" | "untagged") => c.tagged = true,
                    _ => {}
                }
            }
        }
        c
    }
}

/// How serde writes one field.
enum Field<'a> {
    /// Under this name, in the type's shape (`Optional` when `skip_serializing_if` may leave it out).
    Named {
        name: String,
        ty: &'a syn::Type,
        maybe_absent: bool,
        json: bool,
    },
    /// `#[serde(flatten)]`.
    Flatten(&'a syn::Type),
    /// Under a name the macro can't tell.
    Unknown,
    Skipped,
}

fn field<'a>(f: &'a syn::Field, rule: Option<RenameAll>) -> Field<'a> {
    let rust = f.ident.as_ref().map(|i| i.unraw().to_string());
    let mut name = match (rust, rule) {
        (Some(rust), Some(rule)) => Some(rule.field(&rust)),
        _ => None,
    };
    let (mut maybe_absent, mut json, mut flatten) = (false, false, false);
    for attr in f.attrs.iter().filter(|a| a.path().is_ident("serde")) {
        for item in attr_items(attr).unwrap_or_default() {
            match item.key().map(|k| k.to_string()).as_deref() {
                Some("rename") => name = item.str_value().map(|v| v.value()),
                Some("skip" | "skip_serializing") => return Field::Skipped,
                Some("skip_serializing_if") => maybe_absent = true,
                Some("flatten") => flatten = true,
                Some("with" | "serialize_with") => json = true,
                _ => {}
            }
        }
    }
    if flatten {
        return if json { Field::Unknown } else { Field::Flatten(&f.ty) };
    }
    match name {
        Some(name) => Field::Named { name, ty: &f.ty, maybe_absent, json },
        None => Field::Unknown,
    }
}

/// `ferrix::shape::Shape` of `ty`: its `HasShape`, else `Json`.
pub fn shape_of(ty: &syn::Type) -> TokenStream {
    quote! {{
        #[allow(unused_imports)]
        use ::ferrix::shape::{ProbeFallback as _, ProbeHasShape as _};
        // The double borrow picks `HasShape` when the type has it, else the fallback.
        #[allow(clippy::needless_borrow)]
        let shape = (&&::ferrix::shape::ShapeProbe::<#ty>::new()).probe_shape();
        shape
    }}
}

/// A `Shape::Struct` of named fields.
pub fn struct_shape(fields: &syn::FieldsNamed, rule: Option<RenameAll>) -> TokenStream {
    let steps = fields.named.iter().map(|f| match field(f, rule) {
        Field::Named { name, ty, maybe_absent, json } => {
            let shape = if json { quote!(::ferrix::shape::Shape::Json) } else { shape_of(ty) };
            if maybe_absent { quote!(s.optional_field(#name, #shape);) } else { quote!(s.field(#name, #shape);) }
        }
        Field::Flatten(ty) => {
            let shape = shape_of(ty);
            quote!(s.flatten(#shape);)
        }
        Field::Unknown => quote!(s.open();),
        Field::Skipped => quote!(),
    });
    quote! {{
        let mut s = ::ferrix::shape::StructShape::default();
        #(#steps)*
        ::ferrix::shape::Shape::Struct(s)
    }}
}

/// The `HasShape` impl of an `#[object]` type, or nothing for generic types (their shape is `Json`).
pub fn object_impl(item: &syn::Item) -> TokenStream {
    let (ident, generics, attrs) = match item {
        syn::Item::Struct(s) => (&s.ident, &s.generics, &s.attrs),
        syn::Item::Enum(e) => (&e.ident, &e.generics, &e.attrs),
        _ => return quote!(),
    };
    if !generics.params.is_empty() {
        return quote!();
    }
    let container = Container::read(attrs, RenameAll::Camel);
    let body = match item {
        syn::Item::Struct(s) => match &s.fields {
            syn::Fields::Named(named) if container.transparent => match named.named.first() {
                Some(f) if named.named.len() == 1 => shape_of(&f.ty),
                _ => quote!(::ferrix::shape::Shape::Json),
            },
            syn::Fields::Named(named) => struct_shape(named, container.rename_all),
            // A newtype is written as what it wraps; other tuple structs as arrays of mixed values.
            syn::Fields::Unnamed(u) if u.unnamed.len() == 1 => shape_of(&u.unnamed[0].ty),
            _ => quote!(::ferrix::shape::Shape::Json),
        },
        syn::Item::Enum(e) => {
            let unit_only = e.variants.iter().all(|v| matches!(v.fields, syn::Fields::Unit));
            if unit_only && !container.tagged && !e.variants.is_empty() {
                quote!(::ferrix::shape::Shape::Text)
            } else {
                quote!(::ferrix::shape::Shape::Json)
            }
        }
        _ => unreachable!(),
    };
    quote! {
        impl ::ferrix::shape::HasShape for #ident {
            fn shape() -> ::ferrix::shape::Shape {
                ::ferrix::shape::guard(::core::any::type_name::<Self>(), || #body)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_rules_match_serde() {
        let cases = [
            (RenameAll::Camel, "created_by", "createdBy"),
            (RenameAll::Pascal, "created_by", "CreatedBy"),
            (RenameAll::ScreamingSnake, "created_by", "CREATED_BY"),
            (RenameAll::Kebab, "created_by", "created-by"),
            (RenameAll::ScreamingKebab, "created_by", "CREATED-BY"),
            (RenameAll::Upper, "created_by", "CREATED_BY"),
            (RenameAll::Lower, "created_by", "created_by"),
        ];
        for (rule, from, to) in cases {
            assert_eq!(rule.field(from), to);
        }
    }
}
