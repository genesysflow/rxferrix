//! The Ferrix todo example backend.
//!
//! ```sh
//! cargo run -p todo-server -- serve --dev            # http://127.0.0.1:8090
//! cargo run -p todo-server -- functions              # list registered functions
//! ```

mod todos;

fn main() -> ferrix::Result<()> {
    ferrix::Ferrix::new().run()
}
