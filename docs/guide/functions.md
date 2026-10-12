# Functions, tables and indexes

```rust
use ferrix::prelude::*;

#[table(name = "todos")]
#[index(by_done = [done])]
pub struct Todo { pub text: String, pub done: bool }

#[query]
pub async fn list(ctx: &QueryCtx, done: Option<bool>) -> Result<Vec<Doc<Todo>>> {
    let q = match done {
        Some(d) => ctx.db.query::<Todo>().with_index(Todo::by_done, |q| q.eq(Todo::done, d)),
        None => ctx.db.query::<Todo>(),
    };
    q.order(Order::Desc).take(500).await
}

#[mutation]
pub async fn add(ctx: &MutationCtx, text: String) -> Result<Id<Todo>> {
    ctx.db.insert(Todo { text, done: false }).await
}

fn main() -> ferrix::Result<()> { ferrix::Ferrix::new().run() }
```

Functions call each other through the runtime with turbofish: `ctx.run_mutation::<add>(("hi".into(),)).await?`.

**Indexes end in `_creationTime, _id`**: documents with equal indexed fields are in creation order, so
`with_index(Message::by_channel, |q| q.eq(Message::channel, c)).order(Order::Desc)` is newest first, and the creation
time can be bounded after the declared fields: `q.eq(Message::channel, c).gt(Message::_creation_time, since)`.
`.count()` counts an index range without reading the documents.

**Types that aren't tables** (a function's output "view" struct, an enum argument) use `#[object]`: it derives
serde and ts-rs through ferrix (camelCase JSON), so the app needs neither as a dependency. Codegen declares them in
`dataModel.ts` by name (`Array<MessageView>`), with field doc comments as JSDoc. A document can be embedded whole:

```rust
#[object]
pub struct MessageView {
    #[serde(flatten)]
    pub message: Doc<Message>,       // _id, _creationTime and the message's fields, at the top level
    pub author_name: String,         // TS: { authorName: string } & Doc<"messages">
}

#[query]
pub async fn list(ctx: &QueryCtx, channel: Id<Channel>, pagination_opts: PaginationOpts)
    -> Result<PaginationResult<MessageView>> {
    let page = ctx.db.query::<Message>()
        .with_index(Message::by_channel, |q| q.eq(Message::channel, channel))
        .order(Order::Desc)
        .paginate(pagination_opts)
        .await?;
    page.try_map(|message| view(ctx, message)).await      // also map_async, try_map_concurrent
}
```

**Dev mode.** `ctx.is_dev()` tells whether the server runs with `--dev`. `#[action(dev_only)]` (also queries and
mutations) marks a function for development (seeding, reading a dev inbox): the server refuses it outside `--dev`,
and codegen puts it in `devApi` instead of `api`. In `--dev` without SMTP, auth emails are printed to the log by
`DevLogMailer`; `AuthConfig::dev_mailer(m)` replaces it (and can forward to it).
