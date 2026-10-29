---
layout: home

hero:
  name: RxFerrix
  text: Rust backends with live data
  tagline: Define tables and functions in Rust. Subscribe from React or Svelte. Deploy your app with an embedded database.
  image:
    src: /logo.svg
    alt: RxFerrix
  actions:
    - theme: brand
      text: Build your first app
      link: /getting-started
    - theme: alt
      text: Understand the framework
      link: /guide/concepts
    - theme: alt
      text: GitHub
      link: https://github.com/genesysflow/rxferrix

features:
  - title: Live query results
    details: Queries track their database reads. Committed writes update the subscribers whose results may have changed.
  - title: A typed client API
    details: Generate TypeScript function references and data types from your Rust backend for React, Svelte 5, or the core client.
  - title: One server to run
    details: SQLite, auth, file storage, scheduled jobs, search, and the admin dashboard run in your app. Frontend assets can be embedded too.
---

## Start with a working app

Install Rust and Bun, then create a project:

```sh
cargo install rxferrix-cli
rxferrix new my-app
cd my-app
cargo run -- serve --dev
```

In a second terminal, run `bun install` and `bun run dev` inside `my-app/web`. Open http://localhost:5173 in two tabs to see live task updates.

The [walkthrough](/getting-started) explains the files, dashboard setup, and how to rebuild after a backend change. Choose `--web svelte` for Svelte 5 or `--web none` for a backend only.

## Understand the three function types

| Function | What your code does | How clients use it |
| --- | --- | --- |
| Query | Read a consistent snapshot and return data. | Subscribe and receive updated results. |
| Mutation | Validate and change data in one transaction. | Await the write and updated subscriptions. |
| Action | Call external services or coordinate other functions. | Await a result; external effects are not transactional. |

Tables are Rust structs. Functions live in normal Rust modules. Codegen produces `api.tasks.list` and other typed references for your frontend. [Core concepts](/guide/concepts) explains how these pieces fit together.

## Find what you need

| Build | Run |
| --- | --- |
| [Functions and data](/guide/functions) | [Deploy an app](/operations/deploying) |
| [Connect a client](/guide/clients) | [Update a running app](/operations/zero-downtime) |
| [Authentication and authorization](/guide/auth) | [Backups and restores](/operations/backups) |
| [Files](/guide/file-storage) and [background work](/guide/scheduler) | [Logs and metrics](/operations/monitoring) |

For a complete app, explore the repository's [todo](https://github.com/genesysflow/rxferrix/tree/master/examples/todo) or [chat](https://github.com/genesysflow/rxferrix/tree/master/examples/chat-react) example. For exact signatures and wire formats, use the [Rust API](/CONTRACT), [protocol](/PROTOCOL), and [command line](/reference/cli) references.

## Know the deployment model

RxFerrix uses one active server and a SQLite database on local disk. It supports concurrent reads, transactional writes, takeover updates, and recovery from backups or replication. It does not distribute live writes across nodes or persist offline client state. Read [Behavior and limits](/guide/things-to-know) when deciding whether this model fits your app.
