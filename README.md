# RxFerrix

RxFerrix is a Rust backend framework for apps that need live data. Define tables and functions in Rust, generate a typed TypeScript API, and subscribe to it from React, Svelte 5, or the core JavaScript client.

When a mutation changes data a query read, the server updates the query's subscribers. You do not write polling or cache invalidation code. SQLite, authentication, file storage, background jobs, search, and an admin dashboard run inside the app's server process.

**[Documentation](https://genesysflow.github.io/rxferrix/)** · [Get started](docs/getting-started.md) · [How it works](docs/guide/concepts.md)

## Run your first app

Install [Rust](https://rustup.rs/) and [Bun](https://bun.sh/), then create a project:

```sh
cargo install rxferrix-cli
rxferrix new my-app
cd my-app
rxferrix dev
```

In a second terminal, from `my-app`:

```sh
cd web
bun install
bun run dev
```

Open http://localhost:5173 in two tabs. Add a task in one; both lists update. The backend listens at http://127.0.0.1:8090. Its dashboard is at http://127.0.0.1:8090/_/; use the setup link printed by the server to create the first administrator.

React is the default. Use `rxferrix new my-app --web svelte` for Svelte 5, or `--web none` for a backend only.

`rxferrix` creates projects. Commands such as `serve`, `codegen`, and `backup` belong to **your app's binary**: during development, run them as `cargo run -- <command>`.

## Write the backend

The generated app includes this pattern in `src/tasks.rs`:

```rust
use rxferrix::prelude::*;

#[table(name = "tasks")]
pub struct Task {
    pub text: String,
    pub done: bool,
}

#[query]
pub async fn list(ctx: &QueryCtx) -> Result<Vec<Doc<Task>>> {
    ctx.db.query::<Task>().order(Order::Desc).take(100).await
}

#[mutation]
pub async fn add(ctx: &MutationCtx, text: String) -> Result<Id<Task>> {
    let text = text.trim().to_string();
    ensure_user!(!text.is_empty(), "empty_text", "A task needs some text");
    ctx.db.insert(Task { text, done: false }).await
}
```

Declare `mod tasks;` in `src/main.rs`, then start the framework with `rxferrix::RxFerrix::new().run()`. Compiled modules register their tables and functions automatically.

| Function | Use it for |
| --- | --- |
| `#[query]` | Read a consistent database snapshot; clients can subscribe to the result. |
| `#[mutation]` | Validate and write data in one transaction. |
| `#[action]` | Call an external API, send email, or coordinate separate queries and mutations. |

## Use it from the frontend

The generated React app already creates a client and wraps the component tree in `RxFerrixProvider`. Inside that tree:

```tsx
import { useState } from "react";
import { useMutation, useQuery } from "@rxferrix/react";
import { api } from "./rxferrix";

export function TaskList() {
  const tasks = useQuery(api.tasks.list);
  const add = useMutation(api.tasks.add);
  const [error, setError] = useState<string | null>(null);

  async function addTask() {
    try {
      await add({ text: "Buy milk" });
      setError(null);
    } catch (error) {
      setError(error instanceof Error ? error.message : String(error));
    }
  }

  if (tasks === undefined) return <p>Loading…</p>;
  return (
    <>
      <button onClick={addTask}>Add task</button>
      {error && <p>{error}</p>}
      <ul>{tasks.map((task) => <li key={task._id}>{task.text}</li>)}</ul>
    </>
  );
}
```

Query failures go to a React error boundary; [status hooks](docs/guide/clients.md) let you render them inline.

`rxferrix dev` runs `cargo run -- serve --dev` and rebuilds and restarts the backend when you save Rust code. The generated app's configuration writes the client API to `web/src/rxferrix` on every start, so changed functions and types reach the frontend too. Keep generated files generated.

## Choose the next guide

| You want to… | Read |
| --- | --- |
| Understand queries, transactions, and subscriptions | [Core concepts](docs/guide/concepts.md) |
| Add tables, indexes, pagination, and errors | [Functions and data](docs/guide/functions.md) |
| Connect a UI or JavaScript client | [Clients](docs/guide/clients.md) |
| Add sign-in and protect data | [Authentication and authorization](docs/guide/auth.md) |
| Upload files | [File storage](docs/guide/file-storage.md) |
| Schedule work or coordinate durable steps | [Scheduler](docs/guide/scheduler.md), [workflows](docs/guide/workflows.md) |
| Search, count, or report on data | [Search](docs/guide/search.md), [aggregates](docs/guide/aggregates.md), [analytics](docs/guide/analytics.md) |
| Change a stored schema | [Migrations](docs/guide/migrations.md) |
| Run an app in production | [Deploying](docs/operations/deploying.md), [backups](docs/operations/backups.md) |

A deployment uses one active server process and a data directory on local disk. Frontend assets can be embedded in the binary. RxFerrix does not distribute the live database across multiple active servers, and client reconnects do not provide persistent offline storage. Read [behavior and limits](docs/guide/things-to-know.md) before designing around these guarantees.

## Explore the examples

- [Todo](examples/todo/README.md): Svelte 5, live queries, uploads, search, delayed jobs, and a migration.
- [Chat](examples/chat-react/README.md): React, sign-in, channel permissions, pagination, optimistic updates, search, aggregates, and optional analytics.

For framework development, repository layout, tests, and releases, see [Contributing](CONTRIBUTING.md). The detailed [Rust API contract](docs/CONTRACT.md), [wire protocol](docs/PROTOCOL.md), and [design](docs/DESIGN.md) describe the implementation.
