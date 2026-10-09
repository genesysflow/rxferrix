use ferrix::log;
use ferrix::prelude::*;

/// One item on the list.
#[table(name = "todos")]
#[index(by_done = [done])]
pub struct Todo {
    pub text: String,
    pub done: bool,
}

/// All todos, newest first. Pass `done` to show only open or only completed ones.
#[query]
pub async fn list(ctx: &QueryCtx, done: Option<bool>) -> Result<Vec<Doc<Todo>>> {
    let query = match done {
        Some(done) => ctx.db.query::<Todo>().with_index(Todo::by_done, |q| q.eq(Todo::done, done)),
        None => ctx.db.query::<Todo>(),
    };
    query.order(Order::Desc).take(500).await
}

/// How many todos are still open.
#[query]
pub async fn remaining(ctx: &QueryCtx) -> Result<u32> {
    let open = ctx.db.query::<Todo>().with_index(Todo::by_done, |q| q.eq(Todo::done, false)).collect().await?;
    Ok(open.len() as u32)
}

#[mutation]
pub async fn add(ctx: &MutationCtx, text: String) -> Result<Id<Todo>> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err(UserError::new("empty_text", "A todo needs some text").into());
    }
    log::info!(len = text.len(), "adding a todo");
    ctx.db.insert(Todo { text, done: false }).await
}

#[mutation]
pub async fn set_done(ctx: &MutationCtx, id: Id<Todo>, done: bool) -> Result<()> {
    ctx.db.patch(id, json!({ "done": done })).await
}

#[mutation]
pub async fn remove(ctx: &MutationCtx, id: Id<Todo>) -> Result<()> {
    ctx.db.delete(id).await
}

/// Deletes every completed todo; returns how many were removed.
#[mutation]
pub async fn clear_completed(ctx: &MutationCtx) -> Result<u32> {
    let done = ctx.db.query::<Todo>().with_index(Todo::by_done, |q| q.eq(Todo::done, true)).collect().await?;
    for todo in &done {
        ctx.db.delete(todo.id).await?;
    }
    Ok(done.len() as u32)
}

/// Adds `count` sample todos, one mutation each (shows actions composing mutations).
#[action]
pub async fn add_samples(ctx: &ActionCtx, count: u32) -> Result<u32> {
    for i in 1..=count {
        ctx.run_mutation::<add>((format!("Sample todo #{i}"),)).await?;
    }
    Ok(count)
}
