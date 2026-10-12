#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

#[query]
pub async fn list() -> Result<u32> { Ok(1) }

#[mutation]
pub async fn add(text: String) -> Result<u32> { Ok(1) }

fn main() {}
