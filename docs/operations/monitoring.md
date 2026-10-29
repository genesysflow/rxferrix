# Logs and metrics

Use process logs to diagnose startup and infrastructure problems, execution logs to inspect backend calls, and metrics to track the server over time.

## Read process logs

When stdout is not a terminal, `serve` emits JSON lines containing timestamp, level, target, fields, and span context. This suits systemd, Docker, and log collectors.

Choose a format explicitly with `--log-format pretty`, `compact`, or `json`, or `RXFERRIX_LOG_FORMAT`. Use `RUST_LOG` to filter targets and levels.

## Add logs inside functions

```rust
use rxferrix::log;

// Inside a function:
log::info!(task_id = %id, "completed a task");
```

Function logs are captured with the execution and shown in **Dashboard → Logs** and function activity. The execution log lives in `aux.db`. In `--dev`, responses also include function log lines; production clients do not receive those lines.

Log enough context to diagnose a call without including credentials or sensitive payloads. Expected client failures should use `UserError`; unexpected failures retain details in server tooling while the production client receives `Server Error`.

## Enable Prometheus metrics

```sh
./my-app serve --dir /var/lib/my-app --metrics
```

This exposes `/metrics`. Set `--metrics-token` or `RXFERRIX_METRICS_TOKEN` to require a bearer token when scraping.

Metrics cover requests and status codes, function execution times, commits, connections, subscriptions, reruns, cache activity, database sizes, scheduler depth, and dropped log entries. Replication and analytics lag appear when those services are enabled.

Watch sustained errors, slow mutations holding the writer, growing scheduler queues, disk growth, and replication lag. The dashboard's **Overview** gives an interactive view of several of the same signals.

## Check health

`./my-app health` checks `http://127.0.0.1:8090/api/health` by default. Pass another URL when the listener differs. The command exits 0 for success and 1 for failure, making it usable in minimal containers without curl.

During a container takeover, the old process may deliberately stay alive without serving. Follow the [deployment probe guidance](/operations/zero-downtime#requirements-for-a-custom-deployment) so a liveness restart does not make it take the data back.
