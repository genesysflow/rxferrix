#![allow(unused_imports, unused_variables)]
use ferrix::prelude::*;
use ferrix_macros::*;

#[table]
#[index(by_ct = [_creation_time])]
pub struct Todo { pub _id: String, #[serde(rename = "_x")] pub x: u32 }

fn main() {}
