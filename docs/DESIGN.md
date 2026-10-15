# Design

Why RxFerrix is built the way it is: the decisions, how the core works, what was measured, and the known trade-offs.
For the exact API see the [API contract](CONTRACT.md), and for the wire format the [protocol](PROTOCOL.md).

## Decisions

| Area | Decision |
|---|---|
| Backend functions | Rust, as a library. An app depends on the `rxferrix` crate, writes `#[query]` / `#[mutation]` / `#[action]` functions, and its `main` builds one binary. No JavaScript runtime is embedded. |
| Data model | Tables of typed documents with secondary indexes and typed ids (`Id<T>`), stored in SQLite. |
| Schema | Rust structs are the source of truth. The dashboard can browse and edit data but not change the schema. |
| Transport | A WebSocket sync protocol for queries, mutations and actions, plus an HTTP JSON API for server-side rendering, scripts and webhooks. |
| Clients | A framework-agnostic TypeScript core with React and Svelte 5 bindings, and a typed `api` object generated from the functions. |
| Dashboard | A Svelte app embedded in the binary at `/_/`. It uses the same live protocol as apps. |

**Not goals:**

- Several nodes or horizontal scaling: one process, one SQLite database.
- Multi-tenancy or a hosted service.
- Running user code in JavaScript, WASM or a sandbox.
- Raw SQL on the live database from functions. Functions go through the document API so that reactivity stays
  correct. (Read-only SQL over the [analytics copy](/guide/analytics) is allowed in actions: that copy isn't
  reactive.)
- Offline-first sync. Optimistic updates are supported; offline persistence isn't.
- Declarative access rules. Authorization is written in function code.

## Architecture

```text
                         ┌──────────────────────── rxferrix binary (your app) ────────────────────────┐
 Browser / app           │                                                                           │
 ┌───────────────┐  WS   │  axum ──► sync worker (per socket) ──► function runtime                   │
 │ @rxferrix/react │◄─────►│   │        query set, auth, versions     ├─ QueryCtx    (read-only snapshot)
 │ @rxferrix/svelte│       │   │                                      ├─ MutationCtx (the writer)      │
 │ @rxferrix/client│  HTTP │   ├─ /api/{query,mutation,action}        └─ ActionCtx   (any I/O)         │
 └───────────────┘◄─────►│   ├─ /api/auth/*       ┌──────────────┐        │ commit(ts, writes)      │
                         │   ├─ /api/storage/*    │ subscription │◄── write log (recent commits)     │
 Admin browser           │   ├─ /_/ (dashboard)   │ manager      │──► invalidate ► rerun ► Transition│
 ┌───────────────┐       │   └─ your routes       └──────────────┘                                   │
 │ dashboard     │◄─────►│                                                                           │
 └───────────────┘       │  scheduler · auth · file storage · search (FTS5, sqlite-vec) · analytics  │
                         │                                                                           │
                         │  SQLite (WAL): data.db · aux.db (execution log) · storage/ or S3          │
                         └───────────────────────────────────────────────────────────────────────────┘
```

## Storage

All documents of all tables live in two SQLite tables, with a single version of each row (SQLite's WAL snapshots
give readers isolation, so no multi-version rows are needed):

```sql
CREATE TABLE documents (id BLOB PRIMARY KEY, table_number INTEGER NOT NULL, json TEXT NOT NULL, ts INTEGER NOT NULL);
CREATE TABLE index_entries (index_id INTEGER NOT NULL, key BLOB NOT NULL, doc_id BLOB NOT NULL,
                            PRIMARY KEY (index_id, key)) WITHOUT ROWID;
```

Why one generic layout instead of a SQLite table per RxFerrix table:

- Every index lookup is a byte-range scan over `index_entries`, so what a query read is just a set of byte ranges.
  That is what makes [reactivity](#reactivity) cheap and exact.
- Adding or removing an index needs no DDL, only a backfill.
- One code path serves every table, system tables (`_users`, `_storage`, `_scheduled_jobs`, …) included, so the
  dashboard and subscriptions treat them like any other.

**Index keys** are an order-preserving ("memcomparable") encoding of the indexed values: a type tag, then a payload
whose bytes sort like the value. Values of different types sort by type: missing, `null`, integers, floats,
booleans, strings, bytes, arrays, objects. Every index implicitly ends with `_creationTime` and `_id`, so documents
with equal indexed values come in creation order. The encoding's version is stored in the database; opening a
database with an older one rebuilds the affected indexes.

**Ids** are 16 bytes, a 4-byte table number and 12 random bytes, written as 26 characters of base32 that sort like
the bytes. The table can be read from any id, so `ctx.db.get(id)` needs no table name and an id of the wrong table is
rejected.

## Transactions

- **Queries** run on a pool of read-only connections (6 by default). Each opens a read transaction, which pins a WAL
  snapshot, and reads the commit timestamp (ts) of that snapshot: every result is consistent at a single ts.
- **Mutations** run on the single writer connection, one at a time, so they are serializable with no conflicts and no
  retries. That's the right trade-off for one machine. **Group commit**: mutations waiting for the writer share one
  SQLite transaction (up to 64 mutations or 2 ms), each in its own savepoint; the batch commits once and every member
  gets its result after that commit. A mutation known to be slow (over 0.5 ms recently) commits the open batch first,
  so the batch doesn't wait for it.
- **Mutations can't do network I/O**, so they stay short, and a timeout (5 s by default) is checked at each await.
- **Actions** have no transaction; each `ctx.run_query` / `ctx.run_mutation` they make is a transaction of its own.
- **Checkpoints run off the commit path.** SQLite's automatic checkpoint would run inside a commit, holding the
  writer. A background thread checkpoints instead once the WAL passes 1000 pages, and the writer finishes one itself
  past 16 000, so the WAL doesn't grow without bound under constant writes.
- **Retries are safe.** A client that reconnects sends its unanswered mutations again; the server recognizes them by
  session and request id (kept for 10 minutes, written in the mutation's own transaction) and answers with the
  recorded result instead of running them twice.

## Reactivity

**Read sets.** A query records what it read: the key range of each index scan, a point for each `get`, a range per
search index, and how much of the caller's identity it looked at. `take(n)` narrows a scan's range to the part
actually read, so "the latest 50 messages" isn't invalidated by an insert older than the 50th; a `filter` doesn't
narrow it, since every document in the range was read.

**The subscription manager** is one task per server that owns every subscription:

- **Shared queries.** Identical queries (same function, arguments, and the part of the identity they read) run once,
  however many clients watch them. A query that never reads `ctx.auth` is shared by everyone.
- **Versions.** A result computed at ts `r` stays valid until the first commit that writes into its read set. Each
  commit's written keys are looked up in an interval index of all read sets, so only the queries it affects run again.
- **Generations.** Commits are handled in rounds: a round takes a snapshot, invalidates what the new commits touched,
  and queues those queries; runner tasks run them on the reader pool. The manager never waits for a query, a query
  already queued isn't queued twice, and queries that take 5 ms or more have a lane of their own, so slow queries
  don't hold up fast ones.
- **No tearing.** A client gets an update at ts `T` only when every one of its queries has a result valid at `T`, so
  all of its results move together, and never back in time.
- **Read your writes.** The client resolves a mutation, and drops its optimistic update, only once it has results
  at or after the mutation's commit.

Commits are kept in an in-memory write log (60 s, 100 000 commits or 32 MiB), which the manager reads to invalidate.

**Query cache.** A query whose last subscriber leaves stays cached while nothing it read changes (up to 30 minutes,
64 MiB in all), and answers HTTP queries and `ctx.run_query` without running. A query has to miss twice before it is
cached, so one-off queries don't fill it, and a query that reads the time is never cached.

**Fan-out.** Each result is serialized once and that text is shared by every subscriber's message. Clients that
support it get diffs against the result they already have. Sockets set `TCP_NODELAY`.

**Determinism.** A query must be a pure function of the data, its arguments and the caller's identity, or caching
and sharing would serve stale or wrong results. Rust can't enforce that, so it's a contract: `QueryCtx` has no
network access, `ctx.now()` is the snapshot's commit time, and in `--dev` the server reruns queries now and then and
warns when a result differs.

## Measurements

Release builds on one machine, from development (dates are when they were taken):

- **Writes (2026-10-10, NVMe SSD, encrypted btrfs):** one-insert mutations at 8.9k/s with 1 writer, 16.2k/s with 16
  and 18.7k/s with 64 (p99 0.2 / 3.5 / 11 ms); with `--fsync-commits` 1.1k, 7.3k and 12.5k/s. Moving checkpoints off
  the writer raised these from 5.8k, 9.7k and 11.6k/s.
- **Indexes (2026-10-15, `bench_indexes`):** all indexes share `index_entries`. Filling it with 3.3M entries of
  other indexes took a lookup on a small table's index from 22.5 to 25.3 µs (the SQLite read alone stayed at 2.5 µs;
  the B-tree gained one level). Each index costs writes: 16 writers inserted 22k documents/s with 1 index, 12k with
  8, 3.6k with 32. Keys over about 980 bytes spill into overflow pages: scanning 100 entries went from 151 µs to
  1.5 ms (hence the warning past 960 bytes). The writer's page cache, 2 MB → 32 MiB: inserts with 32 indexes
  3.6k → 5.3k/s, patches of all 32 fields 2.1k → 3.2k/s, 10-insert mutations +10%; one-insert mutations unchanged.
- **Readers (2026-10-15, `bench_readers`):** fast queries (10 documents by index) next to slow ones (20k documents),
  half as many slow ones as cores. One reader per core or fewer: the slow queries take every reader and the fast ones
  wait (8 cores, 6 readers: 2.5k fast queries/s at p50 13 ms); two per core: on a saturated CPU the slow ones crawl
  (4/s); 1.5 per core, the default, kept both going (12 readers: 109k/s at p50 0.3 ms, and 148 slow/s). Fast queries
  alone: 151k/s with 6 readers, 285k/s with 16 (16 cores). The slow lane (queries averaging 5 ms or more share
  half the readers, one-off and live alike) then stopped slow queries from starving fast ones at any reader count:
  as many slow queries as readers took fast ones from 2.4k/s at p50 4 ms to 57k/s at 0.3 ms (4 cores), and at the
  default 12 readers on 8 cores, fast queries ran 45% faster (153k/s) while slow ones ran half as often (78/s).
  Sharing the lane with live queries cut fast subscribers' worst update latency next to slow ones from 19 to 0.8 ms.
- **Fan-out (2026-10-10):** 100 clients watching the same 10 queries cost one execution per commit instead of 100,
  with about 0.23 ms from commit to client. Serializing each result once cut the server's CPU for 100 subscribers by
  10x; diffs cut a new post at the top of a 20-post list from 5.5 KB to about 250 bytes.
- **Parallel reads:** 8 readers ran no faster than 1 until SQLite's global page-cache and memory-statistics mutexes
  were turned off at build time (see [Deploying](/operations/deploying#build-with-the-right-sqlite-flags)); with them
  off, about 70k queries/s on 8 readers.
- **Query cache (2026-10-10, HTTP):** get by id 134k → 293k requests/s, the 20 newest of a list 42k → 284k/s.

## Trade-offs and risks

| Risk | How it's handled |
|---|---|
| A slow mutation holds the single writer, and every write queued behind it waits. | No network in mutations, a timeout, durations on the dashboard. Group commit keeps a slow mutation from holding back the batch before it. Since queries already record read sets, optimistic concurrency could be added if this ever matters. |
| Rust compile times slow the development loop. | The `rxferrix` crate is a precompiled dependency, so only the app rebuilds; clients reconnect and resubscribe on their own after a restart. Accepted as the cost of writing the backend in Rust. |
| Non-deterministic queries break caching silently. | The determinism contract, `ctx.now()`, and the check in `--dev`. |
| Data that no longer matches a changed type. | The schema is checked at startup, and the server refuses to start on a mismatch unless [migrations](/guide/migrations) for the table are pending. |
| SQLite's default build flags serialize readers. | The build flags in `.cargo/config.toml` (and in every app made by `rxferrix new`); the server warns at startup when they're missing. |
| DuckDB makes the binary bigger and the build longer. | The analytics copy is an opt-in feature (`olap`). |
| SQL in actions reaching data or files it shouldn't. | One read-only SELECT per call, no file or network access, no extensions, locked settings, limits and a timeout; tables with secrets are never copied. |
| The analytics copy holding back the database. | It only reads the change journal; a copy that falls too far behind is rebuilt from a snapshot instead. |
| One machine is the ceiling. | Accepted: the design favours simplicity and consistency over horizontal scale. Deploys without downtime and continuous replication cover upgrades and recovery. |
