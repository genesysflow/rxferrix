use ferrix::log;
use ferrix::prelude::*;
use ferrix::storage::StoredFile;

/// One item on the list, with an optional image (a stored file).
#[table(name = "todos")]
#[index(by_done = [done])]
pub struct Todo {
    pub text: String,
    pub done: bool,
    pub image: Option<Id<StoredFile>>,
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
    ctx.db.insert(Todo { text, done: false, image: None }).await
}

#[mutation]
pub async fn set_done(ctx: &MutationCtx, id: Id<Todo>, done: bool) -> Result<()> {
    ctx.db.patch(id, json!({ "done": done })).await
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

/// Adds `count` sample todos, one mutation each (shows actions composing mutations).
#[action]
pub async fn add_samples(ctx: &ActionCtx, count: u32) -> Result<u32> {
    for i in 1..=count {
        ctx.run_mutation::<add>((format!("Sample todo #{i}"),)).await?;
    }
    Ok(count)
}
