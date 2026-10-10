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
// reconnects. connectionCount is 0 for the first socket and incremented on each reconnect. features (optional)
// lists protocol features the client supports: "diffs" (results may arrive as diffs, see "Result diffs").
{ "type": "Connect", "sessionId": "8f1c…", "connectionCount": 0, "lastCloseReason": null, "clientVersion": "0.1.0",
  "features": ["diffs"] }

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
// "Auth" below), or a JWT of a configured external issuer ("External tokens"). Valid: the socket's identity becomes
// the token's user (or admin), every query is re-evaluated for the new identity, and the next Transition carries
// endVersion.identity = baseVersion + 1 (with the results that changed). Invalid, expired, or its session ended
// (signed out, revoked): AuthError, then the server closes the socket; the identity is unchanged.
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
    { "type": "QueryUpdated", "queryId": 2, "diff": {"$a": [[/* new doc */], 19, -1]}, "logLines": [] },
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
// The token of an Authenticate was rejected, the socket's token expired, or its session ended while the socket was
// open (guarantee 10). error is one of "malformed access token", "invalid access token signature", "access token
// expired", "session has ended", and for external tokens "unknown token issuer", "unknown token signing key",
// "invalid access token claims", "access token not yet valid". The server closes the socket; the client fetches a
// fresh token (forceRefresh) and reconnects with it, or, when that fails, continues anonymously.
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
10. **Live revocation.** When the session a socket authenticated with ends, the server sends `AuthError`
   ("session has ended") and closes the socket right away (within milliseconds of the commit), without waiting for
   the token's `exp`; no Transition follows it, so identity-dependent results stop at once. A session ends on
   sign-out (`/api/auth/signout`), refresh-token reuse detection, a password reset (other sessions), an admin's
   `revoke_session` or `delete_user`, and any other deletion of its `_sessions` row. For external tokens (no session
   row) the same happens when the provisioned user is deleted. The TS client answers with a forced refresh, which
   fails for an ended session, and then continues anonymously (`getAuth` signs out). Refreshing a session does not
   affect its sockets. The admin log stream (`/api/admin/logs/stream`) ends the same way, with `event: revoked`.
8. **`logLines`** (on `QueryUpdated`, `QueryFailed`, `MutationResponse`, `ActionResponse` and the HTTP responses)
   are the log lines the function emitted while it ran, formatted as `"[LEVEL] message key=value ..."`, including
   those of functions an action called. They are sent **only in `--dev`**; otherwise they are always `[]` (the server
   still keeps them in its execution log). A query rerun whose result didn't change sends nothing, so its new log
   lines aren't sent either.

## Result diffs

A client that sends `"features": ["diffs"]` in `Connect` may receive a `QueryUpdated` with `diff` instead of `value`:
the new result as a change to **the result this socket last received for that queryId** (in a `QueryUpdated`, as a
value or a diff). The server tracks exactly that per socket, so the client stays in sync with nothing more than
applying each diff to its copy of the server's result (not to an optimistically updated view). The server sends
`value` instead whenever the diff wouldn't be clearly smaller, and always for a query's first result on a socket and
after a `QueryFailed`. A new socket starts over: every first result is a `value`.

A diff is a JSON object, applied to the previous result:

- `{"$r": value}`: replace it with `value`.
- `{"$a": [op, …]}`: edit an array. The ops walk the old array from the start: a positive integer `n` keeps the next
  `n` elements, a negative integer `-n` deletes them, an array inserts its elements, and `{"$p": diff}` patches the
  next element with `diff`. Elements after the last op are kept.
- `{"$o": {"s": {key: value, …}, "d": [key, …], "p": {key: diff, …}}}`: edit an object: set keys, delete keys and
  patch keys (each part optional).

A diff that doesn't fit (an op past the end, a missing key, an array diff for a non-array) means the client's copy
isn't what the server diffed against: the client drops the whole Transition (nothing applied) and reconnects, which
starts over from full values. The reference implementations are `ferrix::sync::diff::apply` and
`packages/client/src/diff.ts`; `crates/ferrix/tests/it/diff_fixtures.rs` checks they agree.

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
- **`QueryRemoved`** is sent only for a query the client was sent a result for. Adding and removing the same
  queryId in one `ModifyQuerySet` produces neither a result nor a `QueryRemoved`.

## Shared subscriptions (server side)

Nothing here changes the messages or the guarantees above; it describes how the server meets them.

- The server runs each distinct query once per invalidation and sends the result to every socket subscribed to it.
  Queries are shared by `(udfPath, canonical args, identity scope)`, where the scope is what the query's last run
  read of `ctx.auth`: nothing (shared by every socket), the user or admin id (shared by that principal's sockets;
  all anonymous sockets share one), or the session id (one session). Tokens and session ids never key a query
  that doesn't read the session id.
- The server brings every shared query to one snapshot ts per round, so a Transition's results are valid at its
  `endVersion.ts` even when they come from queries shared with other sockets (guarantee 1). A `ModifyQuerySet` whose
  queries are already live elsewhere is answered from their current results, without running them.
- **Slow sockets.** The server never waits for one socket. While a socket isn't reading, the updates waiting for it
  are merged: it later gets fewer Transitions, each consistent at its `endVersion.ts` and carrying the newest result
  of every query that changed (intermediate states may be skipped, as they may anyway). Each `ModifyQuerySet` and
  `Authenticate` still gets its own Transition (guarantee 3). A socket with more than 1024 of those unanswered is
  closed with `FatalError`.

## Limits

So that no client can grow the server's memory without bound:

- At most 1024 live queries per socket (`MAX_QUERIES`): a `ModifyQuerySet` past it is a `FatalError`.
- At most 64 mutations and actions running per socket (`MAX_IN_FLIGHT`): past it, the server stops reading the socket
  until one finishes (they queue in the client and TCP rather than in the server).
- A socket the client has sent nothing on for 45 seconds is closed with `FatalError` (`SILENCE_LIMIT`; the server
  pings every 15 s and a client answers every `Ping` with `Pong`). A frame that takes over 30 s to send (a client that
  stopped reading) closes the socket too.
- A query running longer than the server's `query_timeout` (default 30 s) fails.

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
- A query may be answered from the server's query cache (no execution) when a result valid at the latest commit is
  cached for the same function, args and identity: the response is the same as if it had run then. So a query right
  after a caller's own mutation always reflects it. (docs/CONTRACT.md §1.10, Query cache.)

## Auth

### Tokens

- **Access token**: a JWT, `{"alg":"HS256","typ":"JWT"}`, claims `{ iss: "ferrix", typ: "access", sub, sid, adm?, iat,
  exp }`. `sub` is the `Id<User>` (or the `Id<Admin>` when `adm: true`), `sid` the `Id<Session>`. Lifetime 15 minutes
  by default. Signed with a key derived from `<data dir>/auth_secret.key` (created on first start, mode 0600; deleting
  it signs everyone out). Validation checks the header, the signature (constant time), `iss`, `typ`, `exp`, and that
  the session still exists and is unexpired (so sign-out and revocation apply immediately to new sockets and HTTP
  calls; sockets already authenticated with it get `AuthError` right away, guarantee 10). The access token never
  outlives its session (its `exp` is capped at the session's expiry).
- **Refresh token**: 43 random URL-safe characters (256 bits), stored only as a keyed BLAKE3 hash. Every refresh
  rotates it. Presenting an already-rotated refresh token revokes the whole session (reuse detection), except within
  10 seconds of its rotation, when it is answered with `stale_refresh_token` and the session survives (two tabs
  refreshing at once). Sessions expire after 30 days without a refresh (sliding). Impersonation sessions (minted by an
  admin, `impersonatedBy` set) have a fixed expiry that refreshing never extends.
- **External tokens** (`AuthConfig::jwt`, docs/CONTRACT.md §1.5): a JWT from a configured issuer (Clerk, Auth0,
  Supabase, any OIDC provider) is accepted wherever an access token is, on `Authenticate` and as an HTTP Bearer token.
  A token whose header says `HS256` is always one of ferrix's own; any other is routed by its `iss` (exact match) to
  the issuer's verifier: RS256, ES256 or EdDSA against the issuer's JWKS (by `kid`), then `exp` (required) and `nbf`
  with a leeway (60s), and `aud`. Its subject maps to a ferrix user (created on first sight), so the identity is an
  ordinary user without a session id. The client simply passes the provider's token getter:
  `client.setAuth(({ forceRefresh }) => clerk.session.getToken({ skipCache: forceRefresh }))`.

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
| `GET /api/auth/oauth/{provider}?redirect_to=<app url>` | | 302 to the provider (PKCE S256 + `state`, plus `nonce` and `response_mode=form_post` for Apple; sets the HttpOnly cookie `ferrix_oauth_state`, `SameSite=Lax`, or `SameSite=None; Secure` for Apple behind an https public URL). |
| `GET /api/auth/oauth/{provider}/callback` | (from the provider) | 302 to the app URL with `#ferrix_access_token=…&ferrix_refresh_token=…&ferrix_expires_in=…&ferrix_user_id=…`, or `#ferrix_error=<code>&ferrix_error_description=…`. |
| `POST /api/auth/oauth/{provider}/callback` | form `code, state, user?` (from Apple) | Apple only (`response_mode=form_post`; 405 for other providers): as the `GET`, answered with 303. `user` (the name) comes on the first sign-in only. |

OAuth presets: `github`, `google`, `discord` (`identify email`; its email only if Discord verified it), `apple` (`name
email`; the profile is the ID token, verified against Apple's JWKS with `iss`, `aud`, `exp` and the flow's `nonce`;
the client secret is an ES256 JWT signed with the app's `.p8` key, generated and rotated by the server), and any OIDC
issuer. The state cookie must equal the callback's `state` (constant-time) and name a pending flow of that provider
(10 minutes, used once), whichever method the callback uses; Apple's cross-site `POST` is why its cookie is
`SameSite=None` (it binds the flow, it doesn't authenticate anything, so a third party posting with it attached can't
complete a sign-in: it doesn't know the value).
| `POST /api/auth/admin/signin` | `{ email, password }` | `Tokens` with `adminId` (`adm` claim). |
| `GET /api/auth/admin/setup` | | `{ needsSetup }` |
| `POST /api/auth/admin/setup` | `{ token, email, password }` | `Tokens`. Creates the first admin with the one-time setup token that `serve` prints as `/_/setup#token=…` when no admin exists (401 `invalid_setup_token`, 400 `setup_done`). The link opens the dashboard's setup page. |

Codes are 6 digits (5 wrong attempts burn a code); link tokens are 256-bit. Both are stored as keyed hashes and work
once. Verification codes last 24 hours, reset codes 30 minutes, sign-in codes 15 minutes.

**Redirects.** `redirectTo` / `redirect_to` must match the allowlist (`--auth-redirect-url`): same scheme, host and
port as an entry, and, when the entry has a path, that path or one below it. In `--dev`, any `http://localhost` or
loopback URL is allowed too. A URL that doesn't match is refused (400 `invalid_redirect`), never replaced. Without
`redirectTo` the first allowlist entry is used. Email links point at the app with the token in the fragment:
`<app url>#ferrix_verify_token=…`, `#ferrix_reset_token=…`, `#ferrix_magic_token=…`; the TS client's
`handleRedirectCallback()` consumes them (and the OAuth fragment) and removes them from the address bar.

## Admin endpoints

`GET /api/admin/logs`, `GET /api/admin/logs/stream` (Server-Sent Events) and `GET /api/admin/stats` serve the
dashboard's execution log and stats; `POST /api/admin/impersonate` mints a short-lived session of a user. They
require `Authorization: Bearer <access token>` of an admin session (also for the event stream, which clients read
with `fetch()` rather than `EventSource`, so tokens never go in URLs). The event stream ends with `event: expired`
when the token expires and with `event: revoked` as soon as the admin's session ends. The dashboard's data views use
the admin-only system functions (`_system/dashboard:*`, `_system/settings:*`, `_system/audit:*`, ...) over this
protocol. All of it is specified in docs/CONTRACT.md §4.

Operations endpoints, with the same admin bearer auth (docs/CONTRACT.md §4.2, §4.4):

- `GET /api/admin/backups`, `POST /api/admin/backups` (`{ includeAux?, includeFiles? }`),
  `GET /api/admin/backups/{name}` (download), `DELETE /api/admin/backups/{name}`.
- `GET /api/admin/export/{table}?format=jsonl|csv`: one table, streamed from a consistent snapshot.
- `POST /api/admin/import/{table}?mode=insert|upsert|replace&format=jsonl|csv`: a JSONL or CSV body (CSV as
  exported, cells typed with the table's shape), validated, then written in batches through the normal write path
  (subscriptions update as each batch commits).
- Staged restores: `POST /api/admin/backups/{name}/restore` or `POST /api/admin/restore?name=<file>` (the archive as
  the body) check a backup completely and stage it; it replaces the database at the next start of the server.
  `GET /api/admin/restore` (also part of `GET /api/admin/backups`) shows it, `DELETE /api/admin/restore` cancels it,
  `POST /api/admin/restore/restart` (only with `--restart-exit-code`) shuts the server down gracefully for its
  supervisor to restart it. Open sockets are closed by the shutdown; clients reconnect as after any restart.
- Continuous replication (`serve --replicate-to`): `GET /api/admin/replication` (`{ enabled: false }`, or the status:
  target, generation, shipped and current ts, lag in commits and ms, local journal size, last segment and snapshot,
  errors, the generations and their restorable ranges; also as `replication` in `GET /api/admin/backups`),
  `POST /api/admin/replication/snapshot` (a snapshot now), and `POST /api/admin/restore/point-in-time` with
  `{ toTs?, toTime?, generation? }`: the state at that commit ts or time is rebuilt from the replication target,
  checked, and staged exactly like an archive (then `restart` or the next start applies it).

`GET /metrics` (Prometheus text) exists only when the server runs with `--metrics`; with `--metrics-token` it needs
`Authorization: Bearer <token>` (a fixed token, not an access token).

## Serving the app

Requests that match no Ferrix route (`/api/*`, `/_/*`, `/metrics`) or app route (`Ferrix::routes`) go to the app's
static frontend when one is configured (`Ferrix::static_dir`, `Ferrix::embed_static`): the file, or `index.html` for
client-side routes. Paths under `/api` and `/_` are never answered with static files. See docs/CONTRACT.md §4.4.

## File storage

Files are uploaded through signed upload URLs and served by id. The Rust side (`ctx.storage`, the `_storage` table,
configuration) is in docs/CONTRACT.md §1.6; `uploadFile(uploadUrl, blob, { onProgress })` in `@ferrix/client`
implements the client side of the upload.

### `POST /api/storage/upload?token=<token>[&filename=<name>]`

The URL comes from `ctx.storage.generate_upload_url()` (a mutation or action returns it to the client). The body is
the raw file content (not multipart form data); `Content-Type` is stored as the file's type (optional; must be a valid
`type/subtype[; params]` of at most 255 printable ASCII bytes). `filename` is optional; only its last path segment is
kept (control characters removed, at most 255 characters). No `Authorization` is needed: the token is the
credential, and `createdBy` is whoever generated the URL.

- **200** `{ "storageId": "<id>" }` (an `Id<"_storage">`) once the blob is stored and its document committed.
- **400** `invalid_content_type`; `upload_failed` (the request body broke off).
- **403** `invalid_upload_url` (missing, malformed or wrongly signed token), `upload_url_expired` (older than an
  hour), `upload_url_used` (an upload with it already succeeded).
- **409** `upload_in_progress` (another upload with the same URL is running).
- **413** `file_too_large`: refused up front when `Content-Length` exceeds the limit (default 100 MiB), otherwise as
  soon as the streamed body does. Nothing is stored and the URL stays usable.
- **429** `rate_limited` with `Retry-After` (per client IP; default burst 60, 120 per minute).

Errors use the auth endpoints' shape (`{ "status": "error", "errorMessage", "errorData": { "code", "data" } }`) and
`Cache-Control: no-store`. A failed upload doesn't use the URL up; it can be retried until it expires.

### `GET /api/storage/{id}[?exp=<s>&sig=<sig>][&thumb=WxH[&fit=contain|cover]][&download=1]` (and `HEAD`)

`ctx.storage.get_url(id)` returns this URL (absolute when the server has a public URL). Anyone holding it can read the
file: ids carry 96 random bits, so URLs are unguessable capabilities, and they never change.

**Signed URLs.** `ctx.storage.get_signed_url(id, ttl)` returns the URL with `exp` (expiry, seconds since the epoch)
and `sig` (base64url HMAC-SHA256 of `<id>\n<exp>` with a server key). Other parameters (`thumb`, `fit`, `download`)
can be appended; the signature covers the file and the expiry only. A response to a signed URL is cached at most
until it expires (`max-age` ≤ the time left). With **private storage** (`--storage-private`), a request without a
signature is refused, so signed URLs are the only way to read files.

- **403** `signature_required` (private storage, no `exp`/`sig`; also for unknown ids), `invalid_signature`
  (malformed, tampered with, or for another file; also when only one of `exp`/`sig` is given), `url_expired`.
  Checked before anything else.
- **200** the content, streamed, with `Content-Type` (as uploaded, else `application/octet-stream`),
  `Content-Length`, `ETag: "<sha256>"`, `Accept-Ranges: bytes`, `Cache-Control: private, max-age=2592000, immutable`
  (a file's content never changes), `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, and
  `Content-Disposition`.
- **304** when `If-None-Match` lists the ETag (or `*`).
- **206** for a single `Range: bytes=a-b | a- | -n`, with `Content-Range: bytes a-b/size`. `If-Range` with another
  ETag, and malformed `Range` headers (one malformed range-spec invalidates the header), get the whole file (200).
- **206 `multipart/byteranges`** for several ranges (RFC 9110 §14.6): `Content-Type: multipart/byteranges;
  boundary=<32 hex chars>` and an exact `Content-Length`; each part is `--<boundary>\r\nContent-Type:
  <type>\r\nContent-Range: bytes a-b/size\r\n\r\n<bytes>\r\n`, and the body ends with `--<boundary>--\r\n`.
  Unsatisfiable ranges in the set are dropped; overlapping and adjacent ranges are coalesced, and parts come in
  ascending order (whatever the request's order). If one range is left after that, the response is a plain single
  `206`. More than 16 ranges: the header is ignored (200 with the whole file).
- **416** when no range is satisfiable, with `Content-Range: bytes */size`.
- **404** `not_found` for unknown or deleted files and ids of other tables.

**Thumbnails** (`thumb=WxH`, or `thumb=N` for an `N×N` box; `fit=contain` (default) or `cover`): for files whose type is `image/jpeg`, `image/png`,
`image/webp` or `image/gif` (the first frame). `contain` fits the image inside `W×H` keeping its aspect ratio and never
scales up; `cover` fills exactly `W×H` (scaled, center cropped). `W` and `H` go from 1 to 1024 unless the server
allows only a list of sizes (`StorageConfig::thumbnail_sizes`). The response is a PNG when the image has
transparency, otherwise a JPEG, with `Content-Type` accordingly, `ETag: "<sha256>.t1.<W>x<H>.<fit>"` (one per
variant; `If-None-Match` → 304), `Content-Disposition: inline`, `Accept-Ranges: none` (ranges are ignored), and the
same caching and security headers. Generated once, then cached on the server; deleting the file deletes them.
- **400** `invalid_thumbnail`: a malformed `thumb` or `fit`, or a size that isn't allowed (the message lists them).
- **415** `unsupported_image`: not one of the four types (by content type, then by the bytes).
- **422** `image_too_large`: more than 40 megapixels (refused from the image header, before decoding: decompression
  bombs cost nothing), or a file over 32 MiB; `invalid_image`: the bytes can't be decoded.

**Content-Disposition and active content.** Images (png, jpeg, gif, webp, avif, bmp, ico), audio, video,
`text/plain`, `text/csv`, `application/json` and `application/pdf` are `inline`; everything else, in particular
HTML, SVG, XML and JavaScript, is an `attachment`, so an upload can't run script on the app's origin. `?download=1`
forces `attachment`. The original file name is included (`filename="…"; filename*=UTF-8''…`). Every response except
inline PDFs (which browsers refuse to render sandboxed) also carries `Content-Security-Policy: default-src 'none';
style-src 'unsafe-inline'; sandbox`, which gives a file rendered by a browser an opaque origin.
