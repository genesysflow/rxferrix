#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

#[table]
#[index(by_x = [x])]
#[aggregate(by_x, group = [x])]
#[aggregate(nope, group = [missing], sum = [_id])]
#[aggregate(twice, group = [x, x], sum = [n])]
#[aggregate(bad, kind = [x])]
pub struct Doc1 { pub x: String, pub n: i64 }

// A non-numeric sum field is caught by the type check the macro emits.
#[table]
#[aggregate(total, sum = [label])]
pub struct Doc2 { pub label: String }

fn main() {}
