# Core concepts

A RxFerrix app is a Rust program. Your program depends on `rxferrix`, defines tables and functions, and starts with `RxFerrix::new().run()`. The compiled binary provides the database, API server, background jobs, and dashboard.

## Tables describe stored documents

A `#[table]` Rust struct defines a document's fields. The framework adds a typed ID and a creation time. Rust code receives `Doc<Task>`; TypeScript receives the task's fields together with `_id` and `_creationTime`.

Declare indexes alongside the table for the ways you need to read it. Schema and index changes come from code. The dashboard edits documents, not the Rust schema.

## Queries describe what the UI needs

A `#[query]` reads a consistent database snapshot. It can return one document, a list, a page, or a custom result assembled from several tables.

The server records the documents and index ranges the query reads. A subscription stays active after its first result. When a committed write touches those reads, the server runs the query again and sends changed results to its subscribers.

The same query can serve many subscribers. Identity-dependent queries include the relevant identity in sharing and invalidation. This is why authorization checks belong inside queries and why query results must be deterministic.

Use the database API, `ctx.auth`, `ctx.env`, and `ctx.now()` for tracked inputs. Do not fetch external APIs, read the system clock, or generate random output in a query. Time passing alone does not rerun a subscription.

## Mutations change data atomically

A `#[mutation]` reads and writes inside one transaction. Returning an error rolls back its writes. Several reads and writes in one mutation succeed or fail together, including transactional scheduling.

One writer serializes mutations. Keep them short and move network calls or long work into actions. Rust does not sandbox function bodies; avoiding external side effects in a mutation is an application requirement.

The client resolves a successful mutation once its subscribed results have caught up with that commit. You can update form state after `await add(...)` without manually refetching the list.

Unanswered mutations can be resent after reconnect. The server deduplicates them within a bounded retry window; this does not make arbitrary new calls or external side effects exactly once. See [behavior and limits](/guide/things-to-know#reconnects-and-retries).

## Actions work with external services

A `#[action]` may call an API, send an email, or run a report. It has no database transaction of its own. Use `ctx.run_query::<f>(args)` and `ctx.run_mutation::<f>(args)` to read or change app data; each call is separate.

An action that sends an email and then fails cannot undo the email. Give external operations an idempotency strategy when retries are possible. For work that must start after a successful write, schedule an internal action from the mutation.

## Registration comes from compiled modules

Declare each backend module with normal Rust `mod` declarations. Macros register its tables and functions when the module is compiled; there is no route file or manual function list.

A function in `src/tasks.rs` is called `tasks:list` on the wire and appears as `api.tasks.list` in generated TypeScript. Adding a file without declaring its module does not register it.

`#[internal_query]`, `#[internal_mutation]`, and `#[internal_action]` make functions available to server code and jobs without exposing them to app clients. A public function must validate its inputs and permissions.

## Code generation connects Rust and TypeScript

Run your app's `codegen` command after changing its API. Generated projects also run codegen at startup under `serve --dev`, using their configured output directory.

Codegen produces function references and types. It does not create HTTP routes you need to maintain, and it does not run a compiler watcher. See [Get started](/getting-started) for the development loop.

## Production uses one active server

The live database is SQLite on local disk. Queries can run concurrently, but writes pass through one writer. A takeover temporarily runs an old and new process together so the new one can prepare before replacing the old one; it does not add a second active database writer.

Use [backups and replication](/operations/backups) for recovery, [takeover](/operations/zero-downtime) for updates, and [analytics](/guide/analytics) for SQL reports over a separate copy. Persistent offline client storage and a distributed multi-writer database are outside the framework's current model.
