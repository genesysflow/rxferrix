//! Secret masking and the write policy for system tables.
//!
//! **Masking.** The auth tables store password hashes (argon2id) and keyed hashes of refresh tokens, used refresh
//! tokens, link tokens and numeric codes. The dashboard never returns them: in every `_`-prefixed table, a field whose
//! name ends in `hash` or `encrypted` (case-insensitive; the latter is `_settings`' sealed secrets) is replaced by
//! [`MASK`] in every document the dashboard returns (and in `_audit_log` diffs). Indexes
//! over such a field can't be used to browse a table either (a range filter on a hash index would let an admin
//! binary-search the hash value), so they are reported as unusable and rejected by `documents`.
//!
//! **Writes.** System tables keep invariants the auth code relies on (a session's refresh-token hash chain, one
//! credential per user, codes that are only valid as keyed hashes, ...), so the generic document mutations refuse to
//! write them, with one exception: in `_users`, the profile fields `name`, `image` and `emailVerified` may be edited.
//! Users and sessions are removed with the dedicated `delete_user` and `revoke_session` functions, which clean up
//! everything that belongs to them.

use serde_json::Value;

use crate::UserError;
use crate::db::catalog::{IndexInfo, TableInfo};

/// What a secret value is replaced with.
pub const MASK: &str = "••••";

/// The `_users` fields an admin may edit.
pub const EDITABLE_USER_FIELDS: &[&str] = &["name", "image", "emailVerified"];

/// The user table's name.
pub const USERS: &str = "_users";

pub fn is_system_table(table: &str) -> bool {
    table.starts_with('_')
}

/// Is `field` of `table` a secret that the dashboard never returns? Hashes, and the sealed secrets of `_settings`
/// (`encrypted`: ciphertext, but there is no reason to hand it out).
pub fn is_secret_field(table: &str, field: &str) -> bool {
    let field = field.to_ascii_lowercase();
    is_system_table(table) && (field.ends_with("hash") || field.ends_with("encrypted"))
}

/// The secret fields among the JSON field names of `table`'s indexes and of `doc`, for display.
pub fn secret_fields(table: &TableInfo) -> Vec<String> {
    let mut fields: Vec<String> = table
        .indexes
        .iter()
        .flat_map(|i| i.fields.iter())
        .filter(|f| is_secret_field(&table.name, f))
        .cloned()
        .collect();
    // Fields that aren't indexed are found in the table's TS type (`name: type` pairs).
    if let Some(def) = &table.def {
        for part in strip_comments(&(def.ts)()).split([',', '{', '}', ';', '\n']) {
            if let Some((name, _)) = part.split_once(':') {
                let name = name.trim().trim_end_matches('?').trim_matches('"');
                if is_secret_field(&table.name, name) && !fields.iter().any(|f| f == name) {
                    fields.push(name.to_string());
                }
            }
        }
    }
    fields.sort();
    fields.dedup();
    fields
}

/// `ts` without `/* … */` and `// …` comments (ts-rs renders doc comments into the type).
fn strip_comments(ts: &str) -> String {
    let mut out = String::with_capacity(ts.len());
    let mut rest = ts;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, r)| r);
        } else if let Some(after) = rest.strip_prefix("//") {
            rest = after.split_once('\n').map_or("", |(_, r)| r);
        } else {
            let c = rest.chars().next().expect("not empty");
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// Can the dashboard browse `table` through `index`?
pub fn index_usable(table: &str, index: &IndexInfo) -> bool {
    !index.is_by_id() && !index.fields.iter().any(|f| is_secret_field(table, f))
}

/// Replace the secret fields of a document of `table` with [`MASK`].
pub fn mask(table: &str, mut doc: Value) -> Value {
    if !is_system_table(table) {
        return doc;
    }
    if let Value::Object(fields) = &mut doc {
        for (name, value) in fields.iter_mut() {
            if is_secret_field(table, name) && !value.is_null() {
                *value = Value::String(MASK.into());
            }
        }
    }
    doc
}

/// Refuse generic writes to a system table (see the module docs); `action` describes the attempted write.
pub fn forbid_system_write(table: &str, action: &str) -> crate::Result<()> {
    if !is_system_table(table) {
        return Ok(());
    }
    let hint = match (table, action) {
        (USERS, "delete") => " Use delete_user, which also removes the user's sessions, accounts and password.",
        (USERS, _) => " Only `name`, `image` and `emailVerified` can be edited.",
        ("_sessions", "delete") => " Use revoke_session.",
        _ => "",
    };
    Err(UserError::new("read_only_table", format!("System table `{table}` can't be {action}d from the dashboard.{hint}"))
        .into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn masks_hashes_in_system_tables_only() {
        let doc = json!({ "_id": "x", "passwordHash": "$argon2id$...", "user": "u", "tokenHash": null });
        let masked = mask("_credentials", doc.clone());
        assert_eq!(masked["passwordHash"], MASK);
        assert_eq!(masked["user"], "u");
        assert_eq!(masked["tokenHash"], Value::Null, "null stays null");
        assert_eq!(mask("files", json!({ "contentHash": "abc" }))["contentHash"], "abc", "app tables are untouched");
        for f in ["refreshTokenHash", "codeHash", "tokenHash", "passwordHash"] {
            assert!(is_secret_field("_sessions", f), "{f}");
        }
        assert!(!is_secret_field("_users", "email"));
    }

    /// Every hash-like field of the auth tables is covered by the naming rule.
    #[test]
    fn auth_tables_secrets_are_masked() {
        use crate::Table;
        use crate::auth::tables::*;
        let expect = [
            (Credential::table_def(), vec!["passwordHash"]),
            (Admin::table_def(), vec!["passwordHash"]),
            (Session::table_def(), vec!["refreshTokenHash"]),
            (UsedRefreshToken::table_def(), vec!["tokenHash"]),
            (VerificationCode::table_def(), vec!["codeHash", "tokenHash"]),
            (User::table_def(), vec![]),
            (Account::table_def(), vec![]),
            (crate::settings::model::Setting::table_def(), vec!["encrypted"]),
        ];
        for (def, fields) in expect {
            let info = TableInfo {
                name: def.name.to_string(),
                number: 1,
                indexes: Vec::new(),
                def: Some(def.clone()),
                search_indexes: Vec::new(),
                aggregates: Vec::new(),
            };
            assert_eq!(secret_fields(&info), fields, "{}", def.name);
        }
    }
}
