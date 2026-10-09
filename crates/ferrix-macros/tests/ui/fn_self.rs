#![allow(unused_imports, unused_variables)]
use ferrix::prelude::*;
use ferrix_macros::*;

pub struct S;

impl S {
    #[query]
    pub async fn list(&self, ctx: &QueryCtx) -> Result<u32> { Ok(1) }
}

fn main() {}
