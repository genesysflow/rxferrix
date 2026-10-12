# Environment variables

API keys, URLs and switches that functions read at run time, set on the dashboard's **Environment** page (or with
`<server> env set NAME` before the first start) instead of in the process environment:

```rust
#[query]
pub async fn banner(ctx: &QueryCtx) -> Result<Option<String>> {
    ctx.env.get("ANNOUNCEMENT").await          // ctx.env.require("NAME") fails with a clear error when unset
}
```

They are reactive: a query that reads a variable depends on it like on a document, so changing, adding or removing it
on the dashboard reruns those queries and every client sees the new result at once (no restart, no redeploy).
Mutations read them in their transaction and actions read the latest value. Values are encrypted at rest with the
server key (`auth_secret.key`), masked in the data browser and the execution log, and changes are audited. Names are
letters, digits and `_`, not starting with a digit (up to 64), values up to 8 KB, at most 200 variables. From the command line (offline):
`env list [--values]`, `env set NAME [VALUE]` (the value is read from stdin when omitted), `env unset NAME`.
`std::env::var` still works, but it is fixed at startup and invisible to the dependency tracking.
