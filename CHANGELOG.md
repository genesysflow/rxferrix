# Changelog

Notable changes to RxFerrix, newest first. Versions before 0.8.0 are in the git history. Numbers in parentheses are pull requests.

## Unreleased

## 0.16.0 - 2026-11-24

### Security

- url 2.5.9: fixes RUSTSEC-2026-0592, a stack overflow in `Url::origin()` on nested `blob:` URLs. rxferrix's own calls (CORS origins in the settings, redirect targets in auth) check that the scheme is `http` or `https` first, and its dependencies don't call it, so it wasn't reachable here. (#120)

### Changed: check these when upgrading

- **The recorded mutation responses moved to another table.** The responses kept for about ten minutes so that a mutation resent after a reconnect runs once are now in `_mutation_responses` (one packed row per commit, more when the responses are large) instead of `_mutation_dedup` (one row per response); the rows move over when the database first opens, and a takeover from an earlier version carries them as before. The server now keeps an index of them in memory: 20 to 40 bytes per WebSocket mutation of the last ten minutes, loaded when the database opens. That is 13 to 23 MiB at 1,000 mutations a second, 130 to 230 MiB at 10,000 (6 million responses, loaded in under a second), and after ten minutes at the 42,000 a second the benchmark below sustains, its process (server and load generator together) used 1.3 GiB, with 421 MiB in the table. Going back to an earlier version loses those ten minutes of responses: a mutation resent across the downgrade can run twice, and a takeover by an earlier version stops at its last step and serves the database as it was. (#119)
- **A group commit can hold more and stay open longer.** Up to 4096 mutations (from 256), and four times as long as the last commit took when that is more than 4 ms, up to 32 ms. Nothing changes while the queue stays under 256 mutations and empties within those 4 ms. With a constant queue, a mutation's result can now wait up to 32 ms for its group where it waited 4 ms (so subscribers see such a writer's commits in larger steps), and throughput is higher (below). `rxferrix::db::MAX_BATCH_AGE` is now that 32 ms bound; the 4 ms is `BATCH_AGE`. (#119)

### Performance

- Mutations over WebSockets are about twice as fast under load on a disk where a commit's fsync takes milliseconds. Measured with the new `bench_transfer` (a mutation that reads two of 100,000 accounts and writes both; 64 sockets with 40 calls in flight each; encrypted btrfs NVMe SSD, commits waiting for the disk, about 3 ms per fsync; medians of three 8-second runs on a new database): 13.3k to 28.3k transfers/s with accounts picked uniformly (p50 latency 181 ms to 60-105 ms: it varies from run to run, 80 ms the median of 16 later runs, which gave 26.9k/s) and 22.2k to 52.5k/s with most transfers between a few hot accounts (110 to 49 ms, steady; 51.2k/s in those later runs). With 1024 sockets and 3 calls in flight each: 9.0k to 20.1k/s and 13.2k to 30.3k/s. Without waiting for the disk: 16.0k to 30.1k/s and 28.2k to 49.4k/s. On tmpfs (no disk cost at all): 36.6k to 50.1k/s and 40.2k to 60.8k/s. Kept up for minutes, the hot-account case averaged 20.3k/s over 5 minutes before and 41.9k/s over 12 minutes now. Two changes (#119):
  - The responses recorded for retried mutations are written as one packed row per commit (about 15 bytes per response). One row per response, keyed by session, wrote a B-tree page for every session with a mutation in the commit. This is the whole gain on tmpfs, and 13.2k to 17.5k/s and 22.2k to 35.2k/s of the btrfs figures.
  - A group commit stays open four times as long as the last commit took (see above): the rest on btrfs, nothing on tmpfs. Groups went from 230 transfers and 7 ms in the commit to 1800 and 22 ms. One-insert mutations in process (`bench_writes`): 46.4k to 68.2k/s with 1024 writers (p99 27 to 19 ms), unchanged at 64 and 256 writers.

### Fixed

- **A mutation could keep part of its writes after SQLite ended its transaction.** SQLite ends a transaction by itself on some errors (a full disk, an I/O error). A mutation that didn't stop at that error, or the next one in the same group commit, then wrote statement by statement outside any transaction: documents and journal rows were kept at a ts the database never reached, while the callers were told that the commit had failed (and, with an idempotency key, a success could be recorded for writes that were gone). Now nothing commits on the writer connection while a group is open except the group itself, and a group that lost its transaction fails as a whole. (#119)
- A busy server no longer stalls once a minute while the execution log is trimmed. The log was trimmed to `max_rows` only on a timer; at 50,000 mutations a second that deleted up to 3 million rows at once, and mutations waiting for the same disk fell from 50k/s to 2k/s for 4 to 5 seconds (21k/s to under 1k/s before this release's other changes). It is now also trimmed whenever an eighth of `max_rows` (and at least 512 rows) has been written since the last time. (#119)

## 0.15.0 - 2026-11-20

### Changed: check these when upgrading

- **Commits wait for the disk by default.** `--fsync-commits` (`AppOptions::fsync_commits`, `RXFERRIX_FSYNC_COMMITS`) is now on, so a power loss or OS crash no longer loses acknowledged writes. Files uploaded to local storage are synced to the disk before the upload is answered too (they weren't even with `--fsync-commits`), as are offline imports' blobs and backups written to a local directory. `TestApp::new` and `TestApp::with_auth` keep commits off the disk: their data is temporary. Each write is acknowledged about one fsync later (about 1 ms on an NVMe SSD), and one writer alone gets about 1k mutations/s; concurrent writers share each fsync. `--fsync-commits=false` brings back the previous behavior. Point-in-time restores and takeover copies still replay without waiting for the disk, and are synced before they replace the database. (#118)

### Performance

- Group commit takes up to 256 mutations and 4 ms per batch, from 64 and 2 ms. With commits waiting for the disk, the batch size had capped throughput at about 23k one-insert mutations/s; it now reaches about 45-47k/s with 256 writers. Throughput with 16 and 64 writers is unchanged; p99 latency at 64 writers rose from 3.5 to 4-5 ms in the same benchmark. (#118)

## 0.14.1 - 2026-11-20

### Security

- lettre 0.11.24 (the SMTP client of the email sender): fixes RUSTSEC-2026-0560 to 0565. The ones that matter most here: an address with a quoted local part, which sign-up accepts, could have its verification or password-reset email delivered to another mailbox (0563), and a bare CR or LF in a message's content could inject SMTP commands (0561). The others are STARTTLS response injection by a man-in-the-middle, unbounded buffering of a server's reply, and a panic on a display name containing CR or LF. rustix 1.1.6 (RUSTSEC-2026-0486). (#117)

## 0.14.0 - 2026-11-20

### Added

- The dashboard's Account page lists the administrator's own sessions (browser, IP, last use, expiry), marks the current one, and can revoke the others; it is backed by the new `_system/dashboard:my_sessions` query. (#116)

### Fixed

- The dashboard's session lists (Users and Account) show a session as expired once its expiry passes while the page is open, instead of only after the list next changes. (#116)

## 0.13.1 - 2026-11-11

### Fixed

- The dashboard drew `://` and other character runs as ligatures with a ligature font (JetBrains Mono, which `ui-monospace` resolves to on many Linux desktops), so `https://example.com` looked like it had one slash. Monospace text shows as typed. (#115)

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
