#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

#[query]
pub async fn list(ctx: &MutationCtx) -> Result<u32> { Ok(1) }

#[mutation]
pub async fn add(ctx: &rxferrix::ActionCtx) -> Result<u32> { Ok(1) }

#[action]
pub async fn act(ctx: ActionCtx) -> Result<u32> { Ok(1) }

#[internal_action]
pub async fn act2(ctx: &mut ActionCtx) -> Result<u32> { Ok(1) }

fn main() {}
