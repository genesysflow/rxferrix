# Ferrix

A single-binary reactive backend in Rust, deployed as one file.
You write typed `#[query]` / `#[mutation]` / `#[action]` functions in Rust. Clients subscribe with `useQuery` over a
WebSocket and get updated results automatically whenever a write affects them.

Status: Phases 0–8 are built (core, shared reactive subscriptions, clients, the admin dashboard, auth, the scheduler
and crons, file storage, and full-text and vector search); see [PLAN.md](PLAN.md) §10 for what each still lacks.
Phase 9 (operations: backups, migrations, release builds) is next.

## Layout

| Path | What |
|---|---|
| `crates/ferrix` | The runtime: SQLite storage, query engine, read sets, write log, sync worker, axum server, CLI, codegen, `TestApp`. |
| `crates/ferrix-macros` | `#[table]`, `#[query]`, `#[mutation]`, `#[action]`, `#[internal_*]`. |
| `packages/client` | `@ferrix/client`: framework-agnostic TS client (sync protocol, optimistic updates, reconnect). |
| `packages/react` | `@ferrix/react`: `FerrixProvider`, `useQuery`, `useMutation`, `useAction`. |
| `packages/svelte` | `@ferrix/svelte`: Svelte 5 runes bindings. |
| `packages/dashboard` | The admin dashboard (Svelte 5 SPA), built into `crates/ferrix/dashboard-dist` and embedded at `/_/`. |
| `examples/todo` | Rust backend and Svelte web app, with Playwright e2e scripts. |
| `examples/chat-react` | The flagship example: a Slack-lite in React (auth, pagination, uploads, search, scheduler). |
| `docs/CONTRACT.md` | Rust API and the exact code each macro generates. |
| `docs/PROTOCOL.md` | WebSocket and HTTP protocol. |

## A backend in a nutshell

```rust
use ferrix::prelude::*;

#[table(name = "todos")]
#[index(by_done = [done])]
pub struct Todo { pub text: String, pub done: bool }

#[query]
pub async fn list(ctx: &QueryCtx, done: Option<bool>) -> Result<Vec<Doc<Todo>>> {
    let q = match done {
        Some(d) => ctx.db.query::<Todo>().with_index(Todo::by_done, |q| q.eq(Todo::done, d)),
        None => ctx.db.query::<Todo>(),
    };
    q.order(Order::Desc).take(500).await
}

#[mutation]
pub async fn add(ctx: &MutationCtx, text: String) -> Result<Id<Todo>> {
    ctx.db.insert(Todo { text, done: false }).await
}

fn main() -> ferrix::Result<()> { ferrix::Ferrix::new().run() }
```

Functions call each other through the runtime with turbofish: `ctx.run_mutation::<add>(("hi".into(),)).await?`.

## Auth

Enable sign-in methods in `main`, then use `ctx.auth` in functions:

```rust
use ferrix::auth::{AuthConfig, GitHub, User};

fn main() -> ferrix::Result<()> {
    ferrix::Ferrix::new()
        .auth(AuthConfig::password().magic_link().oauth(GitHub::from_env()).redirect_url("http://localhost:5173"))
        .run()
}

#[table(name = "notes")]
#[index(by_owner = [owner])]
pub struct Note { pub owner: Id<User>, pub text: String }

#[mutation]
pub async fn add_note(ctx: &MutationCtx, text: String) -> Result<Id<Note>> {
    let owner = ctx.auth.require_user()?;          // UserError "unauthenticated" when signed out
    ctx.db.insert(Note { owner, text }).await
}
```

On the client, `getAuth(client)` signs in and keeps the socket authenticated (tokens in `localStorage`, refreshed
automatically). React has `useFerrixAuth()`, `useAuthActions()` and `useCurrentUser()`; Svelte has `useAuth()`
and `useCurrentUser()`:

```ts
const auth = getAuth(client);
await auth.signIn({ email, password });           // or signUp, requestMagicLink + verifyCode, signInWithOAuth("github")
await auth.handleRedirectCallback();              // on the page OAuth and email links return to
```

Access tokens are 15-minute HS256 JWTs signed with a key from `<data dir>/auth_secret.key`; refresh tokens rotate
on every use. Email goes through `--smtp-url` (printed to the log in `--dev`). With no admin yet, `serve` prints a
one-time `/_/setup#token=…` link; `admin create <email>` works too. Details: [docs/PROTOCOL.md](docs/PROTOCOL.md)
("Auth") and [docs/CONTRACT.md](docs/CONTRACT.md) §1.2, §1.5.

## File storage

Files live in `<data dir>/storage/` by default, or in an S3-compatible bucket (`--storage s3://bucket/prefix`, with
the standard `AWS_*` variables for credentials, region and endpoint, e.g. R2 or MinIO). Uploads take three steps:

```rust
use ferrix::storage::StoredFile;

#[mutation]
pub async fn generate_upload_url(ctx: &MutationCtx) -> Result<String> {
    ctx.storage.generate_upload_url().await        // signed, single-use, valid for an hour
}

#[mutation]
pub async fn set_avatar(ctx: &MutationCtx, file: Id<StoredFile>) -> Result<()> { /* store the id in a document */ }

#[query]
pub async fn avatar_url(ctx: &QueryCtx, file: Id<StoredFile>) -> Result<Option<String>> {
    ctx.storage.get_url(file).await                // "/api/storage/<id>", live: null once the file is deleted
}
```

```ts
const url = await client.mutation(api.files.generate_upload_url, {});
const storageId = await uploadFile(url, file, { onProgress: ({ loaded, total }) => (progress = loaded / total) });
await client.mutation(api.files.set_avatar, { file: storageId });
```

`GET /api/storage/<id>` serves files with ETags, range requests and headers that keep uploads from running script on
your origin. Mutations can also `delete` files (transactionally) and read their metadata; actions can `store` and
`get` bytes directly. Uploads are capped at 100 MB (`--max-upload-size`). File URLs are unguessable capability URLs:
anyone with the URL can read the file. Details: [docs/CONTRACT.md](docs/CONTRACT.md) §1.6 and
[docs/PROTOCOL.md](docs/PROTOCOL.md) ("File storage"). The todo example attaches images to todos.
## Scheduler and crons

Mutations and actions can schedule other mutations and actions (internal ones too), and `main` can declare crons:

```rust
use std::time::Duration;
use ferrix::scheduler::ScheduledJob;

#[mutation]
pub async fn add_later(ctx: &MutationCtx, text: String, delay_seconds: u32) -> Result<Id<ScheduledJob>> {
    // Written in this mutation's transaction: scheduled only if it commits. Scheduling a query doesn't compile.
    ctx.scheduler.run_after::<add>(Duration::from_secs(delay_seconds.into()), (text,)).await
}

fn main() -> ferrix::Result<()> {
    ferrix::Ferrix::new()
        .cron::<purge_completed>("purge-completed", "0 0 * * * *", ())   // sec min hour day month weekday, UTC
        .run()
}
```

Also `run_at::<f>(ms, args)`, `cancel(id)`, `with_retries(n)` (actions), and `.interval::<f>(name, Duration, args)`.
Jobs live in the `_scheduled_jobs` table (so the dashboard and subscriptions see them live). Scheduled mutations run
**exactly once** (the job's completion commits with the mutation's writes); actions run **at most once** (an action
interrupted by a restart is marked failed). Scheduled functions run with no user identity; the execution log shows
them as `scheduler`. Missed cron runs run once at startup, a cron whose previous run is still going is skipped, and
finished jobs are deleted after 7 days. In tests, `TestApp::advance_time(d)` / `run_due_jobs()` run due jobs
deterministically. Details: [docs/CONTRACT.md](docs/CONTRACT.md) §1.7.

## Search

Full-text search uses SQLite FTS5 and vector search uses sqlite-vec, both compiled into the binary. The indexes are
updated in the mutation's transaction, so search results always agree with document reads, and search queries are
reactive like any other query (docs/CONTRACT.md §1.8).

```rust
#[table(name = "messages")]
#[search_index(search_body, search = body, filter = [channel])]
#[vector_index(by_embedding, field = embedding, dims = 1536, filter = [channel])]
pub struct Message { pub channel: String, pub body: String, pub embedding: Option<Vec<f32>> }

#[query]   // most relevant first; the last word is a prefix ("hello wor" matches "hello world")
pub async fn search(ctx: &QueryCtx, channel: String, text: String) -> Result<Vec<Doc<Message>>> {
    ctx.db.query::<Message>()
        .with_search_index(Message::search_body, |q| q.search(Message::body, text).eq(Message::channel, channel))
        .take(20)
        .await
}

#[action]  // vector search is for actions only; returns (id, cosine similarity) pairs
pub async fn similar(ctx: &ActionCtx, embedding: Vec<f32>) -> Result<Vec<Id<Message>>> {
    let hits = ctx.vector_search(Message::by_embedding, &embedding, VectorSearchOpts::new(10)).await?;
    Ok(hits.into_iter().map(|(id, _score)| id).collect())
}
```

User text is split into words that are always matched literally (never parsed as FTS5 syntax); empty text matches
nothing; a query returns at most 1024 documents. A search subscription reruns after any write that changes the text or
a filter field of the table's documents. Rough numbers (release build, a laptop): 50k documents of 12 words: a
one-word query takes 0.35 ms, a broad two-letter prefix matching every document 80 ms; 50k vectors of 384 dimensions:
a k=10 query takes about 18 ms (7 ms with a filter keeping 1 in 10). The todo example has a search box.

## Dashboard

Every server has an admin dashboard at `/_/` (e.g. http://127.0.0.1:8090/_/), embedded in the binary. Create an
administrator with the `/_/setup#token=…` link that `serve` prints on first start, or with
`todo-server admin create you@example.com`, then sign in.

- **Data**: every table (system tables grouped below), with a live, paginated document grid, filters on index
  fields, a side panel with a JSON editor (insert, edit, delete; validated against the table's Rust type), and
  clickable ids that open the referenced document. Secrets (password and token hashes) are never shown, and system
  tables are read-only except the `name`, `image` and `emailVerified` of users.
- **Functions**: every function with its TS signature, and a runner: JSON args, optionally *as a user*, showing the
  result, log lines and duration.
- **Logs**: a live tail of executions (filter by path, kind, outcome; expand for the error, log lines and args).
- **Users**: users, linked providers, sessions (revoke), delete.
- **Files**: stored files with previews (images, video, audio, text), download, delete, and upload by drag and drop.
  `Id<"_storage">` values in the data grid link here.
- **Scheduler**: crons with their schedule, last and next run; scheduled jobs live, by state, with cancel, run now
  and retry.
- **Overview**: commit ts, connections and subscriptions, database sizes, busiest and failing functions.

It talks to the server over the same WebSocket protocol as apps (admin-only `_system/dashboard:*` functions), so every
view is live, plus `/api/admin/*` for logs and stats (docs/CONTRACT.md §4). The built assets are committed, so
`cargo build` needs no JS toolchain; after changing `packages/dashboard` (or the client packages it bundles), run
`bun run build:dashboard` and commit `crates/ferrix/dashboard-dist` (`cargo test` fails while it is stale). For UI
work, `cd packages/dashboard && bun run dev` serves it on :5174 with `/api` proxied to :8090. Build without it with
`default-features = false` on the `ferrix` dependency.

## Running the example

Prerequisites: Rust via mise (`mise.toml`) and bun.

```sh
bun install && bun run build                                    # build the TS packages
cargo run -p todo-server -- serve --dev \
  --codegen-out examples/todo/web/src/lib/ferrix                # backend on :8090; keeps the generated TS API current
cd examples/todo/web && bun run dev                             # web app on :5173 (proxies /api to :8090)
```

Open http://localhost:5173 in two tabs. Other commands: `todo-server functions` lists the registered functions,
and `todo-server codegen --out <dir>` writes the TS API once.

For a bigger app, [examples/chat-react](examples/chat-react/README.md) is a Slack-lite in React (backend on :8097,
app on :5180) with a self-contained e2e test: `cd examples/chat-react/web && bun e2e/chat.ts`.

## Tests

```sh
cargo test --workspace                                          # Rust: unit, proptests, model-based reactivity, WS e2e
cargo test -p ferrix --test it codegen_typechecks_with_tsc -- --ignored   # generated TS type-checks with tsc
bun run typecheck && bun run test                               # TS packages (dashboard included)
bun run check:dashboard                                         # the committed dashboard build matches its sources
(cd packages/dashboard && bun e2e/dashboard.ts)                 # after `cargo build -p todo-server`; starts its own servers
cd examples/todo/web && bun e2e/two-tabs.ts                     # needs the backend and Vite running
bun e2e/reconnect.ts ../../../target/debug/todo-server <data dir>  # Vite running, backend NOT running
```

`bun e2e/dashboard.ts [binary] [screenshot dir]` takes the todo-server binary (default `target/debug/todo-server`)
and optionally saves screenshots of each page.
