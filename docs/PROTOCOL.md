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

// requestId is chosen by the client, is unique per sessionId, and never repeats, except when the client resends
// an unanswered mutation after a reconnect: the server then replays the recorded response (guarantee 7).
{ "type": "Mutation", "requestId": 0, "udfPath": "todos:add", "args": { "text": "hi" } }
{ "type": "Action",   "requestId": 1, "udfPath": "ai:summarize", "args": {} }

// Set or clear identity (token = null clears it: anonymous). token is an access token from /api/auth/* (see
// "Auth" below). Valid: the socket's identity becomes the token's user (or admin), every query reruns, and the
// next Transition carries endVersion.identity = baseVersion + 1. Invalid, expired, or its session ended (signed
// out, revoked): AuthError, then the server closes the socket; the identity is unchanged.
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
// The token of an Authenticate was rejected, or the socket's token expired. error is one of "malformed access
// token", "invalid access token signature", "access token expired", "session has ended". The server closes the
// socket; the client fetches a fresh token (forceRefresh) and reconnects with it.
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
   otherwise it is `"Server Error"`. A mutation that exceeds the server's mutation timeout (default 5s) is rolled
   back and fails with such a server error.
7. **Mutations are idempotent per `(sessionId, requestId)`.** The server records each mutation's response (the
   result and commit ts, or the error) in the same transaction as the mutation's writes, keyed by the socket's
   `Connect.sessionId` and the message's `requestId`, for about 10 minutes. A `Mutation` with a key already recorded
   (on any socket) is not run again: the server answers with the recorded response, so `result`, `ts` and the error
   are identical to the first answer (`logLines` are empty). Concurrent duplicates wait for the first and then get
   its response. A replayed `ts` may be older than the socket's current version; guarantee 4 still holds. Without a
   `Connect`, mutations are not deduplicated. The key also includes the caller's identity (user, admin or
   anonymous): a response is never replayed to a different identity, which runs its own mutation instead.
9. **Token expiry.** While a socket is authenticated, the server sends `AuthError` ("access token expired") and
   closes the socket when the token's `exp` passes, unless the client has sent a fresher token with `Authenticate`
   before then. The TS client refreshes 30s before `exp`, so this only happens to clients that stopped refreshing.
8. **`logLines`** (on `QueryUpdated`, `QueryFailed`, `MutationResponse`, `ActionResponse` and the HTTP responses)
   are the log lines the function emitted while it ran, formatted as `"[LEVEL] message key=value ..."`, including
   those of functions an action called. They are sent **only in `--dev`**; otherwise they are always `[]` (the server
   still keeps them in its execution log). A query rerun whose result didn't change sends nothing, so its new log
   lines aren't sent either.

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
  re-sends every mutation that does not yet have a `MutationResponse`, with its original `requestId` (see guarantee
  7: the server runs each `(sessionId, requestId)` at most once). Actions in flight when the socket drops are
  rejected with a "connection lost" error, because actions are not idempotent.
- **Resolving a mutation**: a mutation's promise resolves only once BOTH its `MutationResponse(success, ts=M)` has
  arrived AND the client has applied a Transition with `endVersion.ts >= M`. Its optimistic update is dropped at
  that same moment. On failure, the promise rejects and the optimistic update is dropped immediately.
- **Query identity** on the client is `udfPath + canonicalJSON(args)` (keys sorted recursively). Two components
  watching the same query share one queryId.

## HTTP API (non-realtime)

`POST /api/query`, `POST /api/mutation`, `POST /api/action` with body `{ "path": "todos:list", "args": {} }` and an
optional `Authorization: Bearer <access token>`. Without the header the call is anonymous; with an invalid, expired
or signed-out token it fails with 401 and `errorData.code = "invalid_token"` (never silently anonymous).

- Response 200 (success): `{ "status": "success", "value": <result>, "logLines": [] }`.
- Response 4xx/5xx (error): `{ "status": "error", "errorMessage": "…", "errorData": null | {code, data}, "logLines": [] }`.
  Status codes: 400 for a user error or bad args, 404 for an unknown function, 500 for anything else.
- Mutation responses also include `"ts": number`.
- `logLines` as in guarantee 8 (only in `--dev`). HTTP mutations are not deduplicated.

## Auth

### Tokens

- **Access token**: a JWT, `{"alg":"HS256","typ":"JWT"}`, claims `{ iss: "ferrix", typ: "access", sub, sid, adm?, iat,
  exp }`. `sub` is the `Id<User>` (or the `Id<Admin>` when `adm: true`), `sid` the `Id<Session>`. Lifetime 15 minutes
  by default. Signed with a key derived from `<data dir>/auth_secret.key` (created on first start, mode 0600; deleting
  it signs everyone out). Validation checks the header, the signature (constant time), `iss`, `typ`, `exp`, and that
  the session still exists and is unexpired (so sign-out and revocation apply immediately to new sockets and HTTP
  calls; a socket that is already authenticated keeps its identity until its token expires or it re-authenticates).
- **Refresh token**: 43 random URL-safe characters (256 bits), stored only as a keyed BLAKE3 hash. Every refresh
  rotates it. Presenting an already-rotated refresh token revokes the whole session (reuse detection), except within
  10 seconds of its rotation, when it is answered with `stale_refresh_token` and the session survives (two tabs
  refreshing at once). Sessions expire after 30 days without a refresh (sliding).

### Endpoints

JSON bodies with camelCase keys. Success: the JSON object below. Failure: `{ "status": "error", "errorMessage",
"errorData": { "code", "data": null } }` with an HTTP status. Responses carry `Cache-Control: no-store`. Endpoints
that check credentials or send email are rate limited per client IP (429 `rate_limited` with `Retry-After`); email
sending is also limited per address. `Tokens` = `{ accessToken, refreshToken, expiresIn, userId? | adminId? }`.

| Endpoint | Body | Response / notes |
|---|---|---|
| `GET /api/auth/providers` | | `{ password, magicLink, signups, oauth: ["github", …] }` |
| `POST /api/auth/signup` | `{ email, password, name? }` | `Tokens`, or `{ verificationRequired: true }`. 400 `email_in_use`, `weak_password` (< 8 chars), `invalid_email`; 403 `signups_disabled`. Sends a verification email when email is configured. |
| `POST /api/auth/signin` | `{ email, password }` | `Tokens`. 401 `invalid_credentials` for both unknown emails and wrong passwords (same timing); 403 `email_not_verified` when verification is required. |
| `POST /api/auth/refresh` | `{ refreshToken }` | `Tokens` (new refresh token). 401 `invalid_refresh_token`, `stale_refresh_token`, `refresh_token_reused` (session revoked), `session_expired`. |
| `POST /api/auth/signout` | `{ refreshToken? }` and/or `Authorization: Bearer` | `{}` always. Deletes the session. |
| `POST /api/auth/verify-email/request` | `{ email, redirectTo? }` | `{}` always (no enumeration). Emails unverified users. |
| `POST /api/auth/verify-email/confirm` | `{ token }` or `{ email, code }` | `{}`. 400 `invalid_code`. |
| `POST /api/auth/reset/request` | `{ email, redirectTo? }` | `{}` always (no enumeration). |
| `POST /api/auth/reset/confirm` | `{ token }` or `{ email, code }`, plus `password` | `Tokens`. Ends all other sessions of the user and marks the email verified. |
| `POST /api/auth/magic/request` | `{ email, redirectTo? }` | `{}` always. Sends a link and a 6-digit code (15 minutes). |
| `POST /api/auth/magic/verify` | `{ token }` or `{ email, code }` | `Tokens`. Creates the user (email verified) on first sign-in when sign-ups are allowed. |
| `GET /api/auth/oauth/{provider}?redirect_to=<app url>` | | 302 to the provider (PKCE S256 + `state`; sets the HttpOnly SameSite=Lax cookie `ferrix_oauth_state`). |
| `GET /api/auth/oauth/{provider}/callback` | (from the provider) | 302 to the app URL with `#ferrix_access_token=…&ferrix_refresh_token=…&ferrix_expires_in=…&ferrix_user_id=…`, or `#ferrix_error=<code>&ferrix_error_description=…`. |
| `POST /api/auth/admin/signin` | `{ email, password }` | `Tokens` with `adminId` (`adm` claim). |
| `GET /api/auth/admin/setup` | | `{ needsSetup }` |
| `POST /api/auth/admin/setup` | `{ token, email, password }` | `Tokens`. Creates the first admin with the one-time setup token that `serve` prints as `/_/setup#token=…` when no admin exists (401 `invalid_setup_token`, 400 `setup_done`). |

Codes are 6 digits (5 wrong attempts burn a code); link tokens are 256-bit. Both are stored as keyed hashes and work
once. Verification codes last 24 hours, reset codes 30 minutes, sign-in codes 15 minutes.

**Redirects.** `redirectTo` / `redirect_to` must match the allowlist (`--auth-redirect-url`): same scheme, host and
port as an entry, and, when the entry has a path, that path or one below it. In `--dev`, any `http://localhost` or
loopback URL is allowed too. A URL that doesn't match is refused (400 `invalid_redirect`), never replaced. Without
`redirectTo` the first allowlist entry is used. Email links point at the app with the token in the fragment:
`<app url>#ferrix_verify_token=…`, `#ferrix_reset_token=…`, `#ferrix_magic_token=…`; the TS client's
`handleRedirectCallback()` consumes them (and the OAuth fragment) and removes them from the address bar.
