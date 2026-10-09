//! Profiles: display names and avatar colors, linked to `_users`.

use std::collections::HashMap;
use std::sync::Mutex;

use ferrix::QueryDb;
use ferrix::auth::User;
use ferrix::prelude::*;

use crate::model::{Channel, Member, Profile, user_error};

const COLORS: &[&str] = &["#e11d48", "#d97706", "#059669", "#0284c7", "#7c3aed", "#db2777", "#0d9488", "#4f46e5"];

/// The signed-in user's profile, or `null` before `ensure` created it.
#[query]
pub async fn me(ctx: &QueryCtx) -> Result<Option<Doc<Profile>>> {
    let user = ctx.auth.require_user()?;
    profile_of(&ctx.db, user).await
}

/// Creates the signed-in user's profile on first sign-in (named after the account's name or email) and joins them
/// to #general (created if missing). Idempotent: the app calls it after every sign-in.
#[mutation]
pub async fn ensure(ctx: &MutationCtx) -> Result<Id<Profile>> {
    let user = ctx.auth.require_user()?;
    if let Some(profile) = profile_of(&ctx.db, user).await? {
        return Ok(profile.id);
    }
    let account = ctx.auth.user(&ctx.db).await?;
    let name = account
        .as_ref()
        .and_then(|u| u.name.clone().or_else(|| u.email.as_ref().map(|e| e.split('@').next().unwrap_or(e).to_string())))
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "anonymous".to_string());
    let color = COLORS[user.as_str().bytes().map(usize::from).sum::<usize>() % COLORS.len()].to_string();
    let id = ctx.db.insert(Profile { user, name, color }).await?;

    let general = match ctx.db.query::<Channel>().with_index(Channel::by_name, |q| q.eq(Channel::name, "general".to_string())).first().await? {
        Some(channel) => channel.id,
        None => {
            let channel = Channel {
                name: "general".into(),
                topic: "Company-wide chatter".into(),
                private: false,
                created_by: user,
            };
            ctx.db.insert(channel).await?
        }
    };
    ctx.db.insert(Member { channel: general, user, last_read_at: ctx.now() }).await?;
    Ok(id)
}

/// Renames the signed-in user.
#[mutation]
pub async fn rename(ctx: &MutationCtx, name: String) -> Result<()> {
    let user = ctx.auth.require_user()?;
    let name = name.trim().to_string();
    if name.is_empty() || name.chars().count() > 40 {
        return Err(user_error("invalid_name", "A name has 1 to 40 characters"));
    }
    let Some(profile) = profile_of(&ctx.db, user).await? else {
        return Err(user_error("no_profile", "Finish signing in first"));
    };
    ctx.db.patch(profile.id, json!({ "name": name })).await
}

pub async fn profile_of(db: &QueryDb, user: Id<User>) -> Result<Option<Doc<Profile>>> {
    db.query::<Profile>().with_index(Profile::by_user, |q| q.eq(Profile::user, user)).first().await
}

/// Looks up (and caches) display names and colors while building a list. Shared by reference (the lock is never
/// held across an await), so it can be used from `PaginationResult::try_map`.
#[derive(Default)]
pub struct Names(Mutex<HashMap<Id<User>, (String, String)>>);

impl Names {
    pub async fn get(&self, db: &QueryDb, user: Id<User>) -> Result<(String, String)> {
        if let Some(found) = self.0.lock().unwrap_or_else(|e| e.into_inner()).get(&user) {
            return Ok(found.clone());
        }
        let found = match profile_of(db, user).await? {
            Some(p) => (p.value.name, p.value.color),
            None => ("someone".to_string(), "#64748b".to_string()),
        };
        self.0.lock().unwrap_or_else(|e| e.into_inner()).insert(user, found.clone());
        Ok(found)
    }
}
