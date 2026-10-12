#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

#[table]
#[vector_index(by_embedding, field = embedding)]
#[vector_index(by_embedding_2, field = embedding, dims = 0)]
pub struct Message { pub embedding: Vec<f32> }

fn main() {}
