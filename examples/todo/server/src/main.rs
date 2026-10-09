//! The Ferrix todo example backend.
//!
//! ```sh
//! cargo run -p todo-server -- serve --dev            # http://127.0.0.1:8090
//! cargo run -p todo-server -- functions              # list registered functions
//! ```
//!
//! When `examples/todo/web/dist` exists at build time (`cd examples/todo/web && bun run build`), the web app is
//! embedded in the binary and served at `/`, so one file is the whole app (see `scripts/release.sh`). Cargo only
//! notices a `dist/` created after the last build of this crate once one of its files changes: run
//! `touch examples/todo/server/src/main.rs` after building the web app for the first time.

mod todos;

/// The built web app (empty when `web/dist` doesn't exist).
#[derive(ferrix::rust_embed::Embed)]
#[folder = "../web/dist/"]
#[crate_path = "ferrix::rust_embed"]
#[allow_missing = true]
struct Web;

fn main() -> ferrix::Result<()> {
    let mut app = ferrix::Ferrix::new()
        // Every hour, on the hour (UTC): delete todos completed more than a day ago.
        .cron::<todos::purge_completed>("purge-completed", "0 0 * * * *", ());
    if Web::get("index.html").is_some() {
        app = app.embed_static::<Web>();
    }
    app.run()
}
