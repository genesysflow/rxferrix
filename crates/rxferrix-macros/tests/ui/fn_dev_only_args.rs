#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

#[action(dev_only = true)]
pub async fn a(ctx: &ActionCtx) -> Result<u32> { Ok(1) }

#[mutation(dev_only, dev_only)]
pub async fn b(ctx: &MutationCtx) -> Result<u32> { Ok(1) }

fn main() {}
