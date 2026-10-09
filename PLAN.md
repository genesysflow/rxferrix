# Ferrix — Project Plan

> A single-binary backend, written in Rust, with
> typed `query` / `mutation` / `action` functions, reactive subscriptions over WebSockets, and an
> embedded admin dashboard where all data is visible and editable.

**Status:** planning · **Audience:** my own apps (tight scope, rough edges acceptable) · **Date:** 2026-10-09

---

## 1. Decisions made

| Area | Decision |
|---|---|
| Backend functions | **Rust, framework mode.** Apps depend on the `ferrix` crate, write `#[query]` / `#[mutation]` / `#[action]` functions, and `main()` builds one binary. No JS runtime is embedded. |
| Data model | **Documents plus a schema.** Tables of typed documents, secondary indexes, typed `Id<T>`, stored in SQLite. |
| Schema source of truth | **Rust structs** (`#[derive(Table)]`). The dashboard can browse and edit data but cannot change the schema. |
| Transport | **WebSocket** sync protocol for queries, mutations, and actions, plus an HTTP JSON API fallback for SSR, curl, and webhooks. |
| Clients | Framework-agnostic **TS core**, plus **React** and **Svelte 5** bindings. A codegen step produces a typed `api` object. |
| Built-in features | Auth, file storage, scheduler and cron, text search, and vector search. |
| Dashboard | Svelte SPA embedded in the binary and served at `/_/`. It uses the same realtime protocol, so it is live too. |

### Non-goals (for now)
- Multi-node or horizontal scaling. One process, one SQLite file.
- Multi-tenancy or a hosted cloud.
- Running user code in JS/TS, WASM, or a sandbox.
- Raw SQL access from user functions. Functions go through the document query API so reactivity stays correct.
- Offline-first, local-first sync. Optimistic updates are in scope; offline persistence is not.
- Declarative access rules. Authorization lives in function code.

---

## 2. What using it looks like (the target DX)

This section is the spec. Everything below exists to make this code work.

### 2.1 Backend (`my-app/server/src/`)

```rust
// messages.rs
use ferrix::prelude::*;

#[derive(Table, Serialize, Deserialize, Type, Clone)]
#[table(name = "messages")]
#[index(by_channel = [channel, _creation_time])]
#[search_index(search_body, search = body, filter = [channel])]
pub struct Message {
    pub channel: Id<Channel>,
    pub author: Id<User>,
    pub body: String,
    pub edited: Option<bool>,
}

/// Arguments become function parameters. The macro generates a `ListArgs`
/// struct that derives Deserialize + specta::Type.
#[query]
pub async fn list(ctx: &QueryCtx, channel: Id<Channel>) -> Result<Vec<Doc<Message>>> {
    ctx.db
        .query::<Message>()
        .with_index(Message::by_channel, |q| q.eq(Message::channel, channel))
        .order(Desc)
        .take(50)
        .await
}

#[mutation]
pub async fn send(ctx: &MutationCtx, channel: Id<Channel>, body: String) -> Result<Id<Message>> {
    let user = ctx.auth.require_user().await?;
    if body.trim().is_empty() {
        return Err(UserError::new("empty_body", "Message cannot be empty").into());
    }
    let id = ctx.db.insert(Message { channel, author: user.id, body, edited: None }).await?;
    ctx.scheduler.run_after::<crate::ai::moderate>(Duration::ZERO, (id,)).await?;
    Ok(id)
}

// ai.rs
#[action]
pub async fn summarize(ctx: &ActionCtx, channel: Id<Channel>) -> Result<String> {
    let msgs = ctx.run_query::<crate::messages::list>((channel,)).await?;
    let summary = call_llm(&msgs).await?;          // actions may do any I/O
    ctx.run_mutation::<crate::channels::set_summary>((channel, summary.clone())).await?;
    Ok(summary)
}
```

```rust
// main.rs
fn main() -> ferrix::Result<()> {
    Ferrix::new()
        .tables(tables![Message, Channel, User])          // or auto-discovered (see §4.2)
        .cron::<crate::files::cleanup>("cleanup-old-files", "0 0 3 * * *", ())
        .auth(Auth::password().oauth(Google::from_env()).oauth(GitHub::from_env()))
        .static_dir("../web/dist")                        // optional: serve the frontend too
        .routes(axum::Router::new().route("/webhooks/stripe", post(stripe_webhook)))
        .run() // CLI: serve | serve --dev | codegen | admin create | backup | export | import
}
```

### 2.2 React client

```tsx
import { useQuery, useMutation, useAction } from "@ferrix/react";
import { api } from "./ferrix/api"; // generated

function Channel({ channel }: { channel: Id<"channels"> }) {
  const messages = useQuery(api.messages.list, { channel }); // undefined while loading
  const send = useMutation(api.messages.send).withOptimisticUpdate((store, args) => {
    const cur = store.getQuery(api.messages.list, { channel: args.channel });
    if (cur) store.setQuery(api.messages.list, { channel: args.channel }, [optimisticMsg(args), ...cur]);
  });
  const summarize = useAction(api.ai.summarize);
  // ...
}
```

### 2.3 Svelte 5 client

```svelte
<script lang="ts">
  import { useQuery, useMutation } from "@ferrix/svelte";
  import { api } from "$lib/ferrix/api";
  let { channel } = $props();

  const messages = useQuery(api.messages.list, () => ({ channel })); // reactive args
  const send = useMutation(api.messages.send);
</script>

{#if messages.isLoading}Loading…
{:else if messages.error}{messages.error.message}
{:else}
  {#each messages.data as m (m._id)}<p>{m.body}</p>{/each}
{/if}
```

---

## 3. Architecture

```
                         ┌──────────────────────── ferrix binary (your app) ────────────────────────┐
 Browser / app           │                                                                           │
 ┌───────────────┐  WS   │  axum ──► Sync worker (per connection) ──► Function runtime               │
 │ @ferrix/react │◄─────►│   │         query set, auth, versions       ├─ QueryCtx   (read-only, snapshot)
 │ @ferrix/svelte│       │   │                                         ├─ MutationCtx(single writer tx)
 │ @ferrix/client│  HTTP │   ├─ /api/{query,mutation,action}           └─ ActionCtx  (any I/O, no tx)
 └───────────────┘◄─────►│   ├─ /api/auth/*        ┌──────────────┐          │ commit(ts, writes)
                         │   ├─ /api/storage/*     │ Subscription │◄── write log (recent commits)
 Admin browser           │   ├─ /_/  (dashboard)   │ manager      │──► invalidate ─► rerun ─► push Transition
 ┌───────────────┐       │   └─ user routes        └──────────────┘
 │ dashboard SPA │◄─────►│                                                                           │
 └───────────────┘       │  Scheduler + cron ── Auth ── File storage (object_store) ── Search (FTS5, sqlite-vec)
                         │                                                                           │
                         │  SQLite (WAL): data.db  ·  aux.db (logs, metrics)  ·  storage/ dir or S3   │
                         └───────────────────────────────────────────────────────────────────────────┘
```

### 3.1 Repository layout

Start with few crates (proc macros need their own crate) and split later only if compile times call for it.

```
ferrix/
├── Cargo.toml                    # workspace
├── crates/
│   ├── ferrix/                   # main lib: modules below, re-exports, Ferrix builder, CLI (clap)
│   │   └── src/{value,id,schema,keys,db,query,runtime,sync,protocol,auth,storage,
│   │            scheduler,search,admin,server,codegen,testing}/
│   └── ferrix-macros/            # #[derive(Table)], #[query], #[mutation], #[action], #[http_action]
├── packages/                     # pnpm workspace
│   ├── client/                   # @ferrix/client (TS core)
│   ├── react/                    # @ferrix/react
│   ├── svelte/                   # @ferrix/svelte
│   └── dashboard/                # Svelte SPA → built into crates/ferrix/dashboard-dist (rust-embed)
├── examples/
│   ├── chat-react/               # server/ (Rust) + web/ (React)
│   └── todo-svelte/              # server/ (Rust) + web/ (Svelte)
└── PLAN.md
```

### 3.2 Key dependencies

| Purpose | Crate / package |
|---|---|
| Async runtime, HTTP, WS | `tokio`, `axum` (with `ws`), `tower-http` (CORS, compression, tracing) |
| Storage | `rusqlite` (`bundled`, includes FTS5), a small read pool written by hand |
| Vector search | `sqlite-vec` (statically linked extension) |
| Serialization | `serde`, `serde_json` |
| Rust → TS types | `specta` + `specta-typescript` (fallback: `ts-rs`) |
| Function auto-registration | `linkme` distributed slices (or `inventory`) |
| Auth | `argon2`, `jsonwebtoken`, `oauth2`, `lettre` (SMTP) |
| Files | `object_store` (one API for the local filesystem and S3) |
| Cron | `cron` (or `croner`) |
| IDs, hashing | `rand`, `data-encoding` (base32), `blake3` |
| Embedding assets | `rust-embed` |
| Observability | `tracing`, `tracing-subscriber` |
| TLS (optional) | `rustls-acme` (automatic Let's Encrypt, `--https`) |
| Testing | `proptest`, `tokio-tungstenite`, `insta` |
| JS side | TypeScript, `vitest`, Playwright, Svelte 5, React 18/19 |

---

## 4. Core design

### 4.1 Values, IDs, documents

- **`Value` enum** (internal): `Null | Int64 | Float64 | Bool | String | Bytes | Array | Object`. This gives a total order for index keys:
  `Null < Int64 < Float64 < Bool < String < Bytes < Array < Object`. A missing field is ordered as `Undefined`, below `Null`.
- **`Id<T>`**: 4-byte table number plus 14 random bytes, encoded as base32 text. The table can be decoded from the ID, so `ctx.db.get(id)` needs no table name and IDs can be validated as belonging to the right table. The string form appears in TS as `Id<"messages">` (a branded string).
- **System fields**: every `Doc<T>` is `{ _id: Id<T>, _creation_time: f64 (ms), ...T }`. These use `#[serde(flatten)]`.
- **Wire and storage encoding**: JSON. Int64 values outside the safe JS range and bytes use a small tagged form (`{"$int":"…"}`, `{"$bytes":"base64"}`). Types are always known from the schema, so this rarely shows up. (Built: `ferrix::Bytes` is `{"$bytes": "<base64>"}`, TS `{ $bytes: string }` with `bytesToJson` / `bytesFromJson` in `@ferrix/client`, and has its own key tag; `$int` is not built: integers are plain JSON numbers.)

### 4.2 Schema from Rust structs

`#[derive(Table)]` generates:
- A `TableSchema` descriptor: name, fields with their types (from `specta::Type`), indexes, search indexes, and vector indexes.
- **Typed field handles** such as `Message::channel: Field<Message, Id<Channel>>`. This makes `q.eq(Message::channel, x)` type-check at compile time, so field names are not stringly typed.
- **Typed index handles** such as `Message::by_channel: Index<Message, (Id<Channel>, f64)>`. Range builders can then only constrain index fields as a prefix and in order.
- Registration into a `linkme` distributed slice, so `tables![]` is optional. Functions register the same way. Function paths come from `module_path!()`: `my_app::messages::list` becomes `"messages:list"`.

**Startup reconciliation** (the code-defined schema is compared with the `_schema` system table):
- New table: register it and assign a table number.
- New index: backfill synchronously at startup (fine at this scale) and log progress.
- Removed index: delete its entries.
- Changed fields: validate every existing document against the new schema. If any fail, **refuse to start** and print the failing doc IDs and a hint (including a migration snippet), unless migrations for the table are pending (below). `ferrix serve --dev` offers to open them in the dashboard (not built yet).
- Migrations (built, docs/CONTRACT.md §1.9): `#[migration(table = T, batch = N)]` functions get each document as raw JSON (or a typed `Doc<Old>`) and return a patch, a `DocChange` or nothing. They run in batches, each one mutation transaction that also advances a cursor in `_migrations`, so a run processes each document exactly once and resumes after a crash or a failure. `ferrix migrate [--name] [--dry-run]`, `ferrix migrate status`, `serve --migrate` and the admin functions `_system/migrations:{list, status, run, cancel}` run and show them. A table whose documents fail the startup check still opens while it has pending migrations, as *migrating*: writes may leave a document invalid only if it already was, and the check runs again (recording the fingerprint) in the transaction where its last migration finishes; a table still invalid after its migrations refuses the next start. Adding `Option<T>` or `#[serde(default)]` fields needs no migration.

### 4.3 Storage layout in SQLite

This is a generic layout with **single-version** storage: SQLite WAL snapshots provide read isolation, so no MVCC rows are needed.

```sql
CREATE TABLE documents (
  id         BLOB PRIMARY KEY,      -- 18-byte internal id
  table_id   INTEGER NOT NULL,
  json       TEXT NOT NULL,         -- user fields + system fields
  ts         INTEGER NOT NULL       -- commit ts of last write
);
CREATE TABLE index_entries (
  index_id   INTEGER NOT NULL,
  key        BLOB NOT NULL,         -- memcomparable(index field values ++ doc id)
  doc_id     BLOB NOT NULL,
  PRIMARY KEY (index_id, key)
) WITHOUT ROWID;
CREATE TABLE _meta (k TEXT PRIMARY KEY, v) ;  -- includes the global commit `ts`
-- system tables (_schema, _users, _sessions, _storage, _scheduled_jobs, ...) are ordinary
-- ferrix tables with reserved names, so the dashboard and reactivity treat them uniformly.
```

Why use generic tables instead of one SQLite table per user table:
- Every index lookup becomes a **byte-range scan** over `index_entries`. Read sets are therefore just byte intervals, which is the main reason for this layout.
- Adding or removing an index needs no DDL, only a backfill.
- There is one code path for every table, system tables included.

**Memcomparable key encoding.** For each value, write a type-tag byte, then an order-preserving payload: sign-flipped big-endian for ints, IEEE-754 bit tricks for floats, and escaped, terminated bytes for strings, bytes, and arrays. Byte order then matches `Value` order exactly. The trailing doc ID makes keys unique. Every index implicitly ends in `_creation_time, _id`. Built-in indexes are `by_id` and `by_creation_time`.

### 4.4 Transactions and commit timestamps

- **Reads (queries).** A pool of N read-only connections. A query starts `BEGIN`, reads `_meta.ts` first (which pins the WAL snapshot and tells us the snapshot timestamp), runs, and ends the transaction. Results are therefore always consistent at a single `ts`.
- **Writes (mutations).** **One writer connection** behind a `tokio::sync::Mutex`. A mutation takes the lock, runs `BEGIN IMMEDIATE`, executes the user's async handler (reads see its own writes), and on success bumps `ts`, writes `_meta.ts`, commits, and appends a `CommitEntry` to the in-memory write log.
  - This gives **serializable isolation for free**, with no OCC conflicts or retries, which is the right tradeoff at this scale.
  - `MutationCtx` exposes no network access, so mutations stay short. A per-mutation timeout (`AppOptions::mutation_timeout`, `--mutation-timeout`, default 5s) is checked at await points; synchronous code cannot be preempted.
  - Upgrade path: the read-set machinery in §4.5 is exactly what OCC needs. If writer contention ever matters, mutations can run on snapshots and be validated at commit.
- **`CommitEntry { ts, writes: Vec<IndexWrite { index_id, old_key: Option<Bytes>, new_key: Option<Bytes> }>, tables_touched }`.** The writer already computes these keys for index maintenance, so they come at no extra cost.
- **Actions** have no transaction. They call `ctx.run_query` and `ctx.run_mutation`, each of which is its own transaction.

### 4.5 Reactivity (the core of the project)

**Read-set recording.** Each query runs with a `ReadSetRecorder` and records:
- For each index range scan: `(index_id, [start, end))`. Two details matter here:
  - **`take(n)` / `first()` narrow the recorded range** to the part actually scanned, from the start bound to the last key returned. "Latest 50 messages" is then not invalidated by inserts older than the 50th.
  - `.filter(...)` does **not** narrow the range. The whole scanned range was read, so the read set covers all of it.
- `db.get(id)` records a point range on `by_id`.
- A search query records a coarse `(table, search_index)` dependency: any write to that table that changes the search field or a filter field invalidates it. (Implemented as the full key range of a synthetic index id per search index, plus `by_id` points for the documents it examined; see docs/CONTRACT.md §1.8.)
- `ctx.auth` access is tracked: whether the run read nothing, the principal (user/admin id), or the session id.

**Subscription manager** (`sync/subscriptions.rs`: one task per app that owns all subscriptions; built):
1. On a new subscription it **deduplicates**: queries are keyed by `(udfPath, canonical args, identity scope)`, where the scope is what the last run read of `ctx.auth` (nothing → shared by everyone; principal → by that user's or admin's sockets, all anonymous sockets share one; session → one session). A query that already has a result valid at the manager's ts is reused with no execution. A new query is keyed by the full identity until its first run; a rerun that reads more of the identity splits the subscribers that no longer match off to queries of their own, run in the same round.
2. Commits are processed in **rounds**. A round opens one snapshot `S` (several at the same ts for big rounds, to rerun on several threads), finds the queries whose read sets contain a key written in `(ts, S.ts]` through an **interval index** (per `index_id`, a treap with max-end augmentation; `sync/interval.rs`, property-tested against the linear `ReadSet::overlaps`) — or all of them if the write log was trimmed — reruns those in `S`, and leaves every other query valid at `S.ts`. **Invalidations coalesce**: commits that land during a round are handled together by the next one.
3. Each connection gets ONE update per round at `S.ts` with the queries of its set whose `result_hash` (blake3) changed, as soon as none of its queries is still running. All results in a Transition are therefore valid at `endVersion.ts`, across shared queries too.
4. `ModifyQuerySet` and `Authenticate` (which re-keys all of the socket's queries) go through the same task: answered at once from cached results, or after the next round when something must run; later commands of that socket wait behind, so replies and commit-driven updates are never reordered.
5. The per-socket worker (`sync/worker.rs`) keeps the protocol state: versions, auth, mutations, actions, read-your-writes (it asks the manager to *cover* a mutation's ts) and pings. Updates reach it through a per-socket mailbox that never blocks the manager: commit-driven updates for a socket that isn't reading are merged into the latest state; a socket with over 1024 unanswered query-set/identity changes gets `FatalError`.
6. Counters (`App::subscription_stats`): connections, shared queries, subscriptions, index ranges, rounds, reruns; `debug` tracing per round.
7. Measured (`tests/it/bench.rs`, release, 200 commits/s): 100 clients watching the same 10 queries cost 1 execution per commit instead of 100, with p50 commit-to-client latency 0.22 ms instead of 1.7 ms (500 clients: 0.42 ms instead of 52 ms, where per-socket workers could no longer keep up). With no sharing possible (every client's queries distinct) it is on par until saturated; when saturated, the round barrier roughly doubles latency (p50 105 ms vs 51 ms at 500 clients × 10 distinct queries). A remaining limit: reruns spread over the read connections scale poorly across threads (per-query cost grows ~5× when 6 run in parallel; likely SQLite-level contention).

The write log is a ring buffer of recent commits (100k entries or 5 minutes).

**Determinism contract.** Queries must be pure functions of (DB snapshot, args, identity). In Rust this can't be enforced the way a V8 sandbox enforces it, so it is a documented contract. To make it easy to follow, `QueryCtx` exposes no network, `ctx.now()` returns the snapshot's commit time, and there is no randomness API.

### 4.6 WebSocket protocol (v1: JSON frames)

The protocol is small: JSON frames over one WebSocket per client.

**Client → server**
| Message | Purpose |
|---|---|
| `Connect { sessionId, connectionCount, lastCloseReason, clientVersion }` | Start or resume a session |
| `Authenticate { baseVersion, token \| null }` | Set or replace identity. Identity-dependent queries are rerun. |
| `ModifyQuerySet { baseVersion, newVersion, add: [{queryId, udfPath, args}], remove: [queryId] }` | Subscribe or unsubscribe |
| `Mutation { requestId, udfPath, args }` | Run a mutation |
| `Action { requestId, udfPath, args }` | Run an action |
| `Event`, `Ping` | Telemetry, keepalive |

**Server → client**
| Message | Purpose |
|---|---|
| `Transition { startVersion, endVersion: {querySet, identity, ts}, modifications: [QueryUpdated{queryId, value, logLines} \| QueryFailed{queryId, errorMessage, errorData} \| QueryRemoved{queryId}] }` | Atomic batch of query result changes, all at one `ts` |
| `MutationResponse { requestId, success, result \| errorMessage, errorData, ts, logLines }` | Mutation outcome plus its commit `ts` |
| `ActionResponse { requestId, success, result \| error, logLines }` | Action outcome |
| `AuthError`, `FatalError`, `Ping` | Errors and keepalive |

Guarantees the protocol must provide:
- **No tearing.** All of a client's query results move together from one `ts` to the next inside a single `Transition`, so two components can never show inconsistent states.
- **Read-your-writes.** The client resolves the `useMutation` promise and drops the optimistic update only after it has received a `Transition` with `endVersion.ts >= mutation.ts`.
- **Reconnect.** The client resends the full query set and any unacknowledged mutations. The server deduplicates mutations by `(sessionId, requestId)`, recorded in the raw SQLite table `_mutation_dedup` (in the same transaction as the mutation) for about 10 minutes, so retries are idempotent.
- **Dev logs.** In `--dev`, `logLines` (the `tracing` events emitted while a function runs, written with `ferrix::log::info!` & co.) are forwarded to the browser console.

**HTTP fallback.** `POST /api/query | /api/mutation | /api/action` with `{ path, args }` and a bearer token. This serves SSR, scripts, other services, and tests. `#[http_action]` or a plain axum router handles webhooks.

### 4.7 Function runtime

- The `#[query]`, `#[mutation]`, and `#[action]` macros:
  1. Generate an args struct from the parameters (`Deserialize + specta::Type`).
  2. Wrap the function as `Arc<dyn Fn(Ctx, serde_json::Value) -> BoxFuture<Result<serde_json::Value>>>`.
  3. Capture the arg and return types for codegen.
  4. Register `{ path, kind, visibility }` in a distributed slice.
  5. Emit a typed **function reference type** (a braced struct with the same name, type namespace only), so `ctx.run_query::<crate::messages::list>((channel,))` is type-checked.
- `#[internal_query]`, `#[internal_mutation]`, and `#[internal_action]` cannot be called from clients. They can be called from other functions, the scheduler, cron, and the dashboard.
- **Errors**: `UserError { code, message, data }` reaches the client with its data intact, as a typed `FerrixError` in TS. Any other error becomes `"Server Error"` in production and shows the full message in `--dev`.
- **Execution log**: every execution (nested `ctx.run_*` calls included) is written asynchronously to `aux.db._logs` with function, kind, duration, outcome, error, log lines, truncated args, and identity. A separate DB file keeps logging off the writer lock. Writes go through a bounded channel to a batching background thread (entries are dropped and counted rather than blocking a function); retention defaults to 7 days or 200k rows. The dashboard reads it through `App::logs()` (`query`, `subscribe` for a live tail, `stats`). See docs/CONTRACT.md §1.4.

### 4.8 Codegen

Running `ferrix codegen --out ../web/src/ferrix` (or automatically on every `serve --dev` start when `FERRIX_CODEGEN_OUT` is set) writes:
- `api.ts`: a nested `api` object (`api.messages.list`) whose entries are `FunctionReference<"query", Args, Return>`.
- `dataModel.ts`: `Doc<"messages">`, `Id<"messages">`, and table type maps.
- Every Rust type reachable from function signatures, via specta.

The dev loop: `cargo watch -x 'run -- serve --dev'` rebuilds and restarts, codegen rewrites the TS files, the browser client reconnects and resubscribes on its own, and the frontend's Vite HMR picks up the changed types.

---

## 5. Client packages

### 5.1 `@ferrix/client` (framework-agnostic core)
- `FerrixClient(url, opts)` with:
  - A **WebSocket manager**: exponential backoff with jitter, heartbeat, and visibility-aware reconnect.
  - **Remote query set** state: versions, applying `Transition`s atomically.
  - A **local query store**, keyed by `(udfPath, canonicalJSON(args))`, with ref-counted subscriptions so unsubscribing has a short grace period and fast remounts don't churn.
  - A **request manager** for in-flight mutations and actions that resolves on the "ts observed" rule.
  - An **optimistic update layer**: a stack of updates replayed over server state, each removed once its mutation's ts is observed.
  - **Auth**: `client.setAuth(fetchToken)`, which refreshes tokens before expiry and re-sends `Authenticate`.
- API: `client.watchQuery(ref, args) → { onUpdate(cb), localQueryResult() }`, `client.mutation(ref, args, { optimisticUpdate })`, `client.action(ref, args)`, `client.query(ref, args)` (one-shot), and `client.connectionState()`.
- Paginated queries: `client.paginatedQuery(ref, args, { initialNumItems })` → `{ subscribe(cb), getCurrent() }` with `{ results, status, isLoading, error, loadMore(n) }`. The server-side `.paginate(opts)` returns `{ page, isDone, continueCursor }`. Once a page is followed by another, the client **re-subscribes it with `endCursor` pinned** to where the next page starts (keeping the old result until the pinned one arrives), so pages don't shift as items are inserted or deleted. Pinning happens on the client, so the protocol needs nothing special.

### 5.2 `@ferrix/react`
- `<FerrixProvider client={…}>`
- `useQuery(ref, args | "skip")` returns `T | undefined` and throws on error to an ErrorBoundary. `useQueryWithStatus` returns `{ data, error, isLoading }` for callers who prefer that shape. Both are built on `useSyncExternalStore`.
- `useMutation(ref)` returns an async function with `.withOptimisticUpdate(fn)`.
- `useAction(ref)`
- `usePaginatedQuery(ref, args | "skip", { initialNumItems })` returns `{ results, status, isLoading, loadMore(n) }` and throws a failed page's error to an ErrorBoundary.
- `useFerrixAuth()` returns `{ isLoading, isAuthenticated, user }`; `useAuthActions()` returns `{ signIn, signOut }`.

### 5.3 `@ferrix/svelte` (Svelte 5 runes)
- `setupFerrix(url)` puts the client in context.
- `useQuery(ref, () => args | "skip")` returns a reactive object `{ data, error, isLoading }`. It is implemented with `createSubscriber` from `svelte/reactivity`, and the args thunk is tracked so the subscription switches when args change.
- `useMutation(ref)` (with `.withOptimisticUpdate`), `useAction(ref)`, `usePaginatedQuery(ref, () => args | "skip", { initialNumItems })` (a reactive `{ results, status, isLoading, error, loadMore }`; changing args starts over), `useAuth()`.
- The dashboard is written in Svelte, so these bindings get used for real from the first day.

---

## 6. Dashboard (`/_/`)

A Svelte SPA built with Vite and embedded with `rust-embed`. It talks to the server over the **same WS protocol**, calling admin-only `_system:*` functions, so every view updates live.

Admins are stored in the `_admins` table and created with `ferrix admin create email@x`. On first run with no admin, the server prints a one-time setup link.

| Page | Features |
|---|---|
| **Data** | Table list with row counts. Read-only schema view (fields, indexes, search indexes). A virtualized document grid that is **live**, filterable and sortable via indexes, with a full-text filter and a JSON/raw view. Insert, edit, and delete are schema-validated and run as audited internal mutations. Clicking an `Id` follows the reference. Bulk delete, plus CSV/JSONL export per table. |
| **Functions** | Every registered function with its kind, args schema, and visibility. A **runner** to call any function with JSON args, optionally *acting as* a chosen user. Per-function stats: calls, p50/p95, error rate. |
| **Logs** | A live tail of executions, filterable by function, status, and user. Expand a row to see log lines, args, error, and duration. |
| **Auth** | Users, linked OAuth accounts, sessions (revoke), email verification status, impersonate. Provider configuration (client IDs and secrets stored encrypted in `_settings`). |
| **Files** | A storage browser with previews, size, content type, and references, plus upload and delete. |
| **Scheduler** | Scheduled jobs (pending, in progress, success, failed) with cancel and retry. Crons with schedule, last run, and next run. |
| **Settings** | SMTP, S3, CORS origins, token lifetimes, backups (download a snapshot, scheduled backups to S3), log retention. |
| **Overview** | Live connections, active subscriptions, rerun rate, mutation latency, DB size. |

---

## 7. Built-in features

### 7.1 Auth
- **System tables**: `_users` (email, `email_verified`, `password_hash`, name, image), `_accounts` (oauth provider + subject), `_sessions` (refresh token hash, user agent, expiry), `_verification_codes`. App-specific profile data goes in the app's own table that references `Id<_users>`.
- **Methods**:
  - Email and password (argon2id), with email verification and password reset over SMTP (`lettre`).
  - OAuth2 using the authorization code flow with PKCE, with presets for Google, GitHub, Discord, and Apple, plus a generic OIDC provider.
  - Magic link and email OTP.
- **Tokens**: a short-lived JWT access token (about 15 minutes, HS256 or EdDSA) plus a rotating refresh token. The client refreshes automatically and sends `Authenticate` on the WebSocket.
- **In functions**: `ctx.auth.user_id() -> Option<Id<User>>`, `ctx.auth.require_user()`, `ctx.auth.identity()`.
  (As built: `require_user()` is synchronous and returns `Id<User>`; `ctx.auth.user(&ctx.db)` loads the document.
  The builder is `AuthConfig::password()` rather than `Auth::password()`, because `ferrix::Auth` is `ctx.auth`'s type.)
- **Endpoints**: `/api/auth/{signup,signin,signout,refresh,verify,reset,oauth/:provider,oauth/:provider/callback}`.
- **Hardening**: rate limits on auth endpoints (`tower` governor), a constant-time compare, and no account enumeration.
- **Later**: accept external JWTs (Clerk, Auth0) through a JWKS URL.

### 7.2 File storage
- Backend: `object_store`, either `LocalFileSystem` at `fx_data/storage/` or S3-compatible storage (R2, MinIO, AWS).
- Flow:
  1. A mutation calls `ctx.storage.generate_upload_url()`, which returns a signed, short-lived URL.
  2. The client `POST`s the file to that URL and gets a `StorageId` back.
  3. A mutation stores the `Id<_storage>` in a document.
  4. `ctx.storage.get_url(id)` produces the serving URL.
- `_storage` table: `sha256`, `size`, `content_type`, `created_by`.
- Serving: `GET /api/storage/:id`, with optional signed or expiring URLs for private files and range requests for media.
- Actions can also call `ctx.storage.store(bytes)` and `ctx.storage.get(id) -> bytes`.
- Later: image thumbnails (`?thumb=200x200`, via the `image` crate) and orphan cleanup through a built-in cron.
- As built (docs/CONTRACT.md §1.6, docs/PROTOCOL.md "File storage"): `ctx.storage` exists in all three contexts
  (queries: `get_url`, `get_metadata`); `_storage` also records the original file name, and its creation time is
  `_creationTime`. URLs are stable capability URLs (`/api/storage/<id>`, the id carries 96 random bits), so
  `get_url` stays deterministic in queries; signed or expiring URLs remain an option for private files.

### 7.3 Scheduler and cron
- `ctx.scheduler.run_after::<fn_ref>(dur, args)` and `run_at::<fn_ref>(time, args)`. Called inside a mutation, the job row is inserted **in the same transaction**, so it is scheduled only if the mutation commits. Both return an `Id<_scheduled_jobs>`, which can be passed to `ctx.scheduler.cancel(id)`.
- A **scheduler task** keeps the next due time in memory, wakes on `Notify` when jobs are inserted, and runs due jobs with bounded concurrency.
- Delivery semantics:
  - **Mutations: exactly once.** The job state change commits in the same transaction as the mutation.
  - **Actions: at most once.** The job is marked `in_progress` before running and set to `failed` if the server restarts mid-run. Opt-in retries with backoff are available.
- **Cron**: `.cron(name, expr, fn_ref, args)` in `main`, reconciled into `_crons` at startup. The cron task enqueues scheduled jobs, so all execution goes through one path. Missed runs during downtime run once on startup, not N times.
- Because jobs are ordinary tables, the dashboard shows them live with no extra work.
- (As built, docs/CONTRACT.md §1.7: the table is `_scheduled_jobs`, states are `pending | inProgress | success |
  failed | canceled`, the builder is `.cron::<f>(name, expr, args)` with 6-field UTC expressions plus
  `.interval::<f>(name, every, args)`, and the scheduler wakes on write-log commits to the jobs table instead of an
  explicit `Notify` from the writer. Mutation jobs run one at a time in the dispatcher, since they serialize on the
  writer anyway.)

### 7.4 Text search (SQLite FTS5)
- `#[search_index(name, search = field, filter = [f1, f2])]` creates an FTS5 table per search index, with filter fields stored as `UNINDEXED` columns.
- The index is maintained **inside the mutation transaction**, so search results are always consistent with document reads. This is the main advantage over tantivy, which would need a separate index kept in sync.
- Query API: `.with_search_index(Message::search_body, |q| q.search(Message::body, "hello wor").eq(Message::channel, c)).take(10)`, ordered by bm25 relevance. The last term gets prefix matching for search-as-you-type.
- Reactivity is coarse (see §4.5), which is good enough.

### 7.5 Vector search (`sqlite-vec`)
- `#[vector_index(name, field = embedding, dims = 1536, filter = [f])]` on a `Vec<f32>` field.
- `ctx.vector_search(Table::index, query_vec, { limit, filter })` is available **in actions only**. Embeddings come from external APIs, which only actions can call, so this keeps queries deterministic and cheap.
- Brute-force or `vec0` virtual tables handle roughly 100k to 1M vectors on a small VPS. If that ever runs out, the fallback is an HNSW crate.

---

## 8. Operations

- **Data dir**: `./fx_data/{data.db, aux.db, storage/, backups/}`, overridable with `--dir`.
- **CLI**: `serve [--http 0.0.0.0:8090] [--https domain] [--dev]`, `codegen`, `migrate`, `admin create|reset-password`, `backup`, `restore`, `export --format jsonl`, `import`.
- **Backups**: the SQLite online backup API (or `VACUUM INTO`), on demand or on a schedule, optionally uploaded to S3. Running Litestream alongside works for continuous replication.
- **Deploy**: one static binary (target `x86_64-unknown-linux-musl`), a systemd unit, a `FROM scratch` Docker image, and an optional `--https` mode with automatic TLS.
- **Serving the frontend**: `.static_dir(...)` or `.embed_static::<Assets>()` serves the SPA from the same binary with SPA fallback routing, so an entire app ships as one binary.
- **Observability**: `tracing` to stdout as JSON in production, optional Prometheus `/metrics`, and dashboard metrics read from `aux.db`.
- **Config**: CLI flags, then env vars (`FERRIX_*`), then `_settings` (set in the dashboard).

---

## 9. Testing strategy

| Layer | Approach |
|---|---|
| Key encoding | `proptest`: for random `Value` pairs, `encode(a) < encode(b)` exactly when `a < b`. Roundtrip tests. |
| Query engine | Table-driven tests over index ranges, ordering, take/first/unique, filter, and pagination cursors. |
| **Reactivity correctness** | **Model-based randomized test.** Run N random subscriptions and M random mutations, then assert that each subscription's last pushed result equals a fresh execution at the same `ts`, and that no update was missed or stale. This is the most important test in the project. |
| Concurrency and races | Commits landing mid-query, invalidation coalescing, reconnect during an in-flight mutation, duplicate `requestId`s. |
| Protocol | In-process server plus a `tokio-tungstenite` client, with snapshot tests (`insta`) of message sequences. |
| TS client | `vitest` with a mock socket: transitions apply atomically, optimistic updates roll back correctly, the read-your-writes ordering holds. |
| End to end | Playwright on `examples/`: two tabs, a mutation in one appears in the other in under 100ms. Auth flows and file upload. |
| **For apps** | `ferrix::testing::TestApp`: an in-memory SQLite instance with `app.query(...)`, `app.mutation(...)`, `app.as_user(id)`, and `app.advance_time(...)` to fire scheduled jobs. |

Rough performance targets for a small 2 vCPU VPS:
- 5k concurrent WebSocket connections.
- More than 1k simple mutations per second.
- Under 10ms p50 from commit to push.
- A server binary under 30MB.

---

## 10. Roadmap

Sizes are relative: **S** ≈ days, **M** ≈ 1–2 weeks, **L** ≈ 2–4 weeks of focused work.

### Phase 0 — Spike: prove the risky parts (M) ✅ done 2026-10-09
Goal: a todo list that updates live across two browser tabs, with as little code as possible.
- [x] `#[query]` / `#[mutation]` / `#[action]` / `#[table]` macros with typed args, registered via `linkme`. TS types come from **ts-rs** (not specta).
- [x] `documents` and `index_entries` storage, memcomparable keys for every JSON type, and built-in `by_id` / `by_creation_time` indexes.
- [x] Single-writer commits with `ts` and a write log.
- [x] Read-set recording (narrowed for `take`/`first`) plus invalidation by linear scan.
- [x] The full WS protocol (docs/PROTOCOL.md) and a full TS client, not just a minimal one.
- **Exit criteria met**: `examples/todo` passes `e2e/two-tabs.ts` (about 2ms tab-to-tab) and `e2e/reconnect.ts` (server restart).
- Design change found during the build: function references are type-namespace-only braced structs, called as `ctx.run_query::<list>(args)` (docs/CONTRACT.md §1).

### Phase 1 — Core engine (L) — mostly done
- [x] Full value model in the key encoding, with proptests, including `ferrix::Bytes` (`{"$bytes": "<base64>"}`, its
  own tag between String and Array; the encoding is versioned in `_meta` and older databases get every index rebuilt).
- [x] `#[table]` with typed field and index handles, catalog reconciliation, and index backfill.
- [x] Validating existing documents against a changed schema at startup (only tables whose schema fingerprint changed; `--skip-schema-check` escape hatch).
- [x] `#[migration]`: batched, resumable, exactly once per run, dry runs, `_migrations`, `ferrix migrate`,
  `serve --migrate`, admin system functions, and lenient startup for tables with pending migrations (CONTRACT §1.9).
  Not yet: a dashboard Migrations page (the system functions exist), dependencies between migrations other than name
  order.
- [x] Query builder: `get`, `with_index` ranges, `order`, `filter`, `take`, `first`, `unique`, `collect`, and `paginate` (opaque cursors over the index key space, `endCursor` pinning, read sets narrowed to the scanned span, a 10k-document scan cap per page).
- [x] Mutations: `insert`, `patch`, `replace`, `delete`. Every write is validated against the table type (`patch` checks the merged document; `insert`/`replace` check that the stored JSON round-trips).
- [x] Actions with `run_query` / `run_mutation` / `run_action`, internal functions, and `UserError`.
- [x] `TestApp` harness.
- [x] A mutation timeout (`AppOptions::mutation_timeout`, `--mutation-timeout`, default 5s; enforced at await points only).

### Phase 2 — Sync and reactivity, done properly (L) — done
- [x] Per-connection sync worker (protocol state), with one consistent snapshot ts per refresh, write-log catch-up, coalescing and concurrent reruns (now in the shared manager below).
- [x] **Shared subscriptions**: each distinct query (path, args, identity scope from its `ctx.auth` reads) runs once per invalidation and is fanned out to every subscriber, in rounds at one snapshot ts (§4.5); per-socket mailboxes that coalesce for slow sockets; a model-based test with overlapping query sets, a dedup proof (50 sockets, one execution per commit) and a benchmark (`tests/it/bench.rs`).
- [x] Interval-tree invalidation index (per `index_id`, property-tested against the linear scan).
- [x] The full protocol: versions, atomic transitions, mutation `ts`, read-your-writes, reconnect and resubscribe.
- [x] Mutation idempotency on reconnect, keyed by `(sessionId, requestId)` and recorded in the mutation's own transaction.
- [x] HTTP API fallback.
- [x] Model-based reactivity test suite.
- [x] Execution logging to `aux.db` (`App::logs()`: query, live tail, per-function stats), and forwarding log lines (`logLines`) to clients in `--dev`.

### Phase 3 — Clients and codegen (L) — mostly done
- [x] `ferrix codegen --out` producing `api.ts`, `dataModel.ts` and `index.ts`; auto codegen in `serve --dev --codegen-out` (or `FERRIX_CODEGEN_OUT`).
- [x] `@ferrix/client` core with optimistic updates.
- [x] Reactive pagination: `client.paginatedQuery` (pins each page's `endCursor` once a next page follows it), `usePaginatedQuery` for React and Svelte, and the `insertAtTop` optimistic helper.
- [x] `@ferrix/react` and `@ferrix/svelte`.
- [x] `examples/todo` (Svelte), with Playwright e2e scripts. `examples/chat-react` is still to do.
- **Milestone reached: usable for a real (unauthenticated) app.**

### Phase 4 — Dashboard v1 (L) — mostly done
- [x] Admin auth (`_admins`, `admin create|reset-password`, `/api/auth/admin/signin`, `ctx.auth.require_admin()`) and
  the setup link (`/_/setup#token=…`, now the dashboard's setup page).
- [x] Admin-only `_system/dashboard:*` functions (docs/CONTRACT.md §4.1): tables, untyped live paginated documents,
  untyped CRUD validated with `TableDef::validate`, the function registry, `run_function` (act as user), users and
  sessions. Secrets (`*Hash` fields of system tables) are masked; system tables are read-only except the profile
  fields of `_users`.
- [x] An untyped (table-name) path through the query engine and pagination, sharing the typed API's scan core.
- [x] `/api/admin/logs`, `/api/admin/logs/stream` (SSE, bearer auth) and `/api/admin/stats` (§4.2).
- [x] The SPA (`packages/dashboard`, Svelte 5) embedded with `rust-embed` (feature `dashboard`) and served at `/_/`:
  sign-in and setup, data browser (live grid, index filters, JSON editor, follow IDs, schema view), function runner,
  live logs tail, users and sessions, overview. Built assets are committed; a test fails when they are stale.
- [ ] Not yet: a virtualized grid, full-text filter, CSV/JSONL export, audited dashboard writes, per-function
  latency charts, impersonation via a real session.

### Phase 5 — Auth (M) — mostly done
- [x] Password (argon2id), email verification, password reset, magic link and OTP, sessions with rotating refresh
  tokens and reuse detection, SMTP (`lettre`; printed to the log in `--dev`), rate limits, `ctx.auth` (docs/PROTOCOL.md
  "Auth", docs/CONTRACT.md §1.2 and §1.5).
- [x] OAuth with PKCE: GitHub and Google presets, generic OIDC via discovery (tested against a mock provider only;
  Discord and Apple presets are still to do).
- [x] Client integration: `getAuth(client)` (`signIn`/`signOut`/…, refresh wired into `setAuth`, cross-tab sync),
  React `useFerrixAuth` / `useAuthActions` / `useCurrentUser`, Svelte `useAuth` / `useCurrentUser`.
- [x] The dashboard Users page (users, linked providers, sessions, revoke, delete; "act as user" in the runner).
- [ ] Provider configuration in `_settings`, editable from the dashboard.
- [ ] External JWTs via JWKS; live revocation of already-authenticated sockets on sign-out (today they keep their
  identity until the access token expires).
- **Milestone: usable for real apps with users.**

### Phase 6 — Scheduler and cron (M) — done
- [x] `ctx.scheduler.run_after` / `run_at` / `cancel` in mutations (in the mutation's transaction) and actions
  (immediately, through internal system mutations); targets must be `SchedulableFunction` (mutations and actions,
  emitted by the macros), so scheduling a query doesn't compile (docs/CONTRACT.md §1.7).
- [x] `_scheduled_jobs` with a `(state, runAt, scheduledAt)` index; the runner: bounded concurrency, same-time jobs in
  scheduling order, woken by write-log commits that touch the jobs table. Mutations exactly once (completion recorded
  in the job's own transaction), actions at most once (`inProgress` committed first, `failed` after a restart),
  opt-in action retries with backoff, anonymous identity logged as `scheduler`.
- [x] Crons and intervals on the `Ferrix` builder, reconciled into `_crons`; missed runs run once; no pile-up.
- [x] Retention of finished jobs (7 days), the admin-only `_system/scheduler:*` functions and the dashboard Scheduler
  page; `TestApp::run_due_jobs` / `advance_time` / `restart` with an advanceable clock.
- [ ] Not yet: canceling a running action, per-cron timezones, a "run cron now" button.

### Phase 7 — File storage (M) — mostly done
- [x] The `_storage` system table (`StoredFile`: sha256, size, content type, `createdBy`, original file name) and the
  local (`<data dir>/storage/`) and S3-compatible backends via `object_store` (`--storage s3://bucket/prefix`,
  `AWS_*` env vars; any `object_store` through the builder). Blobs are named by their storage id and only become
  visible once complete.
- [x] Upload URLs: HMAC-signed with a key derived from `auth_secret.key`, valid for an hour, single-use (recorded in
  the transaction that inserts the file's document), rate limited per IP. `POST /api/storage/upload` streams to the
  backend while hashing and answers 413 past `--max-upload-size` (default 100 MB).
- [x] `ctx.storage`: `get_url` / `get_metadata` in queries (tracked reads), plus `generate_upload_url` and a
  transactional `delete` in mutations (the blob goes after the commit), plus `store` / `get` in actions. An orphan
  sweep removes blobs without a document.
- [x] Serving `GET /api/storage/{id}`: ETag/304, single ranges (206/416), `Cache-Control: private, immutable`,
  `nosniff`, a sandboxing CSP, and active content (HTML, SVG, …) only as attachments. Unguessable capability URLs.
- [x] `uploadFile()` in `@ferrix/client` (XHR progress, `fetch` fallback), re-exported by React and Svelte.
- [x] The dashboard Files page (live list, previews, download, delete, drag-and-drop upload; `_storage` ids in the data
  grid link to it) and image attachments in `examples/todo`.
- [ ] Later: signed, expiring URLs for private files; image thumbnails; multiple-range responses; running the orphan
  sweep as a built-in cron once the scheduler exists.

### Phase 8 — Search (M) — mostly done
- [x] `#[search_index(name, search = f, filter = [..])]` / `#[vector_index(name, field = f, dims = N, filter = [..])]`
  with span-accurate errors, typed `SearchIndex<T>` / `VectorIndex<T>` handles, `TableDef::{search_indexes,
  vector_indexes}` (docs/CONTRACT.md §1.8, §3.1).
- [x] FTS5 search indexes: one FTS5 table per index (filter fields as `UNINDEXED` memcomparable-hex columns),
  maintained in the write transaction by the typed and untyped write paths, reconciled and backfilled at startup,
  checked by `verify_indexes`. `with_search_index(..).take(n)` ranks by bm25; user text becomes quoted terms (AND,
  last one a prefix), so it can't inject FTS5 syntax; empty text matches nothing; 1024-result cap.
- [x] Coarse reactivity: search indexes get ids in the upper half of the index id space; a search query reads that
  id's full range plus `by_id` points of what it examined, and writes changing an entry write `(id, doc id)`. No read
  set / write log type changes.
- [x] `sqlite-vec` vector indexes (`vec0`, cosine, filter fields as metadata columns, statically linked and registered
  with `sqlite3_auto_extension`); `ActionCtx::vector_search` (actions only), dimension-checked, OR/AND equality
  filters, exact results (checked against brute force). ~18 ms per k=10 query over 50k × 384.
- [x] Dashboard schema view lists search and vector indexes; the todo example has a live search box.
- [ ] Not yet: search pagination, a dashboard search box, per-filter-value read sets (finer invalidation), an
  approximate (HNSW) index for millions of vectors, configurable tokenizers.

### Phase 10 — DX fixes found by `examples/chat-react` (M)
- [ ] Index keys end in `_creationTime, _id`, so `eq(channel)` + `order(Desc)` is newest first; rebuild
  existing indexes on upgrade. Today they end in the random `_id`.
- [ ] Codegen names top-level output structs (`Array<MessageView>`, not an inlined object), and field doc comments
  render cleanly.
- [ ] A derive/attribute for non-table output types (serde + ts-rs + camelCase, via ferrix re-exports), so apps
  don't depend on serde/ts-rs directly.
- [ ] `ctx.is_dev()` / app-level dev flag; dev-only functions left out of the public `api`; the built-in dev mailer
  is composable with a custom `Mailer`.
- [ ] `@ferrix/react`: `useAuthActions().providers()`, re-export `AuthProviders`; `usePaginatedQueryWithStatus`
  (non-throwing).
- [ ] Storage URLs root-relative in `--dev` (so they go through a dev proxy) unless a public URL is set explicitly.
- [ ] `PaginationResult::map_async` / `try_map`; `Query::count()` over an index range; `TestApp::create_user`.
- [ ] Docs: optimistic updates are replayed on every server change (create ids/timestamps outside the closure);
  scheduled functions run without identity (pass the user explicitly); `ctx.now()` in queries is not reactive.

### Phase 9 — Operations polish (M)
- [x] Migrations CLI (`ferrix migrate`, `migrate status`, `serve --migrate`; Phase 1's `#[migration]`).
- [ ] Backup and restore, export and import, metrics and overview, `--https` autocert, static frontend serving, musl release builds, and a Docker image.
- [x] An interval-tree invalidation index (done in Phase 2).

---

## 11. Risks and open questions

| Risk | Mitigation |
|---|---|
| **Proc-macro ergonomics.** Async fns with references in ctx, typed field and index handles, and good error messages are hard to get right. | Phase 0 exists to prove this first. Keep the macros thin and the generated code readable. Use `trybuild` tests for compile errors. |
| **Rust compile times hurt the dev loop.** Every backend change means a rebuild. | Keep the `ferrix` crate precompiled as a dependency so only the app crate rebuilds. Use `mold`/`wild` as the linker, `cargo watch`, and seamless client reconnect. This is accepted as the cost of framework mode. |
| **Non-deterministic queries** (`SystemTime::now()`, random numbers) silently break caching. | Documented contract, `ctx.now()`, and a debug-mode check that occasionally reruns a query at the same `ts` and warns if the result hash differs. |
| **Holding the writer lock during an async mutation.** | No network in `MutationCtx`, a timeout, and per-mutation duration metrics in the dashboard. The OCC upgrade path stays open. |
| **Reactive pagination correctness.** | Pin each page's end cursor and cover it with model-based tests. |
| **Schema evolution** with existing data. | Validate at startup and refuse to start on mismatch. Provide `#[migration]` batches and dev-mode dashboard links to bad documents. |
| Write-log and subscription memory growth. | Trim the log to the oldest in-flight `ts`. Cap result sizes (for example, 8MB per query) and documents scanned per query. |

**Defaults I chose; easy to revisit:**
1. `useQuery` in React returns `T | undefined` and throws errors. Svelte returns `{data, error, isLoading}`, which suits runes better.
2. JSON wire frames. MessagePack can be added later behind a protocol flag.
3. FTS5 over tantivy, for transactional consistency and zero extra infrastructure.
4. A single serialized writer over OCC.
5. Generic `documents` / `index_entries` tables over one SQLite table per user table.
6. Auth users live in a system `_users` table, and apps keep profile data in their own table.

---

## 12. First concrete steps

1. `cargo new --lib crates/ferrix && cargo new --lib crates/ferrix-macros`, a workspace `Cargo.toml`, and `pnpm init` for `packages/`.
2. Write `examples/todo-svelte/server/src/main.rs` **first**, exactly as §2 shows it, even though it won't compile yet. Then make it compile. Doing it in this order keeps the API honest.
3. Implement the Phase 0 checklist in order: macros, storage, writer and write log, read sets, WS, TS client.
