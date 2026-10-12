#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

#[query]
pub fn list(ctx: &QueryCtx) -> Result<u32> { Ok(1) }

fn main() {}
