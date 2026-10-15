# Deploying

A RxFerrix app is one binary plus one data directory. Deploying it takes three steps:

1. **Build** a release binary with `scripts/release.sh`, or a Docker image with the `Dockerfile` (below).
2. **Run** it on a server: `my-app serve --dir /var/lib/my-app`. It listens on `127.0.0.1:8090` (`--http` changes
   that).
3. **Expose** it: behind a reverse proxy (pass `--public-url https://…`), or let it get its own certificates with
   `--https` (see [TLS](#tls)).

After that, [deploy new versions without downtime](/operations/zero-downtime) and
[set up backups](/operations/backups).

The data directory (`--dir`, default `fx_data`) holds everything the server keeps:

| Path | What |
|---|---|
| `data.db` | The database. |
| `aux.db` | The execution log (what the dashboard's logs and charts show). |
| `storage/` | Uploaded files, unless they go to S3. |
| `auth_secret.key` | The key that signs sessions and encrypts secrets. Keep a copy somewhere safe. |
| `backups/` | Backups, unless they go elsewhere. |

## Build with the right SQLite flags

Build your app with these SQLite flags. Apps created with `rxferrix new` have them in `.cargo/config.toml` (so does
this repository); for an app created otherwise, put this in its `.cargo/config.toml`. Without it, rusqlite's bundled
SQLite shares one page cache behind a global mutex, so concurrent queries don't run in parallel (measured: 8 parallel
readers ran no faster than one; with the flags they scale to ~4x the single-reader throughput and beyond). RxFerrix logs
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
#[derive(rxferrix::rust_embed::Embed)]
#[folder = "../web/dist/"]                        // relative to the crate
#[crate_path = "rxferrix::rust_embed"]
struct Assets;

rxferrix::RxFerrix::new()
    .embed_static::<Assets>()
    .routes(axum::Router::new().route("/webhooks/stripe", post(stripe)))   // your own routes, if any
    .run()
```

Routes that need the app (a webhook that records a payment, an endpoint that signs a device in) go through
`.app_routes`; their handlers take `State<App>` and call functions by their typed reference, internal ones included:

```rust
async fn stripe(State(app): State<App>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let Ok(event) = verify_signature(&headers, &body) else { return StatusCode::BAD_REQUEST };
    match app.mutation::<payments::record_event>((event,)).await {
        Ok(()) => StatusCode::OK,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,   // the provider retries
    }
}

rxferrix::RxFerrix::new().app_routes(axum::Router::new().route("/webhooks/stripe", post(stripe))).run()
```

Unknown paths get `index.html` (client-side routing) except under `/api` and `/_`, which static files never shadow,
and a missing file with an extension (`/logo.png`) is a 404 unless the client asks for HTML.
HTML is `no-cache`, hashed assets (`assets/*`, `name-<hash>.js`) are cached for a year, and `<file>.br` / `.gz`
next to a file are served to clients that accept them. The todo example embeds `examples/todo/web/dist` when it
exists.

## Release binary

`scripts/release.sh` builds the web app and a static `x86_64-unknown-linux-musl` binary of
`todo-server` into `dist/` (everything linked in: SQLite, FTS5, sqlite-vec, TLS, zstd; about 25 MB). C code is
cross-compiled with `zig cc` through cargo-zigbuild, fetched with mise if not installed. For your app:
`PACKAGE=my-server WEB=web scripts/release.sh`; `FEATURES=olap` adds the analytics copy (DuckDB; about 57 MB).

## systemd

[`deploy/rxferrix.service`](https://github.com/genesysflow/rxferrix/blob/master/deploy/rxferrix.service) runs the binary as a dynamic unprivileged user with
its data in `/var/lib/rxferrix`, behind a reverse proxy or with `--https`; it stops gracefully on SIGTERM, and restarts
on the dashboard's "Restart now" (exit code 75, `RXFERRIX_RESTART_EXIT_CODE`).

## Docker

The [`Dockerfile`](https://github.com/genesysflow/rxferrix/blob/master/Dockerfile) runs `scripts/release.sh` and copies the binary into a `FROM scratch`
image with CA certificates, running as an unprivileged user with the data in the `/data` volume. To build the image
locally you need a Docker daemon you can use (on Linux: membership in the `docker` group, or rootless Docker/Podman;
`podman build` works with the same file) and network access for the base images, Zig and the crates; the first build
takes several minutes (later ones reuse the BuildKit caches). The build context includes `.cargo/config.toml`, which
sets the bundled SQLite's flags: don't pass `LIBSQLITE3_FLAGS` to the build.

```sh
docker build -t todo-server .                     # --build-arg PACKAGE=my-server --build-arg WEB=web for yours
docker run -p 8090:8090 -v todo-data:/data todo-server
docker run -p 8090:8090 -v todo-data:/data -e RXFERRIX_RESTART_EXIT_CODE=75 --restart unless-stopped todo-server
                                                  # lets "Restart now" apply a restore staged in the dashboard
docker run --rm todo-server functions             # any CLI command
```

## TLS

Either terminate TLS in a reverse proxy (pass `--public-url https://…`), or let the server do it:
`serve --https example.com --acme-email you@example.com` gets and renews Let's Encrypt certificates (TLS-ALPN-01 on
port 443, cached in `<dir>/acme`; try `--acme-staging` first) and redirects port 80 to HTTPS. `--tls-cert cert.pem
--tls-key key.pem` uses your own certificate, re-read every 30 s (`--tls-reload-every`): renewed files are picked up
without a restart or dropped connections, and a bad pair keeps the current certificate (logged). Addresses: `--https-addr` (default `0.0.0.0:443`), `--http-redirect`
(default `0.0.0.0:80` with `--https`, none with `--tls-cert`; `off` turns it off).

## Durability

By default SQLite runs in WAL mode with `synchronous = NORMAL`: a crash of the process loses nothing,
but a power loss or OS crash may lose the last commits. `--fsync-commits` (`AppOptions::fsync_commits`) makes every
commit wait for the disk; group commit shares one fsync across the mutations waiting at that moment, so it costs most
when writes come one at a time. WAL checkpoints run on a background thread, not inside commits. Measured on an NVMe
SSD (btrfs, encrypted), one-insert mutations: 8.9k/s with 1 writer, 16.2k/s with 16 and 18.7k/s with 64 by default
(p99 0.2 / 3.5 / 11 ms); 1.1k/s, 7.3k/s and 12.5k/s with `--fsync-commits`.

All writes go through one SQLite connection, whose page cache `--writer-cache-size` sets (default 32 MB, taken as it
is used). Writes that change many index entries at once gain most from it: 32 MB made inserts into a table with 32
indexes about 50% faster than SQLite's default 2 MB. A larger cache measured no faster.

## CI

[`.github/workflows/release.yml`](https://github.com/genesysflow/rxferrix/blob/master/.github/workflows/release.yml) runs every gate of [Tests](https://github.com/genesysflow/rxferrix/blob/master/README.md#development) (end-to-end
tests included, in Chromium) and, in a job of its own, the tests with the `olap` feature; builds the static musl
binary with `scripts/release.sh` (an artifact; attached to the GitHub release for `v*` tags) and, for tags and manual
runs, `todo-server-olap` too; and builds the Docker image alongside, smoke-tests it (`functions`, `/api/health` with a
volume) and pushes it to `ghcr.io/<owner>/todo-server` for `v*` tags. A new push cancels the run it supersedes;
caches are kept even when a run fails, so DuckDB is compiled only when it changes.
