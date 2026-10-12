# Logs and metrics

## Logs

`serve` writes JSON lines when stdout isn't a terminal (systemd, Docker): `timestamp`, `level`, `target`,
`fields` and the span context, one object per line, for Loki, Vector or `jq`. `--log-format pretty|compact|json` (or
`FERRIX_LOG_FORMAT`) chooses explicitly; `RUST_LOG` filters as usual.

## Metrics

`serve --metrics [--metrics-token T]` exposes Prometheus metrics at `/metrics`: requests by route and
status, function executions and durations, commit ts and rate, sockets, subscriptions, reruns, database file sizes,
scheduler queue depth, dropped log entries and, with `--replicate-to` and `--olap`, the replication and analytics
lag.
