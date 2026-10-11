# Ferrix

A single-binary reactive backend in Rust, deployed as one file.
You write typed `#[query]` / `#[mutation]` / `#[action]` functions in Rust. Clients subscribe with `useQuery` over a
WebSocket and get updated results automatically whenever a write affects them.

Status: Phases 0–8 are built (core, shared reactive subscriptions, clients, the admin dashboard, auth, the scheduler
and crons, file storage, and full-text and vector search); see [PLAN.md](PLAN.md) §10 for what each still lacks.
Phase 9 (operations) is built: data migrations (`#[migration]`), backups and restores (also from the dashboard),
continuous replication with point-in-time restores, JSONL/CSV export and import, metrics, JSON logs, TLS (with
certificate reload), frontend serving, release builds and a CI workflow for the binary and the Docker image (see
[Deploying](#deploying)). Phase 10 fixed the rough edges that building the chat
example found (index order, codegen names, `#[object]`, dev mode, storage URLs in dev; see
[Things to know](#things-to-know)).

## Layout

| Path | What |
|---|---|
| `crates/ferrix` | The runtime: SQLite storage, query engine, read sets, write log, sync worker, axum server, CLI, codegen, `TestApp`. |
| `crates/ferrix-macros` | `#[table]`, `#[object]`, `#[query]`, `#[mutation]`, `#[action]`, `#[internal_*]`, `#[migration]`. |
| `crates/ferrix-cli` | The `ferrix` command: `ferrix new` creates an app from its `templates/`. |
| `packages/client` | `@ferrix/client`: framework-agnostic TS client (sync protocol, optimistic updates, reconnect). |
| `packages/react` | `@ferrix/react`: `FerrixProvider`, `useQuery(WithStatus)`, `usePaginatedQuery(WithStatus)`, `useMutation`, `useAction`, auth hooks. |
| `packages/svelte` | `@ferrix/svelte`: Svelte 5 runes bindings. |
| `packages/dashboard` | The admin dashboard (Svelte 5 SPA), built into `crates/ferrix/dashboard-dist` and embedded at `/_/`. |
| `examples/todo` | Rust backend and Svelte web app, with Playwright e2e scripts. |
| `examples/chat-react` | The flagship example: a Slack-lite in React (auth, pagination, uploads, search, scheduler). |
| `docs/CONTRACT.md` | Rust API and the exact code each macro generates. |
| `docs/PROTOCOL.md` | WebSocket and HTTP protocol. |

## Start an app

```sh
cargo install --path crates/ferrix-cli     # the `ferrix` command
ferrix new my-app                          # --web react (default), svelte or none
cd my-app && cargo run -- serve --dev      # then, in a second terminal: cd web && bun install && bun run dev
```

The app is a Rust crate (a sample `tasks` table, query and mutations in `src/tasks.rs`) with the web app in `web/`,
and a `.cargo/config.toml` that builds SQLite for parallel queries (see [Deploying](#deploying)) and points
`serve --dev`'s codegen at `web/src/ferrix`. Built from a checkout, `ferrix new` depends on that checkout: a path
dependency for the crate, and the @ferrix packages packed into `web/vendor` (run `bun install && bun run build` in the
checkout first). `--published` uses crates.io and npm instead, and `--ferrix-path DIR` another checkout.

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

**Indexes end in `_creationTime, _id`**: documents with equal indexed fields are in creation order, so
`with_index(Message::by_channel, |q| q.eq(Message::channel, c)).order(Order::Desc)` is newest first, and the creation
time can be bounded after the declared fields: `q.eq(Message::channel, c).gt(Message::_creation_time, since)`.
`.count()` counts an index range without reading the documents.

**Types that aren't tables** (a function's output "view" struct, an enum argument) use `#[object]`: it derives
serde and ts-rs through ferrix (camelCase JSON), so the app needs neither as a dependency. Codegen declares them in
`dataModel.ts` by name (`Array<MessageView>`), with field doc comments as JSDoc. A document can be embedded whole:

```rust
#[object]
pub struct MessageView {
    #[serde(flatten)]
    pub message: Doc<Message>,       // _id, _creationTime and the message's fields, at the top level
    pub author_name: String,         // TS: { authorName: string } & Doc<"messages">
}

#[query]
pub async fn list(ctx: &QueryCtx, channel: Id<Channel>, pagination_opts: PaginationOpts)
    -> Result<PaginationResult<MessageView>> {
    let page = ctx.db.query::<Message>()
        .with_index(Message::by_channel, |q| q.eq(Message::channel, channel))
        .order(Order::Desc)
        .paginate(pagination_opts)
        .await?;
    page.try_map(|message| view(ctx, message)).await      // also map_async, try_map_concurrent
}
```

**Dev mode.** `ctx.is_dev()` tells whether the server runs with `--dev`. `#[action(dev_only)]` (also queries and
mutations) marks a function for development (seeding, reading a dev inbox): the server refuses it outside `--dev`,
and codegen puts it in `devApi` instead of `api`. In `--dev` without SMTP, auth emails are printed to the log by
`DevLogMailer`; `AuthConfig::dev_mailer(m)` replaces it (and can forward to it).

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
on every use. Signing out, revoking a session from the dashboard, deleting a user or reusing a rotated refresh token
ends the sockets of that session **at once** (`AuthError`, then the client continues signed out), not when the access
token expires. Email goes through `--smtp-url` (printed to the log in `--dev`). With no admin yet, `serve` prints a
one-time `/_/setup#token=…` link; `admin create <email>` works too.

OAuth presets: `GitHub`, `Google`, `Discord` (only a Discord-verified email is used) and `Apple` (Sign in with Apple),
plus `Oidc` for any OpenID Connect issuer; each has `from_env()` (`FERRIX_AUTH_<PROVIDER>_CLIENT_ID`, …) and can also
be set up in the dashboard's Settings. For Apple, give the Services ID, team id, key id and the `.p8` private key
(`FERRIX_AUTH_APPLE_PRIVATE_KEY` or `_PRIVATE_KEY_FILE`): ferrix signs and rotates the short-lived client secret
itself, accepts Apple's `form_post` callback (register `https://<public url>/api/auth/oauth/apple/callback`; the
public URL must be https), verifies the ID token against Apple's keys, and keeps the name Apple sends on the first
sign-in only. The key is a secret: keep it out of source control (the dashboard stores it encrypted).

**Clerk, Auth0, Supabase or any OIDC provider** can sign users in instead (or as well): ferrix verifies their JWTs
against the provider's JWKS (RS256, ES256, EdDSA; `iss`, `aud`, `exp`, `nbf`) and creates a `_users` row for each new
subject, so `ctx.auth.user_id()` and `Id<User>` work the same for every sign-in method:

```rust
.auth(AuthConfig::default()
    .jwt(JwtProvider::new("https://clerk.example.com").any_audience())          // JWKS found by OIDC discovery
    .jwt(JwtProvider::new("https://tenant.auth0.com/").audience("https://api.example.com")))
```

```ts
// Hand the provider's token getter to the client; it refreshes before `exp` and after an AuthError.
client.setAuth(({ forceRefresh }) => clerk.session!.getToken({ skipCache: forceRefresh }));
```

**Runtime settings.** Administrators can change OAuth providers, SMTP, the redirect allowlist, sign-ups, token
lifetimes and CORS origins without a restart (`_system/settings:*`). Secrets are encrypted at rest with a key derived
from `auth_secret.key` and never shown again. Whatever the code, the command line or the environment configures wins
over these settings. Every administrator write is recorded in an audit log (`_audit_log`), and admins can act as a
user through a short-lived, audited session. Details: [docs/PROTOCOL.md](docs/PROTOCOL.md) ("Auth") and
[docs/CONTRACT.md](docs/CONTRACT.md) §1.2, §1.5, §4.1.4, §4.1.5.

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

`GET /api/storage/<id>` serves files with ETags, range requests (several ranges as `multipart/byteranges`) and
headers that keep uploads from running script on your origin. In `--dev`, file and upload URLs are root-relative
(`/api/storage/...`) unless a public URL is set explicitly (`--public-url`), so they go through a dev server's
`/api` proxy (Vite) like every other request. Add `?thumb=200x200` (and `&fit=cover`) to an image's
URL for a thumbnail (JPEG, PNG, WebP, GIF; generated once and cached; oversized images are refused from their header,
before decoding). Mutations can also `delete` files (transactionally) and read their metadata; actions can `store`
and `get` bytes directly. Uploads are capped at 100 MB (`--max-upload-size`). File URLs are unguessable capability
URLs: anyone with the URL can read the file. For private files, use expiring signed URLs and turn on private mode
(`StorageConfig::private(true)` or `--storage-private`), which refuses unsigned URLs:

```rust
#[query]   // in a query the expiry is rounded up to a multiple of the TTL, so the result is stable for that long
pub async fn avatar_url(ctx: &QueryCtx, file: Id<StoredFile>) -> Result<Option<String>> {
    ctx.storage.get_signed_url(file, Duration::from_secs(3600)).await   // ".../api/storage/<id>?exp=…&sig=…"
}
```

With S3, `get_download_url` hands out a **presigned URL to the bucket**: the file downloads straight from S3 (not
through the server), the bucket stays private, and S3 refuses the URL once it expires:

```rust
#[mutation]
pub async fn download_report(ctx: &MutationCtx, file: Id<StoredFile>) -> Result<Option<String>> {
    ctx.auth.require_user()?;   // decide who may see it
    ctx.storage.get_download_url_with(file, Duration::from_secs(15 * 60), DownloadOptions::default().attachment()).await
}
```

S3 serves it with the file's content type and name (unsafe types as attachments). The URL lasts at most 7 days, and
`?thumb=` doesn't work on it (use `get_signed_url` for thumbnails). With local storage the same call returns a signed
URL of the server, so the code works in development too.

Blobs left without a document (a crash mid-upload) and stale thumbnails are removed by the `_storage_sweep` system
cron (every 6 hours). Details: [docs/CONTRACT.md](docs/CONTRACT.md) §1.6 and [docs/PROTOCOL.md](docs/PROTOCOL.md)
("File storage"). The todo example attaches images to todos.
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
        .cron_tz::<daily_digest>("digest", "0 0 9 * * *", "Europe/Bucharest", ())   // 09:00 local, DST-aware
        .run()
}
```

Also `run_at::<f>(ms, args)`, `cancel(id)`, `with_retries(n)` (actions), and `.interval::<f>(name, Duration, args)`.
Canceling works on running actions too: the action sees `ctx.is_canceled()` (or awaits `ctx.canceled()` in a
`select!`) and should stop; after a 5 second grace period it is aborted at its next `.await`. Mutations it already
ran stay committed. Admins can run a cron immediately with `_system/scheduler:run_cron_now`.
Jobs live in the `_scheduled_jobs` table (so the dashboard and subscriptions see them live). Scheduled mutations run
**exactly once** (the job's completion commits with the mutation's writes); actions run **at most once** (an action
interrupted by a restart is marked failed). Scheduled functions run with no user identity; the execution log shows
them as `scheduler`. Missed cron runs run once at startup, a cron whose previous run is still going is skipped, and
finished jobs are deleted after 7 days. In a time zone, a run in the hour skipped by a spring-forward change runs an
hour later, and a time of day in the repeated fall-back hour runs once. In tests, `TestApp::advance_time(d)` / `run_due_jobs()` run due jobs
deterministically. Details: [docs/CONTRACT.md](docs/CONTRACT.md) §1.7.

**Workpools** limit how many actions of one kind run at once, with default retries, and **`on_complete`** runs a
mutation with the outcome once a job has finished (exactly once, in the transaction that records the outcome):

```rust
ferrix::Ferrix::new().workpool(Workpool::new("emails").max_parallelism(5).retries(3))

ctx.scheduler.pool("emails").on_complete::<sent>(user).enqueue::<send_email>((to,)).await?;
// sent(ctx, work_id: Id<ScheduledJob>, context: Id<User>, result: WorkResult<String>)
```

Each pool has its own slots, so a full pool doesn't hold up the others. `ctx.scheduler.cancel_pool("emails")`
cancels everything queued or running in it.

## Workflows

A `#[workflow]` is a durable function made of steps. Each step's outcome is journaled, so a workflow survives
restarts, can sleep for days and wait for events, and each mutation step runs exactly once:

```rust
#[workflow]
pub async fn onboard(step: &WorkflowCtx, user: Id<User>) -> Result<String> {
    let email = step.run_query::<email_of>((user,)).await?;
    step.run_action::<send_welcome>((email,)).with_retries(3).pool("emails").await?;
    step.sleep(Duration::from_secs(3 * 86_400)).await?;
    let answer: String = step.wait_for_event("survey").await?;   // ctx.workflows.send_event(id, "survey", ..)
    step.run_mutation::<save_answer>((user, answer.clone())).await?;
    Ok(answer)
}

// In a mutation (transactional) or an action:
let id: Id<Workflow> = ctx.workflows.on_complete::<onboarded>(user).start::<onboard>((user,)).await?;
```

The body runs again from the start whenever a step finishes. Steps already in the journal return their recorded
result at once, so the body must be deterministic: it touches the database and the outside world only through steps.
A failed step is an `Err` the body can handle. Steps run in parallel with `try_join!`, and `ctx.workflows.cancel(id)`
stops a workflow. Its status is its `_workflows` document, so a query can show progress live. The dashboard has a
Workflows page. Details: [docs/CONTRACT.md](docs/CONTRACT.md) §1.7.1.

## Search

Full-text search uses SQLite FTS5 and vector search uses sqlite-vec, both compiled into the binary. The indexes are
updated in the mutation's transaction, so search results always agree with document reads, and search queries are
reactive like any other query (docs/CONTRACT.md §1.8).

```rust
#[table(name = "messages")]
#[search_index(search_body, search = body, filter = [channel], tokenizer = "porter")]
#[search_index(search_ro, search = body, tokenizer = "snowball", language = "romanian")]   // "orașelor" finds "orașul"
#[vector_index(by_embedding, field = embedding, dims = 1536, filter = [channel], kind = "hnsw")]
pub struct Message { pub channel: String, pub body: String, pub embedding: Option<Vec<f32>> }

#[query]   // most relevant first; the last word is a prefix ("hello wor" matches "hello world")
pub async fn search(ctx: &QueryCtx, channel: String, text: String) -> Result<Vec<Doc<Message>>> {
    ctx.db.query::<Message>()
        .with_search_index(Message::search_body, |q| q.search(Message::body, text).eq(Message::channel, channel))
        .take(20)
        .await
}

#[action]  // an embedding from an API: (id, cosine similarity) pairs, from the HNSW graph
pub async fn similar(ctx: &ActionCtx, embedding: Vec<f32>) -> Result<Vec<Id<Message>>> {
    let hits = ctx.vector_search(Message::by_embedding, &embedding, VectorSearchOpts::new(10)).await?;
    Ok(hits.into_iter().map(|(id, _score)| id).collect())
}

#[query]   // a vector the query already has: exact, deterministic and live (reruns when a nearer one is written)
pub async fn more_like(ctx: &QueryCtx, id: Id<Message>) -> Result<Vec<Id<Message>>> {
    let Some(msg) = ctx.db.get(id).await? else { return Ok(vec![]) };
    let Some(embedding) = &msg.embedding else { return Ok(vec![]) };
    let opts = VectorSearchOpts::new(5).filter(VectorFilter::eq(Message::channel, msg.channel.clone()));
    Ok(ctx.db.vector_search(Message::by_embedding, embedding, opts).await?.into_iter().map(|(id, _)| id).collect())
}
```

User text is split into words that are always matched literally (never parsed as FTS5 syntax); empty text matches
nothing; a query returns at most 1024 documents. Relevance is BM25 per document (term frequency and length, no
corpus statistics), so a document's rank depends only on itself and the query. That makes two things exact:

- **Pagination**: `.paginate(pagination_opts)` instead of `.take(n)` returns the usual `PaginationResult`, so
  `usePaginatedQuery` works as for index queries. Pages never duplicate or skip a match; when documents change, only
  those documents move.
- **Invalidation per filter value**: a search with `.eq(Message::channel, c)` reruns only after writes to messages
  whose channel is (or was) `c`; an unfiltered search reruns after any write to the index.

Tokenizers: `tokenizer = "porter"` (English stemming: "run" finds "running"), `tokenizer = "snowball", language =
"…"` (Snowball stemmers for 18 languages, e.g. `"romanian"`, `"french"`, `"german"`, `"spanish"`, `"russian"`: a
custom FTS5 tokenizer, so "chantait" finds "chanter" and "Häusern" finds "Haus"), `tokenizer = "trigram"` (substring
search: "ell" finds "hello"), default `"unicode61"`; `diacritics = "keep"` stops folding `café` into `cafe` (with
folding, a word typed without its accents still finds the accented forms). Changing the tokenizer or its language
rebuilds the index at startup.

Vector indexes are exact by default (sqlite-vec). For large sets, `kind = "hnsw"` adds an in-memory HNSW graph
(approximate, recall@10 ≈ 0.95 on embedding-like data) that follows commits through the write log, so a search sees
everything committed before it; filters are applied after the graph search with oversampling (exactly, when few
documents match), deletes become tombstones that a background rebuild drops, and graphs of 10k+ vectors are saved
next to `data.db` and validated against the table at startup. `VectorSearchOpts::exact()` bypasses the graph. Tune
it per index with `m`, `ef_construction` (changing either rebuilds the graph) and `ef_search`
(`#[vector_index(.., kind = "hnsw", m = 32, ef_construction = 256, ef_search = 64)]`), or per search with
`VectorSearchOpts::ef(n)`.

Which vector search: `ctx.db.vector_search` in a query (or mutation) is always the exact scan at the query's snapshot
(also on an HNSW index), so it is deterministic and reactive, with reruns narrowed per filter value like text search;
use it when the query vector is already in the database or comes from the client, and the index is small enough for
a scan per rerun. `ctx.vector_search` in an action is for embeddings from an API and for large indexes (the graph),
but is not reactive.

Rough numbers (release build, a laptop): 50k documents of 12 words: a one-word query takes 0.35 ms, a broad two-letter
prefix matching every document 80 ms. Vectors of 384 dimensions, k=10: exact, 18 ms at 50k and ~100 ms at 200k;
HNSW at 200k, ~0.4 ms (built in 9–25 s on 16 threads). The todo example has a search box; the dashboard's
`_system/dashboard:search_documents` backs a search box in the Data page (docs/CONTRACT.md §1.8, §4.1).

## Live aggregates

Counts and sums an app shows live (totals per customer, votes per post, unread counts) without scanning documents:
declare them on the table, and every write keeps them up to date in its own transaction.

```rust
#[table(name = "orders")]
#[aggregate(by_customer, group = [customer], sum = [amount])]
#[aggregate(total, sum = [amount])]
pub struct Order { pub customer: String, pub day: String, pub amount: f64 }

#[query]
pub async fn customer_total(ctx: &QueryCtx, customer: String) -> Result<(u64, f64)> {
    let a = ctx.db.aggregate(Order::by_customer).get(|g| g.eq(Order::customer, customer)).await?;
    Ok((a.count, a.sum(Order::amount)))
}
```

`get` reads one group, `sum_over(|g| g.eq(Order::customer, c).gte(Order::day, from))` combines the groups in a range,
`groups(range, Order::Desc, 10)` lists them, `total()` adds up all of them. They are reactive and narrow: a new order
for Ann reruns the queries that read Ann's group, not Bob's, and editing a field that isn't aggregated reruns none.
Integer sums are exact; float sums are running `f64` totals. Adding or changing an aggregate builds it from the
existing documents at startup (docs/CONTRACT.md §1.12). Cost: about 2 µs per aggregate per write; reading a group of
10k documents takes ~16 µs instead of ~10 ms to count and sum them.

## Analytics (OLAP)

For group-bys, joins and time series over many documents, `serve --olap` keeps a columnar copy of the data in an
embedded DuckDB (`<data dir>/olap.duckdb`), a few hundred milliseconds behind, and actions query it with read-only
SQL. It needs the cargo feature `olap` (DuckDB is built from source: a static binary grows from ~25 to ~57 MB, and a
first build takes a few minutes): `ferrix = { version = "…", features = ["olap"] }`.

```rust
#[object]
pub struct DayStats { pub day: String, pub added: i64, pub completed: i64 }

#[action]
pub async fn stats_by_day(ctx: &ActionCtx) -> Result<Vec<DayStats>> {
    ctx.olap.query_as::<DayStats>(
        "SELECT strftime(to_timestamp(_creationTime / 1000), '%Y-%m-%d') AS day,
                count(*) AS added, count(*) FILTER (WHERE done) AS completed
         FROM todos GROUP BY day ORDER BY day",
        (),
    ).await
}
```

- Tables are named like yours, with `_id`, `_creationTime` (ms) and `_ts` (commit ts) first, then the fields with
  types from the Rust struct: numbers, booleans, strings, `Vec<T>` as lists, nested `#[object]` structs as STRUCTs
  (`address.city`), anything else as JSON. `_users`, `_storage`, `_scheduled_jobs` and a few other system tables are
  there too; tables holding secrets (credentials, sessions, settings) never are.
- Actions only, one SELECT per call, `?` parameters. No files, network, extensions or settings; at most 100k rows;
  interrupted after `--olap-timeout` (30s). Errors are user errors: `olap_unavailable`, `olap_query_failed`,
  `olap_timeout`.
- The copy follows the change journal and reports `as_of_ts` with each result. After `ctx.run_mutation`, query
  through `ctx.olap.latest()` to see its writes. It rebuilds itself from a snapshot when there is none yet, after a
  restore, when a table's Rust type changes (that table only), or when it falls more than `--olap-max-lag` (512 MB of
  changes) behind. Backups skip it: it is derived data.
- Cost on writes: the journal (~8% on tiny inserts, as with replication) plus ~2% for the copy following; measured
  47k → 43k one-document inserts/s at 16 writers.
- `--olap-memory 2GB` and `--olap-threads N` bound DuckDB. `GET /api/admin/olap` shows the state, lag, tables and
  size (also on the dashboard Overview and in `/metrics`); `POST /api/admin/olap/rebuild` starts over. The dashboard's
  Analytics page runs SQL as an administrator, and `POST /api/admin/olap/query` (`format` csv or parquet) downloads a
  whole result (up to 10M rows, written from DuckDB's Arrow batches; the sandbox has no file access).
- In tests: `AppOptions { olap: OlapOptions { enabled: true, background: false, ..Default::default() }, .. }`, then
  `t.olap_sync().await?` before querying. The todo example has `todos:stats_by_day`
  (`cargo run -p todo-server --features olap -- serve --dev --olap`). Details: docs/CONTRACT.md §1.11.

## Migrations

At startup, a table whose Rust type changed has its documents validated; if some don't match, the server refuses to
start, listing them. Make the change backward compatible (`Option<T>`, `#[serde(default)]`), or write a migration.
Migrations get the stored JSON, so they work however far the type has moved on:

```rust
#[table(name = "todos")]
pub struct Todo { pub text: String, pub done: bool, pub priority: i64 }   // `priority` is new

#[migration(table = Todo, batch = 100)]             // optionally `name = "0002_priority"`: a stable id
async fn add_priority(ctx: &MutationCtx, doc: serde_json::Value) -> Result<Option<serde_json::Value>> {
    Ok(doc.get("priority").is_none().then(|| json!({ "priority": 0 })))   // a patch, or None to keep it
}

#[migration(table = Todo, name = "0003_rank", after = ["todos:add_priority"])]   // runs once that one is done
async fn rank(ctx: &MutationCtx, doc: serde_json::Value) -> Result<Option<serde_json::Value>> { /* … */ }
```

Return `DocChange::{Keep, Patch, Replace, Delete}` for more, or take a typed `Doc<OldTodo>` when the old shape still
deserializes. A migration runs in batches, each one transaction that also advances its cursor in the `_migrations`
table, so every document is processed exactly once per run, a crash or a failure resumes where it stopped, and
subscriptions update as documents change.

```sh
todo-server migrate --dry-run          # what would change (with examples); writes nothing
todo-server migrate                    # run every migration that isn't done: dependencies first, then name order
todo-server migrate status             # state and progress of each
todo-server serve --migrate            # or: start, then run them in the background
```

While a migration for a table is pending, the server starts even though the table's documents don't match yet: the
table is *migrating*, writes may leave a document invalid only if it already was, and reads of not-yet-migrated
documents fail. When its last migration finishes, the table is validated again in the same transaction; if anything
is still invalid, the next start refuses. `after = [..]` dependencies are checked at startup (unknown names and cycles
are errors); running one migration before its dependencies are done is refused unless forced (`migrate --name x
--force`). The dashboard's Migrations page shows their dependencies and live progress and runs, cancels,
restarts and dry-runs them (`_system/migrations:{list, status, run, cancel}`). Details:
[docs/CONTRACT.md](docs/CONTRACT.md) §1.9. The todo example has one (`0001_trim_todo_text`).

## Binary data

`ferrix::Bytes` holds binary values in documents, args and results. It travels and is stored as
`{"$bytes": "<base64>"}` (TS: `{ $bytes: string }`), can be indexed (ordered byte-wise), and converts with
`bytesToJson(uint8Array)` / `bytesFromJson(value)` from `@ferrix/client`. For files, use file storage instead.

## Dashboard

Every server has an admin dashboard at `/_/` (e.g. http://127.0.0.1:8090/_/), embedded in the binary. Create an
administrator with the `/_/setup#token=…` link that `serve` prints on first start, or with
`todo-server admin create you@example.com`, then sign in.

- **Data**: every table (system tables grouped below), with a live, virtualized document grid (smooth at 10k+ rows;
  pages load as you scroll), filters on index fields, a search box for tables with a search index (live, most
  relevant first), a quick filter over the loaded rows, JSONL/CSV export, a side panel with a JSON editor (insert,
  edit, delete; validated against the table's Rust type), and clickable ids that open the referenced document. The
  schema view lists indexes, tokenizers (with their language), vector index kinds (with HNSW parameters) and the
  table's analytics columns with their SQL types. Secrets (password and token hashes) are never shown,
  and system tables are read-only except the `name`, `image` and `emailVerified` of users.
- **Functions**: every function with its TS signature, a runner (JSON args, optionally *as a user* through a real,
  short-lived, audited session), and latency (p50/p95) and calls/errors charts over the last hour to 7 days, from
  the execution log.
- **Logs**: a live tail of executions (filter by path, kind, outcome; expand for the error, log lines and args).
- **Users**: users, linked providers, sessions (revoked live; impersonation sessions marked), delete, and "open app as
  user": a short-lived, audited session of the user opened in a new tab at an allowed redirect URL.
- **Files**: stored files with thumbnails and previews (images, video, audio, text), download, delete, and upload by
  drag and drop; works with private storage (signed URLs). `Id<"_storage">` values in the data grid link here.
- **Workflows**: workflows by state, an open one's steps (args, outcome, duration, job), and cancel.
- **Scheduler**: crons with their schedule, time zone, last and next run, and "run now" (system crons last);
  scheduled jobs live, by state, with cancel (running actions too), run now and retry.
- **Migrations**: every `#[migration]` in run order with its dependencies, live progress, errors linking to the failing
  document, the schema check,
  run / cancel / restart and dry runs with before/after examples; migrating tables are highlighted.
- **Backups**: create (optionally with `aux.db` and files), download, delete and **restore** backups (a stored one
  or an archive from your computer: checked completely, then applied at the next start; cancel, or "Restart now"
  under systemd/Docker); the target and the `--backup-every` schedule; with `--replicate-to`, the replication status
  (target, lag, last segment and snapshot, generations) and a point-in-time restore picker (a time or a commit ts);
  export a table (JSONL/CSV) and import JSONL or CSV (insert, upsert or replace).
- **Analytics** (with `--olap`): the copy's state and lag, its tables and columns, a SQL editor (one SELECT, the
  sandbox of `ctx.olap`, audited) with the results as a table, and the whole result as a CSV or Parquet download;
  rebuild the copy.
- **Settings**: OAuth providers (GitHub, Google, Discord, Apple, OIDC), SMTP (with a test email), redirect URLs,
  sign-ups, token lifetimes and CORS origins, applied without a restart. Fields fixed by the code, a flag or the
  environment are read-only; secrets can be replaced but are never shown.
- **Audit log**: every dashboard write, settings change, impersonation and function run, live, filterable by
  administrator, action, table, document and time, with field-level diffs.
- **Overview**: commit ts, commits/s, connections, shared queries and reruns, the scheduler queue, database sizes,
  busiest and failing functions.

Ctrl+K (⌘K) opens a command palette (pages, tables, theme, sign out); Alt+1…9 and Alt+0 open the pages in sidebar
order.

It talks to the server over the same WebSocket protocol as apps (admin-only `_system/dashboard:*` functions), so every
view is live, plus `/api/admin/*` for logs, stats, backups, export/import and impersonation (docs/CONTRACT.md §4). The built assets are committed, so
`cargo build` needs no JS toolchain; after changing `packages/dashboard` (or the client packages it bundles), run
`bun run build:dashboard` and commit `crates/ferrix/dashboard-dist` (`cargo test` fails while it is stale). For UI
work, `cd packages/dashboard && bun run dev` serves it on :5174 with `/api` proxied to :8090. Build without it with
`default-features = false` on the `ferrix` dependency.

## Deploying

A Ferrix app is one binary plus one data directory (`--dir`, default `fx_data`: `data.db`, `aux.db` with the
execution log, `storage/` for files, `auth_secret.key`, `backups/`).

**Build your app with these SQLite flags.** Apps created with `ferrix new` have them in `.cargo/config.toml` (so does
this repository); for an app created otherwise, put this in its `.cargo/config.toml`. Without it, rusqlite's bundled
SQLite shares one page cache behind a global mutex, so concurrent queries don't run in parallel (measured: 8 parallel
readers ran no faster than one; with the flags they scale to ~4x the single-reader throughput and beyond). Ferrix logs
a warning at startup when the flag is missing. It has to be the app's own config: Cargo gives a dependency no way to
set another crate's build flags, and `libsqlite3-sys` reads them only from this variable. (Memory statistics, the
second flag, are also turned off at runtime; the page cache can only be changed at build time.)

```toml
[env]
LIBSQLITE3_FLAGS = "-USQLITE_ENABLE_MEMORY_MANAGEMENT -DSQLITE_DEFAULT_MEMSTATUS=0"
```

**Serve the frontend from the binary.** `.static_dir("./web/dist")` reads a directory at runtime;
`.embed_static::<Assets>()` embeds it at compile time, so the whole app ships as one file:

```rust
#[derive(ferrix::rust_embed::Embed)]
#[folder = "../web/dist/"]                        // relative to the crate
#[crate_path = "ferrix::rust_embed"]
struct Assets;

ferrix::Ferrix::new()
    .embed_static::<Assets>()
    .routes(axum::Router::new().route("/webhooks/stripe", post(stripe)))   // your own routes, if any
    .run()
```

Unknown paths get `index.html` (client-side routing) except under `/api` and `/_`, which static files never shadow.
HTML is `no-cache`, hashed assets (`assets/*`, `name-<hash>.js`) are cached for a year, and `<file>.br` / `.gz`
next to a file are served to clients that accept them. The todo example embeds `examples/todo/web/dist` when it
exists.

**Durability.** By default SQLite runs in WAL mode with `synchronous = NORMAL`: a crash of the process loses nothing,
but a power loss or OS crash may lose the last commits. `--fsync-commits` (`AppOptions::fsync_commits`) makes every
commit wait for the disk; group commit shares one fsync across the mutations waiting at that moment, so it costs most
when writes come one at a time. WAL checkpoints run on a background thread, not inside commits. Measured on an NVMe
SSD (btrfs, encrypted), one-insert mutations: 8.9k/s with 1 writer, 16.2k/s with 16 and 18.7k/s with 64 by default
(p99 0.2 / 3.5 / 11 ms); 1.1k/s, 7.3k/s and 12.5k/s with `--fsync-commits`.

**Release binary.** `scripts/release.sh` builds the web app and a static `x86_64-unknown-linux-musl` binary of
`todo-server` into `dist/` (everything linked in: SQLite, FTS5, sqlite-vec, TLS, zstd; about 20 MB). C code is
cross-compiled with `zig cc` through cargo-zigbuild, fetched with mise if not installed. For your app:
`PACKAGE=my-server WEB=web scripts/release.sh`; `FEATURES=olap` adds the analytics copy (DuckDB; about 57 MB).

**systemd.** [`deploy/ferrix.service`](deploy/ferrix.service) runs the binary as a dynamic unprivileged user with
its data in `/var/lib/ferrix`, behind a reverse proxy or with `--https`; it stops gracefully on SIGTERM, and restarts
on the dashboard's "Restart now" (exit code 75, `FERRIX_RESTART_EXIT_CODE`).

**Docker.** The [`Dockerfile`](Dockerfile) runs `scripts/release.sh` and copies the binary into a `FROM scratch`
image with CA certificates, running as an unprivileged user with the data in the `/data` volume. To build the image
locally you need a Docker daemon you can use (on Linux: membership in the `docker` group, or rootless Docker/Podman;
`podman build` works with the same file) and network access for the base images, Zig and the crates; the first build
takes several minutes (later ones reuse the BuildKit caches). The build context includes `.cargo/config.toml`, which
sets the bundled SQLite's flags: don't pass `LIBSQLITE3_FLAGS` to the build.

```sh
docker build -t todo-server .                     # --build-arg PACKAGE=my-server --build-arg WEB=web for yours
docker run -p 8090:8090 -v todo-data:/data todo-server
docker run -p 8090:8090 -v todo-data:/data -e FERRIX_RESTART_EXIT_CODE=75 --restart unless-stopped todo-server
                                                  # lets "Restart now" apply a restore staged in the dashboard
docker run --rm todo-server functions             # any CLI command
```

**CI.** [`.github/workflows/release.yml`](.github/workflows/release.yml) runs every gate of [Tests](#tests) (end-to-end
tests included, in Chromium) and, in a job of its own, the tests with the `olap` feature; then builds the static musl
binaries with `scripts/release.sh`, `todo-server` and `todo-server-olap` (artifacts; attached to the GitHub release
for `v*` tags) and the Docker image, which it smoke-tests (`functions`, `/api/health` with a
volume) and pushes to `ghcr.io/<owner>/todo-server` for `v*` tags.

**Logs.** `serve` writes JSON lines when stdout isn't a terminal (systemd, Docker): `timestamp`, `level`, `target`,
`fields` and the span context, one object per line, for Loki, Vector or `jq`. `--log-format pretty|compact|json` (or
`FERRIX_LOG_FORMAT`) chooses explicitly; `RUST_LOG` filters as usual.

**TLS.** Either terminate TLS in a reverse proxy (pass `--public-url https://…`), or let the server do it:
`serve --https example.com --acme-email you@example.com` gets and renews Let's Encrypt certificates (TLS-ALPN-01 on
port 443, cached in `<dir>/acme`; try `--acme-staging` first) and redirects port 80 to HTTPS. `--tls-cert cert.pem
--tls-key key.pem` uses your own certificate, re-read every 30 s (`--tls-reload-every`): renewed files are picked up
without a restart or dropped connections, and a bad pair keeps the current certificate (logged). Addresses: `--https-addr` (default `0.0.0.0:443`), `--http-redirect`
(default `0.0.0.0:80` with `--https`; `off`).

**Backups.** `todo-server backup [--out file] [--with-aux] [--with-files] [--include-secret]` takes a consistent
snapshot (`VACUUM INTO`) while the server runs, without blocking writes, into a `.tar.zst` archive (`data.db`, plus
the execution log, local file blobs and `auth_secret.key` when asked; the key only with `--include-secret`, since it
can forge sessions). Keep a copy of `auth_secret.key` somewhere safe either way: it also encrypts the secrets stored
in `_settings`, so a restore elsewhere without it signs everyone out and those settings must be entered again. `serve --backup-every 6h --backup-keep 7 [--backup-to s3://bucket/prefix|dir]` takes them on a
schedule (the cron `ferrix-backup`, visible on the Scheduler page) and deletes old scheduled ones; admins can list,
create, download and delete backups over `/api/admin/backups` (docs/CONTRACT.md §4.2). `todo-server restore
<archive|s3://…>` is offline: it refuses while a server holds the data directory, checks the archive (checksums,
SQLite integrity, schema, indexes) and only then swaps the database, keeping the old files in
`<dir>/pre-restore-<time>/`.

**Restoring from the dashboard** (Backups, "Restore", or an archive uploaded from your computer) never swaps the
database under the running server. The backup is checked there and then, exactly like `restore` does; a bad archive is
refused with the reason. A good one is *staged* in `<dir>/restore-staged/` with a `pending-restore.json` marker, and
the next start of `serve` applies it before opening the database (the current files go to `pre-restore-<time>/`).
Until then the dashboard shows it as pending and can cancel it (or `todo-server restore --cancel` offline). With
`serve --restart-exit-code 75` (set in `deploy/ferrix.service`; for Docker add a restart policy), the dashboard also
offers "Restart now": a graceful shutdown and an exit with that code, for systemd or Docker to start the server again.
Without it, restart the server yourself. A staged restore never restores `auth_secret.key` (only `restore` does), and
it replaces everything else, administrators and sessions included: whoever is signed in may have to sign in again.

**Continuous replication and point-in-time restore.** `serve --replicate-to s3://bucket/prefix` (or a directory)
loses at most about a second of commits and can restore any moment of the last week:

```sh
todo-server serve --replicate-to s3://bucket/replica [--replicate-every 1s] [--snapshot-every 24h] [--replicate-keep 7d]
todo-server restore --from s3://bucket/replica --list                          # generations and restorable ranges
todo-server restore --from s3://bucket/replica --to-time 2026-10-09T12:00:00Z  # or --to-ts N; default: the newest
```

Every commit writes its document changes to a journal table in `data.db` *in the same SQLite transaction* (so the
journal never misses a commit nor holds a rolled-back one); every `--replicate-every` the new entries are shipped as a
checksummed, compressed segment named by its ts range, then deleted locally. A snapshot (an ordinary backup archive)
is taken when replication starts and every `--snapshot-every`; snapshots and segments older than `--replicate-keep`
are deleted, except what the oldest restorable point still needs. A restore takes the newest snapshot before the
point and replays the journal up to it through the normal write path (indexes, search and vector indexes are
rebuilt), then checks it like any restore. From the dashboard (Backups, "Restore to this point") it is staged and
applied at the next start, like an archive. If the target is unreachable, commits pile up locally (lag in
`GET /api/admin/replication`, `/metrics` and the dashboard) and are shipped when it is back; past
`--replicate-max-pending` (1 GB) the local journal is dropped and replication starts over with a new snapshot. A
restored database, or one that was written without replication (e.g. `import` offline), starts a new *generation*
next to the old ones, so history is never overwritten. The journal holds documents, not file contents, but **deleted
files' contents are kept as long as restores reach** (`--replicate-keep`; `--storage-keep-deleted` to choose another
window): the orphan sweep removes them only after that, so a restore to before a deletion finds the file. They are gone
for everything else at once (their document is deleted). Restores leave a local storage directory in place (S3 files
stay in their bucket). Keep a copy of `auth_secret.key`. The cost is one extra row per commit: about 5% fewer commits
per second in a sequential benchmark (docs/CONTRACT.md §4.4).

**Litestream** works too, as an alternative: `data.db` is a normal SQLite database in WAL mode, so
[Litestream](https://litestream.io) can replicate its pages next to the server (`litestream replicate
fx_data/data.db s3://bucket/data.db`; `litestream restore` into an empty data directory before starting the server).
Replicate `data.db` only (`aux.db` is the execution log), with the same caveats for blobs and `auth_secret.key`.
Archive backups, `--replicate-to` and Litestream can be combined.

**Export and import.** `todo-server export [--tables a,b] [--format jsonl|csv] [--out dir]` writes tables from one
consistent snapshot; `todo-server import <dir|file.jsonl|file.csv> [--format jsonl|csv] [--mode
insert|upsert|replace]` loads JSONL or CSV through the normal write path in batches (validated, indexed, ids and
creation times kept), offline. CSV is read as the export writes it (a header row, RFC 4180), and each cell is typed
with the table's Rust type: `42` is a number in a number field and text in a `String` field, JSON cells become arrays
and objects, an empty cell is `null` (or `""` for a non-optional string). Admins can stream one table with
`GET /api/admin/export/{table}?format=csv` and import into a running server with
`POST /api/admin/import/{table}?format=csv`.

**Metrics.** `serve --metrics [--metrics-token T]` exposes Prometheus metrics at `/metrics`: requests by route and
status, function executions and durations, commit ts and rate, sockets, subscriptions, reruns, database file sizes,
scheduler queue depth, dropped log entries and, with `--replicate-to` and `--olap`, the replication and analytics
lag.

## Things to know

- **Limits per connection and per run.** A socket may have 1024 live queries and 64 running mutations/actions;
  queries time out after 30 s (`--query-timeout`), mutations after 5 s; a client silent for 45 s (it answers pings)
  is disconnected. They bound the server's memory whatever a client does (docs/PROTOCOL.md, Limits).
- **Query results are cached while nothing they read changes.** Every subscriber of a query shares one result; after
  the last one leaves it stays cached (up to 30 minutes, 64 MiB in all: `--query-cache-ttl`, `--query-cache-size`)
  and answers the next subscriber, HTTP query or `ctx.run_query` without running, until a write touches what it read.
  Repeated HTTP queries are cached too. A query that reads `ctx.now()` is never cached. The Overview page and
  `/metrics` show the hit rate.

- **Optimistic updates are replayed** on top of every server change until the mutation's result arrives. Create ids
  and timestamps outside the update closure (once per call), not inside it:
  ```ts
  const id = `optimistic:${crypto.randomUUID()}` as Id<"messages">, now = Date.now();
  send.withOptimisticUpdate((store, args) => insertAtTop({ …, item: { _id: id, _creationTime: now, …args } }));
  ```
- **Scheduled functions (and crons) run without an identity**, whoever scheduled them: pass the user as an argument to
  an internal function, e.g. `ctx.scheduler.run_after::<deliver_reminder>(delay, (channel, user, text))`.
- **`ctx.now()` in a query is the snapshot's time, not a reactive clock.** A query reruns when its data changes, not
  when time passes. For expiry, store the state and flip it with a scheduled mutation (`run_at`); format "5 minutes
  ago" in the client.
- **A query's result must depend only on its arguments and the data it reads.** In `--dev` and `TestApp`, ferrix
  reruns each query in the same snapshot now and then and logs a warning (also sent as a log line) if the result
  differs: `SystemTime::now()`, random numbers or `HashMap` order. Use `ctx.now()`, and a `BTreeMap` or a sort.
- **Invalidation is coarse**: any write to a document reruns the queries that read it, even when nothing changed.
  Skip no-op writes, keep fast-changing fields (presence, typing, counters) in small tables of their own, read narrow
  ranges (`eq` prefixes, `_creationTime` bounds, `take(n)`), and count with `.count()`.
- **Tests** (`ferrix::testing::TestApp`, in dev mode): `t.create_user("ann@example.com", Some("password"))` makes a
  user, `t.as_user(id).query::<list>(args)` calls as them, `t.advance_time(d)` runs due jobs.

More in [docs/CONTRACT.md](docs/CONTRACT.md) §1.10.

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
cargo test -p ferrix-cli --test new -- --ignored                 # `ferrix new` apps build, typecheck; no SQLite warning
cargo test -p ferrix --features olap --lib --test it             # with the analytics copy (builds DuckDB: minutes)
FUZZ_SCALE=20 cargo test --release -p ferrix --test it fuzz     # longer randomized protocol/WebSocket/HTTP fuzzing
bun run typecheck && bun run test                               # TS packages (dashboard included)
bun run check:dashboard                                         # the committed dashboard build matches its sources
(cd packages/dashboard && bun e2e/dashboard.ts)                 # after `cargo build -p todo-server`; starts its own servers
cd examples/todo/web && bun e2e/two-tabs.ts                     # needs the backend and Vite running
bun e2e/reconnect.ts ../../../target/debug/todo-server <data dir>  # Vite running, backend NOT running
```

Benchmarks (release builds; ignored by default): `cargo test --release -p ferrix --test it bench_ -- --ignored
--nocapture` runs the subscription fan-out, parallel-read and write benchmarks (`bench_writes`: throughput under
concurrent writers, group commit).

`bun e2e/dashboard.ts [binary] [screenshot dir]` takes the todo-server binary (default `target/debug/todo-server`)
and optionally saves screenshots of each page (named `$SCREENSHOT_PREFIX<page>.png`, default prefix `dashboard-`).
