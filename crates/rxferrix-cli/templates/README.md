# {{name}}

A Rust backend built with RxFerrix. The app binary serves the API, embedded database, admin dashboard{{web_blurb}}.

## Run the backend

From this directory:

```sh
cargo run -- serve --dev
```

The backend listens on http://127.0.0.1:8090 and the dashboard is at http://127.0.0.1:8090/_/. Use the setup link printed on the first start to create an administrator. Local data lives in `fx_data/`.
{{web_steps}}
## Make a backend change

Tables and functions start in `src/tasks.rs`. Add new modules with `mod module_name;` in `src/main.rs`. Stop and rerun the backend after editing Rust code; development mode does not watch source files.

```sh
cargo run -- functions
cargo run -- --help
```

`rxferrix` creates projects. Commands such as `serve`, `functions`, `codegen`, and `backup` belong to this app's binary.

## Build a release

From this directory, after stopping the development backend:

```sh
{{build_steps}}touch src/main.rs             # ensure rebuilt frontend assets are embedded, when present
cargo build --release
./target/release/{{name}} serve --dir /var/lib/{{name}}
```

Keep `.cargo/config.toml`: it configures SQLite for parallel reads. The data directory must persist across releases. Configure public URLs, TLS, sign-in permissions, and backups for production.

The starter task functions are public. See the [documentation](https://genesysflow.github.io/rxferrix/) for authentication, authorization, deployment, and recovery.
