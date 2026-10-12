#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

#[table]
#[index(by_ct = [_creation_time])]
pub struct Todo { pub _id: String, #[serde(rename = "_x")] pub x: u32 }

fn main() {}
