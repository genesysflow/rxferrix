# The dashboard

Every server has an admin dashboard at `/_/` (e.g. http://127.0.0.1:8090/_/), embedded in the binary. Create an
administrator with the `/_/setup#token=…` link that `serve` prints at startup while there is no administrator, or with
`todo-server admin create you@example.com`, then sign in.

- **Data**: every table (system tables grouped below), with a live, virtualized document grid (smooth at 10k+ rows;
  pages load as you scroll; new rows slide in, changed cells flash, deleted rows fade out, up to 5,000 loaded rows), filters on index fields, a search box for tables with a full-text search index (live, most
  relevant first), a quick filter over the loaded rows, JSONL/CSV export, a side panel with a JSON editor (insert,
  edit, delete; validated against the table's Rust type), and clickable ids that open the referenced document. The
  schema view lists indexes, tokenizers (with their language), vector index kinds (with HNSW parameters) and the
  table's analytics columns with their SQL types. Secrets (password and token hashes) are never shown,
  and system tables are read-only except the `name`, `image` and `emailVerified` of users.
- **Functions**: every function with its TS signature, a runner (JSON args, optionally *as a user* through a real,
  short-lived, audited session), latency (p50/p95) and calls/errors charts over the last hour to 7 days, from
  the execution log, and the function's own live log.
- **Logs**: a live tail of executions (filter by path or one function, kind, outcome; expand for the error, log lines
  and args); new executions slide in, tinted by outcome.
- **Users**: users, linked providers, two-factor status (with a reset for users who lost their authenticator),
  sessions (revoked live; impersonation sessions marked), delete, and "open app as
  user": a short-lived, audited session of the user opened in a new tab at an allowed redirect URL.
- **Files**: stored files with thumbnails and previews (images, video, audio, text), download, delete, and upload by
  drag and drop; works with private storage (signed URLs). `Id<"_storage">` values in the data grid link here.
- **Workflows**: workflows by state, an open one's steps (args, outcome, duration, job), and cancel.
- **Scheduler**: crons with their schedule, time zone, last and next run, and "run now" (system crons last);
  scheduled jobs live, by state, with cancel (running actions too), run now and retry.
- **Migrations**: every `#[migration]` in run order with its dependencies, live progress, errors linking to the failing
  document, the schema check,
  run / cancel / restart and dry runs with before/after examples; migrating tables are highlighted.
- **Backups**: create (optionally with `aux.db` and files), download, delete and **restore** backups (a stored one
  or an archive from your computer: checked completely, then applied at the next start; cancel, or "Restart now"
  under systemd/Docker); **Configure…** sets the schedule (interval, how many to keep, files and `aux.db`) and the
  destination (the data directory, another directory, or an S3-compatible bucket with its own credentials, with a
  "Test destination" check), applied at once; with `--replicate-to`, the replication status
  (target, lag, last segment and snapshot, generations) and a point-in-time restore picker (a time or a commit ts);
  export a table (JSONL/CSV) and import JSONL or CSV (insert, upsert or replace).
- **Analytics** (with `--olap`): the copy's state and lag, its tables and columns, a SQL editor (one SELECT, the
  sandbox of `ctx.olap`, audited) with the results as a table, and the whole result as a CSV or Parquet download;
  rebuild the copy.
- **Environment**: environment variables (`ctx.env`): add, edit, reveal, copy, remove, paste a `.env`; live, and
  queries that read a variable rerun as soon as it changes.
- **Settings**: OAuth providers (GitHub, Google, Discord, Apple, OIDC), SMTP (with a test email), redirect URLs,
  sign-ups, token lifetimes and CORS origins, applied without a restart. Fields fixed by the code, a flag or the
  environment are read-only; secrets can be replaced but are never shown.
- **Account**: two-factor authentication for your own administrator account (an authenticator app plus recovery
  codes); signing in then asks for a code after the password. `admin disable-2fa <email>` removes it from the server
  if both are lost.
- **Audit log**: document edits, user and session changes, settings and environment variable changes, impersonation,
  function runs, restores and analytics queries, live, filterable by
  administrator, action, table, document and time, with field-level diffs.
- **Overview**: commit ts, commits/s, connections, shared queries and reruns, the scheduler queue, database sizes,
  busiest and failing functions, the query cache hit rate and the analytics copy's state.

Ctrl+K (⌘K) opens a command palette (pages, tables, theme, sign out); Alt+1…9 and Alt+0 open the first ten pages in
sidebar order.

It talks to the server over the same WebSocket protocol as apps (admin-only `_system/*` functions), so every
view is live, plus `/api/admin/*` for logs, stats, backups, replication, analytics, export/import and impersonation (docs/CONTRACT.md §4). The built assets are committed, so
`cargo build` needs no JS toolchain; after changing `packages/dashboard` (or the client packages it bundles), run
`bun run build:dashboard` and commit `crates/rxferrix/dashboard-dist` (`cargo test` fails while it is stale). For UI
work, `cd packages/dashboard && bun run dev` serves it on :5174 with `/api` proxied to :8090. Build without it with
`default-features = false` on the `rxferrix` dependency.
