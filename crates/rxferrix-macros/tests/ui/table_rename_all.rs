#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

#[table]
#[serde(rename_all = "snake_case")]
pub struct Todo { pub created_by: String }

fn main() {}
