//! The audit log of administrator actions (`_audit_log`) and `_system/audit:list`.
//!
//! Every write an administrator makes through the dashboard's system functions is recorded **in the same
//! transaction** as the write itself, so there is never a write without its entry nor an entry for a write that
//! rolled back: generic document writes (`insert`, `patch`, `replace`, `delete`), `revoke_session`, `delete_user`,
//! settings updates (`settings_update`), impersonation (`impersonate`: a session minted for a user) and the function
//! runner (`run_function`; for a mutation, inside the mutation's own transaction).
//!
//! An entry has the admin's id and email (as they were), the action, the target table and document id, a
//! before/after diff (only the fields that changed for updates; whole documents for inserts and deletes) with
//! secrets masked by the same rule as the data browser (`dashboard::secrets`), free-form `details`, and a timestamp.
//!
//! **Tamper resistance.** `_audit_log` is a system table: the dashboard's generic writes refuse it, and no system
//! function edits or deletes entries; the only deletion is retention (entries older than
//! `AuthConfig::audit_retention`, 90 days by default, swept hourly). Someone with write access to `data.db` itself
//! can still alter it; ship entries elsewhere (e.g. by polling `audit:list`) when that matters. Function runs are
//! also recorded independently in the execution log (`aux.db`).

use std::time::Duration;

use serde_json::{Map, Value};

use crate::auth::tables::Admin;
use crate::db::query::MutationDb;
use crate::prelude::*;
use crate::runtime::App;

/// One administrator action (`_audit_log`).
#[table(name = "_audit_log")]
#[index(by_at = [at])]
#[index(by_admin = [admin, at])]
#[index(by_action = [action, at])]
#[index(by_table = [target_table, at])]
#[index(by_target = [target_id, at])]
pub struct AuditEntry {
    // `Id<Admin>` of who did it (null for actions without an admin, e.g. from the CLI).
    pub admin: Option<String>,
    pub admin_email: Option<String>,
    // insert | patch | replace | delete | revoke_session | delete_user | settings_update | impersonate | run_function
    pub action: String,
    pub target_table: Option<String>,
    pub target_id: Option<String>,
    // The changed fields before and after (whole documents for insert/delete), secrets masked.
    #[ts(type = "any")]
    pub before: Option<Value>,
    #[ts(type = "any")]
    pub after: Option<Value>,
    #[ts(type = "any")]
    pub details: Option<Value>,
    // ms since the epoch (the transaction's time).
    pub at: f64,
}

/// What to record; see [`record`].
#[derive(Debug, Default, Clone)]
pub(crate) struct Record {
    pub action: &'static str,
    pub target_table: Option<String>,
    pub target_id: Option<String>,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub details: Option<Value>,
}

impl Record {
    pub fn new(action: &'static str) -> Self {
        Record { action, ..Default::default() }
    }

    pub fn target(mut self, table: impl Into<String>, id: Option<String>) -> Self {
        self.target_table = Some(table.into());
        self.target_id = id;
        self
    }

    /// Whole documents (secrets masked for system tables).
    pub fn documents(mut self, table: &str, before: Option<Value>, after: Option<Value>) -> Self {
        self.before = before.map(|d| crate::dashboard::secrets::mask(table, d));
        self.after = after.map(|d| crate::dashboard::secrets::mask(table, d));
        self
    }

    /// Only the top-level fields that differ between `before` and `after` (secrets masked).
    pub fn diff(mut self, table: &str, before: &Value, after: &Value) -> Self {
        let (b, a) = diff(before, after);
        self.before = Some(crate::dashboard::secrets::mask(table, b));
        self.after = Some(crate::dashboard::secrets::mask(table, a));
        self
    }

    pub fn details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

/// The fields of two JSON objects that differ: `(before, after)` objects with the same keys.
pub(crate) fn diff(before: &Value, after: &Value) -> (Value, Value) {
    let empty = Map::new();
    let b = before.as_object().unwrap_or(&empty);
    let a = after.as_object().unwrap_or(&empty);
    let (mut out_b, mut out_a) = (Map::new(), Map::new());
    for key in b.keys().chain(a.keys()) {
        if out_b.contains_key(key) {
            continue;
        }
        let (vb, va) = (b.get(key).cloned().unwrap_or(Value::Null), a.get(key).cloned().unwrap_or(Value::Null));
        if vb != va {
            out_b.insert(key.clone(), vb);
            out_a.insert(key.clone(), va);
        }
    }
    (Value::Object(out_b), Value::Object(out_a))
}

/// Append an entry in `db`'s transaction (so it commits or rolls back with the write it describes).
pub(crate) async fn record(db: &MutationDb, admin: Option<Id<Admin>>, rec: Record) -> Result<()> {
    let admin_email = match admin {
        Some(a) => db.get(a).await?.map(|a| a.value.email),
        None => None,
    };
    db.insert(AuditEntry {
        admin: admin.map(|a| a.to_string()),
        admin_email,
        action: rec.action.to_string(),
        target_table: rec.target_table,
        target_id: rec.target_id,
        before: rec.before,
        after: rec.after,
        details: rec.details,
        at: db.txn().now(),
    })
    .await?;
    Ok(())
}

/// Audit entries, newest first, live. Filters: `admin` (an admin id), `action`, `table` (target table), `target` (a
/// document id), and a time range `since` / `until` (ms, inclusive). The most selective filter picks the index; the
/// others are applied while scanning.
#[query]
#[allow(clippy::too_many_arguments)]
pub async fn list(
    ctx: &QueryCtx,
    admin: Option<String>,
    action: Option<String>,
    table: Option<String>,
    target: Option<String>,
    since: Option<f64>,
    until: Option<f64>,
    pagination_opts: PaginationOpts,
) -> Result<PaginationResult<Doc<AuditEntry>>> {
    ctx.auth.require_admin()?;
    let (since, until) = (since.unwrap_or(f64::MIN), until.unwrap_or(f64::MAX));
    let q = ctx.db.query::<AuditEntry>();
    let q = if let Some(t) = target.clone() {
        q.with_index(AuditEntry::by_target, |r| r.eq(AuditEntry::target_id, Some(t)).gte(AuditEntry::at, since).lte(AuditEntry::at, until))
    } else if let Some(a) = admin.clone() {
        q.with_index(AuditEntry::by_admin, |r| r.eq(AuditEntry::admin, Some(a)).gte(AuditEntry::at, since).lte(AuditEntry::at, until))
    } else if let Some(a) = action.clone() {
        q.with_index(AuditEntry::by_action, |r| r.eq(AuditEntry::action, a).gte(AuditEntry::at, since).lte(AuditEntry::at, until))
    } else if let Some(t) = table.clone() {
        q.with_index(AuditEntry::by_table, |r| r.eq(AuditEntry::target_table, Some(t)).gte(AuditEntry::at, since).lte(AuditEntry::at, until))
    } else {
        q.with_index(AuditEntry::by_at, |r| r.gte(AuditEntry::at, since).lte(AuditEntry::at, until))
    };
    q.filter(move |e| {
        admin.as_ref().is_none_or(|a| e.admin.as_ref() == Some(a))
            && action.as_ref().is_none_or(|a| &e.action == a)
            && table.as_ref().is_none_or(|t| e.target_table.as_ref() == Some(t))
    })
    .order(Order::Desc)
    .paginate(pagination_opts)
    .await
}

/// Delete entries older than the retention (`AuthConfig::audit_retention`), in batches. Returns how many.
pub async fn sweep(app: &App) -> Result<usize> {
    let retention = app.auth().config().audit_retention;
    let cutoff = app.db().clock().now_ms() - retention.as_secs_f64() * 1000.0;
    let mut total = 0;
    loop {
        let n = crate::auth::store::write(app, |db| async move {
            let old = db
                .query::<AuditEntry>()
                .with_index(AuditEntry::by_at, |r| r.lt(AuditEntry::at, cutoff))
                .take(500)
                .await?;
            for e in &old {
                db.delete(e.id).await?;
            }
            Ok(old.len())
        })
        .await?;
        total += n;
        if n < 500 {
            return Ok(total);
        }
    }
}

/// Run [`sweep`] now and then every hour, while the app is alive.
pub(crate) fn spawn_retention(app: &App) {
    let weak = app.downgrade();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            let Some(app) = weak.upgrade() else { return };
            match sweep(&app).await {
                Ok(0) => {}
                Ok(n) => tracing::info!("audit: deleted {n} entries past retention"),
                Err(e) => tracing::warn!("audit: retention sweep failed: {e:#}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn diff_keeps_changed_fields_only() {
        let (b, a) = diff(&json!({ "a": 1, "b": 2, "c": 3 }), &json!({ "a": 1, "b": 5, "d": 4 }));
        assert_eq!(b, json!({ "b": 2, "c": 3, "d": null }));
        assert_eq!(a, json!({ "b": 5, "c": null, "d": 4 }));
    }
}
