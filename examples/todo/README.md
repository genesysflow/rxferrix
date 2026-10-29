# Todo example

A task list with a Rust backend and Svelte 5 frontend. It demonstrates live queries, mutations, text search, image uploads, delayed jobs, a cleanup cron, and a data migration.

The example's task data is public. Use the [auth guide](../../docs/guide/auth.md) when adapting it for private user data.

## Run locally

Install Rust and Bun. From the framework repository root:

```sh
bun install
bun run build
cargo run -p todo-server -- serve --dev \
  --codegen-out examples/todo/web/src/lib/rxferrix
```

In a second terminal, from the repository root:

```sh
cd examples/todo/web
bun run dev
```

Open http://localhost:5173 in two tabs. Changes in one should appear in the other. The backend and dashboard are at http://127.0.0.1:8090 and http://127.0.0.1:8090/_/. Use the setup link printed by the backend to create an administrator.

Restart the backend after changing Rust code. The command above regenerates the frontend API at startup. Data lives in the root's `fx_data/` unless `--dir` is supplied.

## Find the implementation

| File | What it demonstrates |
| --- | --- |
| `server/src/todos.rs` | Tables, indexes, live reads, writes, uploads, search, scheduled reminders, and an analytics action. |
| `server/src/migrations.rs` | A repeatable text-trimming migration. |
| `server/src/main.rs` | Cron registration and optional frontend embedding. |
| `web/src/TodoList.svelte` | Queries, mutation handlers, and search UI. |
| `web/src/TodoImage.svelte` | File uploads and live image URLs. |
| `web/src/lib/rxferrix/` | Generated API and data types. |

## Enable analytics

```sh
cargo run -p todo-server --features olap -- serve --dev --olap \
  --codegen-out examples/todo/web/src/lib/rxferrix
```

`todos:stats_by_day` uses the DuckDB copy and requires both the build feature and runtime flag. It is an action, so its result is not a live query.

## Build with the frontend

From the repository root after generating the API:

```sh
cd examples/todo/web
bun run build
cd ../../..
touch examples/todo/server/src/main.rs
cargo build -p todo-server --release
./target/release/todo-server serve --dir ./todo-data
```

The backend embeds `web/dist` at compile time when it exists. The `touch` ensures the new assets are included. The repository's `scripts/release.sh` automates the frontend build and creates a static musl binary.

For framework tests and browser test commands, see [Contributing](../../CONTRIBUTING.md). For app development, start with [Get started](../../docs/getting-started.md).
