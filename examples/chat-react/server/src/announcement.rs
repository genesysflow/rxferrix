//! The announcement banner above every channel: the `CHAT_ANNOUNCEMENT` environment variable, set on the dashboard's
//! Environment page (or `chat-server env set CHAT_ANNOUNCEMENT`). Reading it with `ctx.env` makes the query depend on
//! it, so changing or removing it updates every open chat at once.

use rxferrix::prelude::*;

/// The announcement, if one is set (blank counts as none).
#[query]
pub async fn get(ctx: &QueryCtx) -> Result<Option<String>> {
    ctx.auth.require_user()?;
    Ok(ctx.env.get("CHAT_ANNOUNCEMENT").await?.filter(|s| !s.trim().is_empty()))
}
