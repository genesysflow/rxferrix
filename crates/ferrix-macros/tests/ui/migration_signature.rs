#![allow(unused_imports, unused_variables)]
use ferrix::prelude::*;
use ferrix_macros::*;
use serde_json::Value;

#[migration(table = "todos")]
async fn wrong_ctx(ctx: &QueryCtx, doc: Value) -> Result<()> { Ok(()) }

#[migration(table = "todos")]
fn not_async(ctx: &MutationCtx, doc: Value) -> Result<()> { Ok(()) }

#[migration(table = "todos")]
async fn no_doc(ctx: &MutationCtx) -> Result<()> { Ok(()) }

#[migration(table = "todos")]
async fn extra(ctx: &MutationCtx, doc: Value, more: u32) -> Result<()> { Ok(()) }

#[migration(table = "todos")]
async fn borrowed(ctx: &MutationCtx, doc: &Value) -> Result<()> { Ok(()) }

#[migration(table = "todos")]
async fn no_return(ctx: &MutationCtx, doc: Value) {}

#[migration(table = "todos")]
struct NotAFunction;

fn main() {}
