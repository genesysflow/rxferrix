# Things to know

- **Limits per connection and per run.** A socket may have 1024 live queries (the client refuses the 1025th with
  `too_many_queries`) and 64 running mutations/actions;
  queries time out after 30 s (`--query-timeout`), mutations after 5 s (`--mutation-timeout`); a client silent for 45 s (it answers pings)
  is disconnected. They bound the server's memory whatever a client does (docs/PROTOCOL.md, Limits).
- **Query results are cached while nothing they read changes.** Every subscriber of a query shares one result; after
  the last one leaves it stays cached (up to 30 minutes, 64 MiB in all: `--query-cache-ttl`, `--query-cache-size`)
  and answers the next subscriber, HTTP query or `ctx.run_query` without running, until a write touches what it read.
  Repeated HTTP queries are cached too. A query that reads `ctx.now()` is never cached. The Overview page and
  `/metrics` show the hit rate.

- **Optimistic updates are replayed** on top of every server change until the client has seen the mutation's writes (or it failed). Create ids
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
- **A query's result must depend only on its arguments and the data it reads.** In `--dev` and `TestApp`, rxferrix
  reruns each query in the same snapshot now and then and logs a warning (also sent as a log line) if the result
  differs: `SystemTime::now()`, random numbers or `HashMap` order. Use `ctx.now()`, and a `BTreeMap` or a sort.
- **Invalidation is coarse**: any write to a document reruns the queries that read it, even when nothing changed.
  Skip no-op writes, keep fast-changing fields (presence, typing, counters) in small tables of their own, read narrow
  ranges (`eq` prefixes, `_creationTime` bounds, `take(n)`), and count with `.count()`.
- **Each index costs every write to its table; long index keys cost reads.** An insert writes one entry per index
  (with 32 indexes, about a sixth of the inserts per second of one); a patch rewrites only the entries of the indexes
  whose fields it changes. Keys over 960 bytes (an indexed string of about 930 bytes) don't fit in a page of the
  index's B-tree: scans get about 10x slower, lookups 2x, and the server logs a warning. Index a prefix or a hash of
  such values instead. How many other indexes and entries exist doesn't slow a lookup (`bench_indexes.rs` in the
  integration tests).
- **Tests** (`rxferrix::testing::TestApp`, in dev mode): `t.create_user("ann@example.com", Some("password"))` makes a
  user, `t.as_user(id).query::<list>(args)` calls as them, `t.advance_time(d)` runs due jobs.

More in [docs/CONTRACT.md](../CONTRACT.md) §1.10.
