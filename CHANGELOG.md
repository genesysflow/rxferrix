# Changelog

Notable changes to RxFerrix, newest first. Versions before 0.8.0 are in the git history. Numbers in parentheses are pull requests.

## Unreleased

## 0.13.0 - 2026-11-11

### Changed: check these when upgrading

- **`admin` commands are offline.** `admin create`, `admin reset-password` and `admin disable-2fa` refuse a data directory a server is using; stop the server first. A running server still creates its first administrator through the setup link it prints. Changing an administrator's password therefore needs the server stopped. (#100)
- **Float fields store floats.** A whole number written into an `f32` or `f64` field as a JSON integer is stored as a float, and untyped bounds on such fields (dashboard filters) compare as floats. Documents already holding an integer in a float field are not rewritten: until they are written again, `eq` and range queries on that field, and dashboard filters, do not find them. (#101)
- **Refresh tokens.** A session's next refresh token is derived from the current one with the server's key. A refresh asked again within `refresh_reuse_grace` (10 s) gets the same new token instead of `stale_refresh_token`, so an answer lost on the network no longer ends the session. The client sends an unanswered refresh again after 3 and 6 seconds. (#108)
- **Stored secrets stay with their destination.** A stored SMTP password is kept only for the same host, scheme and `tls` setting, and a stored OAuth client secret only for the same kind and issuer. Changing those asks for the secret again. (#106)
- **Path rate limits** match the path as the file server reads it (percent-decoded, without empty and `.` segments). (#106)
- **`Listen::Acme` and `Listen::CertFiles`** have a new `public_port` field. With `--public-url https://…`, the HTTP redirect goes to that URL's port instead of the port bound. (#105)
- **Client messages.** A `ModifyQuerySet` lists removes before adds. Arguments that fit in no message, and the query that would take a socket past 8 MiB of watched arguments, are refused with `args_too_large` before anything is sent. (#112)
- **A restore that cannot be finished stops startup** with an error that says where its files are, instead of starting on a half-restored directory. Running `restore` again abandons it. (#104)
- **Dependencies.** rusqlite 0.41 (SQLite 3.54.1), zstd 0.14.3, tokio 1.54.1, arrow and parquet 60.1. (#102, #103)
- **Minimum Rust version** is declared: 1.95.

### Fixed

- A subscribed query that timed out and was retried after another commit left its old error in place: debug builds stopped the subscription manager, release builds could send the timeout again. (#98)
- A scheduled action's retry delay overflowed after many attempts and kept the server from starting; a workflow whose step's job had moved to the history could not end. (#99)
- A restore that failed while moving files deleted what it had not moved yet. Restores applied at startup now prune old `pre-restore-*` copies. (#104)
- With `--https` on remapped container ports, the HTTP redirect pointed at the internal port and the image's health check never passed. The redirect listener answers `GET /api/health`, and `health` reads `RXFERRIX_HEALTH_URL`. (#105)
- A sign-in in flight during a password reset or a user deletion still got a session. (#106)
- The external JWT cache grew with every write to a cached user's document. (#106)
- A directory import with a bad file left the tables before it imported; the `import` command cut off the cleanup of unfinished uploads. (#107)
- Query-set changes queued while a connection waited for a slow query were not counted against any size cap. (#110)
- A `*/` in a doc comment broke the generated TypeScript. (#111)
- The client reconnected several times a second when the server refused a message for its size; mutations could settle against the previous socket's results after a reconnect with an expired token; a completed mutation could wait for ever on a client watching nothing. (#112)
- The dashboard's data grid unticked selected rows on every write to the table; revoking a session and cancelling a queued job happened on one click. (#113)
- List thumbnails in the dashboard's Files page stayed broken after a rate-limited request. (#97)
- The `systemd-run` examples in `deploy/rxferrix.service` could hand the state directory to another user. (#109)

### Added

- A `cargo audit` workflow, on lockfile changes and weekly. (#103)
- A CSV export for spreadsheets in the dashboard and at `GET /api/admin/export/{table}?format=csv&spreadsheet=true`: text that would run as a formula gets a leading `'`. (#113)
- Documentation of the replication and restore options. (#109)
- `SECURITY.md`, this changelog, and a CI check of the minimum Rust version.

## 0.12.0 - 2026-11-07

### Changed

- The dashboard was restyled, and documents are edited as a form built from the table's type. (#96)

## 0.11.0 - 2026-11-07

### Performance

- Aggregate range reads are bounded by their end key. (#73)
- Search and vector entries are not rebuilt when their fields are unchanged; search filters are matched inside FTS5, which rebuilds filtered search indexes once on upgrade. (#74, #83)
- Result diffs are computed in the query runner instead of in socket writers. (#75)
- Less work per commit on the write path and in the subscription manager. (#76, #77, #78)
- Call responses are serialized without copying their results; a call's arguments are logged without serializing all of them. (#79, #85)
- Verified external JWTs and their users are cached. (#81)
- Used upload tokens are indexed by expiry. (#82)
- Filtered scans with a small limit read in growing batches. (#84)
- The dashboard's function stats are grouped in memory. (#87)
- The analytics copy applies commits as they arrive. (#88)
- A backup's copy is rewritten only when its journal was large. (#89)
- A round of due scheduled actions is claimed in one write transaction. (#90)
- Exported and imported files are copied without holding them in memory. (#91)

### Fixed

- Search indexes of tables no longer defined in code get their filter tokens on upgrade. (#94)
- An import's multipart upload is aborted when its copy is dropped. (#95)

## 0.10.2 - 2026-11-02

### Fixed

- The sign-up check runs when a magic link is requested. (#72)

## 0.10.1 - 2026-10-30

### Fixed

- WebSocket subscriptions count as query cache lookups in the metrics. (#70)

## 0.10.0 - 2026-10-30

### Added

- `rxferrix dev`: a development runner that rebuilds the app when its Rust sources change. (#59)
- Jitter on scheduled action retries. (#60)
- Slow queries and mutations are shown in the dashboard. (#61)
- Benchmarks over real sockets, and a musl benchmark run. (#62)

### Changed

- New document ids grow with time (a millisecond timestamp, then random bits). `_storage` ids stay fully random. (#67)
- mimalloc is the global allocator by default (feature `mimalloc`). (#63)
- The README was rewritten and the documentation expanded. (#58)

### Performance

- Session checks of access tokens are cached. (#64)
- Connections are checked for an update starting from the query that blocked the last check. (#65)
- A slow client's updates stay merged in its mailbox until its socket has room. (#66)

### Fixed

- HNSW nodes left unreachable by concurrent inserts are reconnected. (#69)

## 0.9.0 - 2026-10-29

### Security

- An email code's guess budget carries over when the code is issued again. (#19)
- Pre-registered accounts: the gaps left when someone registers an address they do not own are closed. (#20)
- Rate limits count IPv6 callers by /64, and concurrent password hashing is capped. (#21)
- Proxy headers are trusted only from the proxy's addresses (`--trust-proxy-from`). (#22)
- WebSocket messages and a socket's live query arguments have size limits. (#23)
- HTTPS serving sends HSTS, redirects only to known hosts, and stored profile picture URLs are filtered. (#24)

### Fixed

- `db.patch` rejects keys that are not fields of the table. (#25)
- A takeover lets go of the data directory only after every database connection closed; it copies only the journal delta at the handover. (#26, #31)
- Restores are checked the way `serve` opens the database, and file swaps are crash-safe. (#27, #48)
- Scheduled actions survive deploys, and crons stay on time. (#28)
- A workflow whose engine job fails is marked failed. (#29)
- Replication ships commits while snapshots are taken, and notices lost segments. (#30)
- Subscriptions that timed out run again on a timer. (#32)
- One connection can no longer hold the subscription frontier indefinitely. (#35)
- Client: query results are frozen so they cannot corrupt the diff base; auth races are closed; a mutation the server would no longer recognize is not sent again. (#36, #37, #38)
- Dashboard: five UI races and traps. (#39)
- TypeScript codegen: output directory from the environment, optional fields, function docs, 64-bit integers. (#40)
- Macros: clearer errors for types that cannot cross the wire, and two silent mistakes are refused. (#42)
- Document writes are atomic per operation. (#47)

### Added

- Shorthands for errors meant for the client (`ensure_user!` and friends). (#43)
- `chrono` and `uuid` features. (#44)
- React and Svelte can keep the previous result while a query's new arguments load. (#46)
- One-parameter functions take their argument bare. (#51)
- Let's Encrypt and thumbnails are optional features. (#54)
- Client, testing and type mapping guides. (#50)

### Performance

- Three costs taken off the subscription hot paths; query results are prepared in the runner tasks. (#33, #52)
- Backfills stream documents instead of loading whole tables. (#34)
- Point-in-time restores fetch segments in parallel. (#49)

## 0.8.0 - 2026-10-27

### Performance

- Document and execution log counts are faster. (#16)
- Scheduler bookkeeping is batched during replication. (#17)
