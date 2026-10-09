#![allow(unused_imports, unused_variables)]
use ferrix::prelude::*;
use ferrix_macros::*;

#[object]
#[derive(serde::Serialize, Clone, PartialEq)]
pub struct View { pub done: bool }

#[object]
#[ts(crate = "ts_rs")]
pub enum Status { Open }

#[object(camel)]
pub struct Args { pub a: u32 }

#[object]
pub fn not_a_type() {}

fn main() {}
