//! Export and import of documents.
//!
//! **Export** ([`export_to_dir`], `rxferrix export`) reads every table in one SQLite read transaction (a consistent
//! snapshot; it works while the server runs and never blocks writes) into a directory:
//!
//! | File | Content |
//! |---|---|
//! | `<table>.jsonl` | one document per line, exactly as stored (with `_id` and `_creationTime`) |
//! | `<table>.csv` | with `--format csv`: a header (`_id`, `_creationTime`, then every other top-level field, sorted) and one row per document; strings as is, `null` and missing fields empty, other values as JSON |
//! | `files/<storage id>` | the blobs of exported `_storage` documents (unless `--no-files`) |
//! | `manifest.json` | `{ format: "rxferrix-export", version, documentFormat, exportedAt, ts, rxferrixVersion, tables: [{ name, number, documents, file }], catalog: { <table>: <number> }, files, missingFiles }` |
//!
//! By default every app table is exported (not the `_`-prefixed system tables, which `--tables` can name).
//!
//! **Import** ([`import_path`], `rxferrix import`, `POST /api/admin/import/{table}`) reads JSONL, or CSV as exported
//! (`--format csv`, `format=csv`, a `.csv` file or a CSV export; cells are typed with the table's shape, see
//! `super::csv`). Documents go
//! through the normal write path in batches of [`ImportOptions::batch_size`] per transaction, so they are validated
//! against the table's Rust type, every index and search index is maintained, and subscriptions see each batch
//! commit. Ids and creation times are kept (a line without `_id` gets a new one). Before writing anything, every line
//! is parsed and validated (and, in `insert` mode, checked not to exist yet), so a bad file fails without partial
//! writes; a crash halfway leaves the batches committed so far. Modes:
//!
//! - `insert` (default): fail if a document with the same id exists;
//! - `upsert`: insert, or replace an existing document with the same id;
//! - `replace`: delete every document of the table first (in batches), then insert.
//!
//! Ids carry their table's number. When the export's `manifest.json` shows a table numbered differently in the target
//! database (tables are numbered in the order they were created), ids of that table are rewritten wherever they
//! appear in imported documents (any string that is a valid id of a table in the manifest's catalog), so references
//! between imported documents stay intact. `_storage` documents are imported with their blobs from `files/`.

use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::Table as _;
use crate::db::catalog::TableInfo;
use crate::id::DocId;
use crate::runtime::App;
use crate::schema::{CREATION_TIME_FIELD, ID_FIELD};

/// `manifest.json`'s `format`.
pub const EXPORT_FORMAT: &str = "rxferrix-export";
pub const EXPORT_VERSION: u32 = 1;
/// The default number of documents per import transaction.
pub const DEFAULT_BATCH_SIZE: usize = 500;

/// The file format of exported tables.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    #[default]
    Jsonl,
    Csv,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Jsonl => "jsonl",
            ExportFormat::Csv => "csv",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            ExportFormat::Jsonl => "application/x-ndjson; charset=utf-8",
            ExportFormat::Csv => "text/csv; charset=utf-8",
        }
    }
}

/// How [`import_path`] treats documents that exist already.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ImportMode {
    /// Fail if a document with the same id exists.
    #[default]
    Insert,
    /// Delete all documents of the table first.
    Replace,
    /// Replace documents with the same id, insert the others.
    Upsert,
}

// ---------------------------------------------------------------------------------------------------------------------
// Reading tables
// ---------------------------------------------------------------------------------------------------------------------

/// A table as stored: its name and number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredTable {
    pub name: String,
    pub number: u32,
}

/// Every table in the database, by name.
pub(crate) fn stored_tables(conn: &Connection) -> crate::Result<Vec<StoredTable>> {
    let mut stmt = conn.prepare("SELECT name, table_number FROM _tables ORDER BY name")?;
    let rows = stmt.query_map([], |r| Ok(StoredTable { name: r.get(0)?, number: r.get(1)? }))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Call `f` with every document of a table (stored JSON), in id order.
pub(crate) fn for_each_document(
    conn: &Connection,
    number: u32,
    mut f: impl FnMut(&[u8], &str) -> crate::Result<()>,
) -> crate::Result<u64> {
    let start = number.to_be_bytes().to_vec();
    let end = crate::keys::prefix_successor(&start).unwrap_or_else(|| vec![0xff; 17]);
    let mut stmt = conn.prepare("SELECT id, json FROM documents WHERE id >= ?1 AND id < ?2 ORDER BY id")?;
    let mut rows = stmt.query(params![start, end])?;
    let mut n = 0;
    while let Some(row) = rows.next()? {
        let id: Vec<u8> = row.get(0)?;
        let json: String = row.get(1)?;
        f(&id, &json)?;
        n += 1;
    }
    Ok(n)
}

/// A read-only view of `<dir>/data.db` in one read transaction (a consistent snapshot).
pub(crate) struct ReadTxn {
    conn: Connection,
}

impl ReadTxn {
    pub(crate) fn open(data_dir: &Path) -> crate::Result<ReadTxn> {
        let path = data_dir.join("data.db");
        anyhow::ensure!(path.exists(), "{} does not exist", path.display());
        crate::db::search::vector::register_extension();
        let conn = crate::db::storage::open_reader(&path)?;
        conn.execute_batch("BEGIN")?;
        // The first read pins the snapshot.
        crate::db::storage::read_meta_u64(&conn, crate::db::storage::META_TS)?;
        Ok(ReadTxn { conn })
    }

    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    pub(crate) fn ts(&self) -> crate::Result<u64> {
        crate::db::storage::read_meta_u64(&self.conn, crate::db::storage::META_TS)
    }
}

impl Drop for ReadTxn {
    fn drop(&mut self) {
        let _ = self.conn.execute_batch("COMMIT");
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------------------------------------------------

/// One CSV field (RFC 4180): quoted when it contains a comma, a quote, CR or LF, or starts or ends with whitespace;
/// quotes are doubled.
pub fn csv_escape(field: &str) -> std::borrow::Cow<'_, str> {
    let needs_quotes = field.contains([',', '"', '\n', '\r'])
        || field.starts_with(char::is_whitespace)
        || field.ends_with(char::is_whitespace);
    if needs_quotes { format!("\"{}\"", field.replace('"', "\"\"")).into() } else { field.into() }
}

/// The CSV text of a JSON value: strings as is, `null` (and missing) empty, anything else as JSON.
pub fn csv_value(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
    }
}

/// Write one CSV record (fields joined by commas, terminated by CRLF).
pub fn write_csv_record<W: Write>(out: &mut W, fields: impl IntoIterator<Item = String>) -> std::io::Result<()> {
    let mut first = true;
    for field in fields {
        if !first {
            out.write_all(b",")?;
        }
        first = false;
        out.write_all(csv_escape(&field).as_bytes())?;
    }
    out.write_all(b"\r\n")
}

/// Write a table: JSONL (as stored) or CSV (two passes over the same read transaction: the columns, then the rows).
/// `transform` may rewrite each document (e.g. mask secrets). Returns the number of documents.
pub(crate) fn write_table<W: Write>(
    conn: &Connection,
    number: u32,
    format: ExportFormat,
    transform: Option<&dyn Fn(Value) -> Value>,
    out: &mut W,
) -> crate::Result<u64> {
    let parse = |json: &str| -> crate::Result<Value> {
        let v: Value = serde_json::from_str(json)?;
        Ok(match transform {
            Some(t) => t(v),
            None => v,
        })
    };
    match format {
        ExportFormat::Jsonl => for_each_document(conn, number, |_, json| {
            match transform {
                Some(_) => serde_json::to_writer(&mut *out, &parse(json)?)?,
                None => out.write_all(json.as_bytes())?,
            }
            out.write_all(b"\n")?;
            Ok(())
        }),
        ExportFormat::Csv => {
            let mut keys = BTreeSet::new();
            for_each_document(conn, number, |_, json| {
                if let Value::Object(m) = parse(json)? {
                    keys.extend(m.into_iter().map(|(k, _)| k).filter(|k| k != ID_FIELD && k != CREATION_TIME_FIELD));
                }
                Ok(())
            })?;
            let columns: Vec<String> =
                [ID_FIELD.to_string(), CREATION_TIME_FIELD.to_string()].into_iter().chain(keys).collect();
            write_csv_record(out, columns.iter().cloned())?;
            for_each_document(conn, number, |_, json| {
                let doc = parse(json)?;
                write_csv_record(out, columns.iter().map(|c| csv_value(doc.get(c))))?;
                Ok(())
            })
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Export to a directory
// ---------------------------------------------------------------------------------------------------------------------

/// Options of [`export_to_dir`].
#[derive(Debug, Clone, Default)]
pub struct ExportOptions {
    /// Tables to export (system tables allowed). `None`: every app table.
    pub tables: Option<Vec<String>>,
    pub format: ExportFormat,
    /// Copy the blobs of exported `_storage` documents into `files/`.
    pub include_files: bool,
}

/// Where blobs are read from and written to: an object store and the key prefix blobs live under.
#[derive(Clone)]
pub struct BlobStore {
    pub store: Arc<dyn ObjectStore>,
    pub prefix: String,
}

impl BlobStore {
    pub fn of_app(app: &App) -> BlobStore {
        let probe = app.storage().blob_path("x");
        let prefix = probe.as_ref().strip_suffix("x").unwrap_or("").trim_end_matches('/').to_string();
        BlobStore { store: app.storage().object_store().clone(), prefix }
    }

    pub fn from_config(config: &crate::storage::StorageConfig, data_dir: &Path) -> crate::Result<BlobStore> {
        let (store, prefix, _) = config.build_store(data_dir)?;
        Ok(BlobStore { store, prefix })
    }

    fn path(&self, id: &str) -> ObjectPath {
        if self.prefix.is_empty() { ObjectPath::from(id) } else { ObjectPath::from(format!("{}/{id}", self.prefix)) }
    }
}

/// `manifest.json` of an export.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportManifest {
    pub format: String,
    pub version: u32,
    pub document_format: ExportFormat,
    pub exported_at: f64,
    pub ts: u64,
    pub rxferrix_version: String,
    pub tables: Vec<ExportedTable>,
    /// Every table of the source database with its number (to rewrite ids on import when numbers differ).
    pub catalog: std::collections::BTreeMap<String, u32>,
    /// Blobs written to `files/`.
    pub files: u64,
    /// `_storage` documents whose blob could not be read.
    pub missing_files: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportedTable {
    pub name: String,
    pub number: u32,
    pub documents: u64,
    pub file: String,
}

/// Export tables of the database in `data_dir` into the directory `out` (created; existing files with the same names
/// are overwritten). See the module docs.
pub async fn export_to_dir(
    data_dir: &Path,
    blobs: Option<BlobStore>,
    out: &Path,
    options: &ExportOptions,
) -> crate::Result<ExportManifest> {
    std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let (dir, out2, opts) = (data_dir.to_path_buf(), out.to_path_buf(), options.clone());
    let (mut manifest, storage_ids) = tokio::task::spawn_blocking(move || -> crate::Result<_> {
        let txn = ReadTxn::open(&dir)?;
        let all = stored_tables(txn.conn())?;
        let selected: Vec<StoredTable> = match &opts.tables {
            None => all.iter().filter(|t| !t.name.starts_with('_')).cloned().collect(),
            Some(names) => {
                let mut v = Vec::new();
                for name in names {
                    let t =
                        all.iter().find(|t| &t.name == name).with_context(|| format!("there is no table `{name}`"))?;
                    if !v.contains(t) {
                        v.push(t.clone());
                    }
                }
                v
            }
        };
        let mut tables = Vec::new();
        let mut storage_ids = Vec::new();
        for t in &selected {
            let file = format!("{}.{}", t.name, opts.format.extension());
            let mut w = BufWriter::new(std::fs::File::create(out2.join(&file))?);
            let documents = write_table(txn.conn(), t.number, opts.format, None, &mut w)?;
            w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
            if t.name == crate::storage::StoredFile::NAME && opts.include_files {
                for_each_document(txn.conn(), t.number, |id, _| {
                    let id: [u8; 16] = id.try_into().map_err(|_| anyhow::anyhow!("corrupt document id"))?;
                    storage_ids.push(DocId(id).encode());
                    Ok(())
                })?;
            }
            tables.push(ExportedTable { name: t.name.clone(), number: t.number, documents, file });
        }
        let manifest = ExportManifest {
            format: EXPORT_FORMAT.into(),
            version: EXPORT_VERSION,
            document_format: opts.format,
            exported_at: crate::db::wall_clock_ms(),
            ts: txn.ts()?,
            rxferrix_version: env!("CARGO_PKG_VERSION").into(),
            tables,
            catalog: all.into_iter().map(|t| (t.name, t.number)).collect(),
            files: 0,
            missing_files: 0,
        };
        Ok((manifest, storage_ids))
    })
    .await??;
    if !storage_ids.is_empty() {
        let blobs = blobs.context("no file storage to read the blobs of `_storage` from")?;
        let files_dir = out.join("files");
        std::fs::create_dir_all(&files_dir)?;
        for id in storage_ids {
            match blobs.store.get(&blobs.path(&id)).await {
                Ok(r) => {
                    let bytes = r.bytes().await.with_context(|| format!("reading the blob of {id}"))?;
                    tokio::fs::write(files_dir.join(&id), &bytes).await?;
                    manifest.files += 1;
                }
                Err(object_store::Error::NotFound { .. }) => manifest.missing_files += 1,
                Err(e) => return Err(e).with_context(|| format!("reading the blob of {id}")),
            }
        }
    }
    std::fs::write(out.join("manifest.json"), serde_json::to_vec_pretty(&manifest)?)?;
    Ok(manifest)
}

// ---------------------------------------------------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------------------------------------------------

/// Options of [`import_path`].
#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub mode: ImportMode,
    /// Documents per transaction.
    pub batch_size: usize,
    /// Allow importing into `_`-prefixed system tables (the CLI does; the admin endpoint doesn't).
    pub allow_system_tables: bool,
    /// The format of the files; `None`: from each file's extension (`.csv` is CSV, anything else JSONL), or from
    /// an export's manifest.
    pub format: Option<ExportFormat>,
}

impl Default for ImportOptions {
    fn default() -> Self {
        ImportOptions {
            mode: ImportMode::Insert,
            batch_size: DEFAULT_BATCH_SIZE,
            allow_system_tables: false,
            format: None,
        }
    }
}

/// What an import did, per table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct TableImport {
    pub table: String,
    /// Documents written.
    #[ts(type = "number")]
    pub documents: u64,
    /// Of those, documents that replaced an existing one (`upsert`).
    #[ts(type = "number")]
    pub replaced: u64,
    /// Documents deleted first (`replace`).
    #[ts(type = "number")]
    pub deleted: u64,
}

/// What [`import_path`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct ImportReport {
    pub tables: Vec<TableImport>,
    /// Blobs written to file storage (from an export's `files/`).
    #[ts(type = "number")]
    pub files: u64,
    /// Imported `_storage` documents with no blob in the export nor in storage.
    #[ts(type = "number")]
    pub missing_files: u64,
    /// Whether ids were rewritten because table numbers differ.
    pub remapped_ids: bool,
}

/// Rewrites ids whose table number differs between the export and the target database.
#[derive(Debug, Clone, Default)]
pub struct IdRemap {
    numbers: HashMap<u32, u32>,
}

impl IdRemap {
    /// From the export's catalog and the target's: every table present in both with a different number.
    pub fn new(source: &std::collections::BTreeMap<String, u32>, target: &crate::db::catalog::Catalog) -> IdRemap {
        let numbers = source
            .iter()
            .filter_map(|(name, &n)| target.table(name).ok().map(|t| (n, t.number)))
            .filter(|(from, to)| from != to)
            .collect();
        IdRemap { numbers }
    }

    pub fn is_empty(&self) -> bool {
        self.numbers.is_empty()
    }

    /// The id with its table number rewritten (or unchanged).
    pub fn id(&self, id: DocId) -> DocId {
        match self.numbers.get(&id.table_number()) {
            Some(&to) => {
                let mut raw = id.0;
                raw[..4].copy_from_slice(&to.to_be_bytes());
                DocId(raw)
            }
            None => id,
        }
    }

    /// Rewrite every string in `value` that is an id of a remapped table.
    pub fn value(&self, value: &mut Value) {
        if self.numbers.is_empty() {
            return;
        }
        match value {
            Value::String(s) if s.len() == crate::id::ID_STR_LEN => {
                if let Ok(id) = DocId::decode(s) {
                    let mapped = self.id(id);
                    if mapped != id {
                        *s = mapped.encode();
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|v| self.value(v)),
            Value::Object(m) => m.values_mut().for_each(|v| self.value(v)),
            _ => {}
        }
    }
}

/// One parsed line: the source id (as written in the file), the id in the target, the user fields, the creation time.
struct ImportDoc {
    source_id: Option<String>,
    id: DocId,
    fields: Map<String, Value>,
    creation_time: Option<f64>,
}

fn parse_line(value: crate::Result<Value>, table: &TableInfo, remap: &IdRemap) -> crate::Result<ImportDoc> {
    let mut value = value?;
    let source_id = value.get(ID_FIELD).and_then(Value::as_str).map(str::to_string);
    remap.value(&mut value);
    let Value::Object(mut fields) = value else { bail!("a document must be a JSON object") };
    let id = match fields.remove(ID_FIELD) {
        None | Some(Value::Null) => DocId::new_random(table.number),
        Some(Value::String(s)) => {
            let id = DocId::decode(&s).map_err(|e| anyhow::anyhow!("`_id`: {e}"))?;
            if id.table_number() != table.number {
                bail!(
                    "`_id` {s} belongs to table number {}, not to `{}` (number {}); export with a manifest to rewrite ids",
                    id.table_number(),
                    table.name,
                    table.number
                );
            }
            id
        }
        Some(other) => bail!("`_id` must be a string, not {other}"),
    };
    let creation_time = match fields.remove(CREATION_TIME_FIELD) {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => Some(n.as_f64().context("bad `_creationTime`")?),
        Some(other) => bail!("`_creationTime` must be a number, not {other}"),
    };
    Ok(ImportDoc { source_id, id, fields, creation_time })
}

/// One record of an import file: its line number and the document (or why that record can't be read). The outer
/// error is for failures reading the file itself.
type Record = crate::Result<(usize, crate::Result<Value>)>;

/// The documents of an import file: JSONL (one per line, blank lines skipped) or CSV with a header
/// (`super::csv`; cells are typed with the table's shape).
fn records(
    path: &Path,
    format: ExportFormat,
    table: &TableInfo,
) -> crate::Result<Box<dyn Iterator<Item = Record> + Send>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = std::io::BufReader::new(file);
    Ok(match format {
        ExportFormat::Jsonl => Box::new(reader.lines().enumerate().filter_map(|(i, l)| match l {
            Ok(l) if l.trim().is_empty() => None,
            Ok(l) => Some(Ok((i + 1, serde_json::from_str(&l).context("invalid JSON")))),
            Err(e) => Some(Err(e.into())),
        })),
        ExportFormat::Csv => {
            let ts = table.def.as_ref().map(|d| (d.ts)());
            let validate = table.def.as_ref().map(|d| d.validate);
            let mut docs = super::csv::CsvDocs::new(reader, ts, validate, table.number)
                .with_context(|| format!("reading {}", path.display()))?;
            Box::new(std::iter::from_fn(move || docs.next_document().transpose()))
        }
    })
}

/// The format of an import file: the one asked for, else from its extension (`.csv`, otherwise JSONL).
pub fn file_format(path: &Path, asked: Option<ExportFormat>) -> ExportFormat {
    asked.unwrap_or_else(|| match path.extension().and_then(|e| e.to_str()) {
        Some(e) if e.eq_ignore_ascii_case("csv") => ExportFormat::Csv,
        _ => ExportFormat::Jsonl,
    })
}

/// The table to import into, checked.
fn import_table(app: &App, name: &str, options: &ImportOptions) -> crate::Result<Arc<TableInfo>> {
    let table = app.db().catalog().table(name)?.clone();
    if name.starts_with('_') && !options.allow_system_tables {
        bail!("`{name}` is a system table; it can't be imported here");
    }
    if table.def.is_none() {
        bail!("table `{name}` is not defined in this app's code; its documents can't be written");
    }
    Ok(table)
}

/// Validate every line of `file` for `table` without writing (pass 1). Returns the ids and the count.
async fn check_file(
    app: &App,
    table: &Arc<TableInfo>,
    file: &Path,
    format: ExportFormat,
    remap: &IdRemap,
    mode: ImportMode,
) -> crate::Result<u64> {
    let validate = table.def.as_ref().expect("checked by import_table").validate;
    let (t, path, rm) = (table.clone(), file.to_path_buf(), remap.clone());
    let (count, ids) = tokio::task::spawn_blocking(move || -> crate::Result<(u64, Vec<DocId>)> {
        let mut errors = Vec::new();
        let mut ids = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut count = 0;
        for record in records(&path, format, &t)? {
            let (n, value) = record?;
            count += 1;
            let result = parse_line(value, &t, &rm).and_then(|doc| {
                if !seen.insert(doc.id) {
                    bail!("duplicate `_id` {}", doc.id);
                }
                let mut full = doc.fields.clone();
                full.insert(ID_FIELD.into(), Value::String(doc.id.encode()));
                let ct =
                    serde_json::Number::from_f64(doc.creation_time.unwrap_or(0.0)).context("bad `_creationTime`")?;
                full.insert(CREATION_TIME_FIELD.into(), ct.into());
                validate(&Value::Object(full))
                    .map_err(|e| anyhow::anyhow!("does not match table `{}`: {e}", t.name))?;
                ids.push(doc.id);
                Ok(())
            });
            if let Err(e) = result {
                errors.push(format!("{}:{n}: {e:#}", path.display()));
                if errors.len() >= 20 {
                    break;
                }
            }
        }
        if !errors.is_empty() {
            bail!("nothing was imported; invalid documents:\n  {}", errors.join("\n  "));
        }
        Ok((count, ids))
    })
    .await??;
    if mode == ImportMode::Insert {
        let snapshot = app.db().snapshot().await?;
        let existing = snapshot.with_conn(|c| {
            let mut found = Vec::new();
            for id in &ids {
                if crate::db::storage::get_document(c, &id.0)?.is_some() {
                    found.push(id.encode());
                    if found.len() >= 5 {
                        break;
                    }
                }
            }
            Ok(found)
        })?;
        if !existing.is_empty() {
            bail!(
                "nothing was imported; documents with these ids exist in `{}` already: {} (use upsert or replace)",
                table.name,
                existing.join(", ")
            );
        }
    }
    Ok(count)
}

/// Delete every document of `table`, in batches (`replace` mode).
async fn delete_all(app: &App, table: &Arc<TableInfo>, batch_size: usize) -> crate::Result<u64> {
    let mut deleted = 0;
    loop {
        let txn = app.db().begin_write().await?;
        let n = txn.with_state(|s| {
            let start = table.number.to_be_bytes().to_vec();
            let end = crate::keys::prefix_successor(&start).unwrap_or_else(|| vec![0xff; 17]);
            let ids: Vec<Vec<u8>> = s
                .conn()
                .prepare("SELECT id FROM documents WHERE id >= ?1 AND id < ?2 ORDER BY id LIMIT ?3")?
                .query_map(params![start, end, batch_size as i64], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            for id in &ids {
                let id: [u8; 16] = id.as_slice().try_into().map_err(|_| anyhow::anyhow!("corrupt document id"))?;
                s.delete(table, DocId(id))?;
            }
            Ok(ids.len())
        })?;
        txn.commit_with(|_, _| Ok(())).await?;
        deleted += n as u64;
        if n < batch_size {
            return Ok(deleted);
        }
    }
}

/// Import one JSONL or CSV file (the format: [`ImportOptions::format`], else from the extension) into `table`.
/// `files` is the directory of blobs (an export's `files/`), for `_storage`.
pub async fn import_file(
    app: &App,
    table: &str,
    file: &Path,
    files: Option<&Path>,
    remap: &IdRemap,
    options: &ImportOptions,
    report: &mut ImportReport,
) -> crate::Result<TableImport> {
    let table = import_table(app, table, options)?;
    let batch_size = options.batch_size.max(1);
    let format = file_format(file, options.format);
    check_file(app, &table, file, format, remap, options.mode).await?;
    let mut result = TableImport { table: table.name.clone(), ..Default::default() };
    if options.mode == ImportMode::Replace {
        result.deleted = delete_all(app, &table, batch_size).await?;
    }
    let is_storage = table.name == crate::storage::StoredFile::NAME;
    let blobs = BlobStore::of_app(app);
    let validate = table.def.as_ref().expect("checked by import_table").validate;
    let overwrite = options.mode != ImportMode::Insert;
    let mut pending: Vec<ImportDoc> = Vec::with_capacity(batch_size);
    let mut records = records(file, format, &table)?;
    loop {
        pending.clear();
        while pending.len() < batch_size {
            let Some(record) = records.next() else { break };
            let (_, value) = record?;
            pending.push(parse_line(value, &table, remap)?);
        }
        if pending.is_empty() {
            break;
        }
        if is_storage {
            for doc in &pending {
                let id = doc.id.encode();
                let source = doc.source_id.as_deref().and_then(|s| files.map(|f| f.join(s))).filter(|p| p.is_file());
                match source {
                    Some(path) => {
                        let bytes = tokio::fs::read(&path).await?;
                        blobs.store.put(&blobs.path(&id), PutPayload::from(bytes)).await.context("storing a blob")?;
                        report.files += 1;
                    }
                    None => {
                        if let Err(object_store::Error::NotFound { .. }) = blobs.store.head(&blobs.path(&id)).await {
                            report.missing_files += 1;
                        }
                    }
                }
            }
        }
        let txn = app.db().begin_write().await?;
        let replaced = txn.with_state(|s| {
            let mut replaced = 0;
            for doc in pending.drain(..) {
                let existed = s.put_imported(&table, doc.id, doc.fields, doc.creation_time, overwrite, |full| {
                    validate(&Value::Object(full.clone())).map_err(|e| {
                        anyhow::anyhow!("imported document {} does not match table `{}`: {e}", doc.id, table.name)
                    })
                })?;
                replaced += existed as u64;
                result.documents += 1;
            }
            Ok(replaced)
        })?;
        txn.commit_with(|_, _| Ok(())).await?;
        result.replaced += replaced;
    }
    Ok(result)
}

/// Import a path: an export directory (with `manifest.json`: its tables, ids rewritten as needed, `files/`), a
/// directory of `<table>.jsonl` files, or one JSONL file (into `table`, default: the file's stem).
pub async fn import_path(
    app: &App,
    path: &Path,
    table: Option<&str>,
    options: &ImportOptions,
) -> crate::Result<ImportReport> {
    let mut report = ImportReport::default();
    let mut jobs: Vec<(String, PathBuf)> = Vec::new();
    let mut remap = IdRemap::default();
    let mut files = None;
    let mut format = options.format;
    if path.is_dir() {
        let manifest_path = path.join("manifest.json");
        if manifest_path.exists() {
            let manifest: ExportManifest =
                serde_json::from_slice(&std::fs::read(&manifest_path)?).context("invalid manifest.json")?;
            if manifest.format != EXPORT_FORMAT {
                bail!("{} is not a rxferrix export manifest", manifest_path.display());
            }
            if options.format.is_some_and(|f| f != manifest.document_format) {
                bail!("{} is a {:?} export", path.display(), manifest.document_format);
            }
            format = Some(manifest.document_format);
            remap = IdRemap::new(&manifest.catalog, app.db().catalog());
            for t in &manifest.tables {
                if table.is_none_or(|only| only == t.name) {
                    jobs.push((t.name.clone(), path.join(&t.file)));
                }
            }
            files = Some(path.join("files"));
        } else {
            // `<table>.jsonl` and `<table>.csv` files (only those of `--format` when given).
            let wanted = |ext: &str| match options.format {
                Some(f) => ext == f.extension(),
                None => ext == "jsonl" || ext == "csv",
            };
            for entry in std::fs::read_dir(path)? {
                let p = entry?.path();
                if p.extension().and_then(|e| e.to_str()).is_some_and(wanted)
                    && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                    && table.is_none_or(|only| only == stem)
                {
                    if jobs.iter().any(|(name, _)| name == stem) {
                        bail!("{} has both {stem}.jsonl and {stem}.csv; pick one with --format", path.display());
                    }
                    jobs.push((stem.to_string(), p.clone()));
                }
            }
            jobs.sort();
        }
        if jobs.is_empty() {
            bail!("nothing to import in {}", path.display());
        }
    } else {
        let name = match table {
            Some(t) => t.to_string(),
            None => path
                .file_stem()
                .and_then(|s| s.to_str())
                .context("can't tell the table from the file name; pass --table")?
                .to_string(),
        };
        jobs.push((name, path.to_path_buf()));
    }
    // Stored files first, so documents referencing them never point at a file that doesn't exist yet.
    jobs.sort_by_key(|(name, _)| name != crate::storage::StoredFile::NAME);
    report.remapped_ids = !remap.is_empty();
    let options = ImportOptions { format, ..options.clone() };
    for (name, file) in jobs {
        let done = import_file(app, &name, &file, files.as_deref(), &remap, &options, &mut report)
            .await
            .with_context(|| format!("importing `{name}` from {}", file.display()))?;
        report.tables.push(done);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_escaping() {
        assert_eq!(csv_escape("plain"), "plain");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_escape("two\nlines"), "\"two\nlines\"");
        assert_eq!(csv_escape("cr\rhere"), "\"cr\rhere\"");
        assert_eq!(csv_escape(" padded"), "\" padded\"");
        assert_eq!(csv_escape(""), "");
        assert_eq!(csv_value(None), "");
        assert_eq!(csv_value(Some(&Value::Null)), "");
        assert_eq!(csv_value(Some(&serde_json::json!("x,y"))), "x,y");
        assert_eq!(csv_value(Some(&serde_json::json!(true))), "true");
        assert_eq!(csv_value(Some(&serde_json::json!(1.5))), "1.5");
        assert_eq!(csv_value(Some(&serde_json::json!({"a": [1, "b"]}))), "{\"a\":[1,\"b\"]}");
        let mut out = Vec::new();
        write_csv_record(&mut out, ["a".to_string(), "b,c".into(), "{\"k\":\"v\"}".into(), String::new()]).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "a,\"b,c\",\"{\"\"k\"\":\"\"v\"\"}\",\r\n");
    }

    #[test]
    fn id_remap() {
        let a = DocId::new_random(3);
        let b = DocId::new_random(7);
        let remap = IdRemap { numbers: [(3, 5)].into_iter().collect() };
        assert_eq!(remap.id(a).table_number(), 5);
        assert_eq!(remap.id(a).0[4..], a.0[4..]);
        assert_eq!(remap.id(b), b);
        let mut v = serde_json::json!({ "ref": a.encode(), "list": [a.encode(), b.encode()], "text": "hello", "n": 3 });
        remap.value(&mut v);
        assert_eq!(v["ref"], remap.id(a).encode());
        assert_eq!(v["list"][0], remap.id(a).encode());
        assert_eq!(v["list"][1], b.encode());
        assert_eq!(v["text"], "hello");
    }
}
