//! Startup schema check: existing documents must deserialize as the current Rust types.
//!
//! Each table's **schema fingerprint** is a hash of its rendered TS type (`TableDef::ts`, plus the declarations of
//! the named types it refers to) and its index definitions.
//! The fingerprint of the last successful check is stored per table in the raw SQLite table `_schema_fingerprints`.
//! At startup, only tables whose fingerprint changed are re-validated, by running `TableDef::validate` (a
//! deserialization into `Doc<T>`) on every document. If any document fails, startup is refused with an error that
//! lists up to [`MAX_REPORTED`] bad ids per table with the serde error for each; the stored fingerprints are left
//! untouched so the check runs again next time.
//!
//! The exception is a table with pending `#[migration]`s: it opens *migrating* (lenient writes, fingerprint not
//! recorded) and is checked again in the transaction where its last migration finishes (`crate::migrations`).
//!
//! The fingerprint is a proxy for "the type changed": changes that don't show in the TS type (e.g. `u32` → `u8`, or
//! a serde attribute such as `deny_unknown_fields`) don't trigger a re-check. Reads of a document that doesn't
//! match still fail with a clear error at that point.

use std::collections::BTreeMap;

use anyhow::Context as _;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::db::catalog::Catalog;
use crate::id::DocId;
use crate::schema::TableDef;

/// Bad document ids reported per table.
pub const MAX_REPORTED: usize = 20;

const DDL: &str =
    "CREATE TABLE IF NOT EXISTS _schema_fingerprints (table_name TEXT PRIMARY KEY, fingerprint TEXT NOT NULL);";

/// The fingerprint of a table definition: hex blake3 of its TS type, the TS declarations of the named types it
/// uses (recursively, so a change inside a nested struct or enum counts too), and its indexes.
pub fn fingerprint(def: &TableDef) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"ferrix-schema-v1\n");
    match crate::codegen::with_session(def.ts) {
        Ok((ts, decls)) => {
            h.update(ts.as_bytes());
            for d in decls {
                h.update(b"\n");
                h.update(d.decl.as_bytes());
            }
        }
        // Dependency collection failed (e.g. two types with one TS name): the inline type alone still works.
        Err(_) => {
            h.update((def.ts)().as_bytes());
        }
    }
    for idx in &def.indexes {
        h.update(b"\nindex ");
        h.update(idx.name.as_bytes());
        for f in &idx.fields {
            h.update(b" ");
            h.update(f.as_bytes());
        }
    }
    h.finalize().to_hex().to_string()
}

/// The documents of one table that don't match its type.
#[derive(Debug, Clone)]
pub struct BadTable {
    pub name: String,
    pub total: usize,
    pub bad: usize,
    /// Up to [`MAX_REPORTED`] `(id, serde error)` pairs, in id order.
    pub examples: Vec<(String, String)>,
}

/// The declared migrations per table, split into those that still have to run (pending, running, failed or
/// canceled) and those that are done. A table whose documents don't match may only open while it has pending ones.
#[derive(Debug, Clone, Default)]
pub struct Migrations {
    pub pending: BTreeMap<String, Vec<String>>,
    pub done: BTreeMap<String, Vec<String>>,
}

/// Validate the documents of every table in `defs` whose fingerprint changed; store the new fingerprints of the tables
/// whose documents all pass. With `skip`, nothing is validated or stored (the check runs again on the next normal
/// start).
///
/// A table with mismatching documents fails the check unless it has pending migrations (`migrations.pending`): such a
/// table is returned as *migrating* (opened in lenient mode, its fingerprint not recorded, see `crate::migrations`).
/// Any other failing table makes this return an error listing the bad documents.
pub fn run(
    conn: &Connection,
    catalog: &Catalog,
    defs: &[TableDef],
    skip: bool,
    migrations: &Migrations,
) -> anyhow::Result<Vec<String>> {
    conn.execute_batch(DDL)?;
    let mut defs_by_name: BTreeMap<&str, &TableDef> = BTreeMap::new();
    for def in defs {
        defs_by_name.entry(def.name).or_insert(def);
    }
    let mut changed: Vec<(&TableDef, String)> = Vec::new();
    for def in defs_by_name.values() {
        let fp = fingerprint(def);
        let stored: Option<String> = conn
            .query_row("SELECT fingerprint FROM _schema_fingerprints WHERE table_name = ?1", [def.name], |r| r.get(0))
            .optional()?;
        if stored.as_deref() != Some(fp.as_str()) {
            changed.push((def, fp));
        }
    }
    if changed.is_empty() {
        return Ok(Vec::new());
    }
    if skip {
        tracing::warn!(
            tables = ?changed.iter().map(|(d, _)| d.name).collect::<Vec<_>>(),
            "schema check skipped (--skip-schema-check): documents of these changed tables were not validated"
        );
        return Ok(Vec::new());
    }

    let mut bad_tables = Vec::new();
    let mut migrating = Vec::new();
    let mut passed = Vec::new();
    for (def, fp) in &changed {
        let table = catalog.table(def.name)?;
        match check_table(conn, table.number, def)? {
            None => passed.push((def, fp)),
            Some(report) => match migrations.pending.get(def.name) {
                Some(names) if !names.is_empty() => {
                    tracing::warn!(
                        table = def.name,
                        invalid = report.bad,
                        documents = report.total,
                        pending_migrations = ?names,
                        "documents don't match the table's type, but migrations for it are pending: writes to the \
                         table are checked leniently until they have run (`ferrix migrate`, `serve --migrate` or \
                         the dashboard)"
                    );
                    migrating.push(def.name.to_string());
                }
                _ => bad_tables.push(report),
            },
        }
    }
    if !bad_tables.is_empty() {
        anyhow::bail!(format_report(&bad_tables, migrations));
    }
    for (def, fp) in passed {
        record_fingerprint_hex(conn, def.name, fp)?;
    }
    Ok(migrating)
}

/// Validate every document of one table against `def`. `None` when they all match.
pub fn check_table(conn: &Connection, table_number: u32, def: &TableDef) -> anyhow::Result<Option<BadTable>> {
    let start = std::time::Instant::now();
    let mut report = BadTable { name: def.name.to_string(), total: 0, bad: 0, examples: Vec::new() };
    for_each_document(conn, table_number, |id, json| {
        report.total += 1;
        let error = match serde_json::from_str::<Value>(json) {
            Ok(v) => (def.validate)(&v).err(),
            Err(e) => Some(format!("stored JSON is invalid: {e}")),
        };
        if let Some(error) = error {
            report.bad += 1;
            if report.examples.len() < MAX_REPORTED {
                report.examples.push((DocId(id).encode(), error));
            }
        }
        Ok(())
    })?;
    if report.total > 0 {
        tracing::info!(
            table = def.name,
            documents = report.total,
            invalid = report.bad,
            elapsed_ms = start.elapsed().as_millis() as u64,
            "validated the documents of a table against its type"
        );
    }
    Ok((report.bad > 0).then_some(report))
}

/// Record that the documents of `def`'s table match its current type (so the next start doesn't re-validate them).
pub fn record_fingerprint(conn: &Connection, def: &TableDef) -> anyhow::Result<()> {
    conn.execute_batch(DDL)?;
    record_fingerprint_hex(conn, def.name, &fingerprint(def))
}

fn record_fingerprint_hex(conn: &Connection, table: &str, fp: &str) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO _schema_fingerprints (table_name, fingerprint) VALUES (?1, ?2)
         ON CONFLICT(table_name) DO UPDATE SET fingerprint = excluded.fingerprint",
        params![table, fp],
    )?;
    Ok(())
}

/// One table's part of the report: the counts and up to [`MAX_REPORTED`] bad ids with their errors.
pub fn format_table(t: &BadTable) -> String {
    let mut s = format!("table `{}`: {} of {} document(s) do not match", t.name, t.bad, t.total);
    if t.bad > t.examples.len() {
        s.push_str(&format!(" (showing the first {})", t.examples.len()));
    }
    s.push_str(":\n");
    for (id, error) in &t.examples {
        s.push_str(&format!("  {id}: {error}\n"));
    }
    s
}

fn format_report(tables: &[BadTable], migrations: &Migrations) -> String {
    let mut s = String::from(
        "schema check failed: existing documents do not match the current Rust types of their tables.\n",
    );
    for t in tables {
        s.push('\n');
        s.push_str(&format_table(t));
        if let Some(done) = migrations.done.get(&t.name) {
            s.push_str(&format!(
                "  (its migrations {} have all run and left these documents invalid: write another one)\n",
                done.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    let table = tables.first().map(|t| t.name.as_str()).unwrap_or("todos");
    let fn_name: String =
        table.trim_start_matches('_').chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    s.push_str(&format!(
        "\nTo fix this, either:\n  \
         - make the change backward compatible: new fields as `Option<T>` or with `#[serde(default)]`, keep old \
         fields or variants (`#[serde(alias = \"...\")]` helps with renames); or\n  \
         - write a migration that rewrites the documents (it gets the raw JSON, so it works on the old shape):\n\n      \
         #[migration(table = \"{table}\")]\n      \
         async fn migrate_{fn_name}(ctx: &MutationCtx, doc: serde_json::Value) -> Result<Option<serde_json::Value>> {{\n          \
         Ok(doc.get(\"newField\").is_none().then(|| json!({{ \"newField\": 0 }})))   // a patch, or None to keep it\n      \
         }}\n\n    \
         While a migration for the table is pending, the server starts (writes to the table are checked leniently); \
         run it with `ferrix migrate`, `serve --migrate` or from the dashboard (docs/CONTRACT.md §1.9); or\n  \
         - fix or delete the listed documents with the previous version of the app; or\n  \
         - start once with `--skip-schema-check` (reads of mismatching documents will then fail at runtime)."
    ));
    s
}

/// Stream `(id, json)` of every document of a table.
fn for_each_document(
    conn: &Connection,
    table_number: u32,
    mut f: impl FnMut([u8; 16], &str) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let start = table_number.to_be_bytes().to_vec();
    let end = crate::keys::prefix_successor(&start).unwrap_or_else(|| vec![0xFF; 17]);
    let mut stmt = conn.prepare("SELECT id, json FROM documents WHERE id >= ?1 AND id < ?2 ORDER BY id")?;
    let mut rows = stmt.query(params![start, end])?;
    while let Some(row) = rows.next()? {
        let id: Vec<u8> = row.get(0)?;
        let id: [u8; 16] = id.try_into().ok().context("corrupt document id")?;
        let json: String = row.get(1)?;
        f(id, &json)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::IndexDef;

    mod v1 {
        #[derive(ts_rs::TS)]
        #[ts(rename = "P")]
        #[allow(dead_code)]
        pub enum Priority {
            Low,
            High,
        }
        #[derive(ts_rs::TS)]
        #[allow(dead_code)]
        pub struct Row {
            pub p: Priority,
        }
    }

    mod v2 {
        #[derive(ts_rs::TS)]
        #[ts(rename = "P")]
        #[allow(dead_code)]
        pub enum Priority {
            Low,
            High,
            Urgent,
        }
        #[derive(ts_rs::TS)]
        #[allow(dead_code)]
        pub struct Row {
            pub p: Priority,
        }
    }

    fn def(ts: fn() -> String) -> TableDef {
        TableDef { name: "rows", indexes: vec![], search_indexes: vec![], vector_indexes: vec![], ts, validate: |_| Ok(()) }
    }

    #[test]
    fn fingerprint_covers_nested_types_and_indexes() {
        let a = def(crate::private::ts_inline::<v1::Row>);
        let b = def(crate::private::ts_inline::<v2::Row>);
        assert_eq!((a.ts)(), (b.ts)(), "same inline type");
        assert_ne!(fingerprint(&a), fingerprint(&b), "a variant added to a nested enum is a schema change");
        assert_eq!(fingerprint(&a), fingerprint(&a.clone()), "deterministic");
        let indexed = TableDef { indexes: vec![IndexDef { name: "by_p", fields: vec!["p"] }], ..a.clone() };
        assert_ne!(fingerprint(&a), fingerprint(&indexed));
    }
}
