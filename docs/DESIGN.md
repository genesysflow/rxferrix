# How RxFerrix works

This page explains the implementation and its trade-offs. Use [Core concepts](/guide/concepts) for an app-level introduction, the [Rust contract](CONTRACT.md) for signatures, and the [protocol](PROTOCOL.md) for wire formats.

## One app process owns the database

An app links the `rxferrix` crate into a Rust executable. Macro-annotated tables and functions register at compile time. `RxFerrix::run` opens the database, starts services, and runs the HTTP/WebSocket server.

SQLite stores the live documents. There is one writer and a pool of snapshot readers. This makes transactions and consistent live updates possible without coordinating a distributed database. The trade-off is that one machine bounds capacity.

React and Svelte bindings use a shared TypeScript client. The generated API describes functions and types; it does not execute Rust in the browser. The embedded dashboard uses the same live transport with administrator-only APIs.

## Storage

Documents from all app and system tables share a generic SQLite layout:

```sql
CREATE TABLE documents (id BLOB PRIMARY KEY, table_number INTEGER NOT NULL, json TEXT NOT NULL, ts INTEGER NOT NULL);
CREATE TABLE index_entries (index_id INTEGER NOT NULL, key BLOB NOT NULL, doc_id BLOB NOT NULL,
                            PRIMARY KEY (index_id, key)) WITHOUT ROWID;
```

An index lookup is a byte-range scan over `index_entries`. Recording those ranges also records what a query depends on. Adding an index needs a backfill rather than a new SQLite table for each app schema.

Index keys use an order-preserving byte encoding. Values sort by type: missing, null, integers, floats, booleans, strings, bytes, arrays, and objects. Ordinary indexes append `_creationTime` and `_id`, giving stable creation ordering within equal declared fields.

IDs contain a four-byte table number, the document's creation time in milliseconds (six bytes) and six random bytes, encoded as 26 base32 characters. Documents and their index entries are stored in ID order, so IDs that grow with time put each insert next to the previous ones instead of at a random page of a large table. The table number lets the runtime reject a typed ID for the wrong table. An ID does not authorize access, and it reveals when its document was created (as `_creationTime` does). Stored files are the exception: their IDs keep twelve random bytes, because a file URL is an unguessable capability.

SQLite WAL snapshots isolate readers; stored documents need only one current row. `aux.db` separately holds execution logs and finished-job history. File bytes live on disk or in object storage. Optional analytics uses a derived DuckDB copy.

## Transactions

A query holds a read-only connection for its run and reads one snapshot timestamp. The default pool size is about 1.5 readers per available core. Queries averaging at least 5 ms share a limited lane to keep slots available for faster reads.

Mutations run serially on the writer. Queued mutations may share a group commit, with one savepoint per mutation. A failed mutation rolls back its savepoint; a failed batch commit fails the batch. Successful writes still get individual commit timestamps, and callers receive success only after commit.

Groups are bounded by 64 mutations or two milliseconds. A recently slow mutation causes the current group to commit before it begins. This protects earlier fast mutations from waiting for it, but while it runs it still holds the single writer.

External I/O belongs in actions. The context API separates transactional reads/writes from external work, but Rust is not sandboxed: the framework cannot prevent an app from making an inappropriate network call or blocking a thread. Timeouts are enforced at await points.

Actions have no transaction. Each runtime query or mutation they call is separate. An external side effect cannot be rolled back if an action later fails.

WAL checkpoints normally run off the write path. Default durability is `synchronous = NORMAL`; `--fsync-commits` waits for disk durability, with a shared fsync for a group.

## Reactivity

A query records document reads, index ranges, search/filter dependencies, environment reads, and the parts of identity it uses. A write log tracks committed changes. The subscription manager matches those changes to read sets and reruns affected queries.

A bounded scan such as `take(50)` records the span actually scanned. A post-read filter does not narrow that span. Changes to a document can invalidate its readers even when the fields relevant to the UI did not change.

Identical queries share executions when their arguments and relevant identity match. A query that never reads auth can be shared across callers. Authorization reads therefore belong in the query or its guard, where they participate in tracking.

The manager processes commits in generations without waiting for one slow query to finish. A client advances only when all its query results are valid at the new timestamp. This prevents the UI from combining query results from incompatible database snapshots.

The client resolves a successful mutation only after subscriptions reach its commit timestamp. Until then, an optional optimistic update is replayed over new server results. Once the commit is visible, the optimistic layer is removed.

## Caching and transport

Idle results can remain cached for up to 30 minutes within a 64 MiB budget. Repeated one-shot queries can be cached too; one-off calls are screened by an admission filter. Writes invalidate cache entries through the same dependencies. Time-dependent queries do not use this idle cache.

Queries must be deterministic for caching and sharing to be correct. Development checks sample a second run in the same snapshot and warn on different results. They help detect mistakes but do not prove determinism.

Each result is serialized once for fan-out. Clients advertising diff support may receive changes against their previous server result. Slow sockets can skip intermediate database states while still receiving a consistent transition.

On reconnect, the client restores subscriptions and resends eligible unanswered mutations. Deduplication records are stored with their writes for about ten minutes, and the client retries within nine minutes under the same identity. Actions and HTTP mutations do not have this reconnect deduplication guarantee.

## Updates and recovery

A takeover prepares a database copy using the new code while the old process keeps serving. It validates schema/index compatibility, catches up with writes, then drains the old server and switches databases. Preparation errors leave the old process running; later initialization failures still need an operator recovery plan.

Backups and continuous journal shipping support recovery. They do not create active replicas for serving app writes. Local SQLite storage and same-host coordination are requirements for the supplied takeover deployment.

## Scope and trade-offs

| Choice | Consequence |
| --- | --- |
| One writer | Simple serializable mutations; a slow mutation delays other writes. |
| Native Rust code | Normal Rust tooling and libraries; compile/restart development loop and no sandbox. |
| Tracked document API | Automatic live updates; app functions do not run raw SQL against the live database. |
| Function-level permissions | Flexible app authorization; each public path must enforce its policy. |
| Optional analytics copy | SQL reports without live-table scans; asynchronous freshness and a larger build. |
| In-memory client state | Reconnects and optimistic views; no persistent offline database. |

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
  were turned off at build time (see [Deploying](/operations/deploying#preserve-the-sqlite-build-configuration)); with them
  off, about 70k queries/s on 8 readers.
- **Query cache (2026-10-10, HTTP):** get by id 134k → 293k requests/s, the 20 newest of a list 42k → 284k/s.

Measurements above describe specific workloads and machines. They are useful for understanding implementation choices, not capacity guarantees for another application. Benchmark your own query shapes, write sizes, and subscriber counts.
