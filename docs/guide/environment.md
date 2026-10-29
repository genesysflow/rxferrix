# App environment variables

Use `ctx.env` for app settings that should change while the server runs: API keys, an announcement, or a feature switch. These values are stored in the database and managed through **Dashboard → Environment**.

They are separate from the process environment used by server flags, SMTP configuration, and cloud credentials.

## Read a value

```rust
use rxferrix::prelude::*;

#[query]
pub async fn banner(ctx: &QueryCtx) -> Result<Option<String>> {
    ctx.env.get("ANNOUNCEMENT").await
}
```

`get` returns `None` when unset. `require("NAME")` returns an error when the value is missing.

A query tracks the variables it reads. Changing, adding, or removing one invalidates dependent queries and updates their subscribers. Mutations read in their transaction; actions read the latest value.

Returning a value from a public function exposes it to the caller. Keep API keys inside authorized server logic rather than returning them to the UI.

## Set values

Use the dashboard on a running server. Before startup, or with the server stopped, run the app's offline commands:

```sh
cargo run -- env set ANNOUNCEMENT "Maintenance on Sunday"
cargo run -- env list
cargo run -- env unset ANNOUNCEMENT
```

Pass `--dir` to each command when the data lives outside `fx_data`. Omitting the value from `env set NAME` reads it from stdin. `env list --values` prints values explicitly.

Names start with a letter or `_`, contain letters, digits, or `_`, and are at most 64 characters. Values are at most 8 KB, with at most 200 variables.

## Storage and process configuration

Values are encrypted at rest with the server key, masked in normal data/log views, and changes are audited. Preserve `auth_secret.key` when moving or restoring the app.

`std::env::var` still reads the process environment, but it does not participate in dependency tracking or dashboard updates. Use it for deployment configuration; use `ctx.env` for app values you want to update at runtime.
