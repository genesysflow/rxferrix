//! The {{name}} backend.
//!
//! ```sh
//! cargo run -- serve --dev     # http://127.0.0.1:8090, dashboard at /_/
//! cargo run -- functions       # list the registered functions
//! cargo run -- --help
//! ```

mod tasks;

fn main() -> rxferrix::Result<()> {
    rxferrix::RxFerrix::new().run()
}
