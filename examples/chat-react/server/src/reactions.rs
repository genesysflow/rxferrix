//! Emoji reactions on messages.

use ferrix::prelude::*;

use crate::model::{Message, Reaction, require_member, user_error};

/// Adds the caller's `emoji` reaction to a message, or removes it if it is already there. Returns whether it was
/// added.
#[mutation]
pub async fn toggle(ctx: &MutationCtx, message: Id<Message>, emoji: String) -> Result<bool> {
    let user = ctx.auth.require_user()?;
    let emoji = emoji.trim().to_string();
    if emoji.is_empty() || emoji.chars().count() > 8 || emoji.chars().any(|c| c.is_ascii_alphanumeric()) {
        return Err(user_error("bad_emoji", "React with an emoji"));
    }
    let Some(doc) = ctx.db.get(message).await? else {
        return Err(user_error("not_found", "This message was deleted"));
    };
    require_member(&ctx.db, doc.channel, user).await?;
    let existing = ctx
        .db
        .query::<Reaction>()
        .with_index(Reaction::by_message_user, |q| {
            q.eq(Reaction::message, message).eq(Reaction::user, user).eq(Reaction::emoji, emoji.clone())
        })
        .first()
        .await?;
    match existing {
        Some(row) => {
            ctx.db.delete(row.id).await?;
            Ok(false)
        }
        None => {
            ctx.db.insert(Reaction { message, user, emoji }).await?;
            Ok(true)
        }
    }
}
