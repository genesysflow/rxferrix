//! Compile-fail tests for every documented error case (CONTRACT.md §3), against the mock runtime.
//! Regenerate the expected `.stderr` files with `TRYBUILD=overwrite`.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
}
