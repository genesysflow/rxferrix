//! Tables and the access helpers every function uses.

use ferrix::QueryDb;
use ferrix::auth::User;
use ferrix::prelude::*;
use ferrix::storage::StoredFile;

/// App-level profile data for a `_users` row: the display name and avatar color shown next to messages.
#[table(name = "profiles")]
#[index(by_user = [user])]
pub struct Profile {
    pub user: Id<User>,
    pub name: String,
    pub color: String,
}

#[table(name = "channels")]
#[index(by_name = [name])]
pub struct Channel {
    pub name: String,
    pub topic: String,
    pub private: bool,
    pub created_by: Id<User>,
}

/// A user's membership in a channel. `last_read_at` (ms) drives the unread counts.
#[table(name = "members")]
#[index(by_channel_user = [channel, user])]
#[index(by_user = [user])]
// Live member counts (the channel header), kept by every join and leave.
#[aggregate(members_per_channel, group = [channel])]
pub struct Member {
    pub channel: Id<Channel>,
    pub user: Id<User>,
    pub last_read_at: f64,
}

// Index keys end in `_creationTime, _id`, so `by_channel` lists a channel's messages in the order they were sent
// (`Order::Desc`: newest first), and `_creation_time` can be bounded after the channel (unread counts).
//
// Live aggregates, kept by every send, edit and delete in the same transaction: message counts per channel (the
// sidebar) and per author within a channel (the header's top posters). A new message reruns only the queries that
// read its channel's counts.
#[table(name = "messages")]
#[index(by_channel = [channel])]
#[search_index(search_body, search = body, filter = [channel])]
#[aggregate(messages_per_channel, group = [channel])]
#[aggregate(messages_per_author, group = [channel, author])]
pub struct Message {
    pub channel: Id<Channel>,
    pub author: Id<User>,
    pub body: String,
    pub attachment: Option<Id<StoredFile>>,
    pub edited: bool,
    // "message" or "reminder" (posted by the scheduler for `/remind`).
    pub kind: String,
}

#[table(name = "reactions")]
#[index(by_message_user = [message, user, emoji])]
pub struct Reaction {
    pub message: Id<Message>,
    pub user: Id<User>,
    pub emoji: String,
}

/// "X is typing…": one row per (channel, user), deleted by a scheduled job once `expires_at` passes.
#[table(name = "typing")]
#[index(by_channel_user = [channel, user])]
#[index(by_expires = [expires_at])]
pub struct Typing {
    pub channel: Id<Channel>,
    pub user: Id<User>,
    pub expires_at: f64,
}

pub fn user_error(code: &str, message: impl Into<String>) -> Error {
    UserError::new(code, message).into()
}

pub async fn get_channel(db: &QueryDb, channel: Id<Channel>) -> Result<Doc<Channel>> {
    db.get(channel).await?.ok_or_else(|| user_error("not_found", "This channel no longer exists"))
}

pub async fn membership(db: &QueryDb, channel: Id<Channel>, user: Id<User>) -> Result<Option<Doc<Member>>> {
    db.query::<Member>()
        .with_index(Member::by_channel_user, |q| q.eq(Member::channel, channel).eq(Member::user, user))
        .first()
        .await
}

/// The channel, if `user` may read it: public channels are readable by every signed-in user (a preview before
/// joining), private ones only by their members.
pub async fn require_readable(db: &QueryDb, channel: Id<Channel>, user: Id<User>) -> Result<Doc<Channel>> {
    let doc = get_channel(db, channel).await?;
    if doc.private && membership(db, channel, user).await?.is_none() {
        return Err(user_error("not_a_member", "This channel is private. Ask a member to invite you."));
    }
    Ok(doc)
}

/// The channel and the membership of `user`, who must have joined it.
pub async fn require_member(db: &QueryDb, channel: Id<Channel>, user: Id<User>) -> Result<(Doc<Channel>, Doc<Member>)> {
    let doc = get_channel(db, channel).await?;
    match membership(db, channel, user).await? {
        Some(member) => Ok((doc, member)),
        None if doc.private => Err(user_error("not_a_member", "This channel is private. Ask a member to invite you.")),
        None => Err(user_error("not_a_member", format!("Join #{} to take part", doc.name))),
    }
}
