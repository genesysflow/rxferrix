//! Channels: listing, creating, joining, inviting, and read tracking (unread counts).

use std::collections::HashSet;

use ferrix::auth::User;
use ferrix::prelude::*;

use crate::model::{Channel, Member, Message, get_channel, membership, require_member, require_readable, user_error};
use crate::profiles::Names;

/// A channel as the sidebar shows it: the channel document, whether the caller joined it, and how many messages it
/// has (a live aggregate: no messages are read).
#[object]
#[derive(Clone, Debug)]
pub struct ChannelSummary {
    #[serde(flatten)]
    pub channel: Doc<Channel>,
    pub is_member: bool,
    pub message_count: u64,
}

/// Someone's number of messages in a channel.
#[object]
#[derive(Clone, Debug)]
pub struct PosterCount {
    pub user: Id<User>,
    pub name: String,
    pub color: String,
    pub messages: u64,
}

#[object]
#[derive(Clone, Debug)]
pub struct MemberInfo {
    pub user: Id<User>,
    pub name: String,
    pub color: String,
}

/// The channel header: the channel, whether the caller joined it, who did, and live counts.
#[object]
#[derive(Clone, Debug)]
pub struct ChannelInfo {
    #[serde(flatten)]
    pub channel: Doc<Channel>,
    pub is_member: bool,
    pub members: Vec<MemberInfo>,
    pub message_count: u64,
    pub member_count: u64,
    /// The channel's five most active people (`messages_per_author`).
    pub top_posters: Vec<PosterCount>,
}

#[object]
#[derive(Clone, Debug)]
pub struct UnreadCount {
    pub channel: Id<Channel>,
    pub count: u32,
}

/// Unread counts stop at this number (the badge shows "99+").
const MAX_UNREAD: usize = 100;

/// Public channels plus the private channels the caller belongs to, by name.
#[query]
pub async fn list(ctx: &QueryCtx) -> Result<Vec<ChannelSummary>> {
    let user = ctx.auth.require_user()?;
    let mine: HashSet<Id<Channel>> = ctx
        .db
        .query::<Member>()
        .with_index(Member::by_user, |q| q.eq(Member::user, user))
        .collect()
        .await?
        .into_iter()
        .map(|m| m.value.channel)
        .collect();
    let channels = ctx.db.query::<Channel>().with_index(Channel::by_name, |q| q).take(500).await?;
    let mut out = Vec::new();
    for channel in channels.into_iter().filter(|c| !c.private || mine.contains(&c.id)) {
        let messages = ctx.db.aggregate(Message::messages_per_channel).get(|g| g.eq(Message::channel, channel.id)).await?;
        out.push(ChannelSummary { is_member: mine.contains(&channel.id), message_count: messages.count, channel });
    }
    Ok(out)
}

/// One channel with its members. Fails with `not_a_member` for a private channel the caller isn't in.
#[query]
pub async fn get(ctx: &QueryCtx, channel: Id<Channel>) -> Result<ChannelInfo> {
    let user = ctx.auth.require_user()?;
    let doc = require_readable(&ctx.db, channel, user).await?;
    let rows = ctx
        .db
        .query::<Member>()
        .with_index(Member::by_channel_user, |q| q.eq(Member::channel, channel))
        .take(1000)
        .await?;
    let names = Names::default();
    let mut members = Vec::with_capacity(rows.len());
    for row in &rows {
        let (name, color) = names.get(&ctx.db, row.user).await?;
        members.push(MemberInfo { user: row.user, name, color });
    }
    members.sort_by_key(|m| m.name.to_lowercase());
    let message_count = ctx.db.aggregate(Message::messages_per_channel).get(|g| g.eq(Message::channel, channel)).await?.count;
    let member_count = ctx.db.aggregate(Member::members_per_channel).get(|g| g.eq(Member::channel, channel)).await?.count;
    // Every author's count in this channel (one row per author), the five largest.
    let mut posters = ctx
        .db
        .aggregate(Message::messages_per_author)
        .groups(|g| g.eq(Message::channel, channel), Order::Asc, 10_000)
        .await?;
    posters.sort_by_key(|a| std::cmp::Reverse(a.value.count));
    let mut top_posters = Vec::new();
    for g in posters.into_iter().take(5) {
        let Some(author) = g.group.get(1).and_then(|v| v.as_str()).and_then(|s| s.parse::<Id<User>>().ok()) else { continue };
        let (name, color) = names.get(&ctx.db, author).await?;
        top_posters.push(PosterCount { user: author, name, color, messages: g.value.count });
    }
    Ok(ChannelInfo { channel: doc, is_member: rows.iter().any(|m| m.user == user), members, message_count, member_count, top_posters })
}

/// Creates a channel (the caller joins it). Names are lowercase letters, digits and dashes, and unique.
#[mutation]
pub async fn create(ctx: &MutationCtx, name: String, topic: Option<String>, private: bool) -> Result<Id<Channel>> {
    let user = ctx.auth.require_user()?;
    let name = name.trim().trim_start_matches('#').to_lowercase();
    if name.is_empty() || name.len() > 32 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(user_error("invalid_name", "Channel names use 1-32 lowercase letters, digits or dashes"));
    }
    let taken = ctx.db.query::<Channel>().with_index(Channel::by_name, |q| q.eq(Channel::name, name.clone())).first().await?;
    if taken.is_some() {
        return Err(user_error("name_taken", format!("#{name} already exists")));
    }
    let topic = topic.unwrap_or_default().trim().chars().take(200).collect();
    let id = ctx.db.insert(Channel { name, topic, private, created_by: user }).await?;
    ctx.db.insert(Member { channel: id, user, last_read_at: ctx.now() }).await?;
    Ok(id)
}

/// Joins a public channel. Private channels need an invitation (`private_channel`).
#[mutation]
pub async fn join(ctx: &MutationCtx, channel: Id<Channel>) -> Result<()> {
    let user = ctx.auth.require_user()?;
    let doc = get_channel(&ctx.db, channel).await?;
    if membership(&ctx.db, channel, user).await?.is_some() {
        return Ok(());
    }
    if doc.private {
        return Err(user_error("private_channel", "This channel is private. Ask a member to invite you."));
    }
    ctx.db.insert(Member { channel, user, last_read_at: ctx.now() }).await?;
    Ok(())
}

#[mutation]
pub async fn leave(ctx: &MutationCtx, channel: Id<Channel>) -> Result<()> {
    let user = ctx.auth.require_user()?;
    if let Some(member) = membership(&ctx.db, channel, user).await? {
        ctx.db.delete(member.id).await?;
    }
    Ok(())
}

/// Adds the user with this email to a channel the caller belongs to (the way into private channels).
#[mutation]
pub async fn invite(ctx: &MutationCtx, channel: Id<Channel>, email: String) -> Result<()> {
    let user = ctx.auth.require_user()?;
    require_member(&ctx.db, channel, user).await?;
    let email = email.trim().to_lowercase();
    let Some(invitee) = ctx.db.query::<User>().with_index(User::by_email, |q| q.eq(User::email, Some(email.clone()))).first().await?
    else {
        return Err(user_error("no_such_user", format!("Nobody has signed up as {email} yet")));
    };
    if membership(&ctx.db, channel, invitee.id).await?.is_none() {
        ctx.db.insert(Member { channel, user: invitee.id, last_read_at: 0.0 }).await?;
    }
    Ok(())
}

/// Marks everything in the channel as read (the app calls it while the channel is open).
#[mutation]
pub async fn mark_read(ctx: &MutationCtx, channel: Id<Channel>) -> Result<()> {
    let user = ctx.auth.require_user()?;
    let Some(member) = membership(&ctx.db, channel, user).await? else { return Ok(()) };
    // Skip the write when nothing is new: every membership write reruns the channel header for all its viewers.
    let newer = ctx
        .db
        .query::<Message>()
        .with_index(Message::by_channel, |q| {
            q.eq(Message::channel, channel).gt(Message::_creation_time, member.last_read_at)
        })
        .first()
        .await?;
    if newer.is_some() {
        ctx.db.patch(member.id, json!({ "lastReadAt": ctx.now() })).await?;
    }
    Ok(())
}

/// How many messages from others arrived in each of the caller's channels since they last read it.
#[query]
pub async fn unread(ctx: &QueryCtx) -> Result<Vec<UnreadCount>> {
    let user = ctx.auth.require_user()?;
    let memberships = ctx.db.query::<Member>().with_index(Member::by_user, |q| q.eq(Member::user, user)).collect().await?;
    let mut out = Vec::new();
    for m in memberships {
        let since_read = |q: ferrix::IndexRange<Message>| {
            q.eq(Message::channel, m.channel).gt(Message::_creation_time, m.last_read_at)
        };
        // Most channels have nothing new: an index count (no documents read) settles those.
        if ctx.db.query::<Message>().with_index(Message::by_channel, since_read).count().await? == 0 {
            continue;
        }
        let unread = ctx
            .db
            .query::<Message>()
            .with_index(Message::by_channel, since_read)
            .filter(move |msg| msg.author != user)
            .take(MAX_UNREAD)
            .await?;
        if !unread.is_empty() {
            out.push(UnreadCount { channel: m.channel, count: unread.len() as u32 });
        }
    }
    Ok(out)
}
