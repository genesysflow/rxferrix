# Command line

There are two executables: the project generator and the app you build. Run app commands through Cargo during development or directly through the compiled binary in production.

## Create an app

```sh
cargo install rxferrix-cli
rxferrix new my-app
rxferrix new my-svelte-app --web svelte
rxferrix new my-backend --web none
```

| Option | Meaning |
| --- | --- |
| `--name NAME` | Set the Cargo package and binary name instead of using the directory name. |
| `--web react\|svelte\|none` | Select a frontend; defaults to React. |
| `--published` | Use crates.io and npm even if the generator was built from a local checkout. |
| `--rxferrix-path DIR` | Use another framework checkout; requires its client packages to be built and Bun available for packing. |

The target directory must be absent or empty. Names start with an ASCII letter and contain letters, digits, `-`, or `_`. `--published` and `--rxferrix-path` are mutually exclusive.

## Run your app

From the directory containing your app's `Cargo.toml`:

```sh
cargo run -- serve --dev
cargo run -- functions
cargo run -- codegen --out web/src/rxferrix
cargo run -- --help
cargo run -- serve --help
```

For a compiled app, replace `cargo run --` with its path, such as `./target/release/my-app`.

| Command | Purpose | Can run beside a running server? |
| --- | --- | --- |
| `serve` | Open the data directory and serve the app. | Use `--takeover` to replace the active server. |
| `functions` | List registered functions and signatures. | Yes. |
| `codegen --out DIR` | Generate TypeScript references and types. | Yes. |
| `check --dir DIR` | Open a temporary copy with the new code and validate it. | Yes. |
| `backup --dir DIR` | Create a consistent archive. | Yes. |
| `export --dir DIR` | Export a consistent snapshot. | Yes. |
| `restore --dir DIR ARCHIVE` | Validate and restore an archive. | No. |
| `import --dir DIR PATH` | Import JSONL or CSV. | No. |
| `migrate --dir DIR` | Run pending migrations. | No; use the dashboard or `serve --migrate` while serving. |
| `migrate status --dir DIR` | Show migration progress offline. | No; use the dashboard while serving. |
| `env list`, `env set NAME [VALUE]`, `env unset NAME` | Manage app variables offline. | No; use the dashboard while serving. |
| `admin create EMAIL` | Create a dashboard administrator. | Use offline for initial setup. |
| `admin reset-password EMAIL` | Reset an administrator's password. | Use offline for recovery. |
| `admin disable-2fa EMAIL` | Remove a lost administrator second factor. | Use offline for recovery. |
| `health [URL]` | Exit successfully when the health endpoint responds. | Yes. |

Run a command's `--help` for its full options. Always pass the same `--dir` used by `serve`; otherwise commands use `fx_data` relative to their working directory.

## Common server options

| Option | Default or behavior |
| --- | --- |
| `--dir DIR` | `fx_data` |
| `--http ADDRESS` | `127.0.0.1:8090` |
| `--dev` | Detailed client errors and development-only functions. |
| `--codegen-out DIR` | With `--dev`, generate the API at startup; otherwise use `RXFERRIX_CODEGEN_OUT` if set. |
| `--public-url URL` | Public backend URL for links and OAuth callbacks. |
| `--auth-redirect-url URL` | Allowed frontend auth return URL; repeatable. |
| `--migrate` | Run pending migrations in the background after startup. |
| `--takeover[=require\|auto]` | Replace the server using the same data directory. |
| `--fsync-commits` | Wait for disk durability before acknowledging commits. |
| `--metrics` | Expose Prometheus metrics at `/metrics`. |
| `--backup-every 6h` | Enable scheduled backups. |
| `--replicate-to LOCATION` | Enable continuous replication. |
| `--olap` | Enable the analytics copy; also requires the `olap` Cargo feature. |

`codegen` defaults to `RXFERRIX_CODEGEN_OUT`, then `rxferrix-generated`. `serve --dev` generates only when `--codegen-out` or `RXFERRIX_CODEGEN_OUT` supplies a directory. Generated web projects set that variable in `.cargo/config.toml`.

Storage, TLS, proxy, timeout, cache, and retention options are documented in the relevant [guides](/guide/functions) and [operations pages](/operations/deploying). `serve --help` is the complete flag list for the version you built.
