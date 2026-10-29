#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

// Serde derives but no TS type: what `#[rxferrix::object]` would add.
#[derive(rxferrix::__private::serde::Serialize, rxferrix::__private::serde::Deserialize)]
#[serde(crate = "rxferrix::__private::serde")]
pub struct Filter {
    pub done: bool,
}

#[query]
async fn list(_ctx: &QueryCtx, filter: Filter) -> Result<Vec<String>> {
    Ok(vec![])
}

fn main() {}
