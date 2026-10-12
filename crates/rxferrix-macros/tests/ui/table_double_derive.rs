#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

#[table]
#[derive(serde::Serialize, Clone, PartialEq)]
pub struct Todo { pub done: bool }

fn main() {}
