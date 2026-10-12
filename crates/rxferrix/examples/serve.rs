//! The smallest RxFerrix app: no tables or functions, just the server and CLI.
//! `cargo run -p rxferrix --example serve -- --dev` (or `-- functions`).

fn main() -> rxferrix::Result<()> {
    rxferrix::RxFerrix::new().run()
}
