#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

#[table(nmae = "todos")]
pub struct A { pub x: u32 }

#[table(name = "")]
pub struct B { pub x: u32 }

#[table(name = todos)]
pub struct C { pub x: u32 }

fn main() {}
