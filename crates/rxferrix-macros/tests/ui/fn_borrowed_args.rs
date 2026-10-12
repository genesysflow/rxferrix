#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

#[query]
pub async fn list(ctx: &QueryCtx, name: &str) -> Result<u32> { Ok(1) }

#[query]
pub async fn list2(ctx: &QueryCtx, name: std::borrow::Cow<'_, str>) -> Result<&'static str> { Ok("") }

fn main() {}
