# Deploys with no downtime

The usual way to deploy a new version is to stop the old server and start the new one. For a few seconds, requests
fail. With RxFerrix, the new version starts **next to** the running one and takes over from it. No request fails, no
write is lost, and the switch pauses requests for well under a second.

## How it works

1. You start the new version on the same data directory. The old one keeps serving.
2. The new version copies the database and opens the copy with the new code: it builds new indexes, checks every
   table against the new types and finds pending [migrations](/guide/migrations).
3. It catches up with what the old server wrote meanwhile, checking those documents too.
4. The old server stops accepting requests, lets the open ones finish, and asks WebSocket clients to reconnect.
5. The new version applies the last writes, swaps the copy in and starts serving.

Steps 4 and 5 take about 0.2–0.5 s under write load in our tests. During that time requests wait rather than fail.
If writes come in faster than the new version can replay them, step 3 stops after 30 s and the rest is replayed in
step 5, so the pause is longer but the takeover still ends.
The old server interrupts a backup or replication snapshot it is taking, and lets go of the data directory only once
every connection to the database is closed, so the swap never moves a file it still uses.
A mutation that a reconnecting client sends again still runs only once. The previous database is kept in
`<data dir>/takeover/previous/` until the next successful takeover replaces it (an attempt that gives up leaves it).

::: tip If the new version can't read your data
Say a field's type changed and existing documents don't fit, with no migration for it. The new version notices in
step 2, prints which documents don't fit, and exits. The old server keeps serving and never notices.

It also checks, before the old server hands over, that it can listen as configured (the TLS certificate files load,
the `--https` domains are valid) and that `--replicate-to` names a usable target. What runs only after the handover
isn't covered: an `on_start` hook that fails, or a storage backend that can't be reached, stops the new version with
the old one already gone. Test those before deploying.

To find out before deploying, run `my-app check --dir <data dir>`. It does step 2 on a copy while the server runs,
and is handy in CI.
:::

## On a server with systemd

[`deploy/rxferrix.service`](https://github.com/genesysflow/rxferrix/blob/master/deploy/rxferrix.service) is set up for it.
Install the new binary over the old one, then reload:

```sh
sudo install -m 0755 dist/my-app /usr/local/bin/my-app
sudo systemctl reload rxferrix          # runs `my-app serve … --takeover` next to the running server
journalctl -u rxferrix -f               # if the new version refused, this says why
```

Without systemd, start the new binary yourself with the same arguments as the running one plus `--takeover`. The
new process takes over the listening socket, so the address doesn't change.

## With Docker Compose

A container can't hand its process over, so the new version runs in a second container on the same volume. The
Docker image is set up for that.

1. Start the app with [`deploy/docker/compose.yaml`](https://github.com/genesysflow/rxferrix/blob/master/deploy/docker/compose.yaml)
   (the app behind Traefik):

   ```sh
   docker build -t my-app .
   IMAGE=my-app:latest docker compose -f deploy/docker/compose.yaml up -d
   ```

2. To deploy, build the new image and run
   [`deploy/docker/deploy.sh`](https://github.com/genesysflow/rxferrix/blob/master/deploy/docker/deploy.sh):

   ```sh
   docker build -t my-app .
   IMAGE=my-app:latest deploy/docker/deploy.sh
   ```

   It starts a second container, which takes over from the first. Once the new one is healthy, it removes the old
   one. If the new one exits instead, it removes the new one and the old one keeps serving.

## On Kubernetes

[`deploy/k8s/rxferrix.yaml`](https://github.com/genesysflow/rxferrix/blob/master/deploy/k8s/rxferrix.yaml) is written for
k3s (local-path volume, Traefik ingress). Set your image, host name and public URL in it, then:

```sh
kubectl apply -f deploy/k8s/rxferrix.yaml
kubectl set image deployment/rxferrix app=ghcr.io/you/my-app:v2    # deploy a new version
kubectl rollout status deployment/rxferrix
```

Kubernetes starts the new pod before stopping the old one, the new pod takes over, and then the old pod is stopped.
If the new version can't read the data, its pod keeps failing (CrashLoopBackOff) while the old pod serves; undo with
`kubectl rollout undo deployment/rxferrix`.

## Using your own setup

The files above get these details right. If you write your own, keep to them:

- **Same host, local disk.** Both containers (or pods) must run on the same machine and share the data directory on
  a local volume. SQLite needs a local disk, and the two versions talk over a socket file in the data directory.
- **Keep the old container alive.** Set `RXFERRIX_AFTER_HANDOVER=wait` (the image does). After handing over, the old
  container idles until it is stopped. If it exited, a restart policy would start it again, and it would take the
  data back.
- **Route to whoever accepts.** The old container stops listening at the switch. Your proxy must send each new
  connection to whichever container accepts it, and retry connections that are refused. With Traefik, add the
  `retry` middleware and point the service at:
  - the Compose service name (`http://app:8090`), not the container addresses, with Docker;
  - the Service's cluster IP (annotation `traefik.ingress.kubernetes.io/service.nativelb: "true"`) on Kubernetes.

  Pointing Traefik at the container or pod addresses doesn't work: it keeps retrying a request on the addresses it
  knew when the request arrived, which no longer include the new server.
- **No liveness probe** on Kubernetes. The old pod doesn't answer while it waits to be stopped, and a restart would
  make it take over again.
- **Use Traefik 3.6 or later** with Docker 29. Older releases can't talk to the Docker API.

## Settings

| Flag | Environment variable | Values |
|---|---|---|
| `--takeover` | `RXFERRIX_TAKEOVER` | `require` (the default for a bare `--takeover`): take over, fail if no server runs. `auto`: take over if a server runs, else start normally. The Docker image sets `auto`. |
| `--after-handover` | `RXFERRIX_AFTER_HANDOVER` | `exit` (default): exit after handing over. `wait`: stay alive, serving nothing, until stopped. The Docker image sets `wait`. |

`my-app health [url]` exits with 0 if the server's `/api/health` answers, and 1 otherwise. The image's
`HEALTHCHECK` uses it, since the image has no curl.

## How it's tested

Every case above runs under write load: `bun scripts/takeover-e2e.ts` (same host and port; two ports behind a
retrying proxy, as with containers; another network namespace), `bun scripts/docker-rollout-e2e.ts` (Docker Compose with Traefik) and
`bun scripts/k3s-rollout-e2e.ts` (a k3s cluster in Docker). Each checks that no request fails and every
acknowledged write is kept; the Docker and k3s tests also deploy a broken version. The protocol is specified in the
[API contract](../CONTRACT.md) (§4.3.1).
