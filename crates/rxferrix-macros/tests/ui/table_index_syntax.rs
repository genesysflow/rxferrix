#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

#[table]
#[index(by_done(done))]
pub struct Todo { pub done: bool }

fn main() {}
