# Get started

This walkthrough creates a task app, runs its backend and frontend, and shows the edit/restart loop you will use while developing.

## 1. Install the tools

You need a current stable [Rust toolchain](https://rustup.rs/) with support for edition 2024 and [Bun](https://bun.sh/). Rust builds the backend; Bun installs and runs the web app.

```sh
cargo install rxferrix-cli
```

This installs `rxferrix`, the project generator and development runner. Server commands come from the app you create next.

## 2. Create a project

```sh
rxferrix new my-app
cd my-app
```

The default frontend is React. Choose `--web svelte` for Svelte 5 or `--web none` for a backend without a frontend.

| Path | Purpose |
| --- | --- |
| `src/main.rs` | Declares backend modules and configures `RxFerrix`. |
| `src/tasks.rs` | The `Task` table and its list, add, update, and delete functions. |
| `web/src/` | The frontend and client setup. |
| `web/src/rxferrix/` | Generated function references and data types. |
| `.cargo/config.toml` | SQLite build flags and, for a web app, the codegen output directory. |
| `fx_data/` | Local database and server state, created when the backend starts. |

Projects normally depend on crates.io and npm packages. A generator installed from a framework checkout uses that checkout when it still exists; pass `--published` to use published packages explicitly. See [framework development](https://github.com/genesysflow/rxferrix/blob/master/CONTRIBUTING.md) for checkout setup.

## 3. Start the backend

From `my-app`:

```sh
rxferrix dev
```

Leave it running. The first build can take a while. The server listens on http://127.0.0.1:8090 and writes the TypeScript API to `web/src/rxferrix`.

`rxferrix dev` runs `cargo run -- serve --dev` and rebuilds the app when its Rust code changes. You can run `cargo run -- serve --dev` yourself instead; it doesn't watch files. `--dev` exposes detailed errors, enables development-only functions, and logs auth email codes when SMTP is not configured. Use it for local development.

## 4. Start the frontend

In a second terminal, from `my-app`:

```sh
cd web
bun install
bun run dev
```

Open http://localhost:5173 in two tabs. Add, complete, or remove a task. Both tabs should update without a refresh.

The frontend connects to its own origin. Vite forwards `/api` requests, including the `/api/sync` WebSocket, to the backend. This also lets uploads and auth requests use the same origin.

## 5. Make a backend change

Open `src/tasks.rs`. A `#[query]` reads data, and a `#[mutation]` changes it. The generated frontend calls them as `api.tasks.list`, `api.tasks.add`, and so on.

For example, change the maximum list size from `.take(100)` to `.take(50)` and save. `rxferrix dev` rebuilds the app and restarts the server, and startup regenerates the client API. Existing clients reconnect. The old server keeps serving while the new version compiles. If the build fails, it keeps serving until you fix the error.

To generate types without starting the server:

```sh
cargo run -- codegen --out web/src/rxferrix
```

Do not edit `api.ts`, `dataModel.ts`, or `index.ts` by hand. Use [the types guide](/guide/types) to understand their naming and serialization.

## 6. Open the dashboard

The dashboard is at http://127.0.0.1:8090/_/. On first start, use the setup link printed in the backend terminal to create an administrator. **Data** shows the tasks; **Functions** lets you run a function with JSON arguments; **Logs** shows executions.

Dashboard administrators are separate from your app's signed-in users. The starter task functions are public; add [authentication and authorization](/guide/auth) when the app needs private data.

## Build and run a release

For a generated app with a frontend, run these from `my-app` after generating the API:

```sh
cd web
bun run build
cd ..
touch src/main.rs
cargo build --release
./target/release/my-app serve --dir ./production-data
```

The generated backend embeds `web/dist` when it exists at compile time. Touching `main.rs` makes Cargo rebuild the app with the new assets. For `--web none`, skip the frontend build and `touch`.

See [Deploying](/operations/deploying) for persistent storage, public URLs, TLS, and updates.

## Continue building

Read [Core concepts](/guide/concepts), then [Functions and data](/guide/functions) and [Clients](/guide/clients). The [todo](https://github.com/genesysflow/rxferrix/tree/master/examples/todo) and [chat](https://github.com/genesysflow/rxferrix/tree/master/examples/chat-react) examples show complete apps you can run and inspect.

If a command fails, first check its working directory. Run backend commands beside the app's `Cargo.toml`, frontend commands inside `web/`, and framework workspace commands from the framework repository root. The [command reference](/reference/cli) lists common commands.
