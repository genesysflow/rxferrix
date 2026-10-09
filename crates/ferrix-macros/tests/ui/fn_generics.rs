#![allow(unused_imports, unused_variables)]
use ferrix::prelude::*;
use ferrix_macros::*;

#[query]
pub async fn list<T: Default>(ctx: &QueryCtx) -> Result<u32> { Ok(1) }

#[query]
pub async fn list2<'a>(ctx: &'a QueryCtx) -> Result<u32> { Ok(1) }

fn main() {}
