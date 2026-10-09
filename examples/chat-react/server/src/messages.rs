//! Messages: the paginated history, sending (with attachments and slash commands), editing, deleting, search, and
//! the scheduled `/remind` delivery.

use std::time::Duration;

use ferrix::auth::User;
use ferrix::log;
use ferrix::prelude::*;
use ferrix::storage::StoredFile;
use ferrix::{MutationDb, QueryDb};

use crate::model::{Channel, Message, Reaction, Typing, require_member, require_readable, user_error};
use crate::profiles::Names;

pub const MAX_BODY: usize = 4000;

/// An attached file, as the app shows it.
#[object]
#[derive(Clone, Debug, PartialEq)]
pub struct Attachment {
    pub url: String,
    pub content_type: Option<String>,
    pub filename: Option<String>,
    pub size: f64,
}

/// One emoji's reactions on a message.
#[object]
#[derive(Clone, Debug, PartialEq)]
pub struct ReactionGroup {
    pub emoji: String,
    pub count: u32,
    /// Whether the caller is among them.
    pub mine: bool,
    pub names: Vec<String>,
}

/// A message as the app shows it: the message document (`_id`, `_creationTime`, `body`, ...) plus the author's
/// name and color, the attached file's URL, and the reactions grouped by emoji.
#[object]
#[derive(Clone, Debug)]
pub struct MessageView {
    #[serde(flatten)]
    pub message: Doc<Message>,
    pub author_name: String,
    pub author_color: String,
    /// The attachment's URL and metadata (`attachment` is its storage id).
    pub file: Option<Attachment>,
    pub reactions: Vec<ReactionGroup>,
}

/// What `send` did: posted a message, or ran a slash command (then `notice` says what happened).
#[object]
#[derive(Clone, Debug)]
pub struct SendResult {
    pub message: Option<Id<Message>>,
    pub notice: Option<String>,
}

/// A channel's messages, newest first (the app shows them bottom-up and loads older pages on scroll).
#[query]
pub async fn list(ctx: &QueryCtx, channel: Id<Channel>, pagination_opts: PaginationOpts) -> Result<PaginationResult<MessageView>> {
    let user = ctx.auth.require_user()?;
    require_readable(&ctx.db, channel, user).await?;
    let page = ctx
        .db
        .query::<Message>()
        .with_index(Message::by_channel, |q| q.eq(Message::channel, channel))
        .order(Order::Desc)
        .paginate(pagination_opts)
        .await?;
    let names = Names::default();
    page.try_map(|message| view(ctx, &names, user, message)).await
}

/// Messages whose text contains every word of `text` (the last one as a prefix), most relevant first.
#[query]
pub async fn search(ctx: &QueryCtx, channel: Id<Channel>, text: String) -> Result<Vec<MessageView>> {
    let user = ctx.auth.require_user()?;
    require_readable(&ctx.db, channel, user).await?;
    let hits = ctx
        .db
        .query::<Message>()
        .with_search_index(Message::search_body, |q| q.search(Message::body, text).eq(Message::channel, channel))
        .take(20)
        .await?;
    let names = Names::default();
    let mut views = Vec::with_capacity(hits.len());
    for message in hits {
        views.push(view(ctx, &names, user, message).await?);
    }
    Ok(views)
}

async fn view(ctx: &QueryCtx, names: &Names, me: Id<User>, message: Doc<Message>) -> Result<MessageView> {
    let (author_name, author_color) = names.get(&ctx.db, message.author).await?;
    let file = match message.attachment {
        Some(file) => match (ctx.storage.get_url(file).await?, ctx.storage.get_metadata(file).await?) {
            (Some(url), Some(meta)) => Some(Attachment {
                url,
                content_type: meta.value.content_type,
                filename: meta.value.filename,
                size: meta.value.size as f64,
            }),
            _ => None,
        },
        None => None,
    };
    let rows = ctx
        .db
        .query::<Reaction>()
        .with_index(Reaction::by_message_user, |q| q.eq(Reaction::message, message.id))
        .take(500)
        .await?;
    let mut reactions: Vec<ReactionGroup> = Vec::new();
    for row in rows {
        let (name, _) = names.get(&ctx.db, row.user).await?;
        let mine = row.user == me;
        match reactions.iter_mut().find(|g| g.emoji == row.emoji) {
            Some(group) => {
                group.count += 1;
                group.mine |= mine;
                group.names.push(name);
            }
            None => reactions.push(ReactionGroup { emoji: row.value.emoji, count: 1, mine, names: vec![name] }),
        }
    }
    Ok(MessageView { message, author_name, author_color, file, reactions })
}

/// Posts a message (text and/or an uploaded image), or runs a slash command:
/// `/remind me in <n> <seconds|minutes|hours> [to] <text>`.
#[mutation]
pub async fn send(ctx: &MutationCtx, channel: Id<Channel>, body: String, attachment: Option<Id<StoredFile>>) -> Result<SendResult> {
    let user = ctx.auth.require_user()?;
    let (_, member) = require_member(&ctx.db, channel, user).await?;
    let body = body.trim().to_string();
    if body.starts_with('/') && attachment.is_none() {
        return command(ctx, channel, user, &body).await;
    }
    if body.is_empty() && attachment.is_none() {
        return Err(user_error("empty_message", "Write something or attach an image"));
    }
    if body.chars().count() > MAX_BODY {
        return Err(user_error("too_long", format!("Messages are limited to {MAX_BODY} characters")));
    }
    if let Some(file) = attachment {
        check_image(ctx, file).await?;
    }
    let id = ctx.db.insert(Message { channel, author: user, body, attachment, edited: false, kind: "message".into() }).await?;
    // Sending ends "typing…" and means the author has read the channel, up to their own message.
    clear_typing(&ctx.db, channel, user).await?;
    let sent_at = ctx.db.get(id).await?.map_or_else(|| ctx.now(), |m| m.creation_time);
    ctx.db.patch(member.id, json!({ "lastReadAt": sent_at })).await?;
    Ok(SendResult { message: Some(id), notice: None })
}

async fn command(ctx: &MutationCtx, channel: Id<Channel>, user: Id<User>, body: &str) -> Result<SendResult> {
    let mut words = body.split_whitespace();
    match words.next() {
        Some("/remind") => {
            let rest: Vec<&str> = words.collect();
            let Some((delay, text)) = parse_remind(&rest) else {
                return Err(user_error("bad_command", "Usage: /remind me in <n> <seconds|minutes|hours> [to] <text>"));
            };
            ctx.scheduler.run_after::<deliver_reminder>(delay, (channel, user, text.clone())).await?;
            let when = describe(delay);
            log::info!(seconds = delay.as_secs(), "reminder scheduled");
            Ok(SendResult { message: None, notice: Some(format!("Okay, I'll remind you in {when}: {text}")) })
        }
        Some(other) => Err(user_error("unknown_command", format!("Unknown command {other}. Try /remind me in 5 minutes to stretch"))),
        None => Err(user_error("empty_message", "Write something or attach an image")),
    }
}

/// `["me", "in", "5", "minutes", "to", "stretch"]` → (5 min, "stretch"). Also `in 10s`, `in 2 hours`.
pub fn parse_remind(words: &[&str]) -> Option<(Duration, String)> {
    let words = match words {
        ["me", rest @ ..] => rest,
        rest => rest,
    };
    let ["in", amount, rest @ ..] = words else { return None };
    // "10s" or "10 s"
    let split = amount.find(|c: char| !c.is_ascii_digit()).unwrap_or(amount.len());
    let n: u64 = amount[..split].parse().ok()?;
    let (unit, rest) = if split < amount.len() { (&amount[split..], rest) } else { (*rest.first()?, &rest[1..]) };
    let secs = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => n,
        "m" | "min" | "mins" | "minute" | "minutes" => n * 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => n * 3600,
        _ => return None,
    };
    if secs == 0 || secs > 7 * 24 * 3600 {
        return None;
    }
    let rest = match rest {
        ["to", rest @ ..] => rest,
        rest => rest,
    };
    let text = rest.join(" ");
    let text = if text.is_empty() { "this".to_string() } else { text };
    Some((Duration::from_secs(secs), text))
}

fn describe(d: Duration) -> String {
    let s = d.as_secs();
    let (n, unit) = if s.is_multiple_of(3600) { (s / 3600, "hour") } else if s.is_multiple_of(60) { (s / 60, "minute") } else { (s, "second") };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

/// Posts a `/remind` message. Scheduled by `send`; runs with no identity, so it takes the user as an argument.
#[internal_mutation]
pub async fn deliver_reminder(ctx: &MutationCtx, channel: Id<Channel>, user: Id<User>, text: String) -> Result<()> {
    if require_member(&ctx.db, channel, user).await.is_err() {
        log::info!("reminder dropped: the user left the channel");
        return Ok(());
    }
    let message = Message { channel, author: user, body: text, attachment: None, edited: false, kind: "reminder".into() };
    ctx.db.insert(message).await?;
    Ok(())
}

/// Step 1 of attaching an image: a short-lived URL to upload it to.
#[mutation]
pub async fn generate_upload_url(ctx: &MutationCtx) -> Result<String> {
    ctx.auth.require_user()?;
    ctx.storage.generate_upload_url().await
}

async fn check_image(ctx: &MutationCtx, file: Id<StoredFile>) -> Result<()> {
    let Some(meta) = ctx.storage.get_metadata(file).await? else {
        return Err(user_error("not_found", "The uploaded file does not exist"));
    };
    if !meta.content_type.as_deref().is_some_and(|ct| ct.starts_with("image/")) {
        ctx.storage.delete(file).await?;
        return Err(user_error("not_an_image", "Only images can be attached"));
    }
    Ok(())
}

/// The message, if the caller wrote it (`forbidden` otherwise).
async fn own_message(db: &QueryDb, user: Id<User>, id: Id<Message>) -> Result<Doc<Message>> {
    let Some(message) = db.get(id).await? else {
        return Err(user_error("not_found", "This message was deleted"));
    };
    if message.author != user {
        return Err(user_error("forbidden", "Only the author can change this message"));
    }
    Ok(message)
}

#[mutation]
pub async fn edit(ctx: &MutationCtx, message: Id<Message>, body: String) -> Result<()> {
    let user = ctx.auth.require_user()?;
    let doc = own_message(&ctx.db, user, message).await?;
    require_member(&ctx.db, doc.channel, user).await?;
    let body = body.trim().to_string();
    if body.is_empty() && doc.attachment.is_none() {
        return Err(user_error("empty_message", "A message can't be empty; delete it instead"));
    }
    if body.chars().count() > MAX_BODY {
        return Err(user_error("too_long", format!("Messages are limited to {MAX_BODY} characters")));
    }
    if body == doc.body {
        return Ok(());
    }
    ctx.db.patch(message, json!({ "body": body, "edited": true })).await
}

/// Deletes a message with its reactions and attachment.
#[mutation]
pub async fn remove(ctx: &MutationCtx, message: Id<Message>) -> Result<()> {
    let user = ctx.auth.require_user()?;
    let doc = own_message(&ctx.db, user, message).await?;
    let reactions = ctx
        .db
        .query::<Reaction>()
        .with_index(Reaction::by_message_user, |q| q.eq(Reaction::message, message))
        .collect()
        .await?;
    for reaction in reactions {
        ctx.db.delete(reaction.id).await?;
    }
    if let Some(file) = doc.attachment
        && ctx.storage.get_metadata(file).await?.is_some()
    {
        ctx.storage.delete(file).await?;
    }
    ctx.db.delete(message).await
}

pub async fn clear_typing(db: &MutationDb, channel: Id<Channel>, user: Id<User>) -> Result<()> {
    let row = db
        .query::<Typing>()
        .with_index(Typing::by_channel_user, |q| q.eq(Typing::channel, channel).eq(Typing::user, user))
        .first()
        .await?;
    if let Some(row) = row {
        db.delete(row.id).await?;
    }
    Ok(())
}

/// Posts `count` numbered sample messages to a channel (for trying infinite scroll): #1 is the oldest, #count the
/// newest. They are inserted in one transaction: clients see them arrive at once, as one block of history (in
/// batches, each batch would land on top of the newest page, which a client that already loaded older pages keeps
/// pinned, so it would grow to hold them all).
#[action]
pub async fn seed(ctx: &ActionCtx, channel: Id<Channel>, count: u32) -> Result<u32> {
    ctx.auth.require_user()?;
    if count == 0 || count > 1000 {
        return Err(user_error("bad_count", "Seed between 1 and 1000 messages"));
    }
    ctx.run_mutation::<insert_seed>((channel, 0, count, count)).await?;
    Ok(count)
}

/// Inserts sample messages `start + 1 ..= start + n` of `total`, in order.
#[internal_mutation]
pub async fn insert_seed(ctx: &MutationCtx, channel: Id<Channel>, start: u32, n: u32, total: u32) -> Result<()> {
    let user = ctx.auth.require_user()?;
    require_member(&ctx.db, channel, user).await?;
    for i in start + 1..=start + n {
        let body = format!("Sample message #{i} of {total}");
        ctx.db.insert(Message { channel, author: user, body, attachment: None, edited: false, kind: "message".into() }).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remind() {
        let p = |s: &str| parse_remind(&s.split_whitespace().collect::<Vec<_>>());
        assert_eq!(p("me in 5 minutes to stretch"), Some((Duration::from_secs(300), "stretch".into())));
        assert_eq!(p("me in 3s check the oven"), Some((Duration::from_secs(3), "check the oven".into())));
        assert_eq!(p("in 2 hours"), Some((Duration::from_secs(7200), "this".into())));
        assert_eq!(p("me in five minutes"), None);
        assert_eq!(p("me in 0 s x"), None);
        assert_eq!(p("me at 5"), None);
        assert_eq!(describe(Duration::from_secs(3)), "3 seconds");
        assert_eq!(describe(Duration::from_secs(60)), "1 minute");
    }
}
