#![allow(unused_imports, unused_variables)]
// The hint the function macros emit for each argument and output type (the expansion snapshots show the calls): here
// on its own, since a whole #[query] also brings serde's and ts-rs's errors, whose text depends on their features.
use rxferrix::prelude::*;

// Serde derives but no TS type: what `#[rxferrix::object]` would add.
#[derive(rxferrix::__private::serde::Serialize, rxferrix::__private::serde::Deserialize)]
#[serde(crate = "rxferrix::__private::serde")]
pub struct Filter {
    pub done: bool,
}

const _: () = rxferrix::__private::assert_wire_arg::<Filter>();
const _: () = rxferrix::__private::assert_wire_output::<Filter>();

fn main() {}
