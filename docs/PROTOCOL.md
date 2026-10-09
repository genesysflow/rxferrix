# Ferrix sync protocol (v1)

Transport: WebSocket at `GET /api/sync`. Every frame is a UTF-8 JSON text frame containing exactly one message.
Every message has a `"type"` discriminator. Field names are camelCase. This document is the **contract between
`crates/ferrix` (server) and `packages/client` (TS)**. If you change it, change both sides.

## Concepts

- **Function path** (`udfPath`): `"<module path>:<function name>"`. Example: `"todos:list"`. Nested modules are
  joined with `/`: `"admin/users:list"`. Functions in the crate root have no module part: `"list"`.
- **Args**: always a JSON object, `{}` when the function takes no parameters.
- **ts**: the global commit timestamp. It is a JSON number (u64 in Rust) and increases by 1 for every committed
  mutation. A query result is "at ts T" when it was computed from the database state after commit T.
- **Query set version** (`querySet`): an integer the client increments on every `ModifyQuerySet`.
- **Identity version** (`identity`): an integer the client increments on every `Authenticate`.
- **StateVersion**: `{ "querySet": number, "identity": number, "ts": number }`. This is the client's view of the world.
  The server tracks the client's current StateVersion. Every `Transition` moves it from `startVersion` to `endVersion`.
  The client must check that `startVersion` equals its current version and treat a mismatch as a protocol error:
  close the socket and reconnect.

## Client → server

```jsonc
// First message on every socket. sessionId is a UUID that stays stable for the life of the client object across
// reconnects. connectionCount is 0 for the first socket and incremented on each reconnect.
{ "type": "Connect", "sessionId": "8f1c…", "connectionCount": 0, "lastCloseReason": null, "clientVersion": "0.1.0" }

// Add or remove subscriptions. baseVersion must equal the server's current querySet version for this socket, and
// newVersion = baseVersion + 1. queryId is chosen by the client, is unique per client, and is never reused.
{ "type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 1, "modifications": [
    { "type": "Add", "queryId": 0, "udfPath": "todos:list", "args": {} },
    { "type": "Remove", "queryId": 3 }
] }

// requestId is chosen by the client, is unique per sessionId, and never repeats.
{ "type": "Mutation", "requestId": 0, "udfPath": "todos:add", "args": { "text": "hi" } }
{ "type": "Action",   "requestId": 1, "udfPath": "ai:summarize", "args": {} }

// Set or clear identity (token = null). Phase 0: the server accepts it, bumps the identity version, reruns all
// queries, and ignores the token value.
{ "type": "Authenticate", "baseVersion": 0, "token": "eyJ…" }

// Keepalive answer to a server Ping (optional for the client to send unprompted).
{ "type": "Pong" }
```

## Server → client

```jsonc
// The ONLY way query results reach the client. All modifications in one Transition are applied atomically and
// are all valid at endVersion.ts. modifications may be empty: this happens when the server only needs to advance
// ts, for example so the client can observe one of its own mutations.
{ "type": "Transition",
  "startVersion": { "querySet": 0, "identity": 0, "ts": 0 },
  "endVersion":   { "querySet": 1, "identity": 0, "ts": 42 },
  "modifications": [
    { "type": "QueryUpdated", "queryId": 0, "value": [/* fn result */], "logLines": [] },
    { "type": "QueryFailed",  "queryId": 1, "errorMessage": "…", "errorData": null, "logLines": [] },
    { "type": "QueryRemoved", "queryId": 3 }
  ] }

// Sent once the mutation has committed (success) or failed. ts is present only on success: the commit ts.
{ "type": "MutationResponse", "requestId": 0, "success": true,  "result": "k3…", "ts": 42, "logLines": [] }
{ "type": "MutationResponse", "requestId": 0, "success": false, "errorMessage": "…", "errorData": {"code":"…"}, "logLines": [] }

{ "type": "ActionResponse", "requestId": 1, "success": true,  "result": "…", "logLines": [] }
{ "type": "ActionResponse", "requestId": 1, "success": false, "errorMessage": "…", "errorData": null, "logLines": [] }

// Unrecoverable for this socket. The server closes it after sending. The client reconnects with backoff.
{ "type": "FatalError", "error": "…" }
{ "type": "AuthError", "error": "…" }

// Sent every 15s. If the client hears nothing for 30s it assumes the socket is dead and reconnects.
{ "type": "Ping" }
```

## Server guarantees

1. **Atomic, consistent transitions.** After a Transition, every query the client has subscribed to has a result
   that is valid at `endVersion.ts`. Results are never mixed from different timestamps.
2. **Monotonic ts.** `endVersion.ts >= startVersion.ts`.
3. **The first result for every Add** in a ModifyQuerySet arrives in the Transition whose `endVersion.querySet`
   equals that ModifyQuerySet's `newVersion`. That Transition may also carry updates to other queries.
4. **Read-your-writes.** After a successful `MutationResponse{ts: M}`, the server sends a Transition with
   `endVersion.ts >= M` promptly, even an empty one.
5. **Errors in a query** produce `QueryFailed` and keep the subscription alive. If a later commit makes the query
   succeed, a `QueryUpdated` follows.
6. A **user error** (`UserError` in Rust) puts `errorData = { "code": string, "data": any }` and
   `errorMessage = message`. Any other error gives `errorData = null`. In `--dev` the message is the real error text;
   otherwise it is `"Server Error"`.

## Clarifications (resolved while building the client)

- **Authenticate versioning**: `baseVersion` must equal the server's current identity version for this socket. The
  server sets identity to `baseVersion + 1` and reflects it in the next Transition's `endVersion.identity`. A
  mismatch is a FatalError. Identity versions are per socket and start at 0.
- **queryIds are per client, not per socket.** After a reconnect the client re-adds live queries with their
  *original* queryIds (the server's state for the new socket is fresh, so any ids are acceptable).
- **connectionCount** counts every socket attempt by this client object, including ones that never opened.
- **lastCloseReason** is a free-form string for diagnostics only.
- **Read-your-writes after a reconnect**: until the client has applied a Transition whose `endVersion.querySet`
  reaches the version of its re-add ModifyQuerySet, it does not count Transitions toward mutation completion (its
  view still holds results from the old socket). The server needs nothing special for this, but guarantee 4 means
  "a Transition at ts ≥ M, consistent for the whole subscribed set".
- **Results for unknown queryIds** (the client removed the query while a Transition was in flight) are ignored by
  the client, but the version still advances.

## Client rules

- **On reconnect**, the client creates a fresh server state (versions start at 0). It sends `Connect`, then
  `Authenticate` if it has a token, then one `ModifyQuerySet` adding every live query (new queryIds are fine), then
  re-sends every mutation that does not yet have a `MutationResponse`. (Phase 0 has no dedup. Phase 2 deduplicates
  by `(sessionId, requestId)`.) Actions in flight when the socket drops are rejected with a "connection lost" error,
  because actions are not idempotent.
- **Resolving a mutation**: a mutation's promise resolves only once BOTH its `MutationResponse(success, ts=M)` has
  arrived AND the client has applied a Transition with `endVersion.ts >= M`. Its optimistic update is dropped at
  that same moment. On failure, the promise rejects and the optimistic update is dropped immediately.
- **Query identity** on the client is `udfPath + canonicalJSON(args)` (keys sorted recursively). Two components
  watching the same query share one queryId.

## HTTP API (non-realtime)

`POST /api/query`, `POST /api/mutation`, `POST /api/action` with body `{ "path": "todos:list", "args": {} }` and an
optional `Authorization: Bearer <token>`.

- Response 200 (success): `{ "status": "success", "value": <result>, "logLines": [] }`.
- Response 4xx/5xx (error): `{ "status": "error", "errorMessage": "…", "errorData": null | {code, data}, "logLines": [] }`.
  Status codes: 400 for a user error or bad args, 404 for an unknown function, 500 for anything else.
- Mutation responses also include `"ts": number`.
