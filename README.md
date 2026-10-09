# Ferrix

A single-binary reactive backend in Rust, deployed as one file.
You write typed `#[query]` / `#[mutation]` / `#[action]` functions in Rust. Clients subscribe with `useQuery` over a
WebSocket and get updated results automatically whenever a write affects them.

Status: Phases 0–8 are built (core, shared reactive subscriptions, clients, the admin dashboard, auth, the scheduler
and crons, file storage, and full-text and vector search); see [PLAN.md](PLAN.md) §10 for what each still lacks.
Phase 9 (operations) is built: data migrations (`#[migration]`), backups, export and import, metrics, TLS, frontend
serving and release builds (see [Deploying](#deploying)).

## Layout

| Path | What |
|---|---|
| `crates/ferrix` | The runtime: SQLite storage, query engine, read sets, write log, sync worker, axum server, CLI, codegen, `TestApp`. |
| `crates/ferrix-macros` | `#[table]`, `#[query]`, `#[mutation]`, `#[action]`, `#[internal_*]`, `#[migration]`. |
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
on every use. Signing out, revoking a session from the dashboard, deleting a user or reusing a rotated refresh token
ends the sockets of that session **at once** (`AuthError`, then the client continues signed out), not when the access
token expires. Email goes through `--smtp-url` (printed to the log in `--dev`). With no admin yet, `serve` prints a
one-time `/_/setup#token=…` link; `admin create <email>` works too.

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
headers that keep uploads from running script on your origin. Add `?thumb=200x200` (and `&fit=cover`) to an image's
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

## Search

Full-text search uses SQLite FTS5 and vector search uses sqlite-vec, both compiled into the binary. The indexes are
updated in the mutation's transaction, so search results always agree with document reads, and search queries are
reactive like any other query (docs/CONTRACT.md §1.8).

```rust
#[table(name = "messages")]
#[search_index(search_body, search = body, filter = [channel], tokenizer = "porter")]
#[vector_index(by_embedding, field = embedding, dims = 1536, filter = [channel], kind = "hnsw")]
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
nothing; a query returns at most 1024 documents. Relevance is BM25 per document (term frequency and length, no
corpus statistics), so a document's rank depends only on itself and the query. That makes two things exact:

- **Pagination**: `.paginate(pagination_opts)` instead of `.take(n)` returns the usual `PaginationResult`, so
  `usePaginatedQuery` works as for index queries. Pages never duplicate or skip a match; when documents change, only
  those documents move.
- **Invalidation per filter value**: a search with `.eq(Message::channel, c)` reruns only after writes to messages
  whose channel is (or was) `c`; an unfiltered search reruns after any write to the index.

Tokenizers: `tokenizer = "porter"` (English stemming: "run" finds "running"), `tokenizer = "trigram"` (substring
search: "ell" finds "hello"), default `"unicode61"`; `diacritics = "keep"` stops folding `café` into `cafe`. Changing
the tokenizer rebuilds the index at startup.

Vector indexes are exact by default (sqlite-vec). For large sets, `kind = "hnsw"` adds an in-memory HNSW graph
(approximate, recall@10 ≈ 0.95 on embedding-like data) that follows commits through the write log, so a search sees
everything committed before it; filters are applied after the graph search with oversampling (exactly, when few
documents match), deletes become tombstones that a background rebuild drops, and graphs of 10k+ vectors are saved
next to `data.db` and validated against the table at startup. `VectorSearchOpts::exact()` bypasses the graph.

Rough numbers (release build, a laptop): 50k documents of 12 words: a one-word query takes 0.35 ms, a broad two-letter
prefix matching every document 80 ms. Vectors of 384 dimensions, k=10: exact, 18 ms at 50k and ~100 ms at 200k;
HNSW at 200k, ~0.4 ms (built in 9–25 s on 16 threads). The todo example has a search box; the dashboard's
`_system/dashboard:search_documents` backs a search box in the Data page (docs/CONTRACT.md §1.8, §4.1).

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
```

Return `DocChange::{Keep, Patch, Replace, Delete}` for more, or take a typed `Doc<OldTodo>` when the old shape still
deserializes. A migration runs in batches, each one transaction that also advances its cursor in the `_migrations`
table, so every document is processed exactly once per run, a crash or a failure resumes where it stopped, and
subscriptions update as documents change.

```sh
todo-server migrate --dry-run          # what would change (with examples); writes nothing
todo-server migrate                    # run every migration that isn't done, in name order
todo-server migrate status             # state and progress of each
todo-server serve --migrate            # or: start, then run them in the background
```

While a migration for a table is pending, the server starts even though the table's documents don't match yet: the
table is *migrating*, writes may leave a document invalid only if it already was, and reads of not-yet-migrated
documents fail. When its last migration finishes, the table is validated again in the same transaction; if anything
is still invalid, the next start refuses. The dashboard backend has admin functions for it
(`_system/migrations:{list, status, run, cancel}`). Details: [docs/CONTRACT.md](docs/CONTRACT.md) §1.9.

## Binary data

`ferrix::Bytes` holds binary values in documents, args and results. It travels and is stored as
`{"$bytes": "<base64>"}` (TS: `{ $bytes: string }`), can be indexed (ordered byte-wise), and converts with
`bytesToJson(uint8Array)` / `bytesFromJson(value)` from `@ferrix/client`. For files, use file storage instead.

## Dashboard

Every server has an admin dashboard at `/_/` (e.g. http://127.0.0.1:8090/_/), embedded in the binary. Create an
administrator with the `/_/setup#token=…` link that `serve` prints on first start, or with
`todo-server admin create you@example.com`, then sign in.

- **Data**: every table (system tables grouped below), with a live, paginated document grid, filters on index
  fields, a side panel with a JSON editor (insert, edit, delete; validated against the table's Rust type), and
  clickable ids that open the referenced document. Secrets (password and token hashes) are never shown, and system
  tables are read-only except the `name`, `image` and `emailVerified` of users.
- **Functions**: every function with its TS signature, and a runner: JSON args, optionally *as a user* (through a
  real, short-lived, audited session), showing the result, log lines and duration.
- **Logs**: a live tail of executions (filter by path, kind, outcome; expand for the error, log lines and args).
- **Users**: users, linked providers, sessions (revoke), delete.
- **Files**: stored files with previews (images, video, audio, text), download, delete, and upload by drag and drop.
  `Id<"_storage">` values in the data grid link here.
- **Scheduler**: crons with their schedule, last and next run; scheduled jobs live, by state, with cancel, run now
  and retry.
- **Overview**: commit ts, connections and subscriptions, database sizes, busiest and failing functions.

Runtime settings, the audit log and "open the app as this user" already have their admin-only backend
(`_system/settings:*`, `_system/audit:list`, `POST /api/admin/impersonate`; docs/CONTRACT.md §4.1.4, §4.1.5, §4.2);
their pages are still to come.

It talks to the server over the same WebSocket protocol as apps (admin-only `_system/dashboard:*` functions), so every
view is live, plus `/api/admin/*` for logs and stats (docs/CONTRACT.md §4). The built assets are committed, so
`cargo build` needs no JS toolchain; after changing `packages/dashboard` (or the client packages it bundles), run
`bun run build:dashboard` and commit `crates/ferrix/dashboard-dist` (`cargo test` fails while it is stale). For UI
work, `cd packages/dashboard && bun run dev` serves it on :5174 with `/api` proxied to :8090. Build without it with
`default-features = false` on the `ferrix` dependency.

## Deploying

A Ferrix app is one binary plus one data directory (`--dir`, default `fx_data`: `data.db`, `aux.db` with the
execution log, `storage/` for files, `auth_secret.key`, `backups/`).

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

**Release binary.** `scripts/release.sh` builds the web app and a static `x86_64-unknown-linux-musl` binary of
`todo-server` into `dist/` (everything linked in: SQLite, FTS5, sqlite-vec, TLS, zstd; about 20 MB). C code is
cross-compiled with `zig cc` through cargo-zigbuild, fetched with mise if not installed. For your app:
`PACKAGE=my-server WEB=web scripts/release.sh`.

**systemd.** [`deploy/ferrix.service`](deploy/ferrix.service) runs the binary as a dynamic unprivileged user with
its data in `/var/lib/ferrix`, behind a reverse proxy or with `--https`; it stops gracefully on SIGTERM.

**Docker.** The [`Dockerfile`](Dockerfile) runs `scripts/release.sh` and copies the binary into a `FROM scratch`
image with CA certificates, running as an unprivileged user with the data in the `/data` volume:

```sh
docker build -t todo-server .                     # --build-arg PACKAGE=my-server --build-arg WEB=web for yours
docker run -p 8090:8090 -v todo-data:/data todo-server
```

**TLS.** Either terminate TLS in a reverse proxy (pass `--public-url https://…`), or let the server do it:
`serve --https example.com --acme-email you@example.com` gets and renews Let's Encrypt certificates (TLS-ALPN-01 on
port 443, cached in `<dir>/acme`; try `--acme-staging` first) and redirects port 80 to HTTPS. `--tls-cert cert.pem
--tls-key key.pem` uses your own certificate. Addresses: `--https-addr` (default `0.0.0.0:443`), `--http-redirect`
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

**Litestream.** `data.db` is a normal SQLite database in WAL mode, so [Litestream](https://litestream.io) can
replicate it continuously next to the server (`litestream replicate fx_data/data.db s3://bucket/data.db`); restore
with `litestream restore` into an empty data directory before starting the server. Replicate `data.db` only (`aux.db`
is the execution log), and keep `auth_secret.key` and local file blobs (`storage/`) backed up separately (or keep
files in S3 with `--storage s3://…`). Archive backups and Litestream can be used together.

**Export and import.** `todo-server export [--tables a,b] [--format jsonl|csv] [--out dir]` writes tables from one
consistent snapshot (CSV is export-only); `todo-server import <dir|file.jsonl> [--mode insert|upsert|replace]` loads
JSONL through the normal write path in batches (validated, indexed, ids and creation times kept), offline. Admins can
stream one table with `GET /api/admin/export/{table}?format=csv` and import into a running server with
`POST /api/admin/import/{table}`.

**Metrics.** `serve --metrics [--metrics-token T]` exposes Prometheus metrics at `/metrics`: requests by route and
status, function executions and durations, commit ts and rate, sockets, subscriptions, reruns, database file sizes,
scheduler queue depth and dropped log entries.

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
