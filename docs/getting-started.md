# Get started

## Create an app

You need [Rust](https://rustup.rs) and [bun](https://bun.sh).

```sh
cargo install rxferrix-cli     # install the `rxferrix` command
rxferrix new my-app            # --web react (default), svelte or none
```

Then run it, the backend and the web app in two terminals:

```sh
cd my-app && cargo run -- serve --dev         # backend on http://127.0.0.1:8090
cd my-app/web && bun install && bun run dev   # web app
```

The app depends on the published [`rxferrix`](https://crates.io/crates/rxferrix) crate and the
[`@rxferrix`](https://www.npmjs.com/org/rxferrix) packages on npm.

To work on RxFerrix itself, build the TypeScript packages in a checkout (`bun install && bun run build`) and install
the command from it (`cargo install --path crates/rxferrix-cli`): apps it creates then use the checkout's crate and
packages (`--published` uses crates.io and npm instead, `--rxferrix-path DIR` another checkout).

## What's in the app

| Path | What |
|---|---|
| `src/main.rs` | Starts the server: `rxferrix::RxFerrix::new().run()`, plus auth, crons and other options. |
| `src/tasks.rs` | A sample `tasks` table with a query and mutations. Add your own modules next to it. |
| `web/` | The web app (React or Svelte). |
| `web/src/rxferrix` | The generated TypeScript API. `serve --dev` rewrites it each time it starts, so restart it after you change your functions. |
| `.cargo/config.toml` | Build flags that let SQLite run queries in parallel (see [Deploying](/operations/deploying)), and where `serve --dev` writes the TypeScript API. |

## Functions

Everything the backend does is a function, and there are three kinds:

| Kind | Use it to | Guarantees |
|---|---|---|
| `#[query]` | read data | Reactive and cached: reruns only when something it read changes. |
| `#[mutation]` | write data | One transaction; runs exactly once, even when a client retries after a reconnect. |
| `#[action]` | call the outside world (APIs, email) | Not transactional; can call queries and mutations. |

```rust
#[query]
pub async fn list(ctx: &QueryCtx, done: Option<bool>) -> Result<Vec<Doc<Task>>> { /* read with ctx.db */ }

#[mutation]
pub async fn add(ctx: &MutationCtx, text: String) -> Result<Id<Task>> { /* write with ctx.db */ }

#[action]
pub async fn notify(ctx: &ActionCtx, id: Id<Task>) -> Result<()> { /* fetch, send email, ctx.run_mutation */ }
```

On the client, `useQuery(api.tasks.list, { done: false })` subscribes to a query, `useMutation(api.tasks.add)` and
`useAction(api.tasks.notify)` return functions to call. The [functions guide](/guide/functions) covers tables,
indexes, pagination and types.

## The dashboard

Every server has an admin dashboard at http://127.0.0.1:8090/_/. On the first start, the server prints a one-time
setup link to create the first administrator. From there you can browse and edit data, run functions, follow the
logs, and manage users, files, backups and settings. See [the dashboard](/guide/dashboard).

## Examples

The repository has two complete apps:

- **`examples/todo`**: a todo list with a Svelte frontend, file uploads, search and a migration.
- **`examples/chat-react`**: a Slack-like chat in React with auth, pagination, uploads, search, scheduled messages,
  live aggregates and analytics.

Run the todo example from the repository root:

```sh
bun install && bun run build
cargo run -p todo-server -- serve --dev --codegen-out examples/todo/web/src/lib/rxferrix    # backend on :8090
cd examples/todo/web && bun run dev                                                       # app on :5173
```

Open http://localhost:5173 in two tabs and watch them stay in sync.

## Next steps

- Add sign-in with [Auth](/guide/auth).
- Store uploads with [File storage](/guide/file-storage).
- Run things later or on a schedule with the [Scheduler](/guide/scheduler).
- Read [Things to know](/guide/things-to-know) before you build something big.
- When you're ready, [deploy it](/operations/deploying).
