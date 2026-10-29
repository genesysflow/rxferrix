# Functions and data

Define tables and functions in backend modules such as `src/tasks.rs`. Declare each module in `main.rs` with `mod tasks;`; the macros register compiled tables and functions automatically.

The examples on this page build on one task table. Start with `use rxferrix::prelude::*;` in the module.

## Define a table

```rust
use rxferrix::prelude::*;

#[table(name = "tasks")]
#[index(by_done = [done])]
pub struct Task {
    pub text: String,
    pub done: bool,
}
```

A stored document adds an ID and creation time to these fields. Rust uses `Doc<Task>` with `.id` and `.creation_time`; the client sees `_id` and `_creationTime`. Creation time is milliseconds since the Unix epoch.

The table macro provides serialization, TypeScript types, and typed field/index handles such as `Task::done` and `Task::by_done`. You do not need a separate schema file. See [Types](/guide/types) for custom result types and field naming, and [Migrations](/guide/migrations) before changing fields in a table that already contains data.

## Read with a query

```rust
#[query]
pub async fn list(ctx: &QueryCtx, done: Option<bool>) -> Result<Vec<Doc<Task>>> {
    let query = match done {
        Some(done) => ctx.db.query::<Task>()
            .with_index(Task::by_done, |q| q.eq(Task::done, done)),
        None => ctx.db.query::<Task>(),
    };
    query.order(Order::Desc).take(100).await
}
```

`done: None` lists all tasks; `Some(false)` uses the index to list open tasks. `Order::Desc` returns newer documents first within the selected range. Clients call `api.tasks.list` with `{ done: false }` or omit the optional argument.

A query reads one consistent snapshot. Subscriptions rerun when a write touches what the query read. Keep the result deterministic: use tracked context inputs, avoid external I/O, and sort collections whose iteration order may vary.

| Read | Result |
| --- | --- |
| `ctx.db.get(id).await?` | One document or `None`. |
| `query.take(100).await?` | Up to 100 matching documents. |
| `query.first().await?` | The first match or `None`. |
| `query.unique().await?` | One match or `None`; an error if there is more than one. |
| `query.count().await?` | The number of matches. |
| `query.paginate(opts).await?` | A live page with continuation information. |
| `query.collect().await?` | Every match; use it only when the range is bounded. |

## Write with a mutation

```rust
#[mutation]
pub async fn add(ctx: &MutationCtx, text: String) -> Result<Id<Task>> {
    let text = text.trim().to_string();
    ensure_user!(!text.is_empty(), "empty_text", "A task needs some text");
    ctx.db.insert(Task { text, done: false }).await
}

#[mutation]
pub async fn set_done(ctx: &MutationCtx, id: Id<Task>, done: bool) -> Result<()> {
    let task = ctx.db.get(id).await?
        .or_user("not_found", "This task no longer exists")?;
    if task.done != done {
        ctx.db.patch(id, json!({ "done": done })).await?;
    }
    Ok(())
}
```

All reads and writes in one mutation share a transaction. An error rolls them back. `insert` returns an ID, `patch` changes named fields, `replace` supplies a complete new value, and `delete` removes a document. Patches use JSON field names: `completed_at` in Rust becomes `"completedAt"` in a patch.

Validate arguments and permissions in the function. A typed ID identifies a table; it does not prove that the caller may read or change that document. See [authorization](/guide/auth#check-access-to-each-document).

## Choose indexes for your reads

An index with fields `[owner, done]` supports an equality prefix on `owner`, then an equality or range on `done`. Constraints follow declared field order. A post-read `.filter(...)` does not replace an index: it still scans the selected range.

Every ordinary index ends with `_creationTime` and `_id`. Equal declared fields therefore sort by creation time. After constraining all declared fields with `eq`, you can bound creation time with a handle such as `Task::_creation_time`.

Use `.count()` when you only need a count. Without a filter it counts the index range without deserializing the documents. For frequently displayed grouped counts or sums, use [aggregates](/guide/aggregates).

## Paginate a growing list

Use pagination when a list can grow beyond a small fixed result. The argument must be named `pagination_opts` in Rust so clients see `paginationOpts`:

```rust
#[query]
pub async fn page(ctx: &QueryCtx, pagination_opts: PaginationOpts)
    -> Result<PaginationResult<Doc<Task>>>
{
    ctx.db.query::<Task>()
        .order(Order::Desc)
        .paginate(pagination_opts)
        .await
}
```

On the client, use `usePaginatedQuery(api.tasks.page, {}, { initialNumItems: 20 })`. The hook manages cursors and keeps loaded pages subscribed. Do not interpret cursor strings or reuse them with another range or sort order.

To return enriched rows, transform the page with `map`, `map_async`, `try_map`, or `try_map_concurrent`; these preserve its pagination metadata. See the [pagination reference](/CONTRACT#_1-2-1-pagination) for boundary and scan-limit behavior.

## Use an action for external work

An action has no direct transaction. It calls queries and mutations through typed function references:

```rust
#[action]
pub async fn add_samples(ctx: &ActionCtx, count: u32) -> Result<u32> {
    ensure_user!(count <= 100, "too_many", "Add at most 100 samples at once");
    for i in 1..=count {
        ctx.run_mutation::<add>((format!("Sample task #{i}"),)).await?;
    }
    Ok(count)
}
```

Each `run_mutation` commits independently. If a later call fails, earlier inserts remain. Use `ctx.run_query::<f>(args)` and `ctx.run_action::<f>(args)` in the same way. Arguments are tuples, `()` for none, or the value itself for one parameter.

Queries and mutations share logic through ordinary Rust helper functions using their existing context. A direct Rust call does not create another transaction or add access checks.

For email or network calls that should follow a committed write, schedule an action from the mutation. See [Scheduler](/guide/scheduler).

## Return useful errors

A `UserError` reaches clients with its code, message, and optional data. Use it for expected failures:

```rust
ensure_user!(!text.trim().is_empty(), "empty_text", "A task needs some text");
let task = ctx.db.get(id).await?.or_user("not_found", "This task no longer exists")?;
```

`bail_user!("locked", "{} is locked", task.text)` returns a user error immediately. Other errors become `"Server Error"` in production; `--dev` exposes their details. Tests can inspect expected failures with `UserError::of(&error)`.

## Keep server helpers internal

Use `#[internal_query]`, `#[internal_mutation]`, or `#[internal_action]` for functions clients should not call directly. Actions, scheduled jobs, and trusted server code can still call them.

Use `#[action(dev_only)]` or the equivalent query/mutation attribute for seeds and dev inbox helpers. These run only in development mode and appear in generated `devApi` rather than `api`. `ctx.is_dev()` reports the runtime mode.
