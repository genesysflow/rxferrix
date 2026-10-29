# Update a running app

Use `serve --takeover` to prepare a new version while the old one continues serving. The new process builds and validates a copy of the database, catches up with writes, then replaces the old process.

This reduces interruption during an update. It still involves a short handover pause and WebSocket reconnects. The supplied proxy configurations retry connections during the switch; arbitrary deployment setups do not automatically get that behavior.

## Check the new version first

Run the new binary against the existing data directory:

```sh
./my-app-v2 check --dir /var/lib/my-app
```

`check` opens a temporary copy with the new code, prepares indexes, and validates schema compatibility. It can run while the old server is active and does not replace it.

If stored data needs transformation, write and test a [migration](/guide/migrations). A pending migration can make startup possible, but old documents still need an on-read transformation or compatible reads until the migration reaches them.

## Start a takeover

Use the same data directory and production configuration as the running server:

```sh
./my-app-v2 serve --dir /var/lib/my-app \
  --public-url https://app.example.com --takeover
```

Add `--migrate` if pending migrations should run after startup. Keep required auth, storage, TLS, and other deployment settings consistent.

The new process prepares a database copy and follows commits from the old process. At handover, the old process drains requests and asks WebSocket clients to reconnect; the new process applies the final changes and opens the prepared database.

The supplied tests typically measure a 0.2–0.5 second handover under their write load. That is a measurement, not a fixed bound. If replay cannot catch up within 30 seconds, more work moves into the handover and the pause grows.

The built-in client reconnects and restores subscriptions. Its supported mutation retry window still applies; see [Behavior and limits](/guide/things-to-know#reconnects-and-retries).

## Understand failure behavior

A schema/index preparation failure leaves the old server serving. Before handing over, the new process also checks listener configuration, TLS inputs, and replication target configuration.

Some resources are initialized only after the old process exits. A failing `on_start` hook or unavailable storage backend can stop the new process after the old one has already handed over. Test those dependencies independently.

The previous database is kept at `<data dir>/takeover/previous/` until a later successful takeover replaces it. This is recovery material, not an automatic application rollback: the new server may already have accepted writes. Keep backups and plan rollback around schema and write compatibility.

## Use systemd

The repository's [service example](https://github.com/genesysflow/rxferrix/blob/master/deploy/rxferrix.service) runs a second process on reload. Adapt its paths and binary name, install the new binary, then:

```sh
sudo systemctl reload rxferrix
journalctl -u rxferrix -f
```

Install new executables by replacing the file, as `install` does; do not overwrite bytes inside a running executable. A failed preparation is reported in the service log while the old process continues.

## Use Docker Compose

From the framework repository, the provided Compose setup runs the todo image behind Traefik:

```sh
docker build -t my-app .
IMAGE=my-app:latest docker compose -f deploy/docker/compose.yaml up -d
```

After building a new image:

```sh
IMAGE=my-app:latest deploy/docker/deploy.sh
```

The deploy script starts a second container on the same volume, waits for its health, and removes the old container after success. If preparation fails, it removes the replacement. Adapt the image build to your actual backend before using this for another app.

## Use Kubernetes

The [k3s example](https://github.com/genesysflow/rxferrix/blob/master/deploy/k8s/rxferrix.yaml) uses a local-path volume and Traefik. Set its image, host, and public URL, then:

```sh
kubectl apply -f deploy/k8s/rxferrix.yaml
kubectl set image deployment/rxferrix app=ghcr.io/you/my-app:v2
kubectl rollout status deployment/rxferrix
```

The replacement pod prepares before the old one is stopped. If preparation fails, undo the rollout while the old pod still serves. This setup keeps both pods on the same host and volume; it is not a general multi-node SQLite deployment.

## Requirements for a custom deployment

- Run both versions on the same machine with the same local data directory. They coordinate through a Unix socket in that directory.
- In containers, keep the old process alive after handover with `RXFERRIX_AFTER_HANDOVER=wait` until the supervisor removes it. Otherwise a restart policy can start it again and trigger another takeover.
- Route new connections to the process that accepts them and retry refused connections during the switch. The supplied Traefik setup routes through the Compose service name or Kubernetes Service cluster IP, rather than a stale per-container address list.
- Do not give the waiting old Kubernetes pod a liveness probe that restarts it after handover.
- With Docker 29, use Traefik 3.6 or later for Docker API compatibility.

## Takeover options

| Option | Meaning |
| --- | --- |
| `--takeover` or `--takeover=require` | Replace a running server; fail when none exists. |
| `--takeover=auto` | Replace a running server, otherwise start normally. |
| `--after-handover=exit` | Old process exits after handover; the default. |
| `--after-handover=wait` | Old process waits without serving until stopped. |

Environment equivalents are `RXFERRIX_TAKEOVER` and `RXFERRIX_AFTER_HANDOVER`. The repository image defaults to `auto` and `wait`.

`my-app health [url]` checks the health endpoint and exits with 0 on success. The minimal Docker image uses it instead of curl.

Framework takeover, Docker, and k3s load tests are described in [Contributing](https://github.com/genesysflow/rxferrix/blob/master/CONTRIBUTING.md#development). The [takeover reference](/CONTRACT#_4-3-1-takeover-serve-takeover-crate-ops-takeover) details the preparation and drain protocol.
