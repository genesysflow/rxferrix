#![allow(unused_imports, unused_variables)]
use ferrix::prelude::*;
use ferrix_macros::*;

use std::rc::Rc;

#[query]
pub async fn list(ctx: &QueryCtx) -> Result<u32> {
    let rc = Rc::new(1);
    let _ = ctx.now();
    async {}.await;
    Ok(*rc)
}

fn main() {}
