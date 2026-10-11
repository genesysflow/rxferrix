# Ferrix — Project Plan

> A single-binary backend, written in Rust, with
> typed `query` / `mutation` / `action` functions, reactive subscriptions over WebSockets, and an
> embedded admin dashboard where all data is visible and editable.

**Status:** roadmap done (Phases 0–12), beyond-roadmap work continues · **Audience:** my own apps (tight scope, rough
edges acceptable) · **Started:** 2026-10-09 · **Updated:** 2026-10-11

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
- Raw SQL access to the live database from user functions. Functions go through the document query API so reactivity
  stays correct. (Read-only SQL over the analytics copy, §7.6, is allowed in actions: that copy is not reactive.)
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
- Migrations (built, docs/CONTRACT.md §1.9): `#[migration(table = T, batch = N, after = [..])]` functions get each document as raw JSON (or a typed `Doc<Old>`) and return a patch, a `DocChange` or nothing. `after` dependencies form a DAG checked at startup; migrations run in topological order (name order as the tie-break), and running one before its dependencies are done is refused unless forced. They run in batches, each one mutation transaction that also advances a cursor in `_migrations`, so a run processes each document exactly once and resumes after a crash or a failure. `ferrix migrate [--name] [--dry-run]`, `ferrix migrate status`, `serve --migrate` and the admin functions `_system/migrations:{list, status, run, cancel}` run and show them. A table whose documents fail the startup check still opens while it has pending migrations, as *migrating*: writes may leave a document invalid only if it already was, and the check runs again (recording the fingerprint) in the transaction where its last migration finishes; a table still invalid after its migrations refuses the next start. Adding `Option<T>` or `#[serde(default)]` fields needs no migration.

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
  key        BLOB NOT NULL,         -- memcomparable(index field values ++ _creationTime) ++ doc id
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

**Memcomparable key encoding.** For each value, write a type-tag byte, then an order-preserving payload: sign-flipped big-endian for ints, IEEE-754 bit tricks for floats, and escaped, terminated bytes for strings, bytes, and arrays. Byte order then matches `Value` order exactly. The trailing doc ID makes keys unique. Every index implicitly ends in `_creation_time, _id` (built in Phase 10, key encoding v3; `by_creation_time` is just `_creationTime, _id`), so equal declared fields are in creation order and `_creationTime` can be bounded after them. Built-in indexes are `by_id` and `by_creation_time`. The encoding version lives in `_meta.key_encoding`; opening an older database rebuilds the indexes whose layout changed.

### 4.4 Transactions and commit timestamps

- **Reads (queries).** A pool of N read-only connections. A query starts `BEGIN`, reads `_meta.ts` first (which pins the WAL snapshot and tells us the snapshot timestamp), runs, and ends the transaction. Results are therefore always consistent at a single `ts`.
- **Writes (mutations).** **One writer connection** behind a `tokio::sync::Mutex`. A mutation takes the lock, opens a savepoint (inside `BEGIN IMMEDIATE`), executes the user's async handler (reads see its own writes), and on success gets the next `ts`. **Group commit:** mutations that queue for the lock share the SQLite transaction; whoever releases the lock commits it (`_meta.ts` once, `COMMIT`, then each member's `CommitEntry` appended to the in-memory write log in ts order) unless another write is waiting to continue it (at most 64 mutations or 2 ms). Callers get their results after that commit. A mutation known to be slow (over 0.5 ms lately, per path) commits the open batch first, so the batch doesn't wait for it.
  - This gives **serializable isolation for free**, with no OCC conflicts or retries, which is the right tradeoff at this scale.
  - `MutationCtx` exposes no network access, so mutations stay short. A per-mutation timeout (`AppOptions::mutation_timeout`, `--mutation-timeout`, default 5s) is checked at await points; synchronous code cannot be preempted.
  - Upgrade path: the read-set machinery in §4.5 is exactly what OCC needs. If writer contention ever matters, mutations can run on snapshots and be validated at commit.
  - Measured (`bench_writes`, release, 8 threads, in-process, 2026-10-10). Before tuning: a one-insert mutation cost ~50 µs of writer time, ~20k mutations/s whether 1 or 64 writers competed; one hot counter ~26k/s; 10 inserts per mutation ~4.8k/s; next to one writer of 20 ms mutations, sixteen fast writers dropped to ~720/s with p50 22 ms (head-of-line blocking). A profile showed 15% of writer time compiling SQL (uncached `_meta` reads, `BEGIN`/`COMMIT`) and half of it per-transaction cost (WAL page writes at `COMMIT`, three `_meta` upserts). After caching statements and group commit: 1 writer 27k/s, 16 writers 41k/s (p50 0.34 ms), 64 writers 45k/s; with idempotency keys 32k/s; with the replication journal 33k/s; with 100 clients x 10 subscribed queries 36k/s (was 13k); 10 inserts per mutation 7k/s (70k documents/s); read 100 documents then insert 7.8k/s; one hot counter 91k/s with no lost updates. The slow-writer case is unchanged (741/s, p50 22 ms): a slow mutation still blocks every write while it runs. OCC would let non-conflicting writes proceed past it; short mutations are now dominated by index maintenance and the commit, which stay serialized either way.
  - On disk (2026-10-10; the numbers above, like every earlier benchmark, ran on tmpfs, where fsync is free): NVMe SSD with btrfs (zstd compression) on dm-crypt, one-insert mutations at 1 / 16 / 64 writers: 5.8k / 9.7k / 11.6k per second by default, 1.1k / 6.3k / 9.3k with `fsync_commits` (synchronous = FULL; group commit shares the fsync). Raw SQLite on the same disk does 40k single-insert commits/s (NORMAL) and 1.4k (FULL). Most of the gap was the WAL auto-checkpoint, which flushed the WAL and the database while holding the writer (p99 17 ms at 16 writers). **Checkpoints now run off the commit path** (`db::checkpoint`): the writer has `wal_autocheckpoint = 0`, its WAL hook wakes a checkpoint thread past 1000 pages, which runs `PASSIVE` checkpoints on its own connection; past 16000 pages the writer finishes one itself before its next transaction (copying only the few frames since), so the WAL restarts (without that it grew by ~500 MB/s under constant writes). After: 8.9k / 16.2k / 18.7k per second by default (p99 0.2 / 3.5 / 11 ms), 1.1k / 7.3k / 12.5k with `fsync_commits`; tmpfs 27.1k / 43.7k / 48.9k. Restarting every 4000 pages instead cost 10-20% and tripled p99 at 64 writers on btrfs. Found on the way: the slow-mutation rule timed a transaction from when it took the writer, so the batch commit it triggered (an fsync with `fsync_commits`) counted as its own work and kept it "slow" for good; it is now timed from its savepoint.
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
1. On a new subscription it **deduplicates**: queries are keyed by `(udfPath, canonical args, identity scope)`, where the scope is what the last run read of `ctx.auth` (nothing → shared by everyone; principal → by that user's or admin's sockets, all anonymous sockets share one; session → one session). A query that already has a result valid at the manager's ts is reused with no execution. A new query is keyed by the full identity until its first run; a rerun that reads more of the identity splits the subscribers that no longer match off to queries of their own, queued at once.
2. Results are kept as **versions**: a run at snapshot ts `r` is valid from `r` up to (not including) the first commit that writes a key in its read set. Commits are processed in **generations** (pipelined rounds). A generation opens a snapshot `S`, closes the versions of the queries whose read sets contain a key written in `(ts, S.ts]` through an **interval index** (per `index_id`, a treap with max-end augmentation; `sync/interval.rs`, property-tested against the linear `ReadSet::overlaps`) — or all of them if the write log was trimmed — moves the frontier `ts` to `S.ts` and queues those queries; every other query stays valid at `S.ts`. Runner tasks (one per snapshot at `S.ts`, over the reader connections) run the queue. The manager never waits for a run, and a new generation starts as soon as commits wait and the queue's fast lane is empty: it does **not** wait for queries still running (queries whose runs average ≥ 5 ms have a lane and runners of their own). **Invalidations coalesce**: a query already queued or running is not queued again, and commits that land while the fast lane drains are handled together. A run that finishes behind the frontier is checked against the commits since its ts, and queued again if they touched its read set.
3. Each connection gets an update at a ts `T` only when every query of its set has a version valid at `T` (`T` = the frontier, or the ts of a run that just finished, using older versions kept while older runs are in flight), never older than its last update, with the queries whose `result_hash` (blake3) differs from what it was sent. All results in a Transition are therefore valid at `endVersion.ts`, across shared queries too. Liveness: queries invalidated together run at one generation; a connection whose queries keep finishing stale at different generations (several slow queries that commits invalidate while they run) is *starving*, and new generations are then held until it has all of them valid at the frontier (a last resort; one slow query is served by updates at its runs' ts).
4. `ModifyQuerySet` and `Authenticate` (which re-keys all of the socket's queries) go through the same task: answered at once when every query of the socket is valid at the frontier, otherwise as soon as they all are valid at one ts; later commands of that socket wait behind, so replies and commit-driven updates are never reordered.
5. The per-socket worker (`sync/worker.rs`) keeps the protocol state: versions, auth, mutations, actions, read-your-writes (it asks the manager to *cover* a mutation's ts) and pings. Updates reach it through a per-socket mailbox that never blocks the manager: commit-driven updates for a socket that isn't reading are merged into the latest state; a socket with over 1024 unanswered query-set/identity changes gets `FatalError`.
6. Counters (`App::subscription_stats`): connections, shared queries, subscriptions, index ranges, rounds (generations), reruns, stale runs, holds; `debug` tracing per generation.
7. Measured (`tests/it/bench.rs`, release, 200 commits/s, 8 runtime threads, 6 readers; a shared 16-thread machine, so medians of interleaved runs): 100 clients watching the same 10 queries cost 1 execution per commit instead of 100, with p50 commit-to-client latency ~0.23 ms instead of 1.7 ms with per-socket workers (500 clients: ~0.5 ms instead of 52 ms). With no sharing possible (every client's queries distinct, 500 clients × 10) p50 is ~25–35 ms (p99 ~145 ms; ~420 reruns per commit), down from 105–165 ms with the round barrier and 51 ms with per-socket workers. With one 20 ms query (blocking work, invalidated by every commit) watched by 20 more clients, the fast clients' p50 is ~0.25–0.6 ms instead of ~11–13 ms (the round barrier made every round wait for the slow query). Reader connections now scale across threads (`bench_parallel_reads`: 8 parallel readers ~70k queries/s at ~110 µs each, against ~18k at ~450 µs each before; the cause was SQLite's global page-cache and memstatus mutexes, see `.cargo/config.toml`). The sharp edge left: queued generations hold a WAL snapshot open while anything is in flight.

The write log is a ring buffer of recent commits (100k entries or 5 minutes).

**Determinism contract.** Queries must be pure functions of (DB snapshot, args, identity). In Rust this can't be enforced the way a V8 sandbox enforces it, so it is a documented contract. To make it easy to follow, `QueryCtx` exposes no network, `ctx.now()` returns the snapshot's commit time, and there is no randomness API.

**Fan-out cost (measured 2026-10-10).** Each result is serialized once,
when computed (the text also feeds the change hash), and every subscriber's `Transition` frame splices that shared
text in: serializing per subscriber had been 70% of the server's CPU (100 subscribers, 100 writes/s: 9.0 s of CPU
before, 0.9 s after). Accepted connections set `TCP_NODELAY`: without it Nagle's algorithm and
delayed ACKs held updates back (p99 35 ms before, 2.3 ms after). Clients that opt in get **result diffs** (docs/PROTOCOL.md):
the sync worker records the result each socket last received per query, and an update is sent as a diff from it when
that is clearly smaller, computed once per pair of results and shared by every socket with the same base (Myers over
array elements, patches for changed elements and keys). A new post at the top of a 20-post list is ~250 bytes instead
of 5.5 KB; at 1,000 subscribers delivery went from p50 9.9 / p99 21 ms to 5.6 / 11 ms.

**Query cache (2026-10-10).** A shared query whose last subscriber leaves stays *idle* if it is current: still in the
invalidation index, dropped (never rerun) by the first commit that writes into its read set, evicted least recently
used beyond a memory budget (64 MiB, estimated from result sizes) and after an idle TTL (30 minutes). Current
successful results (live and idle) are published with the manager's frontier to a read-only map that HTTP queries and
`ctx.run_query` consult without going through the manager; a result is withdrawn before the frontier moves past a
commit that invalidates it, and a lookup needs the frontier at the latest commit, so read-your-writes holds. Misses
are adopted as idle queries on their second occurrence (a doorkeeper keeps one-off queries out). Results of queries
that read the time are never cached. HTTP, one machine: get-by-id over 10k records
134k → 293k requests/s, list of the 20 newest 42k → 284k/s (server CPU per request about 3x and 9x lower); lists
under 22k writes/s, which keep invalidating them: no regression (29.6k → 32.8k/s).

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
| **Migrations** | Every `#[migration]` with live progress, errors (linked to the failing document), schema-check status; run, cancel, restart, dry runs with before/after examples. |
| **Backups** | Backups (create, download, delete; target and schedule), per-table export (JSONL/CSV) and JSONL import. |
| **Audit log** | Every audited admin action, live, filterable (admin, action, table, document, time), with field diffs. |
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
- **External JWTs** (Clerk, Auth0, Supabase, any OIDC issuer) verified through the issuer's JWKS and mapped to
  provisioned `_users` (built in Phase 5).

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
  (queries: `get_url`, `get_signed_url`, `get_metadata`); `_storage` also records the original file name, and its
  creation time is `_creationTime`. URLs are stable capability URLs (`/api/storage/<id>`, the id carries 96 random
  bits), so `get_url` stays deterministic in queries. Signed URLs (`?exp=&sig=`) expire; in queries their expiry is
  rounded up to a multiple of the TTL so results are stable within a window. Private mode (`--storage-private`)
  refuses unsigned reads. Thumbnails (`?thumb=WxH&fit=`) are cached in the blob store; the orphan sweep is the
  system cron `_storage_sweep`.

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
  writer anyway. A running action can be canceled: cooperatively through `ctx.is_canceled()` / `ctx.canceled()`,
  then aborted at its next await after a grace period. Crons can run in an IANA time zone (`cron_tz`), and ferrix
  declares its own system crons, named `_…`.)

### 7.4 Text search (SQLite FTS5)
- `#[search_index(name, search = field, filter = [f1, f2])]` creates an FTS5 table per search index, with filter fields stored as `UNINDEXED` columns.
- The index is maintained **inside the mutation transaction**, so search results are always consistent with document reads. This is the main advantage over tantivy, which would need a separate index kept in sync.
- Query API: `.with_search_index(Message::search_body, |q| q.search(Message::body, "hello wor").eq(Message::channel, c)).take(10)`, ordered by bm25 relevance. The last term gets prefix matching for search-as-you-type.
- Reactivity is per filter value: a search with `.eq(channel, c)` only reruns after writes to documents whose channel
  is or was `c` (docs/CONTRACT.md §1.8). Relevance is a per-document BM25 so that this, and pagination, are exact.
- `tokenizer = "unicode61" | "porter" | "trigram" | "snowball"` (stemming, substring search), `language = "…"`
  (Snowball stemmers for 18 languages through a custom FTS5 tokenizer, `db::search::stem`), `diacritics = "keep"`.
- `.paginate(opts)` like index pagination.

### 7.5 Vector search (`sqlite-vec`)
- `#[vector_index(name, field = embedding, dims = 1536, filter = [f])]` on a `Vec<f32>` field.
- `ctx.vector_search(Table::index, query_vec, { limit, filter })` in actions: embeddings come from external APIs, which only actions can call. It uses the HNSW graph when there is one and is not reactive.
- `ctx.db.vector_search(..)` in queries (built after Phase 8): always the exact `vec0` scan at the query's snapshot, so it stays deterministic, and reactive with read sets narrowed per filter value like text search (docs/CONTRACT.md §1.8).
- Brute-force or `vec0` virtual tables handle roughly 100k vectors comfortably (75–120 ms per query at 200k × 384).
  Beyond that, `kind = "hnsw"` adds an in-memory HNSW graph kept consistent with the committed `vec0` table (done in
  Phase 8; ~0.4 ms per query at 200k × 384), with per-index `m`, `ef_construction`, `ef_search` and a per-search `ef`.

### 7.6 Analytics (OLAP; built in Phase 11)

Ad-hoc analytical queries (group-bys, joins, time series over millions of documents) run on a **columnar copy** of
the data in an embedded DuckDB, a few hundred milliseconds behind the live database, never on the live SQLite file:
its row store of JSON documents is the wrong shape for scans, and long scans would hold reader connections and WAL
snapshots that subscriptions and checkpoints need. Behind the cargo feature `olap` (off by default: DuckDB adds
32 MB and minutes of C++ build to a 25 MB static binary), and on at runtime with `--olap` (docs/CONTRACT.md §1.11).

```rust
#[action]
pub async fn revenue_by_day(ctx: &ActionCtx, since: f64) -> Result<Vec<DayRevenue>> {
    ctx.olap
        .query_as::<DayRevenue>(
            "SELECT date_trunc('day', to_timestamp(_creationTime / 1000)) AS day, sum(amount) AS total
             FROM orders WHERE _creationTime >= ? GROUP BY day ORDER BY day",
            (since,),
        )
        .await
}
// After a mutation, wait until the copy has it: ctx.olap.latest() (or .at_least(ts)).query_as::<T>(..)
```

- **Feed: the change journal** (`_journal`, built for continuous replication in Phase 9; `db::journal`): exactly once, in commit order, the final state of
  every document a commit wrote. `--olap` turns journaling on like `--replicate-to` does. A row is deleted once every
  active consumer released it (`journal::release`; positions in `_meta`); a copy more than `--olap-max-lag` behind
  is rebuilt instead (the rebuild first releases the backlog), so the live database never waits for analytics.
- **Loading**: on first start, after a restore (the copy's id in `_meta` is gone, or the copy is ahead of the
  database), after a journal gap or falling behind, every table is rebuilt from one reader snapshot at ts `S`; when a
  table's shape changes, that table alone. Documents are staged as JSON text (Appender) and converted by DuckDB in
  `INSERT … SELECT` chunks (`TRY_CAST`, `json_transform`: a value that doesn't fit reads as NULL), then the table is
  swapped in atomically with its applied ts; tables at different ts refuse queries until they are level again.
- **Applying**: batches of journal rows (every 250 ms, up to 2000 commits) keep each document's final state; per table
  a delete of the written ids plus an insert of their new states, in one DuckDB transaction that also records the
  applied ts. A query runs on one DuckDB snapshot, so it sees whole commits, and reports `as_of_ts`.
- **Columns come from the Rust types.** `#[table]` / `#[object]` emit a shape (`ferrix::shape`: field names as serde
  writes them, and their types; a type without `HasShape` is JSON rather than a compile error). Mapping: strings and
  `Id<T>` → VARCHAR, `i8`..`i64` → BIGINT, `u64` → UBIGINT, floats → DOUBLE, bool → BOOLEAN, `Bytes` → BLOB,
  `Option<T>` → nullable, nested objects → STRUCT (declaration order), `Vec<T>` → LIST, unit enums → VARCHAR,
  anything else → JSON (an open shape adds `_extra` JSON). System fields `_id`, `_creationTime` and `_ts` come
  first. A changed shape (a fingerprint per table) rebuilds that table.
- **What is copied**: every user table, and the system tables `_users`, `_accounts`, `_storage`, `_scheduled_jobs`,
  `_crons`, `_workflows`, `_workflow_steps`, `_migrations`. Never the ones with secrets (credentials, admins, sessions,
  refresh tokens, verification codes, settings) or the audit log: SQL in actions could otherwise read them.
- **The sandbox**: only a single `SELECT` (CTEs included; DuckDB's `json_serialize_sql` decides) runs, in a
  transaction that is rolled back; `enable_external_access = false` (no files, no HTTP: `read_csv('/etc/passwd')`
  must fail), no extension autoloading, `lock_configuration = true`; a memory limit (`--olap-memory`), a thread
  limit and a timeout (`--olap-timeout`, enforced with DuckDB's interrupt). Not available in queries or mutations:
  results lag the data and are not reactive.
- **Operations**: the copy is derived data in `<data dir>/olap.duckdb`; backups and replication skip it and a
  restore rebuilds it. Its state, lag (commits and seconds), size, rows applied and rebuilds are in
  `GET /api/admin/olap` (`POST /api/admin/olap/rebuild` starts over), `/metrics` (`ferrix_olap_*`), the stats endpoint
  and the dashboard Overview. `TestApp::olap_sync()` brings the copy up to date, so tests are deterministic.
- **Built since**: the dashboard's Analytics page (SQL on the same sandbox, admin only, audited) with CSV and Parquet
  downloads written from Arrow batches (the sandbox has no file access, so not `COPY … TO`); live in-app aggregates
  are a separate, reactive feature (§7.7).

### 7.7 Live aggregates (built in Phase 12)

Counts and sums an app shows live (unread badges, totals per customer, votes per post, a leaderboard of sums) without
scanning documents on every rerun. Declared on a table, maintained **inside every write's own transaction**, read in
queries like an index point lookup, and reactive with invalidation narrowed to the groups a write changed.

```rust
#[table(name = "orders")]
#[aggregate(by_customer, group = [customer], sum = [amount, quantity])]
#[aggregate(total, sum = [amount])]                      // no group: one row for the whole table
pub struct Order { pub customer: String, pub amount: f64, pub quantity: i64, pub day: String }

#[query]
pub async fn customer_total(ctx: &QueryCtx, customer: String) -> Result<(u64, f64)> {
    let a = ctx.db.aggregate(Order::by_customer).get(|g| g.eq(Order::customer, customer)).await?;
    Ok((a.count, a.sum(Order::amount)))
}
// .sum_over(|g| ...prefix eq, then a range on the next group field...) combines the groups in a range;
// .groups(|g| ..., Order::Desc, n) lists groups in key order; .total() adds up every group.
```

- **Storage**: a raw SQLite table `aggregates (agg_id, key, count, sums)` keyed by `(agg_id, key)`, `key` the
  memcomparable encoding of the group values (like an index key without the document id). Aggregate ids come from
  the index id space, like search indexes, so read sets and the write log need nothing new.
- **Maintenance**: where a write adds and removes index entries, it also computes each aggregate's group and values
  for the old and the new document and applies the difference (same group: sums change; another group: the old one
  loses the document, the new one gains it). A group whose count reaches zero is deleted. Each changed group adds a
  write-log entry `(agg_id, key)`: only queries that read that group (or a range containing it) rerun. Sums of
  integer fields are exact (`i64`, checked); float fields add and subtract `f64`s (a long-lived running total can
  accumulate rounding; `rebuild` recomputes). A null or missing field adds nothing; `count` counts documents.
- **Reads**: `get` records a point read of one group key, `sum_over` / `groups` a range read. Missing groups read as
  zero. Works in queries and mutations (a mutation reads its own writes).
- **Definitions**: `#[aggregate(name, group = [fields], sum = [fields])]` on `#[table]`: group fields any indexable
  fields, sum fields numeric (`i*`, `u*`, `f*`, optionals of them; checked at compile time). Stored in the catalog
  with a fingerprint; a new or changed aggregate is backfilled from the documents at startup, like an index.
- **Derived data**: backups carry it; restores, imports, point-in-time replays and migrations go through the write
  path, which maintains it.

---

## 8. Operations

- **Data dir**: `./fx_data/{data.db, aux.db, storage/, backups/}`, overridable with `--dir`.
- **CLI**: `serve [--http 0.0.0.0:8090] [--https domain] [--dev]`, `codegen`, `migrate`, `admin create|reset-password`, `backup`, `restore`, `export --format jsonl`, `import`.
- **Backups**: the SQLite online backup API (or `VACUUM INTO`), on demand or on a schedule, optionally uploaded to S3. Continuous replication: a logical change journal written in each commit's transaction, shipped in segments with periodic snapshots, for point-in-time restores (Litestream alongside works too).
- **Deploy**: one static binary (target `x86_64-unknown-linux-musl`), a systemd unit, a `FROM scratch` Docker image, and an optional `--https` mode with automatic TLS.
- **Serving the frontend**: `.static_dir(...)` or `.embed_static::<Assets>()` serves the SPA from the same binary with SPA fallback routing, so an entire app ships as one binary.
- **Observability**: `tracing` to stdout as JSON in production, optional Prometheus `/metrics`, and dashboard metrics read from `aux.db`.
- **Config**: CLI flags, then env vars (`FERRIX_*`), then `_settings` (set in the dashboard).
- (As built, `crates/ferrix/src/ops`, docs/CONTRACT.md §4.4, README "Deploying": backups are `.tar.zst` archives of a
  `VACUUM INTO` snapshot plus, on request, `aux.db`, local file blobs and `auth_secret.key`, kept in `<dir>/backups`,
  any directory or S3 (`--backup-to`); scheduled with `--backup-every/--backup-keep` as the cron `ferrix-backup`;
  `restore` is offline behind `<dir>/ferrix.lock` and validates before swapping. `export` writes JSONL/CSV from one
  snapshot, `import` loads JSONL or CSV through the write path; a restore can be staged from the dashboard and is
  applied at the next start. `/metrics` is off unless `--metrics`. `--https <domain>` uses
  `rustls-acme` (TLS-ALPN-01, cache in `<dir>/acme`), `--tls-cert/--tls-key` takes files (reloaded when they
  change). `scripts/release.sh` builds the static musl binary with cargo-zigbuild; `Dockerfile` (`FROM scratch`),
  `deploy/ferrix.service`, built in CI by `.github/workflows/release.yml`. Logs are JSON lines off a terminal
  (`--log-format`); `_settings` holds the runtime auth settings. Continuous replication (`ops::replicate`,
  `db::journal`): `serve --replicate-to` journals every commit's document changes into `_journal` inside its own
  transaction, ships them every `--replicate-every` as `generations/<g>/journal/<from>-<to>.zst` and takes snapshots
  every `--snapshot-every` (`generations/<g>/snapshots/`), with `--replicate-keep` retention; `restore --from
  <target> [--to-ts | --to-time]` and `POST /api/admin/restore/point-in-time` rebuild a point from a snapshot plus the
  replayed journal.)

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

### Phase 1 — Core engine (L) — done
- [x] Full value model in the key encoding, with proptests, including `ferrix::Bytes` (`{"$bytes": "<base64>"}`, its
  own tag between String and Array; the encoding is versioned in `_meta` and older databases get every index rebuilt).
- [x] `#[table]` with typed field and index handles, catalog reconciliation, and index backfill.
- [x] Validating existing documents against a changed schema at startup (only tables whose schema fingerprint changed; `--skip-schema-check` escape hatch).
- [x] `#[migration]`: batched, resumable, exactly once per run, dry runs, `_migrations`, `ferrix migrate`,
  `serve --migrate`, admin system functions, and lenient startup for tables with pending migrations (CONTRACT §1.9).
  The dashboard's Migrations page (live progress, run / cancel / restart, dry runs with examples).
- [x] Migration dependencies: `#[migration(after = ["0001_x", ..])]`, a DAG validated at startup (unknown names,
  cycles, duplicates), topological run order with name order as the tie-break everywhere (`ferrix migrate`,
  `serve --migrate`, `run_pending_migrations`, "Run all pending"); running one before its dependencies are done is
  refused (`dependencies_not_done`) unless forced; `list` and the Migrations page show the dependencies.
- [x] Query builder: `get`, `with_index` ranges, `order`, `filter`, `take`, `first`, `unique`, `collect`, and `paginate` (opaque cursors over the index key space, `endCursor` pinning, read sets narrowed to the scanned span, a 10k-document scan cap per page).
- [x] Mutations: `insert`, `patch`, `replace`, `delete`. Every write is validated against the table type (`patch` checks the merged document; `insert`/`replace` check that the stored JSON round-trips).
- [x] Actions with `run_query` / `run_mutation` / `run_action`, internal functions, and `UserError`.
- [x] `TestApp` harness.
- [x] A mutation timeout (`AppOptions::mutation_timeout`, `--mutation-timeout`, default 5s; enforced at await points only).

### Phase 2 — Sync and reactivity, done properly (L) — done
- [x] Per-connection sync worker (protocol state), with one consistent snapshot ts per refresh, write-log catch-up, coalescing and concurrent reruns (now in the shared manager below).
- [x] **Shared subscriptions**: each distinct query (path, args, identity scope from its `ctx.auth` reads) runs once per invalidation and is fanned out to every subscriber, in rounds at one snapshot ts (§4.5); per-socket mailboxes that coalesce for slow sockets; a model-based test with overlapping query sets, a dedup proof (50 sockets, one execution per commit) and a benchmark (`tests/it/bench.rs`).
- [x] **Pipelined generations** instead of a round barrier (§4.5): runs on runner tasks, versioned results, a slow-query lane, updates at a run's ts, holds for starving connections; slow queries in the model test and liveness tests. Reader connections scale across threads (SQLite built without `SQLITE_ENABLE_MEMORY_MANAGEMENT` and memstatus; `.cargo/config.toml`).
- [x] Interval-tree invalidation index (per `index_id`, property-tested against the linear scan).
- [x] The full protocol: versions, atomic transitions, mutation `ts`, read-your-writes, reconnect and resubscribe.
- [x] Mutation idempotency on reconnect, keyed by `(sessionId, requestId)` and recorded in the mutation's own transaction.
- [x] HTTP API fallback.
- [x] Model-based reactivity test suite.
- [x] Execution logging to `aux.db` (`App::logs()`: query, live tail, per-function stats), and forwarding log lines (`logLines`) to clients in `--dev`.

### Phase 3 — Clients and codegen (L) — done
- [x] `ferrix codegen --out` producing `api.ts`, `dataModel.ts` and `index.ts`; auto codegen in `serve --dev --codegen-out` (or `FERRIX_CODEGEN_OUT`).
- [x] `@ferrix/client` core with optimistic updates.
- [x] Reactive pagination: `client.paginatedQuery` (pins each page's `endCursor` once a next page follows it), `usePaginatedQuery` for React and Svelte, and the `insertAtTop` optimistic helper.
- [x] `@ferrix/react` and `@ferrix/svelte`.
- [x] `examples/todo` (Svelte), with Playwright e2e scripts, and `examples/chat-react` (React; Phase 10).
- **Milestone reached: usable for a real (unauthenticated) app.**

### Phase 4 — Dashboard v1 (L) — done
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
- [x] Audited dashboard writes: `_audit_log` written in each write's own transaction (generic writes,
  `revoke_session`, `delete_user`, settings, impersonation, `run_function`), `_system/audit:list` (live, filterable),
  90-day retention (docs/CONTRACT.md §4.1.5), and the dashboard's Audit log page (live, filters, field diffs).
- [x] Impersonation via a real session: `run_function` "as user" runs through a short-lived, audited session marked
  `impersonatedBy`; `POST /api/admin/impersonate` mints one to open the app as a user (§4.2), from the Users page's
  "Open app as user" (a new tab, with a warning and the expiry).
- [x] Dashboard v2: a virtualized grid (10k+ rows, pages load as it scrolls, still live), a quick filter over loaded
  rows, the search box (`search_documents`), JSONL/CSV export, per-function latency and calls/errors charts
  (`GET /api/admin/stats/timeseries`, `Logs::timeseries`), and the Settings, Audit log, Migrations and Backups pages;
  grouped navigation, a Ctrl+K command palette, Alt+1…0. Covered by `packages/dashboard/e2e/dashboard.ts`.

### Phase 5 — Auth (M) — done
- [x] Password (argon2id), email verification, password reset, magic link and OTP, sessions with rotating refresh
  tokens and reuse detection, SMTP (`lettre`; printed to the log in `--dev`), rate limits, `ctx.auth` (docs/PROTOCOL.md
  "Auth", docs/CONTRACT.md §1.2 and §1.5).
- [x] OAuth with PKCE: GitHub and Google presets, generic OIDC via discovery (flows tested against mock providers).
- [x] The presets checked against the real providers (no full sign-in: that needs registered apps and a browser):
  `live_presets_match_the_providers` (`--ignored`, network) compares endpoints, scopes, PKCE, client authentication
  and signing algorithms with Google's, Apple's and Discord's discovery documents and probes GitHub's endpoints; the
  JWKS the three published on 2026-10-11 are fixtures that must parse (Discord's key is 4096-bit RSA).
- [x] Discord and Sign in with Apple presets (`Discord::from_env()`, `Apple::from_env()`, also as `_settings` kinds
  with the Apple `.p8` key encrypted at rest): Discord's verified email only; Apple's ES256 client secret (generated,
  cached, rotated), `form_post` callback (`POST`, state cookie `SameSite=None; Secure` on https), the ID token verified
  with the JWKS code (iss, aud, exp, nonce), the name of the first sign-in (docs/CONTRACT.md §1.5; mock-provider tests
  in `tests/it/oauth_presets.rs`).
- [x] Client integration: `getAuth(client)` (`signIn`/`signOut`/…, refresh wired into `setAuth`, cross-tab sync),
  React `useFerrixAuth` / `useAuthActions` / `useCurrentUser`, Svelte `useAuth` / `useCurrentUser`.
- [x] The dashboard Users page (users, linked providers, sessions, revoke, delete; "act as user" in the runner).
- [x] Provider configuration in `_settings` (OAuth providers, SMTP, redirect allowlist, sign-ups, token lifetimes,
  CORS origins), secrets encrypted at rest (XChaCha20-Poly1305, key from `auth_secret.key`), applied without a
  restart, config wins over settings; admin-only `_system/settings:get|update|send_test_email` (docs/CONTRACT.md
  §4.1.4), and the dashboard's Settings page (locked fields read-only, write-only secrets, inline validation, test
  email).
- [x] External JWTs via JWKS (`AuthConfig::jwt(JwtProvider)`: RS256/ES256/EdDSA, cached JWKS with rate-limited
  refetch on unknown `kid`, iss/aud/exp/nbf with leeway, subjects provisioned as `_users` + `_accounts`), on WS and
  HTTP Bearer (§1.5).
- [x] Live revocation: sockets and the admin log stream end with `AuthError` / `event: revoked` as soon as their
  session (or, for external tokens, user) is deleted (docs/PROTOCOL.md guarantee 10).
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
- [x] Canceling a running action: the job commits as `canceled` (reason recorded), the action is signaled after the
  commit (`ActionCtx::is_canceled` / `canceled()`) and aborted at its next await after `cancel_grace` (5s);
  committed nested mutations stay, an in-flight one rolls back; never retried. `ctx.scheduler.cancel` /
  `cancel_with_reason` and the dashboard's `cancel_job` work for actions in progress.
- [x] Per-cron time zones (`cron_tz` / `CronSpec::timezone`, `chrono-tz`): spring-forward gaps run later by the gap,
  fall-back folds run a time of day once (every-hour expressions in both hours); `_crons.timezone`.
- [x] "Run cron now": `_system/scheduler:run_cron_now { name, force? }` (admin; no pile-up unless forced), with the
  dashboard's "Run now" button (offering to force while the previous run is unfinished), time zone and system
  columns, and cancel for running actions.
- [x] Workpools (beyond the roadmap): `Ferrix::workpool(Workpool::new(name)
  .max_parallelism(n).retries(r).backoff(..))`, `ctx.scheduler.pool(name)…enqueue/run_after/run_at`, a pool's own
  slots (a full pool holds up no other), `on_complete::<mutation>` run exactly once with the outcome, `cancel_pool`.
- [x] Durable workflows (beyond the roadmap): `#[workflow]` bodies of steps (`run_query` / `run_mutation` /
  `run_action` with retries and pools, `sleep`, `wait_for_event`, parallel with `try_join!`), replayed from the
  `_workflow_steps` journal in a mutation as each step finishes; per-step determinism checks, at most 10k steps; the
  dashboard's Workflows page (docs/CONTRACT.md §1.7).

### Phase 7 — File storage (M) — done
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
- [x] Signed, expiring URLs (`ctx.storage.get_signed_url(id, ttl)`: HMAC with a key derived from the storage key;
  exact expiry in mutations and actions, rounded up to a multiple of the TTL in queries so results stay
  deterministic) and private mode (`StorageConfig::private`, `--storage-private`: unsigned reads get 403).
- [x] Image thumbnails (`?thumb=WxH&fit=contain|cover`, JPEG/PNG/WebP/GIF via the `image` crate): header-checked
  pixel limit before decoding (decompression bombs), decoder limits, size limits or an allowlist, cached under
  `_thumbs/<id>/`, per-variant ETags, deleted with the file and swept as orphans.
- [x] Multiple-range responses (`multipart/byteranges`, coalesced, at most 16 ranges).
- [x] The orphan sweep runs as the system cron `_storage_sweep` (internal action `_system/storage:sweep_orphans`),
  replacing the ad-hoc background task.
- [x] Dashboard: downloads through `downloadUrl` and thumbnails added with `&` to signed URLs (private mode works),
  `?thumb=96x96&fit=cover` previews in the Files list.
- [x] Direct downloads (beyond the roadmap): `ctx.storage.get_download_url(id, ttl)` presigns a SigV4 URL to the S3
  bucket (signed content type, disposition and cache control; 1 s to 7 days), or the server's own signed URL with local
  storage. Signed URL expiries in queries count from the app's clock, not the snapshot's (which may be hours old).

### Phase 8 — Search (M) — done
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
- [x] Search pagination: `SearchQuery::paginate` (and untyped `RawSearchQuery::paginate`) with the same
  `PaginationOpts`/`PaginationResult` and pinned-`endCursor` behaviour as index pagination, so `usePaginatedQuery`
  works. Relevance is a per-document BM25 (no corpus statistics), so `(score, rowid)` is a stable ordering key:
  cursors are positions in it (fingerprinted by index, terms and filters), pinned pages partition the matches at
  every snapshot, and only written documents move across changes (docs/CONTRACT.md §1.8 "Pagination").
- [x] Per-filter-value read sets: synthetic keys `[filter column][encoded value][doc id]`; a search with `eq`
  reads only its value's prefix (old and new values are written), unfiltered searches the full range. Randomized
  soundness test (no missed change) plus a pinned-pages partition test.
- [x] Configurable tokenizers: `tokenizer = "unicode61" | "porter" | "trigram"`, `language = "english"` (porter),
  `diacritics = "remove" | "keep"`; validated by the macro (trybuild) and at startup; changing one rebuilds the index.
- [x] Approximate vector indexes: `kind = "hnsw"`, an in-house HNSW graph (no crate fit: deletes, a validated
  persisted form, concurrent search + insert) on top of the `vec0` table: updated after commit from the write log,
  tombstones plus background rebuilds, post-filtering with oversampling (exact below 4096 matches), persisted next to
  `data.db` for ≥ 10k vectors and validated against the table at startup. Recall@10 ≥ 0.9 asserted against exact
  search; 200k × 384: built in 9–25 s on 16 threads, ~0.4 ms per query at recall ≈ 0.95 (embedding-like data).
- [x] Backend for a dashboard search box: admin-only `_system/dashboard:search_documents` (docs/CONTRACT.md §4.1).
- [x] The dashboard's search box (live, paginated; the index, tokenizer and filter fields shown) and tokenizer /
  HNSW details in the schema view.
- [x] Stemmers for other languages: `tokenizer = "snowball", language = "romanian" | "french" | "german" | …` (18
  languages, `rust-stemmers`), a custom FTS5 tokenizer wrapping `unicode61`, registered on every connection by the
  auto extension (readers included); accents kept for stemming, then folded, with the unaccented spelling's stem as
  a colocated synonym; `ferrix_score` unchanged. Validated by the macro and at startup; a language change rebuilds.
- [x] Vector search in queries: `ctx.db.vector_search` (exact `vec0` scan at the snapshot, also on HNSW indexes, so
  deterministic), reactive with per-filter-value read sets (one prefix per `or` branch; vector entries write the same
  synthetic keys as search entries); randomized soundness test and live updates over WebSocket.
- [x] Tunable HNSW parameters: `m`, `ef_construction`, `ef_search` per index (validated), `VectorSearchOpts::ef(n)`
  per search; changing `m` or `ef_construction` rebuilds the graph and replaces its saved file.
- [x] HNSW robustness: a node is linked on every layer before neighbours link back to it (concurrent inserts under
  CPU contention used to leave ~0.2% of the nodes unreachable), and the background thread is stopped and joined
  when the `Database` drops.

### Phase 10 — DX fixes found by `examples/chat-react` (M)
- [x] Index keys end in `_creationTime, _id`, so `eq(channel)` + `order(Desc)` is newest first; rebuild
  existing indexes on upgrade. Key encoding v3: a v2 database rebuilds its ordinary indexes on open (search and
  vector indexes keep their layout); cursors hash the version, so old ones are rejected. `_creationTime` can be
  bounded after an eq on every declared field; listing a system field in `#[index]` stays a compile error.
- [x] Codegen names top-level output structs (`Array<MessageView>`, not an inlined object), and field doc comments
  render cleanly (JSDoc on their own lines, one field per line). Only table types and args structs are inlined.
- [x] A derive/attribute for non-table output types (serde + ts-rs + camelCase, via ferrix re-exports), so apps
  don't depend on serde/ts-rs directly: `#[ferrix::object]`; `#[serde(flatten)] doc: Doc<T>` embeds a document
  (TS `{ … } & Doc<"t">`).
- [x] `ctx.is_dev()` / app-level dev flag; dev-only functions left out of the public `api`; the built-in dev mailer
  is composable with a custom `Mailer`. (`#[query(dev_only)]` etc., refused outside `--dev`, generated into
  `devApi`; `DevLogMailer` and `AuthConfig::dev_mailer`.)
- [x] `@ferrix/react`: `useAuthActions().providers()`, re-export `AuthProviders`; `usePaginatedQueryWithStatus`
  (non-throwing). Svelte: `useAuth().providers` (its paginated query already returned `error`).
- [x] Storage URLs root-relative in `--dev` (so they go through a dev proxy) unless a public URL is set explicitly.
- [x] `PaginationResult::map_async` / `try_map`; `Query::count()` over an index range; `TestApp::create_user`.
  (Also `try_map_concurrent` and `TestApp::as_user`. The async maps take closures returning futures rather than
  `AsyncFnMut`: the futures of borrowing async closures aren't provably `Send` yet.)
- [x] Docs: optimistic updates are replayed on every server change (create ids/timestamps outside the closure);
  scheduled functions run without identity (pass the user explicitly); `ctx.now()` in queries is not reactive.
  (README "Things to know", CONTRACT §1.10, with coarse-invalidation tips.)

- [x] A dev-mode determinism check (§11): `AppOptions::determinism_check` (on in `serve --dev` and `TestApp`, every
  10 s per query) reruns a query in the same snapshot and warns, in the server log and as a log line of the run, where
  the two results differ. Runs that read the time are skipped. The whole suite runs with it on: none of ferrix's own
  queries trips it.

### Phase 9 — Operations polish (M) — done
- [x] Backup and restore: online `VACUUM INTO` snapshots as `.tar.zst` (optional `aux.db`, local blobs, and
  `auth_secret.key` only with `--include-secret`), `ferrix backup`, scheduled backups with rotation to a directory or
  S3 (`--backup-every/--backup-keep/--backup-to`, the cron `ferrix-backup`), an offline `ferrix restore` that refuses
  while a server holds the data dir and validates (checksums, integrity check, schema, `verify_indexes`) before
  swapping, and admin endpoints to list, create, download and delete backups.
- [x] Export and import: `ferrix export` (JSONL/CSV, one snapshot, `files/` for blobs), `ferrix import` (JSONL through
  the write path in validated batches; ids and creation times kept, ids rewritten when table numbers differ;
  insert/upsert/replace), streaming `GET /api/admin/export/{table}` and `POST /api/admin/import/{table}`.
- [x] Prometheus `/metrics` (off by default, `--metrics`, `--metrics-token`).
- [x] `--https <domain>` (Let's Encrypt via `rustls-acme`, HTTP→HTTPS redirect, `--acme-email`, `--acme-staging`,
  cache in `<dir>/acme`) and `--tls-cert/--tls-key`; graceful shutdown on SIGTERM.
- [x] Static frontend serving: `.static_dir()`, `.embed_static::<A>()`, `.routes(axum::Router)`; SPA fallback,
  caching, precompressed files. The todo example embeds its web app.
- [x] Release builds: `scripts/release.sh` (static `x86_64-unknown-linux-musl` via cargo-zigbuild, ~20 MB),
  `Dockerfile` (`FROM scratch`), `.dockerignore`, `deploy/ferrix.service`.
- [x] Migrations CLI (`ferrix migrate`, `migrate status`, `serve --migrate`; Phase 1's `#[migration]`). Running them
  (not `status`) takes the data directory lock like `import`.
- [x] Dashboard UI for backups (create, download, delete; target and schedule), export/import, and the live counters
  of `--metrics` on the Overview (commits/s, queue depth, shared queries, reruns; served by `/api/admin/stats`).
- [x] JSON logs: `--log-format json|pretty|compact` (`FERRIX_LOG_FORMAT`; JSON lines by default off a terminal), with
  the function log capture unchanged (docs/CONTRACT.md §4.4).
- [x] Restoring from the dashboard: staged restores (validated like `ferrix restore` while the server runs, applied at
  the next start before the database opens, crash-safe and checksummed, cancelable), `--restart-exit-code` for a
  supervised "Restart now" (docs/CONTRACT.md §4.2, §4.4).
- [x] CSV import (`ferrix import --format csv`, `format=csv` on the admin endpoint, the dashboard), cells typed with
  the table's shape; round-trips the CSV export.
- [x] Hot reload of `--tls-cert`/`--tls-key` (`--tls-reload-every`, atomic swap, a bad pair keeps the current one).
- [x] CI: `.github/workflows/release.yml` (gates, the musl release, the Docker image build and smoke test, pushes on
  tags). Its first GitHub run (2026-10-11) failed in `cargo test`: opening a database while another connection to it
  closed failed with "database is locked", because the FTS5 extension runs SQL during the open, before any busy
  timeout (fixed: it sets one). Later runs found three more: the runner's disk filled up (freed, and line-tables-only
  debug info), the writer's WAL-restarting checkpoint was dropped when it collided with the thread's (fixed), and the
  chat e2e once stalled scrolling back through 200 messages (now a 20 s wait that reports where it stopped, with
  screenshots; the same scroll took under 1 s on the next run, so a rare race in the example's infinite scroll may
  remain). All three jobs, Docker included, passed on GitHub on 2026-10-11.
- [x] Incremental, continuous backups with point-in-time restore: a logical change journal (`_journal`, written in
  each commit's own SQLite transaction: exactly once, never a rolled-back write) shipped as checksummed zstd segments
  named by ts range to a directory or S3 (`--replicate-to`, `--replicate-every`), periodic snapshots
  (`--snapshot-every`), retention (`--replicate-keep`), generations for restored or diverged databases, a bounded
  local backlog while the target is down (`--replicate-max-pending`); `restore --from [--to-ts | --to-time]`, the
  staged `POST /api/admin/restore/point-in-time`, the lag in `GET /api/admin/replication`, `/metrics` and the
  dashboard (docs/CONTRACT.md §4.4). Litestream remains an alternative.
- [x] An interval-tree invalidation index (done in Phase 2).
- [x] `ferrix new` (`crates/ferrix-cli`, the `ferrix` command): creates an app (crate, sample table and functions,
  React/Svelte/no web app) whose `.cargo/config.toml` carries the SQLite build flags, since a dependency can't set
  them. Depends on the checkout it was built from (path dependency, @ferrix packages packed into `web/vendor`) or on
  the published versions. Tested end to end by `cargo test -p ferrix-cli --test new -- --ignored`.

### Phase 11 — Analytics (OLAP) (L) — done
Design: §7.6. Each step ends with tests and docs (README, docs/CONTRACT.md).
- [x] **Spike (S), go/no-go: go** (2026-10-11, branch `spike/olap`; `tests/olap_spike.rs`, `olap_probe`). DuckDB 1.5.6
  (`duckdb` 1.10506.0, `bundled` + `json`) builds and runs in the static musl release binary: 57.0 MB with `olap`
  against 24.7 MB without (+32 MB; nothing calling DuckDB, the linker drops it all, so the feature costs nothing
  unused); a clean musl build takes about 9 minutes with it, 3 without, and a rebuild 2.5 minutes. Measured (release,
  16 threads): the Appender loads 1M rows in 2.0 s (~510k rows/s); a batch of 1k changes (delete the ids, append the
  new states, one transaction) takes ~50 ms; group-bys over 1M rows take 2–14 ms (342 ms bucketing by day through
  `to_timestamp`); 24 MiB on disk. A second connection only ever sees whole transactions. The lockdown settings
  (`enable_external_access = false`, extension autoload off, `lock_configuration`) block files, HTTP, `ATTACH`,
  `COPY TO` and `INSTALL`, but not writes, and `prepare()` accepts several statements: the SELECT-only check is
  `json_serialize_sql`, which rejects anything but one SELECT (CTEs pass). `interrupt()` stops a query within
  milliseconds. Still to check in the sandbox step: side effects reachable from a SELECT (`query()`, `nextval`,
  `setseed`, `checkpoint()`), and that the check sees exactly the string that runs.
- [x] **Shapes from the macros (M)**: `TableDef::shape` and `ferrix::shape` (`Shape`, `HasShape` for the std types,
  `Id<T>`, `Bytes`, `Doc<T>`; `#[object]` implements it for non-generic types; anything else is JSON through autoref
  specialization, so no type fails to compile); serde's `rename_all` / `rename` / `skip` / `skip_serializing_if` /
  `flatten` / `serialize_with` followed; recursion guarded. `Shape::Text` rather than `String`: an enum variant named
  `String` made rustc print `std::string::String` in every diagnostic. Macro snapshots and a runtime test.
- [x] **The copy (M)**: `crate::olap` (`columns`, `engine`, `values`): journal consumers (`journal::release`, the
  replicator switched over; journaling stays on for the copy when the replicator gives up), rebuilds (all, or the
  tables whose fingerprint changed) staged as JSON and converted by DuckDB, batched apply with per-table applied ts,
  restore detection through the copy id in `_meta` (scrubbed by restores), the fall-behind budget, the system-table
  allowlist; unit enums of system tables as text. Tests (`tests/it/olap.rs`): typed values, a model test against the
  live data (inserts, patches, deletes in many batches), restarts, a restore, a journal gap, a shape change, falling
  behind, rebuilding while writes continue, replication and the copy sharing one journal.
- [x] **`ctx.olap` in actions (S–M)**: `query`, `query_as::<T>`, `at_least(ts)`, `latest()`, positional parameters
  (`OlapParams`); the sandbox; row cap, concurrency limit, timeout via interrupt; user errors `olap_unavailable`,
  `olap_query_failed`, `olap_timeout`; `TestApp::olap_sync`. Tests: writes, DDL, `SET`, `PRAGMA`, `ATTACH`, `COPY`,
  `INSTALL`, `CHECKPOINT`, `query('DELETE …')`, `read_csv` / `read_text` / `glob` of files, HTTP and `getenv` all
  rejected; nothing written; secrets absent; limits; read-your-writes in the background.
- [x] **Operations and release (S)**: `--olap`, `--olap-memory`, `--olap-threads`, `--olap-timeout`, `--olap-max-lag`;
  `GET /api/admin/olap`, `POST /api/admin/olap/rebuild`; `ferrix_olap_*` metrics; `olap` in the stats endpoint and an
  Analytics card on the Overview; the `olap` CI job; the `todo-server-olap` release binary (smoke-tested with
  `serve --olap`); the todo example's `todos:stats_by_day`.
- [x] The dashboard's Analytics page (the copy's state, tables and columns, a SQL editor with results, rebuild) on
  `POST /api/admin/olap/query` (the same sandbox, audited), with CSV and Parquet downloads of whole results
  (`OlapService::export`; Parquet from DuckDB's Arrow batches through the `parquet` crate, since the sandbox has no
  file access; up to 10M rows in 5 minutes). The schema view lists each table's analytics columns.
- Live in-app aggregates: Phase 12.
- [x] Deletes on big tables (checked 2026-10-11, no change): a batch of 1k changes on an in-memory table takes 4.1 ms
  at 1M rows and 6.8 ms at 5M with the semi-join on `_id` the copy uses; an ART index on `_id` makes it slower (6.3
  and 9.3 ms, with literal `IN` lists 8.1 and 8.5 ms) and bulk loads ~25% slower. DuckDB's vectorized scan of one
  column wins; the spike's 50 ms came from a `PRIMARY KEY` on a file-backed table, which the copy doesn't use.

### Phase 12 — Live aggregates (L) — done
Design: §7.7.
- [x] Catalog and storage: `TableDef::aggregates` (`AggregateDef`), `_aggregates` and `aggregate_values`, ids from
  `0xC000_0000`, reconciliation (new or changed: rebuilt from the documents; removed: deleted), rebuilt on a key
  encoding change, recomputed by `verify_indexes` (`db::aggregate`).
- [x] Maintenance in `WriteState::write` next to the index entries (so migrations, imports and point-in-time replays
  maintain them too); a write that changes no group key or summed value touches nothing; a write-log entry per
  changed group; exact integer sums (falling back to `f64` past `i64`), `f64` once a float joins.
- [x] Reads: `ctx.db.aggregate(T::name)` with `get`, `sum_over`, `total`, `groups(range, order, limit)`, reusing the
  index range builder over the group fields; `sum`, `sum_i64`, `average`.
- [x] `#[aggregate]` in `#[table]` (shared name namespace, field checks, `assert_sum_field`), the mock, snapshots and a
  UI test of the errors.
- [x] Tests (`tests/it/aggregates.rs`, `websocket::aggregate_subscriptions_update_per_group`): values, ranges and
  listings, a model test against the documents, read sets narrowed to the changed groups, live updates over a socket
  only for the changed group, rollbacks, mutations reading their own writes, backfill, redefinition, removal and
  tamper detection; `TestApp::query_run` (a query with its read set). The dashboard's schema view lists aggregates.
### Performance check after Phases 11–12 (2026-10-11)
Release builds, same machine, today's start (`be3fd1c`) against the result, alternating: `bench_writes` (every
scenario within ~1%: 44.6k against 44.3k one-insert mutations/s at 16 writers, replication 34.7k against 34.5k,
with 100 subscribed clients 38.9k against 39.2k), `bench_subscriptions` (identical executions per commit, latencies
within noise), `bench_parallel_reads` (76.3k against 75.8k queries/s at 8 readers), `bench_journal_overhead` (-2 to
-4% for journaling on both sides). The new paths (`bench_features`):
- Aggregates: one-document inserts 46.9k/s without, 37.2k/s with three aggregates (~2.2 µs per aggregate per
  write); one group of 10k documents read in ~16 µs instead of 10.2 ms by counting and summing them. A
  one-statement upsert adding the sums in SQL (`json_each`) was slower (27.5k/s) and was not kept.
- `--olap`: 47.4k inserts/s without, 43.7k with the journal alone, 42.7k with the copy following (p99 0.47 → 0.59
  ms); the copy is current ~20 ms after writes stop. Found and fixed on the way: the copy parsed journal rows on the
  async threads the writers' tasks run on, released (trimmed) the journal after every batch (each a writer-lock
  transaction), and sized rows with `length()`, which reads the text; now parsing runs on a blocking thread,
  releases happen once a second in chunks of 2048 commits, and sizes come from `octet_length()` (p99 was 0.8 ms).

---

## 11. Risks and open questions

| Risk | Mitigation |
|---|---|
| **Proc-macro ergonomics.** Async fns with references in ctx, typed field and index handles, and good error messages are hard to get right. | Phase 0 exists to prove this first. Keep the macros thin and the generated code readable. Use `trybuild` tests for compile errors. |
| **Rust compile times hurt the dev loop.** Every backend change means a rebuild. | Keep the `ferrix` crate precompiled as a dependency so only the app crate rebuilds. Use `mold`/`wild` as the linker, `cargo watch`, and seamless client reconnect. This is accepted as the cost of framework mode. |
| **Non-deterministic queries** (`SystemTime::now()`, random numbers) silently break caching. | Documented contract, `ctx.now()`, and the dev-mode check (`AppOptions::determinism_check`, done) that reruns a query at the same `ts` now and then and warns if the result differs. |
| **Holding the writer lock during an async mutation.** | No network in `MutationCtx`, a timeout, and per-mutation duration metrics in the dashboard. Group commit keeps a known-slow mutation from holding back the batch before it, but not the writes queued behind it. The OCC upgrade path stays open. |
| **Reactive pagination correctness.** | Pin each page's end cursor and cover it with model-based tests. |
| **Schema evolution** with existing data. | Validate at startup and refuse to start on mismatch. Provide `#[migration]` batches and dev-mode dashboard links to bad documents. |
| **SQLite build flags.** rusqlite's bundled SQLite (`SQLITE_ENABLE_MEMORY_MANAGEMENT`, memstatus on) serializes reader connections on global mutexes. | The workspace sets `LIBSQLITE3_FLAGS` in `.cargo/config.toml`, and `ferrix new` gives every app the same file (a dependency can't set it for them); memstatus is also turned off at runtime. Apps created otherwise must add it (ferrix logs a warning at startup). A runtime-only fix would be a custom page cache (`SQLITE_CONFIG_PCACHE2`), or upstreaming a feature to `libsqlite3-sys`. |
| **DuckDB in a static musl binary** (C++, size, build time). | Phase 11 starts with a go/no-go spike; the feature is off by default; fallbacks are a glibc-only `olap` build or DataFusion over Parquet. |
| **SQL in actions reaching data or files it shouldn't.** | One read-only SELECT per call, `enable_external_access = false`, no extensions, locked configuration, limits and a timeout; secrets never copied; sandbox escape tests. |
| **The analytics copy holding back the live database.** | It only reads the journal; trimming gives up on a copy past its budget, which is rebuilt from a snapshot instead. |
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
