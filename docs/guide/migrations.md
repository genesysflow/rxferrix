# Change stored data

A Rust table type describes the documents the app expects to read. On startup, RxFerrix validates existing data when that type changes. If documents no longer match and no migration covers the table, startup fails with the offending documents.

For a compatible change, use an optional field or a suitable `#[serde(default)]`. For existing values that need transformation, declare a migration in a compiled backend module.

## Add a missing field

Suppose `Task` gains a required `priority: i64` field. A migration can read raw stored JSON, including documents the new type cannot deserialize yet:

```rust
use rxferrix::prelude::*;

#[table(name = "tasks")]
pub struct Task {
    pub text: String,
    pub done: bool,
    pub priority: i64,
}

#[migration(table = Task, name = "0002_priority", batch = 100)]
pub async fn add_priority(_ctx: &MutationCtx, doc: serde_json::Value)
    -> Result<Option<serde_json::Value>>
{
    Ok(doc.get("priority").is_none().then(|| json!({ "priority": 0 })))
}
```

Add `serde_json` to your app's dependencies when using its types directly. The returned object is a patch; `None` keeps the document. For other changes, return `DocChange::Keep`, `Patch`, `Replace`, or `Delete`.

A typed `Doc<OldType>` is also supported when that type can deserialize the stored shape. Explicit migration names remain stable across function or module renames. Use `after = ["0002_priority"]` on a later migration to declare a dependency on this one.

## Check and run a migration

Use a copy of production data to inspect the result before deploying. From the app project:

```sh
cargo run -- check --dir ./data-copy
cargo run -- migrate --dir ./data-copy --dry-run
cargo run -- migrate --dir ./data-copy
cargo run -- migrate status --dir ./data-copy
```

`check` opens a temporary copy as a takeover would. A dry run reports changes without persisting them. Offline `migrate` and `migrate status` refuse to use a directory held by a server. On a running server, use **Dashboard → Migrations**, or start the server with `serve --migrate` to run pending work in the background.

Migrations run in dependency order, then name order. Each batch commits its writes and progress cursor together. After a failure or restart, processing resumes from the recorded cursor. Inspect the failure before restarting or forcing a migration.

## Keep reads working while data changes

A pending migration lets the server open a table whose stored documents do not all match yet. Ordinary typed reads can still fail on those old documents.

For a transformation that depends only on the document, use an on-read migration:

```rust
#[table(name = "notes")]
pub struct Note {
    pub title: String,
    pub tags: Vec<String>,
}

#[migration(table = Note, name = "0004_tags", on_read)]
fn tags_from_tag(doc: &serde_json::Value) -> Result<Option<serde_json::Value>> {
    Ok(doc.get("tag")
        .filter(|_| doc.get("tags").is_none())
        .map(|tag| json!({ "tags": [tag] })))
}
```

This synchronous function has no database context. Typed reads apply the pending transformation in memory, so they can see the new shape while the background migration persists it.

**Indexes and search filters still use stored values until each document is migrated.** On-read conversion does not make an index on a new field complete immediately. Plan the rollout so queries remain correct during that interval.

## Deploy a schema change

After checking a data copy, start the new binary with the same data directory and `serve --takeover --migrate`. The new process prepares and checks a copy while the old server serves; migration work continues after takeover. See [Updating a running app](/operations/zero-downtime).

The final pending migration for a table validates the documents it processes. A document left invalid fails the run and identifies the document. The dashboard shows dependencies, progress, dry-run samples, cancellation, and restart controls.

Use `migrate --name NAME --restart` to reprocess from the beginning, or `--force` to bypass unfinished dependencies only when you intend that behavior. Unknown dependency names and cycles fail validation. The [migration reference](/CONTRACT#_1-9-migrations) describes the state and write rules; the todo example includes `0001_trim_todo_text`.
