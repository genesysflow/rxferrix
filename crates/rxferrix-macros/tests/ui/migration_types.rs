#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;
use serde_json::Value;

// The result must be a patch (`Option<Value>`), a `DocChange` or `()`.
#[migration(table = "todos")]
async fn bad_output(ctx: &MutationCtx, doc: Value) -> Result<u32> { Ok(1) }

// The document parameter must deserialize.
pub struct NotDeserialize;

#[migration(table = "todos")]
async fn bad_doc(ctx: &MutationCtx, doc: NotDeserialize) -> Result<()> { Ok(()) }

// `table = T` needs a table type.
#[migration(table = String)]
async fn not_a_table(ctx: &MutationCtx, doc: Value) -> Result<()> { Ok(()) }

fn main() {}
