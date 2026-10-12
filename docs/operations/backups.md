# Backups and restores

## Backups

`todo-server backup [--out file] [--with-aux] [--with-files] [--include-secret]` takes a consistent
snapshot (`VACUUM INTO`) while the server runs, without blocking writes, into a `.tar.zst` archive (`data.db`, plus
the execution log, local file blobs and `auth_secret.key` when asked; the key only with `--include-secret`, since it
can forge sessions). Keep a copy of `auth_secret.key` somewhere safe either way: it also encrypts the secrets stored
in `_settings`, so a restore elsewhere without it signs everyone out and those settings must be entered again. `serve --backup-every 6h --backup-keep 7 [--backup-to s3://bucket/prefix|dir]` takes them on a
schedule (the cron `rxferrix-backup`, visible on the Scheduler page) and deletes old scheduled ones. The same schedule
and destination can be set on the dashboard instead (Backups, Configure…; stored in `_settings`, the S3 secret key
encrypted like the other settings secrets): it takes effect without a restart, and a command-line flag wins over it
(`--backup-every` for the schedule, `--backup-to` for the destination). Admins can list,
create, download and delete backups over `/api/admin/backups` (docs/CONTRACT.md §4.2). `todo-server restore
<archive|s3://…>` is offline: it refuses while a server holds the data directory, checks the archive (checksums,
SQLite integrity, schema, indexes) and only then swaps the database, keeping the old files in
`<dir>/pre-restore-<time>/`.

## Restoring from the dashboard

Restoring from the dashboard (Backups, "Restore", or an archive uploaded from your computer) never swaps the
database under the running server. The backup is checked there and then, exactly like `restore` does; a bad archive is
refused with the reason. A good one is *staged* in `<dir>/restore-staged/` with a `pending-restore.json` marker, and
the next start of `serve` applies it before opening the database (the current files go to `pre-restore-<time>/`).
Until then the dashboard shows it as pending and can cancel it (or `todo-server restore --cancel` offline). With
`serve --restart-exit-code 75` (set in `deploy/rxferrix.service`; for Docker add a restart policy), the dashboard also
offers "Restart now": a graceful shutdown and an exit with that code, for systemd or Docker to start the server again.
Without it, restart the server yourself. A staged restore never restores `auth_secret.key` (only `restore` does), and
it replaces everything else, administrators and sessions included: whoever is signed in may have to sign in again.

## Continuous replication and point-in-time restore

`serve --replicate-to s3://bucket/prefix` (or a directory)
loses at most about a second of commits and can restore any moment of the last week:

```sh
todo-server serve --replicate-to s3://bucket/replica [--replicate-every 1s] [--snapshot-every 24h] [--replicate-keep 7d]
todo-server restore --from s3://bucket/replica --list                          # generations and restorable ranges
todo-server restore --from s3://bucket/replica --to-time 2026-10-09T12:00:00Z  # or --to-ts N; default: the newest
```

Every commit writes its document changes to a journal table in `data.db` *in the same SQLite transaction* (so the
journal never misses a commit nor holds a rolled-back one); every `--replicate-every` the new entries are shipped as a
checksummed, compressed segment named by its ts range, then deleted locally. A snapshot (an ordinary backup archive)
is taken when a generation starts and every `--snapshot-every`; snapshots and segments older than `--replicate-keep`
are deleted, except what the oldest restorable point still needs. A restore takes the newest snapshot before the
point and replays the journal up to it through the normal write path (indexes, search and vector indexes are
rebuilt), then checks it like any restore. From the dashboard (Backups, "Restore to this point") it is staged and
applied at the next start, like an archive. If the target is unreachable, commits pile up locally (lag in
`GET /api/admin/replication`, `/metrics` and the dashboard) and are shipped when it is back; past
`--replicate-max-pending` (1 GB) the local journal is dropped and replication starts over with a new snapshot. A
restored database, or one that was written without replication (e.g. `import` offline), starts a new *generation*
next to the old ones, so history is never overwritten. The journal holds documents, not file contents, but **deleted
files' contents are kept as long as restores reach** (`--replicate-keep`; `--storage-keep-deleted` to choose another
window): the orphan sweep removes them only after that, so a restore to before a deletion finds the file. They are gone
for everything else at once (their document is deleted). Restores leave a local storage directory in place (S3 files
stay in their bucket). Keep a copy of `auth_secret.key`. The cost is one extra row per commit: about 5% fewer commits
per second in a sequential benchmark (docs/CONTRACT.md §4.4).

## Litestream

Litestream works too, as an alternative: `data.db` is a normal SQLite database in WAL mode, so
[Litestream](https://litestream.io) can replicate its pages next to the server (`litestream replicate
fx_data/data.db s3://bucket/data.db`; `litestream restore` into an empty data directory before starting the server).
Replicate `data.db` only (`aux.db` is the execution log), with the same caveats for blobs and `auth_secret.key`.
Archive backups, `--replicate-to` and Litestream can be combined.

## Export and import

`todo-server export [--tables a,b] [--format jsonl|csv] [--out dir]` writes tables from one
consistent snapshot; `todo-server import <dir|file.jsonl|file.csv> [--format jsonl|csv] [--mode
insert|upsert|replace]` loads JSONL or CSV through the normal write path in batches (validated, indexed, ids and
creation times kept), offline. CSV is read as the export writes it (a header row, RFC 4180), and each cell is typed
with the table's Rust type: `42` is a number in a number field and text in a `String` field, JSON cells become arrays
and objects, an empty cell is `null` (or `""` for a non-optional string). Fields the table doesn't define are left
out, and the report lists them, so files converted from another backend may carry extra columns. Admins can stream
one table with `GET /api/admin/export/{table}?format=csv` and import into a running server with
`POST /api/admin/import/{table}?format=csv`.
