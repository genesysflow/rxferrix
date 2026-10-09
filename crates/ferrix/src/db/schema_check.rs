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

struct BadTable {
    name: String,
    total: usize,
    bad: usize,
    examples: Vec<(String, String)>,
}

/// Validate the documents of every table in `defs` whose fingerprint changed; store the new fingerprints if all
/// documents pass. With `skip`, nothing is validated or stored (the check runs again on the next normal start).
pub fn run(conn: &Connection, catalog: &Catalog, defs: &[TableDef], skip: bool) -> anyhow::Result<()> {
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
        return Ok(());
    }
    if skip {
        tracing::warn!(
            tables = ?changed.iter().map(|(d, _)| d.name).collect::<Vec<_>>(),
            "schema check skipped (--skip-schema-check): documents of these changed tables were not validated"
        );
        return Ok(());
    }

    let mut bad_tables = Vec::new();
    for (def, _) in &changed {
        let table = catalog.table(def.name)?;
        let start = std::time::Instant::now();
        let mut report = BadTable { name: def.name.to_string(), total: 0, bad: 0, examples: Vec::new() };
        for_each_document(conn, table.number, |id, json| {
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
                "schema changed; validated existing documents"
            );
        }
        if report.bad > 0 {
            bad_tables.push(report);
        }
    }
    if !bad_tables.is_empty() {
        anyhow::bail!(format_report(&bad_tables));
    }
    for (def, fp) in &changed {
        conn.execute(
            "INSERT INTO _schema_fingerprints (table_name, fingerprint) VALUES (?1, ?2)
             ON CONFLICT(table_name) DO UPDATE SET fingerprint = excluded.fingerprint",
            params![def.name, fp],
        )?;
    }
    Ok(())
}

fn format_report(tables: &[BadTable]) -> String {
    let mut s = String::from(
        "schema check failed: existing documents do not match the current Rust types of their tables.\n",
    );
    for t in tables {
        s.push_str(&format!("\ntable `{}`: {} of {} document(s) do not match", t.name, t.bad, t.total));
        if t.bad > t.examples.len() {
            s.push_str(&format!(" (showing the first {})", t.examples.len()));
        }
        s.push_str(":\n");
        for (id, error) in &t.examples {
            s.push_str(&format!("  {id}: {error}\n"));
        }
    }
    s.push_str(
        "\nTo fix this, either:\n  \
         - make the change backward compatible: new fields as `Option<T>` or with `#[serde(default)]`, keep old \
         fields or variants (`#[serde(alias = \"...\")]` helps with renames); or\n  \
         - migrate the data: fix or delete the listed documents with the previous version of the app; or\n  \
         - start once with `--skip-schema-check` (reads of mismatching documents will then fail at runtime).",
    );
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
