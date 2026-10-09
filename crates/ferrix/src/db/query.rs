//! The document query API used by functions: `QueryDb` (reads), `MutationDb` (reads + writes), `Query` and
//! `IndexRange`.
//!
//! Every read is an index range scan (or a `by_id` point lookup) and is recorded in the context's [`ReadSet`].

use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::db::catalog::{Catalog, IndexInfo, TableInfo};
use crate::db::pagination::{PaginationOpts, PaginationResult};
use crate::db::readset::{KeyRange, ReadSet};
use crate::db::{Snapshot, WriteTxn, check_table, storage};
use crate::id::Id;
use crate::keys;
use crate::schema::{BY_CREATION_TIME, Doc, Field, Index, Order, Table};

/// Rows fetched per SQL round trip when scanning.
pub(super) const SCAN_BATCH: usize = 256;

#[derive(Clone)]
pub(crate) enum Source {
    Snapshot(Arc<Snapshot>),
    Write(Arc<WriteTxn>),
}

/// Read access to the database, at a snapshot (queries) or inside the write transaction (mutations).
pub struct QueryDb {
    source: Source,
    catalog: Arc<Catalog>,
    read_set: Arc<Mutex<ReadSet>>,
}

impl QueryDb {
    pub(crate) fn new(source: Source, catalog: Arc<Catalog>) -> Self {
        QueryDb { source, catalog, read_set: Arc::new(Mutex::new(ReadSet::new())) }
    }

    /// A handle to this context's read set. The runtime keeps one so it can still read it after the handler (which
    /// owns the context) has finished, including when the handler failed.
    pub(crate) fn read_set_handle(&self) -> Arc<Mutex<ReadSet>> {
        self.read_set.clone()
    }

    pub(super) fn with_conn<R>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<R>) -> anyhow::Result<R> {
        match &self.source {
            Source::Snapshot(s) => s.with_conn(f),
            Source::Write(t) => t.with_state(|s| f(s.conn())),
        }
    }

    pub(super) fn record(&self, index_id: crate::db::catalog::IndexId, range: KeyRange) {
        self.read_set.lock().unwrap_or_else(|e| e.into_inner()).record(index_id, range);
    }

    fn table<T: Table>(&self) -> anyhow::Result<&Arc<TableInfo>> {
        self.catalog.table(T::NAME)
    }

    /// Fetch a document by id. Errors if the id belongs to another table.
    pub async fn get<T: Table>(&self, id: Id<T>) -> crate::Result<Option<Doc<T>>> {
        let table = self.table::<T>()?;
        self.check_id(table, id)?;
        self.record(table.by_id().id, KeyRange::point(id.as_bytes()));
        let json = self.with_conn(|c| storage::get_document(c, id.as_bytes()))?;
        json.map(|j| parse_doc::<T>(&j)).transpose()
    }

    /// Start a query over table `T` (default: `by_creation_time`, full range, ascending).
    pub fn query<T: Table>(&self) -> Query<'_, T> {
        Query { db: self, index: BY_CREATION_TIME, range: IndexRange::new(), order: Order::Asc, filter: None }
    }

    fn check_id<T>(&self, table: &TableInfo, id: Id<T>) -> anyhow::Result<()> {
        if id.table_number() != table.number {
            let other = self.catalog.table_by_number(id.table_number()).map(|t| t.name.as_str()).unwrap_or("<unknown>");
            bail!("id {id} belongs to table `{other}`, not `{}`", table.name);
        }
        check_table(table, id.doc_id())
    }
}

pub(super) fn parse_doc<T: Table>(json: &str) -> anyhow::Result<Doc<T>> {
    serde_json::from_str(json).with_context(|| format!("document does not match table `{}`", T::NAME))
}

/// Read-write access for mutations. Derefs to [`QueryDb`]; reads see the mutation's own writes.
pub struct MutationDb {
    reads: QueryDb,
    txn: Arc<WriteTxn>,
}

impl Deref for MutationDb {
    type Target = QueryDb;
    fn deref(&self) -> &QueryDb {
        &self.reads
    }
}

/// The document about to be written must deserialize as `Doc<T>`.
fn check_doc<T: Table>(doc: &Map<String, Value>, what: &str) -> anyhow::Result<()> {
    use serde::Deserialize as _;
    Doc::<T>::deserialize(doc).map(drop).with_context(|| {
        let id = doc.get(crate::schema::ID_FIELD).and_then(Value::as_str).unwrap_or("?");
        format!("{what} document {id} does not match table `{}`; nothing was written", T::NAME)
    })
}

fn to_fields<T: Table>(value: &T) -> anyhow::Result<Map<String, Value>> {
    match serde_json::to_value(value)? {
        Value::Object(m) => Ok(m),
        other => bail!("table `{}` must serialize to a JSON object, got {other}", T::NAME),
    }
}

impl MutationDb {
    pub(crate) fn new(txn: Arc<WriteTxn>) -> Self {
        MutationDb { reads: QueryDb::new(Source::Write(txn.clone()), txn.catalog.clone()), txn }
    }

    /// Insert a document. The stored JSON must round-trip: it is checked to deserialize back as `Doc<T>` (a
    /// `Serialize` impl that doesn't match its `Deserialize`, e.g. `#[serde(skip_serializing)]` on a required
    /// field, is rejected here rather than breaking every later read).
    pub async fn insert<T: Table>(&self, value: T) -> crate::Result<Id<T>> {
        let table = self.table::<T>()?.clone();
        let fields = to_fields(&value)?;
        let id = self.txn.with_state(|s| s.insert(&table, fields, |doc| check_doc::<T>(doc, "inserted")))?;
        Ok(Id::from_doc_id(id))
    }

    /// Replace a document's user fields (system fields are kept). Round-trip checked like [`MutationDb::insert`].
    pub async fn replace<T: Table>(&self, id: Id<T>, value: T) -> crate::Result<()> {
        let table = self.table::<T>()?.clone();
        self.check_id(&table, id)?;
        let fields = to_fields(&value)?;
        self.txn.with_state(|s| s.replace(&table, id.doc_id(), fields, |doc| check_doc::<T>(doc, "replaced")))
    }

    /// Shallow-merge `patch` (a JSON object) into the document. A `null` value sets the field to null. The merged
    /// document must still deserialize as `Doc<T>`; otherwise the patch is rejected and nothing is written.
    pub async fn patch<T: Table>(&self, id: Id<T>, patch: Value) -> crate::Result<()> {
        let table = self.table::<T>()?.clone();
        self.check_id(&table, id)?;
        let Value::Object(patch) = patch else { bail!("patch must be a JSON object") };
        self.txn.with_state(|s| s.patch(&table, id.doc_id(), patch, |merged| check_doc::<T>(merged, "patched")))
    }

    pub async fn delete<T: Table>(&self, id: Id<T>) -> crate::Result<()> {
        let table = self.table::<T>()?.clone();
        self.check_id(&table, id)?;
        self.txn.with_state(|s| s.delete(&table, id.doc_id()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpKind {
    Eq,
    Gt,
    Gte,
    Lt,
    Lte,
}

impl OpKind {
    fn name(self) -> &'static str {
        match self {
            OpKind::Eq => "eq",
            OpKind::Gt => "gt",
            OpKind::Gte => "gte",
            OpKind::Lt => "lt",
            OpKind::Lte => "lte",
        }
    }
}

struct RangeOp {
    field: &'static str,
    kind: OpKind,
    value: Result<Value, String>,
}

/// Constraints on an index: equality on a prefix of its fields, then at most one lower and one upper bound on the
/// next field. Checked when the query runs.
pub struct IndexRange<T> {
    ops: Vec<RangeOp>,
    _p: PhantomData<fn() -> T>,
}

impl<T> IndexRange<T> {
    fn new() -> Self {
        IndexRange { ops: Vec::new(), _p: PhantomData }
    }

    fn push<V: Serialize>(mut self, f: Field<T, V>, kind: OpKind, v: V) -> Self {
        let value = serde_json::to_value(v).map_err(|e| e.to_string());
        self.ops.push(RangeOp { field: f.name(), kind, value });
        self
    }

    pub fn eq<V: Serialize>(self, f: Field<T, V>, v: V) -> Self {
        self.push(f, OpKind::Eq, v)
    }
    pub fn gt<V: Serialize>(self, f: Field<T, V>, v: V) -> Self {
        self.push(f, OpKind::Gt, v)
    }
    pub fn gte<V: Serialize>(self, f: Field<T, V>, v: V) -> Self {
        self.push(f, OpKind::Gte, v)
    }
    pub fn lt<V: Serialize>(self, f: Field<T, V>, v: V) -> Self {
        self.push(f, OpKind::Lt, v)
    }
    pub fn lte<V: Serialize>(self, f: Field<T, V>, v: V) -> Self {
        self.push(f, OpKind::Lte, v)
    }

    /// Turn the constraints into a byte range over the index keys.
    fn to_key_range(&self, table: &str, index: &IndexInfo) -> anyhow::Result<KeyRange> {
        let ctx = || format!("invalid range for index `{}` of table `{table}` (fields {:?})", index.name, index.fields);
        let mut prefix = Vec::new();
        let mut pos = 0;
        let mut ops = self.ops.iter().peekable();
        // Equality prefix.
        while let Some(op) = ops.next_if(|op| op.kind == OpKind::Eq) {
            let Some(expected) = index.fields.get(pos) else {
                bail!("{}: too many fields; `eq({})` has no index field left", ctx(), op.field);
            };
            if op.field != expected {
                bail!("{}: expected `{expected}` at position {pos}, got `eq({})`", ctx(), op.field);
            }
            let v = op.value.as_ref().map_err(|e| anyhow::anyhow!("{}: cannot serialize value: {e}", ctx()))?;
            keys::encode_value(v, &mut prefix);
            pos += 1;
        }
        // At most one lower and one upper bound, both on the next field.
        let mut lower: Option<(OpKind, Vec<u8>)> = None;
        let mut upper: Option<(OpKind, Vec<u8>)> = None;
        for op in ops {
            let Some(expected) = index.fields.get(pos) else {
                bail!("{}: `{}({})` has no index field left", ctx(), op.kind.name(), op.field);
            };
            if op.kind == OpKind::Eq {
                bail!("{}: `eq({})` must come before range bounds", ctx(), op.field);
            }
            if op.field != expected {
                bail!("{}: range bound must be on `{expected}` (the field after the eq prefix), got `{}({})`", ctx(), op.kind.name(), op.field);
            }
            let v = op.value.as_ref().map_err(|e| anyhow::anyhow!("{}: cannot serialize value: {e}", ctx()))?;
            let mut bound = prefix.clone();
            keys::encode_value(v, &mut bound);
            let slot = if matches!(op.kind, OpKind::Gt | OpKind::Gte) { &mut lower } else { &mut upper };
            if slot.is_some() {
                bail!("{}: more than one {} bound", ctx(), if matches!(op.kind, OpKind::Gt | OpKind::Gte) { "lower" } else { "upper" });
            }
            *slot = Some((op.kind, bound));
        }
        // Each encoded value is self-delimiting, so "all keys whose field equals v" is the range of keys starting
        // with `bound`, i.e. [bound, prefix_successor(bound)).
        let start = match lower {
            None => prefix.clone(),
            Some((OpKind::Gte, b)) => b,
            Some((_, b)) => keys::prefix_successor(&b).expect("encoded values never end in 0xFF only"),
        };
        let end = match upper {
            None => keys::prefix_successor(&prefix),
            Some((OpKind::Lt, b)) => Some(b),
            Some((_, b)) => keys::prefix_successor(&b),
        };
        Ok(KeyRange { start, end })
    }
}

type Filter<'a, T> = Box<dyn Fn(&Doc<T>) -> bool + Send + Sync + 'a>;

/// A query over one index of table `T`.
pub struct Query<'a, T> {
    pub(super) db: &'a QueryDb,
    index: &'static str,
    range: IndexRange<T>,
    pub(super) order: Order,
    pub(super) filter: Option<Filter<'a, T>>,
}

impl<'a, T: Table> Query<'a, T> {
    pub fn with_index(mut self, index: Index<T>, range: impl FnOnce(IndexRange<T>) -> IndexRange<T>) -> Self {
        self.index = index.name();
        self.range = range(IndexRange::new());
        self
    }

    pub fn order(mut self, order: Order) -> Self {
        self.order = order;
        self
    }

    /// Keep only documents matching `pred`. Filtered-out documents still count as read.
    pub fn filter(mut self, pred: impl Fn(&Doc<T>) -> bool + Send + Sync + 'a) -> Self {
        self.filter = Some(Box::new(pred));
        self
    }

    pub async fn collect(self) -> crate::Result<Vec<Doc<T>>> {
        self.run(None).await
    }

    pub async fn take(self, n: usize) -> crate::Result<Vec<Doc<T>>> {
        self.run(Some(n)).await
    }

    pub async fn first(self) -> crate::Result<Option<Doc<T>>> {
        Ok(self.run(Some(1)).await?.into_iter().next())
    }

    /// The only matching document, if any. Errors if more than one matches.
    pub async fn unique(self) -> crate::Result<Option<Doc<T>>> {
        let mut docs = self.run(Some(2)).await?;
        if docs.len() > 1 {
            bail!("unique() found more than one document in table `{}`", T::NAME);
        }
        Ok(docs.pop())
    }

    /// One page of results, with a cursor to continue from. See [`PaginationOpts`] for the semantics.
    ///
    /// ```ignore
    /// #[query]
    /// pub async fn list(ctx: &QueryCtx, channel: String, pagination_opts: PaginationOpts)
    ///     -> Result<PaginationResult<Doc<Message>>> {
    ///     ctx.db.query::<Message>()
    ///         .with_index(Message::by_channel, |q| q.eq(Message::channel, channel))
    ///         .order(Order::Desc)
    ///         .paginate(pagination_opts)
    ///         .await
    /// }
    /// ```
    pub async fn paginate(self, opts: PaginationOpts) -> crate::Result<PaginationResult<Doc<T>>> {
        crate::db::pagination::paginate(self, opts).await
    }

    /// The scanned index and the key range of the query's constraints on it.
    pub(super) fn resolve(&self) -> anyhow::Result<(&'a IndexInfo, KeyRange)> {
        let table: &'a Arc<TableInfo> = self.db.table::<T>()?;
        let index = table
            .index(self.index)
            .with_context(|| format!("index `{}` not found on table `{}`", self.index, table.name))?;
        if index.is_by_id() {
            bail!("index `by_id` cannot be scanned; use db.get(id)");
        }
        let range = self.range.to_key_range(&table.name, index)?;
        Ok((index, range))
    }

    async fn run(self, limit: Option<usize>) -> crate::Result<Vec<Doc<T>>> {
        let db = self.db;
        let (index, range) = self.resolve()?;
        let mut out = Vec::new();
        if limit == Some(0) {
            return Ok(out);
        }

        let mut cursor: Option<Vec<u8>> = None;
        let mut limit_hit = false;
        let result: anyhow::Result<()> = async {
            loop {
                let batch_size = match (limit, &self.filter) {
                    (Some(n), None) => (n - out.len()).min(SCAN_BATCH),
                    _ => SCAN_BATCH,
                };
                let rows = db.with_conn(|c| {
                    storage::scan_index_batch(c, index.id, &range, self.order, cursor.as_deref(), batch_size)
                })?;
                let exhausted = rows.len() < batch_size;
                for (key, json) in rows {
                    let doc = parse_doc::<T>(&json)?;
                    cursor = Some(key);
                    if self.filter.as_ref().is_none_or(|f| f(&doc)) {
                        out.push(doc);
                        if limit == Some(out.len()) {
                            limit_hit = true;
                            return Ok(());
                        }
                    }
                }
                if exhausted {
                    return Ok(());
                }
                // Let other queries sharing this snapshot make progress between batches.
                tokio::task::yield_now().await;
            }
        }
        .await;

        // Read-set narrowing: when the scan stopped because the limit was reached, only the part up to the last
        // scanned key was read. Writes beyond it cannot change the result (they would sort after the n-th row), so
        // e.g. "latest 50" isn't invalidated by inserts of older rows. Rows rejected by the filter don't narrow
        // anything: they were read. On error, record the whole range (a superset is always safe).
        let recorded = match (limit_hit && result.is_ok(), &cursor, self.order) {
            (true, Some(last), Order::Asc) => KeyRange { start: range.start, end: Some(keys::immediate_successor(last)) },
            (true, Some(last), Order::Desc) => KeyRange { start: last.clone(), end: range.end },
            _ => range,
        };
        db.record(index.id, recorded);
        result.map(|()| out)
    }
}
