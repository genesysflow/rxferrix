//! `#[search_index(...)]` and `#[vector_index(...)]` on `#[table]` structs (CONTRACT.md §3.1).
//!
//! ```ignore
//! #[search_index(search_body, search = body, filter = [channel], tokenizer = "porter", language = "english",
//!                diacritics = "keep")]
//! #[vector_index(by_embedding, field = embedding, dims = 1536, filter = [channel], kind = "hnsw")]
//! ```

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::parse::ParseStream;
use syn::punctuated::Punctuated;
use syn::{Token, bracketed};

/// At most this many filter fields per search or vector index (sqlite-vec allows 16 metadata columns).
pub const MAX_FILTER_FIELDS: usize = 16;
/// Largest `dims` accepted (sqlite-vec's limit for `float[N]` columns).
pub const MAX_DIMENSIONS: usize = 8192;
/// `tokenizer = ...` values.
const TOKENIZERS: &[&str] = &["unicode61", "porter", "trigram"];
/// `language = ...` values (FTS5's Porter stemmer is English only). Keep in sync with `ferrix::Tokenizer::LANGUAGES`.
const LANGUAGES: &[&str] = &["english"];

/// A string argument and where it was written.
pub type StrArg = Option<(String, Span)>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Search,
    Vector,
}

impl Kind {
    pub fn attr(self) -> &'static str {
        match self {
            Kind::Search => "search_index",
            Kind::Vector => "vector_index",
        }
    }
}

/// One parsed `#[search_index]` / `#[vector_index]` attribute.
pub struct SpecialIndexSpec {
    pub kind: Kind,
    pub name: syn::Ident,
    /// `search = f` or `field = f`.
    pub field: Option<syn::Ident>,
    /// `dims = N` (vector indexes only).
    pub dims: Option<(usize, Span)>,
    pub filter: Vec<syn::Ident>,
    pub filter_span: Option<Span>,
    /// `tokenizer = "..."` (search indexes only), validated.
    pub tokenizer: StrArg,
    /// `language = "..."` (search indexes with the porter tokenizer), validated.
    pub language: StrArg,
    /// `diacritics = "remove" | "keep"` (search indexes), validated.
    pub diacritics: StrArg,
    /// `kind = "exact" | "hnsw"` (vector indexes), validated.
    pub vector_kind: StrArg,
    /// Problems found while parsing that don't prevent reading the rest (reported together with the others).
    pub errors: Vec<syn::Error>,
}

impl SpecialIndexSpec {
    pub fn parse(kind: Kind, attr: &syn::Attribute) -> syn::Result<SpecialIndexSpec> {
        let usage = match kind {
            Kind::Search => {
                "expected `#[search_index(name, search = field, filter = [field, ...], tokenizer = \"...\")]`"
            }
            Kind::Vector => {
                "expected `#[vector_index(name, field = field, dims = N, filter = [field, ...], kind = \"...\")]`"
            }
        };
        let syn::Meta::List(list) = &attr.meta else {
            return Err(syn::Error::new_spanned(attr, usage));
        };
        if list.tokens.is_empty() {
            return Err(syn::Error::new_spanned(attr, usage));
        }
        list.parse_args_with(|input: ParseStream| Self::parse_body(kind, usage, input))
    }

    fn parse_body(kind: Kind, usage: &str, input: ParseStream) -> syn::Result<SpecialIndexSpec> {
        let name: syn::Ident = input
            .parse()
            .map_err(|e| syn::Error::new(e.span(), usage))?;
        if input.peek(Token![=]) {
            return Err(input.error(format!("the index name comes first, without `=`; {usage}")));
        }
        let mut spec = SpecialIndexSpec {
            kind,
            name,
            field: None,
            dims: None,
            filter: Vec::new(),
            filter_span: None,
            tokenizer: None,
            language: None,
            diacritics: None,
            vector_kind: None,
            errors: Vec::new(),
        };
        let mut seen_filter = false;
        while !input.is_empty() {
            input.parse::<Token![,]>()?;
            if input.is_empty() {
                break;
            }
            let key: syn::Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            let key_name = key.to_string();
            let field_key = match kind {
                Kind::Search => "search",
                Kind::Vector => "field",
            };
            if key_name == field_key {
                let f: syn::Ident = input.parse()?;
                if spec.field.is_some() {
                    spec.errors.push(syn::Error::new(
                        key.span(),
                        format!("duplicate `{field_key}` argument"),
                    ));
                }
                spec.field = Some(f);
            } else if key_name == "dims" && kind == Kind::Vector {
                let lit: syn::LitInt = input.parse()?;
                if spec.dims.is_some() {
                    spec.errors
                        .push(syn::Error::new(key.span(), "duplicate `dims` argument"));
                }
                match lit.base10_parse::<usize>() {
                    Ok(n) if (1..=MAX_DIMENSIONS).contains(&n) => spec.dims = Some((n, lit.span())),
                    _ => {
                        spec.errors.push(syn::Error::new(
                            lit.span(),
                            format!("`dims` must be an integer between 1 and {MAX_DIMENSIONS}"),
                        ));
                        spec.dims = Some((1, lit.span()));
                    }
                }
            } else if key_name == "filter" {
                let content;
                let bracket = bracketed!(content in input);
                let fields = Punctuated::<syn::Ident, Token![,]>::parse_terminated(&content)?;
                if seen_filter {
                    spec.errors
                        .push(syn::Error::new(key.span(), "duplicate `filter` argument"));
                }
                seen_filter = true;
                spec.filter = fields.into_iter().collect();
                spec.filter_span = Some(bracket.span.join());
            } else if let Some((allowed, slot)) = match (kind, key_name.as_str()) {
                (Kind::Search, "tokenizer") => Some((TOKENIZERS, &mut spec.tokenizer)),
                (Kind::Search, "language") => Some((LANGUAGES, &mut spec.language)),
                (Kind::Search, "diacritics") => Some((&["remove", "keep"][..], &mut spec.diacritics)),
                (Kind::Vector, "kind") => Some((&["exact", "hnsw"][..], &mut spec.vector_kind)),
                _ => None,
            } {
                let lit: syn::LitStr = input.parse()?;
                let value = lit.value();
                if slot.is_some() {
                    spec.errors
                        .push(syn::Error::new(key.span(), format!("duplicate `{key_name}` argument")));
                }
                if !allowed.contains(&value.as_str()) {
                    let list = allowed.iter().map(|a| format!("\"{a}\"")).collect::<Vec<_>>().join(", ");
                    let what = if key_name == "language" {
                        format!("unsupported language \"{value}\"; the porter stemmer supports {list}")
                    } else {
                        format!("unknown {key_name} \"{value}\"; expected one of {list}")
                    };
                    spec.errors.push(syn::Error::new(lit.span(), what));
                }
                *slot = Some((value, lit.span()));
            } else {
                let expected = match kind {
                    Kind::Search => "`search`, `filter`, `tokenizer`, `language` or `diacritics`",
                    Kind::Vector => "`field`, `dims`, `filter` or `kind`",
                };
                return Err(syn::Error::new(
                    key.span(),
                    format!(
                        "unknown #[{}] argument `{key_name}`; expected {expected}",
                        kind.attr()
                    ),
                ));
            }
        }
        if let Some((_, span)) = &spec.language
            && spec.tokenizer.as_ref().is_none_or(|(t, _)| t != "porter")
        {
            spec.errors.push(syn::Error::new(
                *span,
                "`language` only applies to `tokenizer = \"porter\"` (stemming)",
            ));
        }
        Ok(spec)
    }

    /// The `::ferrix::Tokenizer` expression of a search index.
    pub fn tokenizer_expr(&self) -> TokenStream {
        let base = match self.tokenizer.as_ref().map(|(t, _)| t.as_str()) {
            Some("porter") => {
                let language = self.language.as_ref().map_or("english", |(l, _)| l.as_str());
                quote!(::ferrix::Tokenizer::porter(#language))
            }
            Some("trigram") => quote!(::ferrix::Tokenizer::trigram()),
            _ => quote!(::ferrix::Tokenizer::unicode61()),
        };
        match self.diacritics.as_ref().map(|(d, _)| d.as_str()) {
            Some("keep") => quote!(#base.keep_diacritics()),
            _ => base,
        }
    }

    /// The `::ferrix::VectorIndexKind` expression of a vector index.
    pub fn vector_kind_expr(&self) -> TokenStream {
        match self.vector_kind.as_ref().map(|(k, _)| k.as_str()) {
            Some("hnsw") => quote!(::ferrix::VectorIndexKind::Hnsw),
            _ => quote!(::ferrix::VectorIndexKind::Exact),
        }
    }
}

/// What a field type looks like syntactically, for early errors with good spans. Types the macro can't see through
/// (aliases, paths to other types) are `Unknown`: the generated `__private::assert_*_field` check catches those.
#[derive(Debug, PartialEq, Eq)]
pub enum TypeShape {
    /// `String` / `Option<String>`.
    Text,
    /// `Vec<f32>` / `Option<Vec<f32>>`.
    F32Vec,
    /// Clearly something else (`Vec<f64>`, `bool`, `u32`, `Option<i64>`, ...).
    Other,
    Unknown,
}

const PRIMITIVES: &[&str] = &[
    "bool", "char", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128",
    "usize", "f32", "f64",
];

fn single_generic(seg: &syn::PathSegment) -> Option<&syn::Type> {
    match &seg.arguments {
        syn::PathArguments::AngleBracketed(a) if a.args.len() == 1 => match a.args.first() {
            Some(syn::GenericArgument::Type(t)) => Some(t),
            _ => None,
        },
        _ => None,
    }
}

fn last_segment(ty: &syn::Type) -> Option<&syn::PathSegment> {
    match ty {
        syn::Type::Path(p) if p.qself.is_none() => p.path.segments.last(),
        syn::Type::Group(g) => last_segment(&g.elem),
        syn::Type::Paren(p) => last_segment(&p.elem),
        _ => None,
    }
}

pub fn type_shape(ty: &syn::Type) -> TypeShape {
    shape(ty, true)
}

fn shape(ty: &syn::Type, allow_option: bool) -> TypeShape {
    match ty {
        syn::Type::Array(_)
        | syn::Type::Tuple(_)
        | syn::Type::Reference(_)
        | syn::Type::Slice(_) => {
            return TypeShape::Other;
        }
        _ => {}
    }
    let Some(seg) = last_segment(ty) else {
        return TypeShape::Unknown;
    };
    let name = seg.ident.to_string();
    match name.as_str() {
        "String" if seg.arguments.is_none() => TypeShape::Text,
        "Option" if allow_option => match single_generic(seg) {
            Some(inner) => match shape(inner, false) {
                TypeShape::Unknown => TypeShape::Unknown,
                s => s,
            },
            None => TypeShape::Unknown,
        },
        "Vec" => match single_generic(seg).and_then(last_segment) {
            Some(inner) if inner.ident == "f32" && inner.arguments.is_none() => TypeShape::F32Vec,
            Some(inner)
                if PRIMITIVES.contains(&inner.ident.to_string().as_str())
                    || inner.ident == "String" =>
            {
                TypeShape::Other
            }
            _ => TypeShape::Unknown,
        },
        n if PRIMITIVES.contains(&n) => TypeShape::Other,
        "Option" | "HashMap" | "BTreeMap" | "HashSet" | "BTreeSet" | "Id" => TypeShape::Other,
        _ => TypeShape::Unknown,
    }
}
