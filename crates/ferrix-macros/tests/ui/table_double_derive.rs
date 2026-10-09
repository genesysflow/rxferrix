#![allow(unused_imports, unused_variables)]
use ferrix::prelude::*;
use ferrix_macros::*;

#[table]
#[derive(serde::Serialize, Clone, PartialEq)]
pub struct Todo { pub done: bool }

fn main() {}
