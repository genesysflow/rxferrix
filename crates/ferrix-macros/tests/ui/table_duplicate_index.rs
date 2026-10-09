#![allow(unused_imports, unused_variables)]
use ferrix::prelude::*;
use ferrix_macros::*;

#[table]
#[index(by_done = [done])]
#[index(by_done = [created_by])]
pub struct Todo { pub done: bool, pub created_by: Option<String> }

fn main() {}
