#![allow(unused_imports, unused_variables)]
use ferrix::prelude::*;
use ferrix_macros::*;

#[table]
#[search_index(search_body, search = bdy, filter = [channel, chanel])]
pub struct Message { pub channel: String, pub body: String }

fn main() {}
