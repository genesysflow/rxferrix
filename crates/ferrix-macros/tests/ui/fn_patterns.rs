#![allow(unused_imports, unused_variables)]
use ferrix::prelude::*;
use ferrix_macros::*;

#[query]
pub async fn list(ctx: &QueryCtx, (a, b): (u32, u32), _: u32, ref c: u32) -> Result<u32> { Ok(a + b + *c) }

fn main() {}
