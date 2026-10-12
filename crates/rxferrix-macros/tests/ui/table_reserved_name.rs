#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

#[table(name = "_users")]
pub struct Users {
    pub email: String,
}

fn main() {}
