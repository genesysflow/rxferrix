# Behavior and limits

These details affect app design even when the API is small. Use this page when deciding how to handle reconnects, time, growing lists, and write load.

## Reconnects and retries

The TypeScript client reconnects and restores active subscriptions. It resends unanswered mutations with the same request identity; the server retains their responses for about ten minutes and replays them rather than committing the mutation again.

The client stops resending after nine minutes or when the caller's identity has changed. It then rejects with `OutcomeUnknown`: the mutation may or may not already have committed. Check application state before issuing a new operation. A new call is a new request, so it is not covered by the old request's deduplication.

Actions are not automatically replayed after connection loss. An in-flight action rejects with `ConnectionLost`, and external work it already performed remains. HTTP mutations also have no request deduplication. This is a bounded reconnect guarantee, not universal exactly-once execution.

The client keeps pending state in memory. A page reload or process restart is not persistent offline recovery.

## Query results and caching

A query result can be shared by subscribers with the same function, arguments, and relevant identity. While nothing it read changes, the server can reuse it.

After the last subscriber leaves, results can remain cached for up to 30 minutes within a 64 MiB idle-cache budget (`--query-cache-ttl`, `--query-cache-size`). Repeated HTTP queries and action `run_query` calls can also use the cache. Reading `ctx.now()` or creating a time-dependent signed URL prevents idle query-cache reuse.

Queries must be deterministic. In `--dev` and `TestApp`, the server sometimes reruns a query against the same snapshot and warns when results differ. Avoid system time, random results, and unstable `HashMap` iteration order; use tracked time and stable ordering.

## Time is not a subscription trigger

`ctx.now()` in a query is its snapshot's time. A query reruns when tracked inputs change, not when a minute passes. A signed file URL in a query also does not renew merely because it expires.

For expiry, store state and schedule a mutation to change it. Format relative times such as “five minutes ago” in the client. Request a fresh signed URL when the view needs one.

## Keep client results immutable

Results are read-only. Copy before sorting or editing: `[...tasks].sort(...)`. Optimistic updates should build new arrays and objects, and generate IDs or timestamps once per invocation outside the replayed closure.

Changing query arguments normally returns to loading. React's `useQueryKeepingPrevious` and Svelte's `{ keepPreviousData: true }` keep the last value while a new result loads; `isStale` identifies it.

## Keep reads narrow and writes small

An ordinary query that read a document can rerun after any write to that document, including a no-op patch. Skip unnecessary writes. Store frequently changing presence or typing data separately from widely read profiles.

Use equality prefixes, index ranges, and `.take(n)` to bound reads. `.filter(...)` still reads candidates in its range. Prefer `.count()` over collecting documents just to count them, and paginate lists that can keep growing.

Every index adds write work and storage. Long index keys can spill into extra B-tree pages; the server warns above 960 bytes. Index a short prefix or a suitable hash when you do not need ordering on the full value.

One writer serializes all mutations. Avoid external I/O and long CPU work in mutations, and monitor their duration. The runtime does not sandbox Rust code.

## Know the default limits

| Limit | Default |
| --- | --- |
| Distinct live queries per connection | 1024; the client rejects the next with `too_many_queries`. |
| Combined live query arguments per socket | 8 MiB. |
| WebSocket message / ordinary HTTP API body | 2 MiB; file uploads use a separate limit. |
| Running mutations and actions per socket | 64, with backpressure after that. |
| Query timeout | 30 seconds (`--query-timeout`). |
| Mutation timeout | 5 seconds (`--mutation-timeout`). |
| File upload size | 100 MB (`--max-upload-size`). |

Timeouts and cancellation take effect at await points; they cannot preempt an infinite CPU loop. The protocol also bounds socket silence and outgoing send time. See [protocol limits](/PROTOCOL#limits) for exact behavior.

## Jobs have no caller identity

Scheduled jobs, crons, and workflow steps do not carry the user identity that requested them. Check authorization before scheduling, pass required user IDs explicitly, and make job functions internal. Recheck current permissions when delayed work requires it.

## Plan for one active server

The database belongs on local disk and has one active server writer. Takeover prepares a replacement, rather than scaling writes across machines. Replication supports recovery rather than active replicas serving the app.

By default, a process crash preserves committed data, while power or OS failure may lose recent commits. Use `--fsync-commits` when acknowledgement must wait for disk durability. Keep [backups](/operations/backups) and retain `auth_secret.key` independently.
