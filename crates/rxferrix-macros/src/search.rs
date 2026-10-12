//! `#[search_index(...)]` and `#[vector_index(...)]` on `#[table]` structs (CONTRACT.md §3.1).
//!
//! ```ignore
//! #[search_index(search_body, search = body, filter = [channel], tokenizer = "porter", language = "english",
//!                diacritics = "keep")]
//! #[search_index(search_ro, search = body, tokenizer = "snowball", language = "romanian")]
//! #[vector_index(by_embedding, field = embedding, dims = 1536, filter = [channel], kind = "hnsw")]
//! #[vector_index(by_tuned, field = embedding, dims = 1536, kind = "hnsw", m = 32, ef_construction = 256, ef_search = 64)]
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
const TOKENIZERS: &[&str] = &["unicode61", "porter", "trigram", "snowball"];
/// `language = ...` values of `tokenizer = "porter"` (FTS5's Porter stemmer is English only). Keep in sync with
/// `rxferrix::Tokenizer::LANGUAGES`.
const PORTER_LANGUAGES: &[&str] = &["english"];
/// `language = ...` values of `tokenizer = "snowball"`. Keep in sync with `rxferrix::Tokenizer::SNOWBALL_LANGUAGES`.
pub const SNOWBALL_LANGUAGES: &[&str] = &[
    "arabic",
    "danish",
    "dutch",
    "english",
    "finnish",
    "french",
    "german",
    "greek",
    "hungarian",
    "italian",
    "norwegian",
    "portuguese",
    "romanian",
    "russian",
    "spanish",
    "swedish",
    "tamil",
    "turkish",
];
/// Ranges of the HNSW parameters (`rxferrix::HnswParams::{M, EF_CONSTRUCTION, EF_SEARCH}_RANGE`; keep in sync).
pub const HNSW_M: (usize, usize) = (4, 64);
pub const HNSW_EF_CONSTRUCTION: (usize, usize) = (16, 2048);
pub const HNSW_EF_SEARCH: (usize, usize) = (1, 4096);

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
    /// `m = N`, `ef_construction = N`, `ef_search = N` (HNSW vector indexes), validated.
    pub m: Option<(usize, Span)>,
    pub ef_construction: Option<(usize, Span)>,
    pub ef_search: Option<(usize, Span)>,
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
                "expected `#[vector_index(name, field = field, dims = N, filter = [field, ...], kind = \"...\", m = N, ef_construction = N, ef_search = N)]`"
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
        let name: syn::Ident = input.parse().map_err(|e| syn::Error::new(e.span(), usage))?;
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
            m: None,
            ef_construction: None,
            ef_search: None,
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
                    spec.errors.push(syn::Error::new(key.span(), format!("duplicate `{field_key}` argument")));
                }
                spec.field = Some(f);
            } else if key_name == "dims" && kind == Kind::Vector {
                let lit: syn::LitInt = input.parse()?;
                if spec.dims.is_some() {
                    spec.errors.push(syn::Error::new(key.span(), "duplicate `dims` argument"));
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
            } else if let Some(((lo, hi), slot)) = match (kind, key_name.as_str()) {
                (Kind::Vector, "m") => Some((HNSW_M, &mut spec.m)),
                (Kind::Vector, "ef_construction") => Some((HNSW_EF_CONSTRUCTION, &mut spec.ef_construction)),
                (Kind::Vector, "ef_search") => Some((HNSW_EF_SEARCH, &mut spec.ef_search)),
                _ => None,
            } {
                let lit: syn::LitInt = input.parse()?;
                if slot.is_some() {
                    spec.errors.push(syn::Error::new(key.span(), format!("duplicate `{key_name}` argument")));
                }
                match lit.base10_parse::<usize>() {
                    Ok(n) if (lo..=hi).contains(&n) => *slot = Some((n, key.span())),
                    _ => spec.errors.push(syn::Error::new(
                        lit.span(),
                        format!("`{key_name}` must be an integer between {lo} and {hi}"),
                    )),
                }
            } else if key_name == "filter" {
                let content;
                let bracket = bracketed!(content in input);
                let fields = Punctuated::<syn::Ident, Token![,]>::parse_terminated(&content)?;
                if seen_filter {
                    spec.errors.push(syn::Error::new(key.span(), "duplicate `filter` argument"));
                }
                seen_filter = true;
                spec.filter = fields.into_iter().collect();
                spec.filter_span = Some(bracket.span.join());
            } else if let Some((allowed, slot)) = match (kind, key_name.as_str()) {
                (Kind::Search, "tokenizer") => Some((TOKENIZERS, &mut spec.tokenizer)),
                (Kind::Search, "language") => Some((SNOWBALL_LANGUAGES, &mut spec.language)),
                (Kind::Search, "diacritics") => Some((&["remove", "keep"][..], &mut spec.diacritics)),
                (Kind::Vector, "kind") => Some((&["exact", "hnsw"][..], &mut spec.vector_kind)),
                _ => None,
            } {
                let lit: syn::LitStr = input.parse()?;
                let value = lit.value();
                if slot.is_some() {
                    spec.errors.push(syn::Error::new(key.span(), format!("duplicate `{key_name}` argument")));
                }
                if !allowed.contains(&value.as_str()) {
                    let list = allowed.iter().map(|a| format!("\"{a}\"")).collect::<Vec<_>>().join(", ");
                    let what = if key_name == "language" {
                        format!("unsupported language \"{value}\"; the snowball stemmers support {list}")
                    } else {
                        format!("unknown {key_name} \"{value}\"; expected one of {list}")
                    };
                    spec.errors.push(syn::Error::new(lit.span(), what));
                }
                *slot = Some((value, lit.span()));
            } else {
                let expected = match kind {
                    Kind::Search => "`search`, `filter`, `tokenizer`, `language` or `diacritics`",
                    Kind::Vector => "`field`, `dims`, `filter`, `kind`, `m`, `ef_construction` or `ef_search`",
                };
                return Err(syn::Error::new(
                    key.span(),
                    format!("unknown #[{}] argument `{key_name}`; expected {expected}", kind.attr()),
                ));
            }
        }
        let tokenizer = spec.tokenizer.as_ref().map(|(t, _)| t.as_str());
        match (&spec.language, tokenizer) {
            (Some((l, span)), Some("porter")) if !PORTER_LANGUAGES.contains(&l.as_str()) => {
                // Only report languages that exist at all (an unknown one was reported above).
                if SNOWBALL_LANGUAGES.contains(&l.as_str()) {
                    spec.errors.push(syn::Error::new(
                        *span,
                        format!(
                            "unsupported language \"{l}\"; the porter stemmer supports \"english\" (use `tokenizer = \"snowball\"` for other languages)"
                        ),
                    ));
                }
            }
            (Some(_), Some("porter" | "snowball")) => {}
            (Some((_, span)), _) => spec.errors.push(syn::Error::new(
                *span,
                "`language` only applies to `tokenizer = \"porter\"` or `tokenizer = \"snowball\"` (stemming)",
            )),
            (None, Some("snowball")) => {
                let (_, span) = spec.tokenizer.as_ref().expect("matched");
                spec.errors.push(syn::Error::new(
                    *span,
                    "`tokenizer = \"snowball\"` needs a `language`, e.g. `language = \"french\"`",
                ));
            }
            (None, _) => {}
        }
        if spec.vector_kind.as_ref().is_none_or(|(k, _)| k != "hnsw") {
            for (name, arg) in [("m", spec.m), ("ef_construction", spec.ef_construction), ("ef_search", spec.ef_search)]
            {
                if let Some((_, span)) = arg {
                    spec.errors.push(syn::Error::new(
                        span,
                        format!("`{name}` only applies to `kind = \"hnsw\"` (approximate vector indexes)"),
                    ));
                }
            }
        }
        Ok(spec)
    }

    /// The `::rxferrix::Tokenizer` expression of a search index.
    pub fn tokenizer_expr(&self) -> TokenStream {
        let base = match self.tokenizer.as_ref().map(|(t, _)| t.as_str()) {
            Some("porter") => {
                let language = self.language.as_ref().map_or("english", |(l, _)| l.as_str());
                quote!(::rxferrix::Tokenizer::porter(#language))
            }
            Some("trigram") => quote!(::rxferrix::Tokenizer::trigram()),
            Some("snowball") => {
                let language = self.language.as_ref().map_or("english", |(l, _)| l.as_str());
                quote!(::rxferrix::Tokenizer::snowball(#language))
            }
            _ => quote!(::rxferrix::Tokenizer::unicode61()),
        };
        match self.diacritics.as_ref().map(|(d, _)| d.as_str()) {
            Some("keep") => quote!(#base.keep_diacritics()),
            _ => base,
        }
    }

    /// The `::rxferrix::VectorIndexKind` expression of a vector index.
    pub fn vector_kind_expr(&self) -> TokenStream {
        match self.vector_kind.as_ref().map(|(k, _)| k.as_str()) {
            Some("hnsw") => quote!(::rxferrix::VectorIndexKind::Hnsw),
            _ => quote!(::rxferrix::VectorIndexKind::Exact),
        }
    }

    /// The `::rxferrix::HnswParams` expression of a vector index: the defaults with the given parameters.
    pub fn hnsw_params_expr(&self) -> TokenStream {
        let fields: Vec<TokenStream> =
            [("m", self.m), ("ef_construction", self.ef_construction), ("ef_search", self.ef_search)]
                .into_iter()
                .filter_map(|(name, arg)| {
                    let (n, _) = arg?;
                    let ident = syn::Ident::new(name, Span::call_site());
                    Some(quote!(#ident: #n))
                })
                .collect();
        match fields.len() {
            0 => quote!(::rxferrix::HnswParams::DEFAULT),
            3 => quote!(::rxferrix::HnswParams { #(#fields,)* }),
            _ => quote!(::rxferrix::HnswParams { #(#fields,)* ..::rxferrix::HnswParams::DEFAULT }),
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
    "bool", "char", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize", "f32",
    "f64",
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
        syn::Type::Array(_) | syn::Type::Tuple(_) | syn::Type::Reference(_) | syn::Type::Slice(_) => {
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
            Some(inner) if PRIMITIVES.contains(&inner.ident.to_string().as_str()) || inner.ident == "String" => {
                TypeShape::Other
            }
            _ => TypeShape::Unknown,
        },
        n if PRIMITIVES.contains(&n) => TypeShape::Other,
        "Option" | "HashMap" | "BTreeMap" | "HashSet" | "BTreeSet" | "Id" => TypeShape::Other,
        _ => TypeShape::Unknown,
    }
}
