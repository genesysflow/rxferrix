#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;
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

#[migration(table = "todos", after = "0001_first")]
async fn after_not_a_list(ctx: &MutationCtx, doc: Value) -> Result<()> { Ok(()) }

#[migration(table = "todos", name = "0002_second", after = ["0001_first", "0001_first", "no spaces", first, "0002_second"])]
async fn bad_after(ctx: &MutationCtx, doc: Value) -> Result<()> { Ok(()) }

fn main() {}
