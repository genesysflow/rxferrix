#!/bin/sh
# Roll out a new image of the `app` service with no downtime (see compose.yaml):
#
#   IMAGE=todo-server:latest deploy/docker/deploy.sh      # the image tag compose.yaml uses (rebuilt or pulled)
#
# 1. Starts a second `app` container next to the running one (same volume). It takes over (RXFERRIX_TAKEOVER=auto).
# 2. Waits until it is healthy, i.e. serving. If it exits or restarts while the old one still serves, this version
#    can't take over: it is removed and the running container carries on (its logs say why).
# 3. Stops and removes the old container (which handed over and was waiting, serving nothing).
set -eu

cd "$(dirname "$0")"
SERVICE=${SERVICE:-app}
TIMEOUT=${TIMEOUT:-1800} # seconds: a big database takes a while to prepare
compose() { docker compose -f compose.yaml "$@"; }

old=$(compose ps -q "$SERVICE")
if [ -z "$old" ]; then
    echo "nothing running yet: starting $SERVICE"
    compose up -d "$SERVICE"
    exit 0
fi

echo "starting a new $SERVICE container next to $(echo "$old" | cut -c1-12)"
compose up -d --no-deps --no-recreate --scale "$SERVICE=2" "$SERVICE"
new=$(compose ps -q "$SERVICE" | grep -v "$old" | head -n 1)
[ -n "$new" ] || { echo "no new container was started" >&2; exit 1; }

elapsed=0
while :; do
    health=$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{end}}' "$new")
    running=$(docker inspect -f '{{.State.Running}}' "$new")
    restarts=$(docker inspect -f '{{.RestartCount}}' "$new")
    if [ "$health" = healthy ]; then
        break
    fi
    # It exited or restarted: unless the old one has handed over already (it then refuses connections; the new one
    # is restarted by the restart policy and serves), this version can't take over.
    if { [ "$running" != true ] || [ "$restarts" -gt 0 ]; } && docker exec "$old" /app health >/dev/null 2>&1; then
        echo "the new container can't take over; the running one carries on. Its last log lines:" >&2
        docker logs --tail 40 "$new" >&2 || true
        docker rm -f "$new" >/dev/null
        exit 1
    fi
    if [ "$elapsed" -ge "$TIMEOUT" ]; then
        echo "the new container isn't healthy after ${TIMEOUT}s; leaving both running for you to look at" >&2
        exit 1
    fi
    sleep 1
    elapsed=$((elapsed + 1))
done

echo "the new container serves; removing the old one"
docker stop "$old" >/dev/null
docker rm "$old" >/dev/null
echo "deployed: $(docker inspect -f '{{.Config.Image}}' "$new") in $(echo "$new" | cut -c1-12)"
