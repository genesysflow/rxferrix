# Use the admin dashboard

Every default-feature server embeds a dashboard at `/_/`, such as http://127.0.0.1:8090/_/. It manages the running app using administrator credentials.

## Create the first administrator

On first startup, open the setup link printed in the backend terminal. It contains a one-time token for creating the first administrator.

For offline setup, use the app's command:

```sh
cargo run -- admin create you@example.com
```

Use `--dir` if the server's data directory is not the default `fx_data`. Dashboard administrators are separate from your app users. Enable two-factor authentication for an administrator on **Account**; if its authenticator and recovery codes are lost, use `admin disable-2fa EMAIL` for recovery.

## Inspect and change app data

**Data** lists app and system tables. Select a table to browse its live documents, filter by indexed fields, inspect its schema, or open a document. Tables with text-search indexes also offer search.

The JSON editor can insert, patch, or delete app documents. Writes are validated against the table's Rust type. Change the schema in Rust and deploy it; the dashboard does not define new schema fields or indexes.

System tables are generally read-only. Selected user profile fields are editable, while credentials and token hashes are hidden. Treat all administrator edits as privileged operations; they are recorded in the audit log.

## Debug a function

**Functions** lists registered functions and signatures. Select one, supply JSON arguments, and run it. Use `{}` for a function with no arguments and camelCase argument keys.

You can run as a user through a short-lived, audited impersonation session. The admin runner is trusted server tooling and bypasses app-wide guards; use client calls or `TestApp::as_user` to test the guard itself.

**Logs** shows live executions, errors, arguments, and captured function logs. Expand an entry for details, or filter by function, kind, or outcome. **Hide background jobs** excludes scheduler/workflow activity across overview charts, function activity, and logs.

## Find the right page

| Page | Use it to |
| --- | --- |
| Overview | Inspect connections, commits, reruns, busy/failing functions, cache reuse, database sizes, and service state. |
| Users | Inspect app accounts and sessions, revoke access sessions, reset a lost second factor, delete users, or open an audited session as a user. |
| Files | Upload, preview, download, or delete stored files, including with private storage. |
| Scheduler | Inspect crons, run a cron now, cancel jobs, retry failures, and browse finished-job history. |
| Workflows | Inspect workflow state and recorded steps, or cancel a run. |
| Migrations | Check schema state, preview changes, run/cancel/restart migrations, and inspect failures. |
| Backups | Create/download backups, configure schedules and destinations, and stage restores. |
| Analytics | Query the optional DuckDB copy and export results as CSV or Parquet. |
| Environment | Add or update reactive app variables read through `ctx.env`. |
| Settings | Configure auth providers, SMTP, redirects, sign-ups, email templates, CORS, trusted proxy headers, and request rate limits. |
| Audit log | Review administrator edits, settings changes, impersonation, function runs, and recovery operations. |
| Account | Manage the administrator's own second factor and recovery codes. |

Settings fixed by code, flags, or process environment appear as read-only. Other settings apply without restarting. Secrets can be replaced without being shown again.

A dashboard restore is validated and staged for the next startup. It does not replace the database under the active server. See [Backups and restores](/operations/backups#restore-from-the-dashboard) before using it.

## Navigate and customize the build

Ctrl+K (⌘K) opens the command palette. Alt+1 through Alt+9 and Alt+0 open the first ten sidebar pages.

The dashboard uses administrator-only system functions over the same sync protocol as app clients, plus admin HTTP endpoints for operations. See the [system API reference](/CONTRACT#_4-built-in-system-tables-and-functions) when building admin integrations.

The built assets ship with the Rust crate, so normal app builds do not need a JavaScript toolchain for the dashboard. Set `default-features = false` on the `rxferrix` dependency to omit it. Framework contributors should follow the [dashboard build instructions](https://github.com/genesysflow/rxferrix/blob/master/CONTRIBUTING.md#development).
