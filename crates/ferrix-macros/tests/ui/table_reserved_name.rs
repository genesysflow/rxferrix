#![allow(unused_imports, unused_variables)]
use ferrix::prelude::*;
use ferrix_macros::*;

#[table(name = "_users")]
pub struct Users {
    pub email: String,
}

fn main() {}
