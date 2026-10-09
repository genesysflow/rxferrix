#![allow(unused_imports, unused_variables)]
use ferrix::prelude::*;
use ferrix_macros::*;

#[table]
#[search_index(by_stem, search = body, tokenizer = "snowball")]
#[search_index(by_klingon, search = body, tokenizer = "snowball", language = "klingon")]
#[search_index(by_lang, search = body, language = "english")]
#[search_index(by_french, search = body, tokenizer = "porter", language = "french")]
#[search_index(by_accents, search = body, tokenizer = "trigram", diacritics = "strip")]
#[vector_index(by_embedding, field = embedding, dims = 3, kind = "ivf")]
#[vector_index(by_exact, field = embedding, dims = 3, ef_search = 64)]
#[vector_index(by_hnsw, field = embedding, dims = 3, kind = "hnsw", m = 1000, ef_construction = 8)]
pub struct Message { pub body: String, pub embedding: Vec<f32> }

fn main() {}
