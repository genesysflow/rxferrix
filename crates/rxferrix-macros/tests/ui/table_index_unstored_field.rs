#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

#[table]
#[index(by_cache = [cache])]
#[index(by_extra = [extra])]
pub struct Todo {
    pub done: bool,
    #[serde(skip)]
    pub cache: u32,
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, String>,
}

fn main() {}
