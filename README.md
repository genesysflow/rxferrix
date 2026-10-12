# Ferrix

A reactive backend in Rust that ships as a single binary.

You write your backend as typed Rust functions. Clients subscribe to queries over a WebSocket, and their results
update by themselves whenever a write changes the data they read. The database, auth, file storage, scheduler,
search and an admin dashboard are built in, and the whole app, frontend included, deploys as one file plus one data
directory.

- **Live by default.** `useQuery` results stay current. There is no cache to invalidate and no refetching to write.
- **Typed end to end.** Rust structs are the schema, and a TypeScript API is generated from your functions.
- **One file to run.** SQLite is embedded. Copy the binary to a server, or use the Docker image, and start it.
- **Safe to change.** Deploys and schema changes happen without downtime; a version that can't read the existing
  data refuses to take over, and the old one keeps serving.

**Documentation: https://genesysflow.github.io/ferrix/**

All planned phases (0–10) are built; [PLAN.md](PLAN.md) has the design and, in §10, what each part still lacks.

## Quick start

You need Rust (via [mise](https://mise.jdx.dev), see `mise.toml`) and [bun](https://bun.sh).

```sh
bun install && bun run build               # build the TypeScript packages
cargo install --path crates/ferrix-cli     # the `ferrix` command
ferrix new my-app                          # --web react (default), svelte or none
cd my-app && cargo run -- serve --dev      # backend on http://127.0.0.1:8090
cd my-app/web && bun install && bun run dev   # web app, in a second terminal
```

The new app is a Rust crate with a sample `tasks` table and functions in `src/tasks.rs`, and a web app in `web/`.
While `serve --dev` runs, the TypeScript API in `web/src/ferrix` is regenerated whenever the functions change. The
admin dashboard is at http://127.0.0.1:8090/_/: open the setup link the server prints on first start to create an
administrator.

A `ferrix` command installed from this checkout makes apps that use the checkout's crate and @ferrix packages;
`--published` uses crates.io and npm instead, and `--ferrix-path DIR` another checkout.

## What the code looks like

The backend: a table and the functions that use it.

```rust
use ferrix::prelude::*;

#[table(name = "todos")]
#[index(by_done = [done])]
pub struct Todo { pub text: String, pub done: bool }

#[query]
pub async fn list(ctx: &QueryCtx) -> Result<Vec<Doc<Todo>>> {
    ctx.db.query::<Todo>().order(Order::Desc).take(100).await
}

#[mutation]
pub async fn add(ctx: &MutationCtx, text: String) -> Result<Id<Todo>> {
    ctx.db.insert(Todo { text, done: false }).await
}

fn main() -> ferrix::Result<()> { ferrix::Ferrix::new().run() }
```

The frontend (React; Svelte works the same way):

```tsx
import { useMutation, useQuery } from "@ferrix/react";
import { api } from "./ferrix";                     // generated from the functions above

function Todos() {
  const todos = useQuery(api.todos.list);          // undefined while loading, then always up to date
  const add = useMutation(api.todos.add);
  return (
    <>
      <button onClick={() => add({ text: "Buy milk" })}>Add</button>
      <ul>{todos?.map((t) => <li key={t._id}>{t.text}</li>)}</ul>
    </>
  );
}
```

When anyone adds a todo, every open `list` updates. There are three kinds of function:

| Kind | Use it to | Guarantees |
|---|---|---|
| `#[query]` | read data | Reactive and cached: reruns only when something it read changes. |
| `#[mutation]` | write data | One transaction; runs exactly once, even when a client retries after a reconnect. |
| `#[action]` | call the outside world (APIs, email) | Not transactional; can call queries and mutations. |

## Features

Each one is explained, with examples, in the [guide](https://genesysflow.github.io/ferrix/guide/functions).

| Feature | In short |
|---|---|
| [Tables and indexes](docs/guide/functions.md) | Rust structs as tables, typed index queries, pagination, `#[object]` types for results. |
| [Auth](docs/guide/auth.md) | Passwords, magic links, OAuth (GitHub, Google, Discord, Apple, OIDC), or JWTs from Clerk, Auth0, Supabase. |
| [File storage](docs/guide/file-storage.md) | Uploads to disk or S3, thumbnails, signed and presigned URLs. |
| [Scheduler and crons](docs/guide/scheduler.md) | Run functions later or on a schedule (time zones included), with workpools and retries. |
| [Workflows](docs/guide/workflows.md) | Durable multi-step functions that survive restarts, sleep for days and wait for events. |
| [Search](docs/guide/search.md) | Full-text search (stemming in 18 languages) and vector search, both reactive. |
| [Live aggregates](docs/guide/aggregates.md) | Counts and sums per group, kept current on every write, read without a scan. |
| [Environment variables](docs/guide/environment.md) | Set on the dashboard, encrypted; queries rerun when one changes. |
| [Analytics](docs/guide/analytics.md) | SQL over a columnar copy of the data (DuckDB), for reports and dashboards. |
| [Migrations](docs/guide/migrations.md) | Change a table's type and migrate documents in the background, without downtime. |
| [Admin dashboard](docs/guide/dashboard.md) | Data, functions, logs, users, files, scheduler, migrations, backups, analytics, settings, audit log. |

Before you build something real, read [Things to know](docs/guide/things-to-know.md): limits, caching, and what
makes a query rerun.

## Deploying

A Ferrix app is one binary and one data directory (`--dir`, default `fx_data`). The details are in
[the operations docs](https://genesysflow.github.io/ferrix/operations/deploying).

```sh
scripts/release.sh                                  # a static Linux binary of the todo example in dist/ (~20 MB)
dist/todo-server serve --dir /var/lib/my-app        # listens on 127.0.0.1:8090 (--http to change)
```

- **Where to run it:** directly ([`deploy/ferrix.service`](deploy/ferrix.service) for systemd), in Docker
  ([`Dockerfile`](Dockerfile), [`deploy/docker`](deploy/docker)) or on Kubernetes ([`deploy/k8s`](deploy/k8s), written
  for k3s). Put it behind a reverse proxy, or let it get Let's Encrypt certificates itself with `--https`.
- **Deploys without downtime:** `serve --takeover` starts the new version next to the running one. It prepares a copy
  of the data with the new code (indexes, schema check) while the old one keeps serving, then switches over with a
  pause of well under a second and without losing a write. If the new version can't read the data, it exits and the
  old one keeps serving. Under systemd this is `systemctl reload`; with Docker,
  `deploy/docker/deploy.sh`; on Kubernetes, `kubectl set image`.
- **Backups:** scheduled backups to disk or S3 (set up on the dashboard or with `--backup-every`), continuous
  replication with point-in-time restore (`--replicate-to`), and restores from the dashboard.
- **Observability:** JSON logs, Prometheus metrics (`--metrics`), and the dashboard's logs and charts.

## Examples

- [`examples/todo`](examples/todo): the todo app from above with a Svelte frontend, file uploads, search and a
  migration.
- [`examples/chat-react`](examples/chat-react/README.md): a Slack-like chat in React with auth, pagination, uploads,
  search, scheduled messages, live aggregates and analytics.

Run the todo example:

```sh
bun install && bun run build
cargo run -p todo-server -- serve --dev --codegen-out examples/todo/web/src/lib/ferrix    # backend on :8090
cd examples/todo/web && bun run dev                                                       # app on :5173
```

Open http://localhost:5173 in two tabs and watch them stay in sync. `todo-server functions` lists the registered
functions, and `todo-server codegen --out <dir>` writes the TypeScript API once; `todo-server --help` shows every
command (backups, restores, migrations, export and import, …).

## Repository layout

| Path | What |
|---|---|
| `crates/ferrix` | The runtime: storage, query engine, reactivity, server, CLI, codegen, test helpers. |
| `crates/ferrix-macros` | The `#[table]`, `#[query]`, `#[mutation]`, `#[action]`, `#[migration]`, … macros. |
| `crates/ferrix-cli` | The `ferrix` command (`ferrix new`). |
| `packages/client` | `@ferrix/client`: the TypeScript client (sync, optimistic updates, reconnects). |
| `packages/react`, `packages/svelte` | `@ferrix/react` and `@ferrix/svelte` bindings. |
| `packages/dashboard` | The admin dashboard (Svelte), embedded in the binary at `/_/`. |
| `examples/` | The todo and chat apps. |
| `deploy/` | systemd, Docker Compose and Kubernetes files. |
| `docs/` | The documentation site (VitePress): guide, operations, [API contract](docs/CONTRACT.md), [protocol](docs/PROTOCOL.md). |

## Development

```sh
cargo fmt --all --check                       # formatting (rustfmt); `cargo fmt --all` fixes it
cargo test --workspace                        # Rust tests: unit, property, reactivity model, WebSocket e2e
bun run typecheck && bun run test             # TypeScript packages, dashboard included
```

More test suites (all run in CI by [`.github/workflows/release.yml`](.github/workflows/release.yml)):

```sh
cargo test -p ferrix --test it codegen_typechecks_with_tsc -- --ignored   # the generated TS type-checks
cargo test -p ferrix-cli --test new -- --ignored                          # `ferrix new` apps build
cargo test -p ferrix --features olap --lib --test it                      # with analytics (builds DuckDB: minutes)
FUZZ_SCALE=20 cargo test --release -p ferrix --test it fuzz               # longer protocol fuzzing
bun run check:dashboard                       # the committed dashboard build matches its sources
(cd packages/dashboard && bun e2e/dashboard.ts)   # dashboard e2e, after `cargo build -p todo-server`
cd examples/todo/web && bun e2e/two-tabs.ts       # needs the backend and Vite running
bun e2e/reconnect.ts ../../../target/debug/todo-server <data dir>   # Vite running, backend not
bun scripts/takeover-e2e.ts                   # zero-downtime takeovers under load, after `cargo build -p todo-server`
bun scripts/docker-rollout-e2e.ts             # the same with Docker Compose and Traefik (needs Docker and the image)
bun scripts/k3s-rollout-e2e.ts                # the same on k3s in Docker
```

Benchmarks: `cargo test --release -p ferrix --test it bench_ -- --ignored --nocapture`.

The dashboard's built files are committed, so `cargo build` needs no JavaScript toolchain. After changing
`packages/dashboard` or the client packages, run `bun run build:dashboard` and commit `crates/ferrix/dashboard-dist`
(`cargo test` fails while it is out of date). `cd packages/dashboard && bun run dev` serves it on :5174 for UI work.

The documentation site lives in `docs/` (VitePress): `bun run docs:dev` serves it with live reload, and
`bun run docs:build` builds it. Pushing to `master` publishes it to GitHub Pages
([`.github/workflows/docs.yml`](.github/workflows/docs.yml)).
