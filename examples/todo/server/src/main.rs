//! The Ferrix todo example backend.
//!
//! ```sh
//! cargo run -p todo-server -- serve --dev            # http://127.0.0.1:8090
//! cargo run -p todo-server -- functions              # list registered functions
//! ```

mod todos;

fn main() -> ferrix::Result<()> {
    ferrix::Ferrix::new()
        // Every hour, on the hour (UTC): delete todos completed more than a day ago.
        .cron::<todos::purge_completed>("purge-completed", "0 0 * * * *", ())
        .run()
}
