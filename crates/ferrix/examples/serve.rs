//! The smallest Ferrix app: no tables or functions, just the server and CLI.
//! `cargo run -p ferrix --example serve -- --dev` (or `-- functions`).

fn main() -> ferrix::Result<()> {
    ferrix::Ferrix::new().run()
}
