# Backups and restores

Back up the database, file contents, and server key according to how your app uses them. A database archive alone is not necessarily a complete recovery of uploaded files or encrypted settings.

All commands below belong to your app's binary. They use `fx_data` unless you pass `--dir`; always use the actual server data directory.

## Create a backup

```sh
./my-app backup --dir /var/lib/my-app --out ./backup.tar.zst
```

This creates a consistent database snapshot while the server runs. Optional contents are explicit:

| Option | Includes |
| --- | --- |
| `--with-aux` | Execution logs and finished-job history from `aux.db`. |
| `--with-files` | Local file blobs. |
| `--include-secret` | `auth_secret.key`, which signs sessions and encrypts secrets. |

Keep an independent secure copy of `auth_secret.key` even if you exclude it from archives. Without the original key, a restore elsewhere signs users out and cannot decrypt the app's stored secrets. Protect archives that include it accordingly.

For S3-backed files, preserve the bucket contents separately. The analytics copy is derived and does not need backup.

## Schedule backups

Use **Dashboard → Backups → Configure** to choose an interval, retention count, optional files/logs, and local or S3 destination. Changes apply to the running server, and the panel can test a destination.

Or configure startup flags:

```sh
./my-app serve --dir /var/lib/my-app \
  --backup-every 6h --backup-keep 7 --backup-with-files \
  --backup-to s3://my-bucket/backups
```

The backup cron is visible in **Scheduler**. Flags override the corresponding dashboard settings. S3 targets can use the process's AWS configuration; the dashboard can also store destination credentials encrypted.

## Restore an archive offline

Stop the server before using the restore command:

```sh
./my-app restore --dir /var/lib/my-app ./backup.tar.zst
```

The command validates checksums, SQLite integrity, schema, and indexes before replacing the database. It refuses to restore over an active server. Previous files go to `pre-restore-<time>/`; the two newest such recovery directories are retained.

A restore interrupted during replacement is completed on the next startup or restore attempt. Test a restore to a separate directory periodically so you know the archive, key, and file contents are sufficient.

## Restore from the dashboard

Choose a stored backup or upload an archive on **Backups**. The server validates it and stages a pending restore. The active database keeps running until the next startup applies the replacement.

Cancel a staged restore from the dashboard, or with `restore --cancel` offline. **Restart now** is available when a restart exit code and a working supervisor are configured. Otherwise restart the process yourself.

A staged restore never replaces `auth_secret.key`. It does replace database contents, including administrators and sessions, so you may need to sign in again after restart.

## Enable point-in-time recovery

Continuous replication records committed changes and ships them to a directory or S3 target, alongside periodic snapshots:

```sh
./my-app serve --dir /var/lib/my-app \
  --replicate-to s3://my-bucket/replica
```

Defaults are one-second shipping, daily snapshots, and a seven-day recovery window. Shipping is asynchronous: the interval is not a guarantee of maximum data loss when the destination or network is unavailable.

List recovery generations and ranges, then restore offline:

```sh
./my-app restore --dir /var/lib/my-app --from s3://my-bucket/replica --list
./my-app restore --dir /var/lib/my-app --from s3://my-bucket/replica \
  --to-time 2026-10-29T12:00:00Z
```

Use `--to-ts N` for a commit timestamp. With no target point, the newest recoverable state is used. The dashboard's point-in-time picker stages the same type of restore for the next startup.

The journal is written in the transaction with the document changes. A restore chooses a snapshot and replays changes through the requested point, rebuilding derived indexes as needed.

## Understand replication retention

Scheduler-only progress defaults to shipping at most once per minute (`--replicate-jobs-every 60s`) when no other writes are pending. After losing a server, more recent job progress may be missing, so some work can be attempted again. External side effects still need duplicate handling.

If the destination is unreachable, the journal accumulates locally. Watch lag in the dashboard or metrics. Above `--replicate-max-pending` (1 GB by default), replication drops the local backlog and must begin a new snapshot generation when possible. Retention cannot recover a period that was never shipped.

Restores and offline writes such as imports can start a new generation. Replication is recovery history, not an active failover server.

The journal contains metadata, not file bytes. Deleted file blobs are retained for the recovery window by default with replication, but you still need the local storage directory or S3 objects. Tune this with `--storage-keep-deleted`. Preserve `auth_secret.key` independently.

## Export or import documents

```sh
./my-app export --dir /var/lib/my-app --tables tasks --format jsonl --out ./export
./my-app import --dir /var/lib/my-app ./export --mode insert
```

Exports use one consistent snapshot and may run beside the server. Offline imports validate documents and update indexes in batches. Modes are `insert`, `upsert`, and `replace`; consult `import --help` before choosing a mode that overwrites data.

CSV is supported with `--format csv`. Cells are interpreted using the Rust field types; arrays and objects use JSON cells. Unknown columns are omitted and reported. IDs and creation times from exports are preserved.

The dashboard can export or import a table on a running server through admin endpoints. Use archive backups for whole-app recovery and document exports for moving or inspecting selected data.

## Use Litestream instead

`data.db` is a normal SQLite WAL database, so [Litestream](https://litestream.io/) can replicate it alongside the server. Restore into an empty data directory before startup. The same separate requirements apply to `auth_secret.key`, local blobs, and S3 objects; `aux.db` contains logs and job history.

See the [operations reference](/CONTRACT#_4-4-operations-backups-export-and-import-crate-ops) for archive contents, replication formats, and restore validation.
