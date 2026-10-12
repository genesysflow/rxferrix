# Running Ferrix in production

A Ferrix app is one binary plus one data directory (`--dir`, default `fx_data`: `data.db`, `aux.db` with the
execution log, `storage/` for files, `auth_secret.key`, `backups/`).

## Contents

- [Build with the right SQLite flags](#build-with-the-right-sqlite-flags)
- [Serve the frontend from the binary](#serve-the-frontend-from-the-binary)
- [Durability](#durability)
- [Release binary](#release-binary)
- [systemd](#systemd)
- [Docker](#docker)
- [CI](#ci)
- [Logs](#logs)
- [TLS](#tls)
- [Deploys with no downtime](#deploys-with-no-downtime)
- [Zero-downtime deploys with Docker or Kubernetes](#zero-downtime-deploys-with-docker-or-kubernetes)
- [Backups](#backups)
- [Restoring from the dashboard](#restoring-from-the-dashboard)
- [Continuous replication and point-in-time restore](#continuous-replication-and-point-in-time-restore)
- [Litestream](#litestream)
- [Export and import](#export-and-import)
- [Metrics](#metrics)

## Build with the right SQLite flags

Build your app with these SQLite flags. Apps created with `ferrix new` have them in `.cargo/config.toml` (so does
this repository); for an app created otherwise, put this in its `.cargo/config.toml`. Without it, rusqlite's bundled
SQLite shares one page cache behind a global mutex, so concurrent queries don't run in parallel (measured: 8 parallel
readers ran no faster than one; with the flags they scale to ~4x the single-reader throughput and beyond). Ferrix logs
a warning at startup when the flag is missing. It has to be the app's own config: Cargo gives a dependency no way to
set another crate's build flags, and `libsqlite3-sys` reads them only from this variable. (Memory statistics, the
second flag, are also turned off at runtime; the page cache can only be changed at build time.)

```toml
[env]
LIBSQLITE3_FLAGS = "-USQLITE_ENABLE_MEMORY_MANAGEMENT -DSQLITE_DEFAULT_MEMSTATUS=0"
```

## Serve the frontend from the binary

`.static_dir("./web/dist")` reads a directory at runtime;
`.embed_static::<Assets>()` embeds it at compile time, so the whole app ships as one file:

```rust
#[derive(ferrix::rust_embed::Embed)]
#[folder = "../web/dist/"]                        // relative to the crate
#[crate_path = "ferrix::rust_embed"]
struct Assets;

ferrix::Ferrix::new()
    .embed_static::<Assets>()
    .routes(axum::Router::new().route("/webhooks/stripe", post(stripe)))   // your own routes, if any
    .run()
```

Unknown paths get `index.html` (client-side routing) except under `/api` and `/_`, which static files never shadow.
HTML is `no-cache`, hashed assets (`assets/*`, `name-<hash>.js`) are cached for a year, and `<file>.br` / `.gz`
next to a file are served to clients that accept them. The todo example embeds `examples/todo/web/dist` when it
exists.

## Durability

By default SQLite runs in WAL mode with `synchronous = NORMAL`: a crash of the process loses nothing,
but a power loss or OS crash may lose the last commits. `--fsync-commits` (`AppOptions::fsync_commits`) makes every
commit wait for the disk; group commit shares one fsync across the mutations waiting at that moment, so it costs most
when writes come one at a time. WAL checkpoints run on a background thread, not inside commits. Measured on an NVMe
SSD (btrfs, encrypted), one-insert mutations: 8.9k/s with 1 writer, 16.2k/s with 16 and 18.7k/s with 64 by default
(p99 0.2 / 3.5 / 11 ms); 1.1k/s, 7.3k/s and 12.5k/s with `--fsync-commits`.

## Release binary

`scripts/release.sh` builds the web app and a static `x86_64-unknown-linux-musl` binary of
`todo-server` into `dist/` (everything linked in: SQLite, FTS5, sqlite-vec, TLS, zstd; about 20 MB). C code is
cross-compiled with `zig cc` through cargo-zigbuild, fetched with mise if not installed. For your app:
`PACKAGE=my-server WEB=web scripts/release.sh`; `FEATURES=olap` adds the analytics copy (DuckDB; about 57 MB).

## systemd

[`deploy/ferrix.service`](../deploy/ferrix.service) runs the binary as a dynamic unprivileged user with
its data in `/var/lib/ferrix`, behind a reverse proxy or with `--https`; it stops gracefully on SIGTERM, and restarts
on the dashboard's "Restart now" (exit code 75, `FERRIX_RESTART_EXIT_CODE`).

## Docker

The [`Dockerfile`](../Dockerfile) runs `scripts/release.sh` and copies the binary into a `FROM scratch`
image with CA certificates, running as an unprivileged user with the data in the `/data` volume. To build the image
locally you need a Docker daemon you can use (on Linux: membership in the `docker` group, or rootless Docker/Podman;
`podman build` works with the same file) and network access for the base images, Zig and the crates; the first build
takes several minutes (later ones reuse the BuildKit caches). The build context includes `.cargo/config.toml`, which
sets the bundled SQLite's flags: don't pass `LIBSQLITE3_FLAGS` to the build.

```sh
docker build -t todo-server .                     # --build-arg PACKAGE=my-server --build-arg WEB=web for yours
docker run -p 8090:8090 -v todo-data:/data todo-server
docker run -p 8090:8090 -v todo-data:/data -e FERRIX_RESTART_EXIT_CODE=75 --restart unless-stopped todo-server
                                                  # lets "Restart now" apply a restore staged in the dashboard
docker run --rm todo-server functions             # any CLI command
```

## CI

[`.github/workflows/release.yml`](../.github/workflows/release.yml) runs every gate of [Tests](../README.md#development) (end-to-end
tests included, in Chromium) and, in a job of its own, the tests with the `olap` feature; builds the static musl
binary with `scripts/release.sh` (an artifact; attached to the GitHub release for `v*` tags) and, for tags and manual
runs, `todo-server-olap` too; and builds the Docker image alongside, smoke-tests it (`functions`, `/api/health` with a
volume) and pushes it to `ghcr.io/<owner>/todo-server` for `v*` tags. A new push cancels the run it supersedes;
caches are kept even when a run fails, so DuckDB is compiled only when it changes.

## Logs

`serve` writes JSON lines when stdout isn't a terminal (systemd, Docker): `timestamp`, `level`, `target`,
`fields` and the span context, one object per line, for Loki, Vector or `jq`. `--log-format pretty|compact|json` (or
`FERRIX_LOG_FORMAT`) chooses explicitly; `RUST_LOG` filters as usual.

## TLS

Either terminate TLS in a reverse proxy (pass `--public-url https://…`), or let the server do it:
`serve --https example.com --acme-email you@example.com` gets and renews Let's Encrypt certificates (TLS-ALPN-01 on
port 443, cached in `<dir>/acme`; try `--acme-staging` first) and redirects port 80 to HTTPS. `--tls-cert cert.pem
--tls-key key.pem` uses your own certificate, re-read every 30 s (`--tls-reload-every`): renewed files are picked up
without a restart or dropped connections, and a bad pair keeps the current certificate (logged). Addresses: `--https-addr` (default `0.0.0.0:443`), `--http-redirect`
(default `0.0.0.0:80` with `--https`; `off`).

## Deploys with no downtime

`todo-server serve --takeover` (same arguments as the running server) starts the new
version next to the old one on the same data directory: it copies the database (a consistent snapshot; the old server
keeps writing), opens the copy with the new code (new and changed indexes, search and vector indexes and aggregates
are built there, the schema check runs, pending migrations are found), and replays what the old server committed
meanwhile from its change journal, checking every document against the new types. Then it takes the old server's
listening sockets: the old one stops accepting (new connections wait in the socket instead of being refused), lets
open requests finish, closes WebSockets with code 1012 so clients reconnect at once, and exits; the new one replays
the last commits, swaps the copy in (the old `data.db` stays in `takeover/previous/`) and serves. Acknowledged writes
are never lost, mutations retried by a reconnecting client still run once, and the pause is the old server's drain
plus the swap: about 0.2–0.5 s in our tests under write load, during which requests wait. **If the new version can't
serve the data** (a changed type that existing documents don't fit, with no migration for it), it exits with the
schema report and the old server carries on unaffected. `todo-server check` runs the same preparation on a copy
(also while the server runs) and reports, for CI or before a deploy. Under systemd, `systemctl reload` does it: see
`deploy/ferrix.service` (`Type=notify`, `NotifyAccess=all`, `ExecReload`).

## Zero-downtime deploys with Docker or Kubernetes

A container can't hand its process over, so a rollout starts
the new version in a second container on the same volume, and the image is set up for it: `FERRIX_TAKEOVER=auto`
(`serve --takeover=auto`: take over if a server is running in the data directory, else start normally) and
`FERRIX_AFTER_HANDOVER=wait` (the container taken over stays up, serving nothing, until it is stopped, so a restart
policy doesn't bring it back to take over again). The new container listens on its own address in its own network
namespace (a server only hands its sockets to a process in the same namespace, on the same address); the old one
closes its socket at the handover, so connections to it are refused from then on. The proxy must send each new
connection to whichever container accepts it and retry refused ones: Traefik's `retry` middleware, with the Compose
service name (`http://app:8090`: Docker's DNS gives every container of the service, and each connection tries them
in turn) or a Kubernetes Service's cluster IP (`service.nativelb`) as the address. Routing to the containers'
addresses fails instead: Traefik keeps retrying a request on the addresses it had when the request came in. Both
containers must run on the same host with the data on a local volume. `todo-server health [url]` checks
`/api/health` from inside the container (the image's `HEALTHCHECK`; the image has no curl).

- [`deploy/docker/compose.yaml`](../deploy/docker/compose.yaml): the app behind Traefik (3.6 or later: older releases
  can't talk to Docker 29). [`deploy/docker/deploy.sh`](../deploy/docker/deploy.sh) rolls out a new image: it starts a
  second container, waits until it is healthy (or removes it if it exits while the old one still serves) and removes
  the old one.
- [`deploy/k8s/ferrix.yaml`](../deploy/k8s/ferrix.yaml) (k3s: local-path volume, Traefik ingress): one replica, a
  rolling update that starts the new pod before stopping the old one (`maxSurge: 1`, `maxUnavailable: 0`), a pod
  affinity that keeps them on one node, a startup probe long enough to prepare a big database, a readiness probe, no
  liveness probe (the old pod doesn't answer while it waits), and Traefik's retry on the ingress. `kubectl set image
  deployment/ferrix app=…` deploys; a version that can't serve the data stays in CrashLoopBackOff while the old pod
  serves (`kubectl rollout undo`).

Tested under write load, through Traefik, by `bun scripts/docker-rollout-e2e.ts` and `bun scripts/k3s-rollout-e2e.ts`
(k3s in a Docker container; both need a Docker daemon and the image built as `todo-server`): no failed request, every
acknowledged write kept, about 1.5 s from start to finish for a small database. `bun scripts/takeover-e2e.ts` runs the
same checks without Docker (same host; separate ports behind a proxy that behaves like Traefik; another network
namespace with `unshare`).

## Backups

`todo-server backup [--out file] [--with-aux] [--with-files] [--include-secret]` takes a consistent
snapshot (`VACUUM INTO`) while the server runs, without blocking writes, into a `.tar.zst` archive (`data.db`, plus
the execution log, local file blobs and `auth_secret.key` when asked; the key only with `--include-secret`, since it
can forge sessions). Keep a copy of `auth_secret.key` somewhere safe either way: it also encrypts the secrets stored
in `_settings`, so a restore elsewhere without it signs everyone out and those settings must be entered again. `serve --backup-every 6h --backup-keep 7 [--backup-to s3://bucket/prefix|dir]` takes them on a
schedule (the cron `ferrix-backup`, visible on the Scheduler page) and deletes old scheduled ones. The same schedule
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
`serve --restart-exit-code 75` (set in `deploy/ferrix.service`; for Docker add a restart policy), the dashboard also
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
is taken when replication starts and every `--snapshot-every`; snapshots and segments older than `--replicate-keep`
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
and objects, an empty cell is `null` (or `""` for a non-optional string). Admins can stream one table with
`GET /api/admin/export/{table}?format=csv` and import into a running server with
`POST /api/admin/import/{table}?format=csv`.

## Metrics

`serve --metrics [--metrics-token T]` exposes Prometheus metrics at `/metrics`: requests by route and
status, function executions and durations, commit ts and rate, sockets, subscriptions, reruns, database file sizes,
scheduler queue depth, dropped log entries and, with `--replicate-to` and `--olap`, the replication and analytics
lag.
