#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

// Visibly wrong field types are reported by the macro itself.
#[table]
#[search_index(by_count, search = count)]
#[vector_index(by_wide, field = wide, dims = 3)]
pub struct Doc1 { pub count: u32, pub wide: Vec<f64> }

fn main() {}
