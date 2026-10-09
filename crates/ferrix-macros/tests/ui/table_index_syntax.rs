#![allow(unused_imports, unused_variables)]
use ferrix::prelude::*;
use ferrix_macros::*;

#[table]
#[index(by_done(done))]
pub struct Todo { pub done: bool }

fn main() {}
