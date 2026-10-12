#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

type Embedding = Vec<f64>;
type Text = u32;

// Types hidden behind aliases are caught by the type check the macro emits.
#[table]
#[search_index(by_text, search = text)]
#[vector_index(by_embedding, field = embedding, dims = 3)]
pub struct Doc1 { pub text: Text, pub embedding: Embedding }

fn main() {}
