#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

pub struct Filter {
    pub done: bool,
}

#[query]
async fn list(_ctx: &QueryCtx, filter: Filter) -> Result<Vec<String>> {
    Ok(vec![])
}

fn main() {}
