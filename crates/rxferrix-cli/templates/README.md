# {{name}}

A RxFerrix app: the backend is the Rust crate in `src/` (tables, queries, mutations and actions),
built into a single binary that also serves the dashboard{{web_blurb}}.

## Develop

```sh
cargo run -- serve --dev     # http://127.0.0.1:8090; the dashboard is at http://127.0.0.1:8090/_/
```
{{web_steps}}
`serve --dev` prints a one-time link that signs you in to the dashboard as an admin. Data lives in `fx_data/`.

## Build for production

```sh
{{build_steps}}cargo build --release         # one binary: target/release/{{name}}
./target/release/{{name}} serve --dir /var/lib/{{name}}
```

Keep `.cargo/config.toml`: it builds SQLite so that queries run in parallel (RxFerrix logs a warning at startup when it
is missing).
