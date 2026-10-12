//! The {{name}} backend.
//!
//! ```sh
//! cargo run -- serve --dev     # http://127.0.0.1:8090, dashboard at /_/; writes web/src/rxferrix
//! cargo run -- functions       # list the registered functions
//! cargo run -- --help
//! ```
//!
//! When `web/dist` exists at build time (`cd web && bun run build`), the web app is embedded in the binary and served
//! at `/`, so one file is the whole app. Cargo only notices a `dist/` created after the last build once a file of this
//! crate changes: run `touch src/main.rs` after building the web app for the first time.

mod tasks;

/// The built web app (empty when `web/dist` doesn't exist).
#[derive(rxferrix::rust_embed::Embed)]
#[folder = "web/dist/"]
#[crate_path = "rxferrix::rust_embed"]
#[allow_missing = true]
struct Web;

fn main() -> rxferrix::Result<()> {
    let mut app = rxferrix::RxFerrix::new();
    if Web::get("index.html").is_some() {
        app = app.embed_static::<Web>();
    }
    app.run()
}
