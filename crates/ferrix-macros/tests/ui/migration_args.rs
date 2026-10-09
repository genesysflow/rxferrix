#![allow(unused_imports, unused_variables)]
use ferrix::prelude::*;
use ferrix_macros::*;
use serde_json::Value;

#[migration]
async fn no_table(ctx: &MutationCtx, doc: Value) -> Result<()> { Ok(()) }

#[migration(table = "todos", batch = 0)]
async fn zero_batch(ctx: &MutationCtx, doc: Value) -> Result<()> { Ok(()) }

#[migration(table = "todos", batch = 20000, name = "not ok!")]
async fn bad_values(ctx: &MutationCtx, doc: Value) -> Result<()> { Ok(()) }

#[migration(table = "todos", every = 5)]
async fn unknown_arg(ctx: &MutationCtx, doc: Value) -> Result<()> { Ok(()) }

#[migration(table = 5)]
async fn bad_table(ctx: &MutationCtx, doc: Value) -> Result<()> { Ok(()) }

fn main() {}
