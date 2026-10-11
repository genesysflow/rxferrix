//! Environment variables: named values (API keys, feature switches, URLs) set from the dashboard or the command line
//! and read by functions with `ctx.env`, without a restart or a redeploy.
//!
//! **Reactive.** They live in the `_env` system table, and `ctx.env.get` reads them through the context's database
//! handle, so a query that reads a variable depends on it like on any document: when the value changes (or the
//! variable is set or removed), every subscription to such a query reruns and clients get the new result. Mutations
//! read them in their transaction (a concurrent change conflicts and retries, like any read); actions read the latest
//! value. `std::env::var` is none of this: it is fixed at startup and invisible to the dependency tracking.
//!
//! **Encrypted at rest.** Values are sealed with the server's settings key (`crate::auth::secretbox`, bound to the
//! variable's name), so the database, its backups and exports hold ciphertext only; the dashboard's data browser masks
//! the `valueEncrypted` field. Admins see the values on the Environment page (`list`). The execution log never
//! records them: `set`'s `value` argument is masked (`crate::settings::redact_args`), and the audit log records
//! which variable changed, not to what.
//!
//! **Limits.** Names are 1 to 64 characters, letters, digits and `_`, not starting with a digit (`OPENAI_API_KEY`);
//! values are at most [`MAX_VALUE_BYTES`]; at most [`MAX_VARIABLES`] variables.
//!
//! | Path `_system/env:…` | Kind | |
//! |---|---|---|
//! | `list` | query (admin) | every variable, with its value, by name (live) |
//! | `set` | mutation (admin) | `{ name, value }`: create or change a variable |
//! | `remove` | mutation (admin) | `{ name }` |
//! | `put` | internal mutation | `{ name, value: string \| null }`: what `ferrix env set/unset` runs |
//!
//! From the command line (offline, like `import`): `<server> env list`, `env set NAME [VALUE]` (the value is read
//! from stdin when omitted, so it stays out of the shell history), `env unset NAME`.

use std::sync::Arc;

use anyhow::anyhow;
use serde::{Deserialize, Serialize};

use crate::audit::{self, Record};
use crate::auth::secretbox::SecretBox;
use crate::db::query::{QueryDb, Source};
use crate::prelude::*;
use crate::runtime::App;

/// The table's name.
pub const TABLE: &str = "_env";
/// Longest value, in bytes.
pub const MAX_VALUE_BYTES: usize = 8 * 1024;
/// Most variables.
pub const MAX_VARIABLES: usize = 200;

/// An environment variable (`_env`). The value is sealed (`v1.…`) with the variable's name as associated data.
#[table(name = "_env")]
#[index(by_name = [name])]
pub struct EnvVar {
    pub name: String,
    pub value_encrypted: String,
    pub updated_at: f64,
    // `Id<Admin>` of the last editor (none from the command line).
    pub updated_by: Option<String>,
}

/// `ctx.env`: read environment variables (see the module docs).
pub struct Env {
    source: EnvSource,
    sbox: Arc<SecretBox>,
}

enum EnvSource {
    /// A query's snapshot or a mutation's transaction: the read is tracked.
    Db(QueryDb),
    /// An action: each read is at the latest commit.
    App(App),
}

impl Env {
    pub(crate) fn in_db(db: QueryDb, sbox: Arc<SecretBox>) -> Env {
        Env { source: EnvSource::Db(db), sbox }
    }

    pub(crate) fn in_action(app: App) -> Env {
        let sbox = Arc::new(app.auth().keys.secret_box());
        Env { source: EnvSource::App(app), sbox }
    }

    /// The key that seals values (for contexts built from this one).
    pub(crate) fn secret_box(&self) -> Arc<SecretBox> {
        self.sbox.clone()
    }

    /// The variable's value, or `None` if it isn't set. In a query, the result depends on it: a change reruns the
    /// query.
    pub async fn get(&self, name: &str) -> Result<Option<String>> {
        match &self.source {
            EnvSource::Db(db) => read(db, &self.sbox, name).await,
            EnvSource::App(app) => {
                let db = QueryDb::new(Source::Snapshot(app.db().snapshot().await?), app.db().catalog().clone());
                read(&db, &self.sbox, name).await
            }
        }
    }

    /// The variable's value; a server error naming it if it isn't set.
    pub async fn require(&self, name: &str) -> Result<String> {
        self.get(name).await?.ok_or_else(|| {
            anyhow!("environment variable `{name}` is not set (set it on the dashboard's Environment page or with `env set`)")
        })
    }
}

async fn row(db: &QueryDb, name: &str) -> Result<Option<Doc<EnvVar>>> {
    db.query::<EnvVar>().with_index(EnvVar::by_name, |q| q.eq(EnvVar::name, name.to_string())).first().await
}

async fn read(db: &QueryDb, sbox: &SecretBox, name: &str) -> Result<Option<String>> {
    match row(db, name).await? {
        Some(doc) => Ok(Some(open(sbox, name, &doc.value.value_encrypted)?)),
        None => Ok(None),
    }
}

/// What a value is bound to: a sealed value copied into another variable's row fails to open.
fn aad(name: &str) -> String {
    format!("env:{name}")
}

fn seal(sbox: &SecretBox, name: &str, value: &str) -> String {
    sbox.seal(&aad(name), value.as_bytes())
}

fn open(sbox: &SecretBox, name: &str, sealed: &str) -> Result<String> {
    Ok(String::from_utf8(sbox.open(&aad(name), sealed)?)?)
}

/// A valid variable name, or the user error `invalid_name`.
pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err(UserError::new(
            "invalid_name",
            "A name is 1 to 64 letters, digits and underscores, not starting with a digit (e.g. OPENAI_API_KEY)",
        )
        .into())
    }
}

fn validate_value(value: &str) -> Result<()> {
    if value.len() > MAX_VALUE_BYTES {
        return Err(UserError::new("invalid_value", format!("A value is at most {} KB", MAX_VALUE_BYTES / 1024)).into());
    }
    Ok(())
}

/// Set (`Some`) or remove (`None`) a variable in `ctx`'s transaction, with an audit entry. Returns whether anything
/// changed.
async fn put_var(
    ctx: &MutationCtx,
    admin: Option<Id<crate::auth::tables::Admin>>,
    name: &str,
    value: Option<&str>,
) -> Result<bool> {
    validate_name(name)?;
    let sbox = ctx.env.secret_box();
    let existing = row(&ctx.db, name).await?;
    let action = match (&existing, value) {
        (_, Some(value)) => {
            validate_value(value)?;
            if let Some(doc) = &existing
                && open(&sbox, name, &doc.value.value_encrypted).ok().as_deref() == Some(value)
            {
                return Ok(false);
            }
            if existing.is_none() && ctx.db.query::<EnvVar>().count().await? as usize >= MAX_VARIABLES {
                return Err(UserError::new("too_many_variables", format!("At most {MAX_VARIABLES} variables")).into());
            }
            let var = EnvVar {
                name: name.to_string(),
                value_encrypted: seal(&sbox, name, value),
                updated_at: ctx.now(),
                updated_by: admin.map(|a| a.to_string()),
            };
            match &existing {
                Some(doc) => {
                    ctx.db.replace(doc.id, var).await?;
                    "env_update"
                }
                None => {
                    ctx.db.insert(var).await?;
                    "env_create"
                }
            }
        }
        (Some(doc), None) => {
            ctx.db.delete(doc.id).await?;
            "env_delete"
        }
        (None, None) => return Ok(false),
    };
    let rec = Record::new(action).target(TABLE, existing.map(|d| d.id.to_string())).details(json!({ "name": name }));
    audit::record(&ctx.db, admin, rec).await?;
    Ok(true)
}

/// An environment variable as the dashboard shows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct EnvVarView {
    pub name: String,
    pub value: String,
    pub updated_at: f64,
    pub updated_by: Option<String>,
}

/// Every variable with its value, by name. Live: the dashboard's Environment page.
#[query]
pub async fn list(ctx: &QueryCtx) -> Result<Vec<EnvVarView>> {
    ctx.auth.require_admin()?;
    let sbox = ctx.env.secret_box();
    let mut rows = ctx.db.query::<EnvVar>().collect().await?;
    rows.sort_by(|a, b| a.value.name.cmp(&b.value.name));
    rows.into_iter()
        .map(|doc| {
            let v = doc.value;
            Ok(EnvVarView {
                value: open(&sbox, &v.name, &v.value_encrypted)?,
                name: v.name,
                updated_at: v.updated_at,
                updated_by: v.updated_by,
            })
        })
        .collect()
}

/// Create or change a variable. Queries that read it rerun.
#[mutation]
pub async fn set(ctx: &MutationCtx, name: String, value: String) -> Result<()> {
    let admin = ctx.auth.require_admin()?;
    put_var(ctx, Some(admin), &name, Some(&value)).await?;
    Ok(())
}

/// Remove a variable (a no-op if it isn't set). Queries that read it rerun.
#[mutation]
pub async fn remove(ctx: &MutationCtx, name: String) -> Result<()> {
    let admin = ctx.auth.require_admin()?;
    put_var(ctx, Some(admin), &name, None).await?;
    Ok(())
}

/// Set or remove a variable without an admin (the command line).
#[internal_mutation]
pub async fn put(ctx: &MutationCtx, name: String, value: Option<String>) -> Result<bool> {
    put_var(ctx, None, &name, value.as_deref()).await
}

/// The variables' names and values, for `env list` (by name).
pub(crate) async fn list_all(app: &App) -> Result<Vec<(String, String)>> {
    let sbox = app.env_box();
    let db = QueryDb::new(Source::Snapshot(app.db().snapshot().await?), app.db().catalog().clone());
    let mut rows = db.query::<EnvVar>().collect().await?;
    rows.sort_by(|a, b| a.value.name.cmp(&b.value.name));
    rows.into_iter()
        .map(|doc| Ok((doc.value.name.clone(), open(&sbox, &doc.value.name, &doc.value.value_encrypted)?)))
        .collect()
}
