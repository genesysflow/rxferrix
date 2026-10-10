use ferrix::prelude::*;

/// One item on the list.
#[table(name = "tasks")]
pub struct Task {
    pub text: String,
    pub done: bool,
}

/// All tasks, newest first.
#[query]
pub async fn list(ctx: &QueryCtx) -> Result<Vec<Doc<Task>>> {
    ctx.db.query::<Task>().order(Order::Desc).take(100).await
}

#[mutation]
pub async fn add(ctx: &MutationCtx, text: String) -> Result<Id<Task>> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err(UserError::new("empty_text", "A task needs some text").into());
    }
    ctx.db.insert(Task { text, done: false }).await
}

#[mutation]
pub async fn set_done(ctx: &MutationCtx, id: Id<Task>, done: bool) -> Result<()> {
    ctx.db.patch(id, json!({ "done": done })).await
}

#[mutation]
pub async fn remove(ctx: &MutationCtx, id: Id<Task>) -> Result<()> {
    ctx.db.delete(id).await
}
