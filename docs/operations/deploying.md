# Deploy an app

A RxFerrix deployment runs your app's compiled binary with a persistent data directory on local disk. The binary can also serve the frontend. Start here for the first deployment; use [Updating a running app](/operations/zero-downtime) for later releases.

## Build your app

For an app created with `rxferrix new`, run from its project root:

```sh
cargo run -- codegen --out web/src/rxferrix
cd web
bun install
bun run build
cd ..
touch src/main.rs
cargo build --release
```

The generated backend embeds `web/dist` when it exists at compile time. Touching the backend entry point ensures Cargo recompiles it after the frontend changes. For a backend-only app, just run `cargo build --release`.

Deploy the resulting `target/release/my-app` to a machine compatible with its build target. A normal release build uses the platform's usual linking; it is not automatically a portable static Linux executable.

The framework repository's `scripts/release.sh` builds its example app as a static musl binary using Zig/cargo-zigbuild. Its default is `todo-server`; the repository Docker image also runs that example. Adapt these files to your own app rather than treating the example image as your backend.

<a id="build-with-the-right-sqlite-flags"></a>

## Preserve the SQLite build configuration

Generated projects include this `.cargo/config.toml` setting:

```toml
[env]
LIBSQLITE3_FLAGS = "-USQLITE_ENABLE_MEMORY_MANAGEMENT -DSQLITE_DEFAULT_MEMSTATUS=0"
```

Keep it in your app. It removes SQLite build-time bottlenecks that otherwise serialize concurrent reads. A dependency cannot set this Cargo environment option on behalf of the consuming app. The server warns when the relevant flag is missing.

## Start with a persistent data directory

```sh
./my-app serve --dir /var/lib/my-app --public-url https://app.example.com
```

The default listener is `127.0.0.1:8090`. Use `--http` to change it. Configure a process supervisor and make the data directory writable by the service user.

| Data path | Contents |
| --- | --- |
| `data.db` | Documents, indexes, auth data, jobs, settings, and migration state. |
| `aux.db` | Execution logs and finished-job history. |
| `storage/` | Local uploaded files, unless using an external object store. |
| `auth_secret.key` | Session-signing and secret-encryption key. Preserve it independently. |
| `backups/` | Default backup destination. |
| `olap.duckdb` | Optional derived analytics copy; rebuilt when needed. |

`fx_data` is the default relative data directory. Use an explicit production path so commands started from different working directories refer to the same app state.

Create the first dashboard administrator using the setup link in the server log. Configure [auth](/guide/auth), public frontend return URLs, and SMTP for the methods your app uses. Configure [backups](/operations/backups) before relying on the deployment.

## Serve the frontend

Generated apps already include an embedded frontend. For an existing project, choose runtime files with `.static_dir("./web/dist")` or compiled assets with `.embed_static::<Assets>()`:

```rust
#[derive(rxferrix::rust_embed::Embed)]
#[folder = "web/dist/"] // Relative to this Cargo package's directory.
#[crate_path = "rxferrix::rust_embed"]
struct Assets;

fn main() -> rxferrix::Result<()> {
    rxferrix::RxFerrix::new().embed_static::<Assets>().run()
}
```

Adjust the folder when the backend Cargo package is in a subdirectory; the repository todo example uses `../web/dist/`.

Unknown navigation paths can fall back to `index.html` for client routing. Static assets never shadow `/api` or `/_`. Missing assets such as `/logo.png` return 404 unless HTML was requested. HTML is served with revalidation; hashed assets get long-lived caching. Adjacent `.br` and `.gz` variants can be served when accepted by the client.

## Put HTTPS in front of the server

With a reverse proxy, forward HTTP and WebSocket traffic and set `--public-url` to the external backend URL. The public URL is used for links and OAuth callbacks; it does not configure a proxy or listener for you.

Alternatively, let the app obtain Let's Encrypt certificates:

```sh
./my-app serve --dir /var/lib/my-app \
  --https app.example.com --acme-email you@example.com
```

This uses TLS-ALPN-01 on port 443 and redirects port 80 to HTTPS. The domain must resolve to the server and those ports must be reachable. Certificates are cached under the data directory's `acme/`. Use `--acme-staging` when testing issuance.

For your own certificate, use `--tls-cert cert.pem --tls-key key.pem --public-url https://app.example.com`. The pair is reloaded every 30 seconds by default; an invalid replacement leaves the current pair active. `--https-addr` changes the HTTPS listener, and `--http-redirect` configures or disables the redirect listener.

HTTPS enables one-year HSTS by default. `--no-hsts` disables it for setups such as untrusted test certificates. See `serve --help` for TLS options.

## Configure client IPs and request limits

Behind a proxy, configure **Settings → IP proxy headers**, or use `--trust-proxy` for the last `X-Forwarded-For` entry. Restrict trusted source addresses with `--trust-proxy-from IP_OR_CIDR`, or ensure the server can only be reached through a proxy that overwrites the header.

The dashboard shows the resolved client IP. Check it before setting per-IP limits; otherwise all users may share the proxy's limit.

**Settings → Rate limiting** supports rules by path prefix or function pattern, optionally for guests or signed-in users. Exceeding a rule produces `rate_limited` with retry information. Admin/system traffic and health/metrics endpoints are excluded; auth endpoints also have built-in limits. The [settings reference](/CONTRACT#_4-1-4-settings-settings-system-settings) describes exact exemptions.

## Use systemd or containers

The repository supplies deployment examples:

| Setup | Starting point |
| --- | --- |
| systemd | [`deploy/rxferrix.service`](https://github.com/genesysflow/rxferrix/blob/master/deploy/rxferrix.service) |
| Docker build | [`Dockerfile`](https://github.com/genesysflow/rxferrix/blob/master/Dockerfile) |
| Docker Compose with Traefik | [`deploy/docker`](https://github.com/genesysflow/rxferrix/tree/master/deploy/docker) |
| Kubernetes on k3s | [`deploy/k8s`](https://github.com/genesysflow/rxferrix/tree/master/deploy/k8s) |

Adapt binary names, images, paths, and hostnames to your app. Keep the local data volume persistent. Use one active writer per directory; multiple replicas are not horizontal database scaling.

To try the repository's todo image from the framework root:

```sh
docker build -t todo-server .
docker run -p 8090:8090 -v todo-data:/data todo-server
```

For dashboard-triggered restarts, configure `--restart-exit-code 75` (or `RXFERRIX_RESTART_EXIT_CODE`) and a supervisor/restart policy that starts the process again. Without a supervisor, the exit code alone does not restart anything.

## Choose durability and capacity settings

SQLite uses WAL with `synchronous = NORMAL` by default. A process crash preserves committed writes, but power loss or an OS crash may lose recent commits. `--fsync-commits` waits for disk durability before acknowledging writes; group commit shares the cost across queued mutations.

Writes use one connection; `--writer-cache-size` defaults to 32 MB. Read concurrency defaults to about 1.5 connections per available core and can be changed with `--readers`. Queries averaging at least 5 ms share a limited slow lane so they cannot consume all read slots. Measure your workload before changing these settings.

Enable [logs and metrics](/operations/monitoring), then use [takeover](/operations/zero-downtime) for compatible updates and [migrations](/guide/migrations) for stored data changes.

## Add HTTP routes or startup services

Use `.app_routes(axum::Router<App>)` for handlers needing the running `App`; extract it with `State<App>` and call typed functions such as `app.mutation::<payments::record_event>(args)`. Use `.routes(...)` for routes without app state. Validate webhook signatures and other endpoint access in your handler; these trusted runtime calls bypass app-wide client guards.

Use `.on_start(|app| async move { ... })` to initialize a service before accepting requests. Start long-lived work through `app.spawn` and stop it when `app.closed()` resolves. Shutdown waits up to ten seconds for this work before releasing the database.

Startup hooks run after the old process has handed over during takeover. A failing hook can therefore leave the replacement unavailable. Test its resources before deploying; see the [builder/runtime reference](/CONTRACT#_1-4-logging-the-execution-log-and-runtime-options) for lifecycle details.
