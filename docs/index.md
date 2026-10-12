---
layout: home

hero:
  name: Ferrix
  text: A reactive backend in Rust
  tagline: Write typed Rust functions. Clients get live results. Ship it all as one binary.
  image:
    src: /logo.svg
    alt: Ferrix
  actions:
    - theme: brand
      text: Get started
      link: /getting-started
    - theme: alt
      text: Read the guide
      link: /guide/functions
    - theme: alt
      text: GitHub
      link: https://github.com/genesysflow/ferrix

features:
  - icon: ⚡
    title: Live by default
    details: Clients subscribe to queries, and results update by themselves whenever a write changes the data they read. No cache to invalidate, no refetching to write.
  - icon: 🦀
    title: Typed end to end
    details: Rust structs are the schema. A TypeScript API is generated from your functions, for React and Svelte.
  - icon: 📦
    title: One file to deploy
    details: SQLite is embedded, and so is your frontend if you like. Copy the binary to a server, or use the Docker image, and start it.
  - icon: 🔁
    title: Deploys without downtime
    details: A new version prepares next to the running one and takes over in well under a second, without losing a write. One that can't read the data steps back.
  - icon: 🔋
    title: Batteries included
    details: Auth, file storage, crons, durable workflows, full-text and vector search, live aggregates, analytics and migrations.
  - icon: 🛠️
    title: An admin dashboard
    details: Browse and edit data live, run functions, tail logs, manage users, files, backups, migrations and settings, at /_/ in every server.
---

## What it looks like

A table and two functions on the server:

```rust
use ferrix::prelude::*;

#[table(name = "todos")]
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

And a React component that uses them. When anyone adds a todo, every open list updates:

```tsx
import { useMutation, useQuery } from "@ferrix/react";
import { api } from "./ferrix"; // generated from the functions above

function Todos() {
  const todos = useQuery(api.todos.list);
  const add = useMutation(api.todos.add);
  return (
    <>
      <button onClick={() => add({ text: "Buy milk" })}>Add</button>
      <ul>{todos?.map((t) => <li key={t._id}>{t.text}</li>)}</ul>
    </>
  );
}
```

[Get started →](/getting-started)
