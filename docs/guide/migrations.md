# Migrations

At startup, a table whose Rust type changed has its documents validated; if some don't match, the server refuses to
start, listing them. Make the change backward compatible (`Option<T>`, `#[serde(default)]`), or write a migration.
Migrations get the stored JSON, so they work however far the type has moved on:

```rust
#[table(name = "todos")]
pub struct Todo { pub text: String, pub done: bool, pub priority: i64 }   // `priority` is new

#[migration(table = Todo, batch = 100)]             // optionally `name = "0002_priority"`: a stable id
async fn add_priority(ctx: &MutationCtx, doc: serde_json::Value) -> Result<Option<serde_json::Value>> {
    Ok(doc.get("priority").is_none().then(|| json!({ "priority": 0 })))   // a patch, or None to keep it
}

#[migration(table = Todo, name = "0003_rank", after = ["todos:add_priority"])]   // runs once that one is done
async fn rank(ctx: &MutationCtx, doc: serde_json::Value) -> Result<Option<serde_json::Value>> { /* … */ }
```

Return `DocChange::{Keep, Patch, Replace, Delete}` for more, or take a typed `Doc<OldTodo>` when the old shape still
deserializes. A migration runs in batches, each one transaction that also advances its cursor in the `_migrations`
table, so every document is processed exactly once per run, a crash or a failure resumes where it stopped, and
subscriptions update as documents change.

```sh
todo-server migrate --dry-run          # what would change (with examples); writes nothing
todo-server migrate                    # run every migration that isn't done: dependencies first, then name order
todo-server migrate status             # state and progress of each
todo-server serve --migrate            # or: start, then run them in the background
```

While a migration for a table is pending, the server starts even though the table's documents don't match yet: the
table is *migrating*, writes may leave a document invalid only if it already was, and typed reads of not-yet-migrated
documents fail, unless the migration is an **on-read** one: a plain function of the stored document, which typed
reads also apply in memory while it is pending, so queries see every document in the new shape from the first moment:

```rust
#[table(name = "notes")]
pub struct Note { pub title: String, pub tags: Vec<String> }    // was `tag: String`

#[migration(table = Note, name = "0004_tags", on_read)]            // no ctx, not async
fn tags_from_tag(doc: &serde_json::Value) -> Result<Option<serde_json::Value>> {
    Ok(doc.get("tag").filter(|_| doc.get("tags").is_none()).map(|tag| json!({ "tags": [tag] })))
}
```

**A schema change with no downtime**, then: change the type, write the migration (`on_read` if it doesn't need the
database), check it (`todo-server check` and `migrate --dry-run` on a copy, or in CI), deploy with `serve --takeover
--migrate` (the old version keeps serving until the new one is ready, see [OPERATIONS.md](../operations/zero-downtime.md)), and the migration runs in the
background while reads already see the new shape. Index ranges and search filters on a migrated field see the old
stored values until the migration has reached those documents. The last migration of a table checks every document
it processes, so finishing it needs no scan of the table and holds up no writes; one it leaves invalid fails the run,
naming the document, and a table that still doesn't match refuses the next start. `after = [..]` dependencies are checked at startup (unknown names and cycles
are errors); running one migration before its dependencies are done is refused unless forced (`migrate --name x
--force`). The dashboard's Migrations page shows their dependencies and live progress and runs, cancels,
restarts and dry-runs them (`_system/migrations:{list, status, run, cancel}`). Details:
[docs/CONTRACT.md](../CONTRACT.md) §1.9. The todo example has one (`0001_trim_todo_text`).
