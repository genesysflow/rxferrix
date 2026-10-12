#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

#[table]
#[index(by_done = [done, created_byy])]
pub struct Todo { pub done: bool, pub created_by: Option<String> }

fn main() {}
