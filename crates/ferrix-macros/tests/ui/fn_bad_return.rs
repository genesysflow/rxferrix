#![allow(unused_imports, unused_variables)]
use ferrix::prelude::*;
use ferrix_macros::*;

#[query]
pub async fn list(ctx: &QueryCtx) -> Option<u32> { Some(1) }

#[query]
pub async fn list2(ctx: &QueryCtx) -> u32 { 1 }

fn main() {}
