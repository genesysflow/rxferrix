#![allow(unused_imports, unused_variables)]
use rxferrix::prelude::*;
use rxferrix_macros::*;

#[table]
#[index(by_channel = [channel])]
#[search_index(by_channel, search = body)]
#[vector_index(body, field = embedding, dims = 3)]
#[vector_index(by_creation_time, field = embedding, dims = 3)]
pub struct Message { pub channel: String, pub body: String, pub embedding: Vec<f32> }

fn main() {}
