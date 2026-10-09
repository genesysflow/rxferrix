//! The catalog: table numbers and index ids, reconciled at startup with the tables registered in code.
//!
//! Reconciliation runs in one write transaction before the server accepts traffic:
//! - new tables get the next table number (tables are never deleted automatically);
//! - every table has the built-in `by_id` (no `index_entries` rows; it still has an id so that read sets and write
//!   logs can name it) and `by_creation_time` (`["_creationTime"]`) indexes;
//! - new indexes, and indexes whose fields changed, are rebuilt by a synchronous backfill;
//! - indexes removed from code lose their entries.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use anyhow::{Context, bail};
use rusqlite::{Connection, params};
use serde_json::Value;

use crate::db::search::{self, SearchIndexInfo};
use crate::db::storage;
use crate::keys;
use crate::schema::{BY_CREATION_TIME, BY_ID, CREATION_TIME_FIELD, TableDef};

pub type IndexId = u32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexInfo {
    pub id: IndexId,
    pub name: String,
    /// JSON field names. Empty for `by_id`, whose key is just the 16 id bytes.
    pub fields: Vec<String>,
}

impl IndexInfo {
    pub fn is_by_id(&self) -> bool {
        self.name == BY_ID
    }
}

#[derive(Debug, Clone)]
pub struct TableInfo {
    pub name: String,
    pub number: u32,
    /// All indexes, `by_id` included.
    pub indexes: Vec<IndexInfo>,
    /// The table's definition in code (its TS type and validator). `None` for a table that exists in the database
    /// but is no longer defined in code: its documents can still be read, but untyped writes are refused.
    pub def: Option<TableDef>,
    /// Full-text search and vector indexes (`db::search`).
    pub search_indexes: Vec<SearchIndexInfo>,
}

impl TableInfo {
    pub fn index(&self, name: &str) -> Option<&IndexInfo> {
        self.indexes.iter().find(|i| i.name == name)
    }

    pub fn by_id(&self) -> &IndexInfo {
        self.index(BY_ID).expect("every table has by_id")
    }

    /// Indexes backed by `index_entries` rows (everything but `by_id`).
    pub fn stored_indexes(&self) -> impl Iterator<Item = &IndexInfo> {
        self.indexes.iter().filter(|i| !i.is_by_id())
    }
}

#[derive(Debug, Default)]
pub struct Catalog {
    by_name: HashMap<String, Arc<TableInfo>>,
    by_number: HashMap<u32, Arc<TableInfo>>,
}

impl Catalog {
    pub fn table(&self, name: &str) -> anyhow::Result<&Arc<TableInfo>> {
        self.by_name.get(name).with_context(|| format!("table `{name}` is not registered"))
    }

    pub fn table_by_number(&self, number: u32) -> Option<&Arc<TableInfo>> {
        self.by_number.get(&number)
    }

    pub fn tables(&self) -> impl Iterator<Item = &Arc<TableInfo>> {
        self.by_number.values()
    }

    fn load(conn: &Connection, defs: &[TableDef]) -> anyhow::Result<Catalog> {
        let mut tables: BTreeMap<u32, TableInfo> = BTreeMap::new();
        {
            let mut stmt = conn.prepare("SELECT table_number, name FROM _tables")?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let number: u32 = r.get(0)?;
                tables.insert(
                    number,
                    TableInfo { name: r.get(1)?, number, indexes: Vec::new(), def: None, search_indexes: Vec::new() },
                );
            }
        }
        let mut stmt = conn.prepare("SELECT index_id, table_number, name, fields FROM _indexes ORDER BY index_id")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let table_number: u32 = r.get(1)?;
            let fields: String = r.get(3)?;
            let info = IndexInfo { id: r.get(0)?, name: r.get(2)?, fields: serde_json::from_str(&fields)? };
            if let Some(t) = tables.get_mut(&table_number) {
                t.indexes.push(info);
            }
        }
        let mut search_indexes = search::load(conn)?;
        let mut catalog = Catalog::default();
        for (number, mut t) in tables {
            t.search_indexes = search_indexes.remove(&number).unwrap_or_default();
            t.def = defs.iter().find(|d| d.name == t.name).cloned();
            let t = Arc::new(t);
            catalog.by_name.insert(t.name.clone(), t.clone());
            catalog.by_number.insert(number, t);
        }
        Ok(catalog)
    }
}

/// Validate code-defined tables: unique names (identical duplicates are tolerated), sane index definitions.
fn validate_defs(defs: &[TableDef]) -> anyhow::Result<Vec<TableDef>> {
    let mut seen: BTreeMap<&str, &TableDef> = BTreeMap::new();
    for def in defs {
        if def.name.is_empty() {
            bail!("table name must not be empty");
        }
        if let Some(prev) = seen.get(def.name) {
            if *prev != def {
                bail!("table `{}` is registered twice with different definitions", def.name);
            }
            continue;
        }
        let mut names = std::collections::HashSet::new();
        for idx in &def.indexes {
            if idx.name == BY_ID || idx.name == BY_CREATION_TIME {
                bail!("table `{}`: index name `{}` is reserved", def.name, idx.name);
            }
            if !names.insert(idx.name) {
                bail!("table `{}`: duplicate index `{}`", def.name, idx.name);
            }
            if idx.fields.is_empty() {
                bail!("table `{}`: index `{}` has no fields", def.name, idx.name);
            }
        }
        search::validate_def(def)?;
        seen.insert(def.name, def);
    }
    Ok(seen.into_values().cloned().collect())
}

/// Bring the stored catalog in line with `defs` and return it. Runs in its own write transaction.
pub fn reconcile(conn: &mut Connection, defs: &[TableDef]) -> anyhow::Result<Catalog> {
    let defs = validate_defs(defs)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    search::ensure_schema(&tx)?;
    let existing = Catalog::load(&tx, &defs)?;
    let mut next_table: u32 = tx.query_row("SELECT COALESCE(MAX(table_number), 0) + 1 FROM _tables", [], |r| r.get(0))?;
    let mut next_index: u32 = tx.query_row("SELECT COALESCE(MAX(index_id), 0) + 1 FROM _indexes", [], |r| r.get(0))?;

    for def in &defs {
        let (number, current_indexes) = match existing.by_name.get(def.name) {
            Some(t) => (t.number, t.indexes.clone()),
            None => {
                let n = next_table;
                next_table += 1;
                tx.execute("INSERT INTO _tables (table_number, name) VALUES (?1, ?2)", params![n, def.name])?;
                tracing::info!(table = def.name, number = n, "created table");
                (n, Vec::new())
            }
        };

        let mut desired: Vec<(String, Vec<String>)> = vec![
            (BY_ID.to_string(), vec![]),
            (BY_CREATION_TIME.to_string(), vec![CREATION_TIME_FIELD.to_string()]),
        ];
        desired.extend(def.indexes.iter().map(|i| (i.name.to_string(), i.fields.iter().map(|f| f.to_string()).collect())));

        for (name, fields) in &desired {
            let fields_json = serde_json::to_string(fields)?;
            let rebuild_id = match current_indexes.iter().find(|i| &i.name == name) {
                Some(cur) if &cur.fields == fields => None,
                Some(cur) => {
                    tracing::info!(table = def.name, index = %name, "index fields changed; rebuilding");
                    tx.execute("UPDATE _indexes SET fields = ?1 WHERE index_id = ?2", params![fields_json, cur.id])?;
                    tx.execute("DELETE FROM index_entries WHERE index_id = ?1", [cur.id])?;
                    Some(cur.id)
                }
                None => {
                    let id = next_index;
                    next_index += 1;
                    tx.execute(
                        "INSERT INTO _indexes (index_id, table_number, name, fields) VALUES (?1, ?2, ?3, ?4)",
                        params![id, number, name, fields_json],
                    )?;
                    Some(id)
                }
            };
            if let Some(id) = rebuild_id
                && name != BY_ID
            {
                backfill(&tx, number, id, fields)?;
            }
        }

        for cur in &current_indexes {
            if !desired.iter().any(|(n, _)| n == &cur.name) {
                tracing::info!(table = def.name, index = %cur.name, "index removed; deleting its entries");
                tx.execute("DELETE FROM index_entries WHERE index_id = ?1", [cur.id])?;
                tx.execute("DELETE FROM _indexes WHERE index_id = ?1", [cur.id])?;
            }
        }
        search::reconcile_table(&tx, def, number)?;
    }
    tx.commit()?;
    Catalog::load(conn, &defs)
}

fn backfill(conn: &Connection, table_number: u32, index_id: IndexId, fields: &[String]) -> anyhow::Result<()> {
    let docs = storage::table_documents(conn, table_number)?;
    if !docs.is_empty() {
        tracing::info!(table_number, index_id, docs = docs.len(), "backfilling index");
    }
    for (id, json) in docs {
        let doc: Value = serde_json::from_str(&json)?;
        let obj = doc.as_object().context("stored document is not an object")?;
        let key = keys::index_key(obj, fields, &id);
        storage::insert_index_entry(conn, index_id, &key, &id)?;
    }
    Ok(())
}
