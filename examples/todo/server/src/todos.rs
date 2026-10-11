use std::time::Duration;

use ferrix::log;
use ferrix::prelude::*;
use ferrix::storage::StoredFile;
use ferrix::scheduler::ScheduledJob;

/// One item on the list, with an optional image (a stored file).
#[table(name = "todos")]
#[index(by_done = [done])]
#[search_index(search_text, search = text, filter = [done])]
pub struct Todo {
    pub text: String,
    pub done: bool,
    pub image: Option<Id<StoredFile>>,
    // When it was checked off (ms since the epoch); `purge_completed` deletes it a day later. (A plain comment: a
    // doc comment would be copied into the generated TS.)
    pub completed_at: Option<f64>,
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

/// Todos whose text contains every word of `text` (the last word as a prefix, for search-as-you-type), most
/// relevant first. Pass `done` to search only open or only completed ones.
#[query]
pub async fn search(ctx: &QueryCtx, text: String, done: Option<bool>) -> Result<Vec<Doc<Todo>>> {
    ctx.db
        .query::<Todo>()
        .with_search_index(Todo::search_text, |q| {
            let q = q.search(Todo::text, text);
            match done {
                Some(done) => q.eq(Todo::done, done),
                None => q,
            }
        })
        .take(50)
        .await
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
    ctx.db.insert(Todo { text, done: false, image: None, completed_at: None }).await
}

/// Adds the todo after `delay_seconds` (a delayed reminder): schedules `add`, in this mutation's transaction.
/// Returns the scheduled job, which `ctx.scheduler.cancel` (or the dashboard) can cancel.
#[mutation]
pub async fn add_later(ctx: &MutationCtx, text: String, delay_seconds: u32) -> Result<Id<ScheduledJob>> {
    if text.trim().is_empty() {
        return Err(UserError::new("empty_text", "A todo needs some text").into());
    }
    ctx.scheduler.run_after::<add>(Duration::from_secs(delay_seconds.into()), (text,)).await
}

#[mutation]
pub async fn set_done(ctx: &MutationCtx, id: Id<Todo>, done: bool) -> Result<()> {
    let completed_at = done.then(|| ctx.now());
    ctx.db.patch(id, json!({ "done": done, "completedAt": completed_at })).await
}

/// Deletes a todo and its image.
#[mutation]
pub async fn remove(ctx: &MutationCtx, id: Id<Todo>) -> Result<()> {
    if let Some(todo) = ctx.db.get(id).await? {
        delete_image(ctx, &todo.value).await?;
    }
    ctx.db.delete(id).await
}

/// Deletes the todo's image file, if it has one. Transactional: if the mutation fails, the file stays.
async fn delete_image(ctx: &MutationCtx, todo: &Todo) -> Result<()> {
    if let Some(image) = todo.image
        && ctx.storage.get_metadata(image).await?.is_some()
    {
        ctx.storage.delete(image).await?;
    }
    Ok(())
}

/// Step 1 of attaching an image: a short-lived URL to upload it to (`uploadFile` in `@ferrix/client`).
#[mutation]
pub async fn generate_upload_url(ctx: &MutationCtx) -> Result<String> {
    ctx.storage.generate_upload_url().await
}

/// Step 2: attach the uploaded image to a todo (`image: null` removes it). The previous image is deleted.
#[mutation]
pub async fn set_image(ctx: &MutationCtx, id: Id<Todo>, image: Option<Id<StoredFile>>) -> Result<()> {
    let Some(todo) = ctx.db.get(id).await? else {
        return Err(UserError::new("not_found", "This todo no longer exists").into());
    };
    if let Some(image) = image {
        let Some(file) = ctx.storage.get_metadata(image).await? else {
            return Err(UserError::new("not_found", "The uploaded file does not exist").into());
        };
        if !file.content_type.as_deref().is_some_and(|ct| ct.starts_with("image/")) {
            ctx.storage.delete(image).await?;
            return Err(UserError::new("not_an_image", "Only images can be attached").into());
        }
    }
    if todo.image != image {
        delete_image(ctx, &todo.value).await?;
    }
    ctx.db.patch(id, json!({ "image": image })).await
}

/// The URL of a todo's image, or `null` once it is deleted (live, like any query).
#[query]
pub async fn image_url(ctx: &QueryCtx, image: Id<StoredFile>) -> Result<Option<String>> {
    ctx.storage.get_url(image).await
}

/// Deletes every completed todo; returns how many were removed.
#[mutation]
pub async fn clear_completed(ctx: &MutationCtx) -> Result<u32> {
    let done = ctx.db.query::<Todo>().with_index(Todo::by_done, |q| q.eq(Todo::done, true)).collect().await?;
    for todo in &done {
        delete_image(ctx, &todo.value).await?;
        ctx.db.delete(todo.id).await?;
    }
    Ok(done.len() as u32)
}

/// Deletes todos completed more than a day ago. Runs hourly as a cron (see `main.rs`); internal, so clients can't
/// call it.
#[internal_mutation]
pub async fn purge_completed(ctx: &MutationCtx) -> Result<u32> {
    let cutoff = ctx.now() - 24.0 * 3600.0 * 1000.0;
    let done = ctx.db.query::<Todo>().with_index(Todo::by_done, |q| q.eq(Todo::done, true)).collect().await?;
    let mut purged = 0;
    for todo in done.iter().filter(|t| t.completed_at.is_some_and(|at| at < cutoff)) {
        ctx.db.delete(todo.id).await?;
        purged += 1;
    }
    log::info!(purged, "purged completed todos");
    Ok(purged)
}

/// Adds `count` sample todos, one mutation each (shows actions composing mutations).
#[action]
pub async fn add_samples(ctx: &ActionCtx, count: u32) -> Result<u32> {
    for i in 1..=count {
        ctx.run_mutation::<add>((format!("Sample todo #{i}"),)).await?;
    }
    Ok(count)
}

/// Todos added and completed per day.
#[object]
pub struct DayStats {
    pub day: String,
    pub added: i64,
    pub completed: i64,
}

/// Todos added and completed per day, in SQL over the analytics copy. Needs `serve --olap` and a binary built with
/// the `olap` feature (`cargo run -p todo-server --features olap -- serve --dev --olap`); fails with
/// `olap_unavailable` otherwise.
#[action]
pub async fn stats_by_day(ctx: &ActionCtx) -> Result<Vec<DayStats>> {
    ctx.olap
        .query_as::<DayStats>(
            "SELECT strftime(to_timestamp(_creationTime / 1000), '%Y-%m-%d') AS day,
                    count(*) AS added,
                    count(*) FILTER (WHERE done) AS completed
             FROM todos GROUP BY day ORDER BY day",
            (),
        )
        .await
}
