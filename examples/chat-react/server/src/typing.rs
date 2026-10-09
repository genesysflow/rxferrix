//! Typing indicators. `start` upserts a row that expires a few seconds later; a scheduled job (`expire`) deletes it
//! then, and an hourly cron (`prune`) sweeps up anything a failed job left behind.

use std::time::Duration;

use ferrix::auth::User;
use ferrix::log;
use ferrix::prelude::*;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::messages::clear_typing;
use crate::model::{Channel, Typing, require_member, require_readable};
use crate::profiles::Names;

/// How long "typing…" lasts after the last keystroke the app reported (it reports at most every 2 seconds).
pub const TYPING_TTL: Duration = Duration::from_secs(4);

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct TypingUser {
    pub user: Id<User>,
    pub name: String,
}

/// Who else is typing in the channel.
#[query]
pub async fn list(ctx: &QueryCtx, channel: Id<Channel>) -> Result<Vec<TypingUser>> {
    let user = ctx.auth.require_user()?;
    require_readable(&ctx.db, channel, user).await?;
    let rows = ctx
        .db
        .query::<Typing>()
        .with_index(Typing::by_channel_user, |q| q.eq(Typing::channel, channel))
        .take(50)
        .await?;
    let mut names = Names::default();
    let mut out = Vec::new();
    for row in rows.into_iter().filter(|r| r.user != user) {
        let (name, _) = names.get(&ctx.db, row.user).await?;
        out.push(TypingUser { user: row.user, name });
    }
    Ok(out)
}

/// The caller is typing in the channel (until `TYPING_TTL` after the last call).
#[mutation]
pub async fn start(ctx: &MutationCtx, channel: Id<Channel>) -> Result<()> {
    let user = ctx.auth.require_user()?;
    require_member(&ctx.db, channel, user).await?;
    let expires_at = ctx.now() + TYPING_TTL.as_millis() as f64;
    let existing = ctx
        .db
        .query::<Typing>()
        .with_index(Typing::by_channel_user, |q| q.eq(Typing::channel, channel).eq(Typing::user, user))
        .first()
        .await?;
    match existing {
        // Its expiry job is still pending: it sees the later `expires_at` and reschedules itself.
        Some(row) => ctx.db.patch(row.id, json!({ "expiresAt": expires_at })).await,
        None => {
            let id = ctx.db.insert(Typing { channel, user, expires_at }).await?;
            ctx.scheduler.run_after::<expire>(TYPING_TTL, (id,)).await?;
            Ok(())
        }
    }
}

/// The caller stopped typing (cleared the composer, or switched channels).
#[mutation]
pub async fn stop(ctx: &MutationCtx, channel: Id<Channel>) -> Result<()> {
    let user = ctx.auth.require_user()?;
    clear_typing(&ctx.db, channel, user).await
}

/// Deletes a typing row once it has expired, or checks again when it will.
#[internal_mutation]
pub async fn expire(ctx: &MutationCtx, id: Id<Typing>) -> Result<()> {
    let Some(row) = ctx.db.get(id).await? else { return Ok(()) };
    if row.expires_at <= ctx.now() {
        ctx.db.delete(id).await
    } else {
        ctx.scheduler.run_at::<expire>(row.expires_at, (id,)).await?;
        Ok(())
    }
}

/// Hourly cron: deletes typing rows that expired more than a minute ago (their job failed or never ran).
#[internal_mutation]
pub async fn prune(ctx: &MutationCtx) -> Result<u32> {
    let cutoff = ctx.now() - 60_000.0;
    let stale = ctx.db.query::<Typing>().with_index(Typing::by_expires, |q| q.lt(Typing::expires_at, cutoff)).take(1000).await?;
    for row in &stale {
        ctx.db.delete(row.id).await?;
    }
    log::info!(pruned = stale.len(), "pruned stale typing rows");
    Ok(stale.len() as u32)
}
