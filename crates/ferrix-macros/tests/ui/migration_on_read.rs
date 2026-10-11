#![allow(unused_imports, unused_variables)]
use ferrix::prelude::*;
use ferrix_macros::*;
use serde_json::Value;

#[migration(table = "todos", on_read)]
async fn async_on_read(doc: &Value) -> Result<()> { Ok(()) }

#[migration(table = "todos", on_read)]
fn with_ctx(ctx: &MutationCtx, doc: &Value) -> Result<()> { Ok(()) }

#[migration(table = "todos", on_read)]
fn by_value(doc: Value) -> Result<()> { Ok(()) }

#[migration(table = "todos", on_read, on_read)]
fn twice(doc: &Value) -> Result<()> { Ok(()) }

#[migration(table)]
fn bare_table(doc: &Value) -> Result<()> { Ok(()) }

fn main() {}
