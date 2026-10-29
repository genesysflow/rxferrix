//! Live aggregates (`#[aggregate(name, group = [..], sum = [..])]`): per group of values, the number of documents and
//! the sums of some fields, maintained inside every write's own transaction and read like an index point lookup.
//! Contract: docs/CONTRACT.md §1.12.
//!
//! **Storage.** `_aggregates (agg_id, table_number, name, definition)` describes them (ids from
//! [`AGGREGATE_ID_BASE`] up, so they never meet index or search index ids) and `aggregate_values (agg_id, key, count,
//! sums)` holds one row per non-empty group: `key` is the memcomparable encoding of the group values (`keys`, like an
//! index key without the document id), `sums` a JSON array with one number per sum field.
//!
//! **Maintenance** ([`Writes`], called by `WriteState::write` next to the index entries): for each aggregate, the
//! group and values of the old and the new document; a write that changes neither touches nothing. Otherwise the old
//! group loses the old values and the new group gains the new ones (one group: the difference), rows reaching a count
//! of zero are deleted, and each changed group adds `(agg_id, key)` to the commit's write-log entries, so only queries
//! that read that group, or a range holding it, rerun. Sums stay exact integers while every contribution is one
//! (checked; past `i64` they become floats) and are `f64` once a float joins. A missing, null or non-numeric field
//! contributes nothing. It all happens in the write's transaction (savepoint), so it commits and rolls back with it.
//!
//! **Reads** ([`AggregateQuery`]): `get` (one group: an exact `eq` on every group field), `sum_over` (the groups of an
//! index-style range over the group fields, combined), `groups` (them listed in key order). Each records its key
//! range in the read set. Missing groups read as zero.
//!
//! **Definitions** are reconciled at startup ([`reconcile_table`]): a new or changed aggregate is rebuilt from the
//! documents, a removed one deleted. [`verify`] recomputes them all (backups, restores).

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

use super::catalog::{IndexId, TableInfo};
use super::query::{IndexRange, QueryDb, RangeOps};
use super::readset::KeyRange;
use crate::keys;
use crate::schema::{Aggregate, Field, Order, Table, TableDef};

/// A group's count and sums.
type Counts = (i64, Vec<Sum>);
/// A stored group: its key, count and sums.
type GroupRow = (Vec<u8>, i64, Vec<Sum>);

/// Aggregate ids start above every index (`_indexes`) and search index (`SPECIAL_INDEX_BASE` up) id.
pub const AGGREGATE_ID_BASE: IndexId = 0xC000_0000;

pub(crate) const DDL: &str = "
CREATE TABLE IF NOT EXISTS _aggregates (agg_id INTEGER PRIMARY KEY, table_number INTEGER NOT NULL, name TEXT NOT NULL,
    definition TEXT NOT NULL, UNIQUE(table_number, name));
CREATE TABLE IF NOT EXISTS aggregate_values (agg_id INTEGER NOT NULL, key BLOB NOT NULL, count INTEGER NOT NULL,
    sums TEXT NOT NULL, PRIMARY KEY (agg_id, key)) WITHOUT ROWID;";

/// What an aggregate computes (stored as JSON in `_aggregates.definition`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AggregateSpec {
    pub group_fields: Vec<String>,
    pub sum_fields: Vec<String>,
}

/// An aggregate of a table, as the catalog knows it.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateInfo {
    pub id: IndexId,
    pub name: String,
    pub spec: AggregateSpec,
}

// ---------------------------------------------------------------------------------------------------------------------
// Numbers
// ---------------------------------------------------------------------------------------------------------------------

/// A running sum: exact while every contribution is an integer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sum {
    Int(i64),
    Float(f64),
}

impl Sum {
    const ZERO: Sum = Sum::Int(0);

    fn of(v: Option<&Value>) -> Sum {
        match v {
            Some(Value::Number(n)) => n.as_i64().map(Sum::Int).unwrap_or_else(|| Sum::Float(n.as_f64().unwrap_or(0.0))),
            _ => Sum::ZERO,
        }
    }

    fn add(self, other: Sum) -> Sum {
        match (self, other) {
            (Sum::Int(a), Sum::Int(b)) => a.checked_add(b).map_or(Sum::Float(a as f64 + b as f64), Sum::Int),
            (a, b) => Sum::Float(a.as_f64() + b.as_f64()),
        }
    }

    fn neg(self) -> Sum {
        match self {
            Sum::Int(a) => a.checked_neg().map_or(Sum::Float(-(a as f64)), Sum::Int),
            Sum::Float(f) => Sum::Float(-f),
        }
    }

    pub fn as_f64(self) -> f64 {
        match self {
            Sum::Int(i) => i as f64,
            Sum::Float(f) => f,
        }
    }

    fn is_zero(self) -> bool {
        match self {
            Sum::Int(i) => i == 0,
            Sum::Float(f) => f == 0.0,
        }
    }

    fn to_json(self) -> Value {
        match self {
            Sum::Int(i) => Value::from(i),
            Sum::Float(f) => Number::from_f64(f).map_or(Value::from(0), Value::Number),
        }
    }

    fn from_json(v: &Value) -> Sum {
        Sum::of(Some(v))
    }
}

/// A group's (or a range of groups') count and sums.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateValue {
    pub count: u64,
    /// One per sum field, in the declared order.
    pub sums: Vec<Sum>,
    sum_fields: Vec<String>,
}

impl AggregateValue {
    fn zero(spec: &AggregateSpec) -> AggregateValue {
        AggregateValue { count: 0, sums: vec![Sum::ZERO; spec.sum_fields.len()], sum_fields: spec.sum_fields.clone() }
    }

    fn absorb(&mut self, count: i64, sums: &[Sum]) {
        self.count = (self.count as i64 + count).max(0) as u64;
        for (s, d) in self.sums.iter_mut().zip(sums) {
            *s = s.add(*d);
        }
    }

    /// The sum of `field` (`0.0` if it isn't a sum field of this aggregate).
    pub fn sum<T, V>(&self, field: Field<T, V>) -> f64 {
        self.sum_raw(field.name()).map_or(0.0, Sum::as_f64)
    }

    /// The sum of `field` while every contribution was an integer, else `None`.
    pub fn sum_i64<T, V>(&self, field: Field<T, V>) -> Option<i64> {
        match self.sum_raw(field.name())? {
            Sum::Int(i) => Some(i),
            Sum::Float(_) => None,
        }
    }

    /// The sum of the field with JSON name `name`.
    pub fn sum_raw(&self, name: &str) -> Option<Sum> {
        self.sum_fields.iter().position(|f| f == name).map(|i| self.sums[i])
    }

    /// `sum(field) / count` (`None` without documents).
    pub fn average<T, V>(&self, field: Field<T, V>) -> Option<f64> {
        (self.count > 0).then(|| self.sum(field) / self.count as f64)
    }
}

/// One group of [`AggregateQuery::groups`]: its values (the group fields', in order) and its count and sums.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateGroup {
    pub group: Vec<Value>,
    pub value: AggregateValue,
}

// ---------------------------------------------------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------------------------------------------------

pub(crate) fn ensure_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(DDL)?;
    Ok(())
}

/// Every aggregate, by table number.
pub(crate) fn load(conn: &Connection) -> anyhow::Result<BTreeMap<u32, Vec<AggregateInfo>>> {
    let mut out: BTreeMap<u32, Vec<AggregateInfo>> = BTreeMap::new();
    let mut stmt = conn.prepare("SELECT agg_id, table_number, name, definition FROM _aggregates ORDER BY agg_id")?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let definition: String = r.get(3)?;
        out.entry(r.get(1)?).or_default().push(AggregateInfo {
            id: r.get(0)?,
            name: r.get(2)?,
            spec: serde_json::from_str(&definition)?,
        });
    }
    Ok(out)
}

/// Checks the macro can't do for hand-written definitions.
pub(crate) fn validate_def(def: &TableDef) -> anyhow::Result<()> {
    let mut names = std::collections::HashSet::new();
    for a in &def.aggregates {
        anyhow::ensure!(!a.name.is_empty(), "table `{}`: an aggregate needs a name", def.name);
        anyhow::ensure!(names.insert(a.name), "table `{}`: duplicate aggregate `{}`", def.name, a.name);
        for f in a.group_fields.iter().chain(&a.sum_fields) {
            anyhow::ensure!(
                !f.is_empty() && !f.starts_with('_'),
                "table `{}`: aggregate `{}` can't use the field `{f}`",
                def.name,
                a.name
            );
        }
    }
    Ok(())
}

/// Bring the table's aggregates in line with its definition: create, rebuild (changed) or delete (removed) them.
pub(crate) fn reconcile_table(conn: &Connection, def: &TableDef, table_number: u32) -> anyhow::Result<()> {
    let existing: Vec<AggregateInfo> = load(conn)?.remove(&table_number).unwrap_or_default();
    for a in &def.aggregates {
        let spec = AggregateSpec {
            group_fields: a.group_fields.iter().map(|s| s.to_string()).collect(),
            sum_fields: a.sum_fields.iter().map(|s| s.to_string()).collect(),
        };
        let info = match existing.iter().find(|e| e.name == a.name) {
            Some(e) if e.spec == spec => continue,
            Some(e) => {
                tracing::info!(table = def.name, aggregate = a.name, "aggregate changed; rebuilding it");
                conn.execute(
                    "UPDATE _aggregates SET definition = ?1 WHERE agg_id = ?2",
                    params![serde_json::to_string(&spec)?, e.id],
                )?;
                AggregateInfo { id: e.id, name: e.name.clone(), spec }
            }
            None => {
                let max: Option<IndexId> = conn.query_row("SELECT MAX(agg_id) FROM _aggregates", [], |r| r.get(0))?;
                let id = max.unwrap_or(AGGREGATE_ID_BASE).max(AGGREGATE_ID_BASE) + 1;
                conn.execute(
                    "INSERT INTO _aggregates (agg_id, table_number, name, definition) VALUES (?1, ?2, ?3, ?4)",
                    params![id, table_number, a.name, serde_json::to_string(&spec)?],
                )?;
                tracing::info!(table = def.name, aggregate = a.name, "new aggregate; building it");
                AggregateInfo { id, name: a.name.to_string(), spec }
            }
        };
        backfill(conn, table_number, &info)?;
    }
    for e in existing.iter().filter(|e| !def.aggregates.iter().any(|a| a.name == e.name)) {
        tracing::info!(table = def.name, aggregate = %e.name, "aggregate removed; deleting it");
        conn.execute("DELETE FROM aggregate_values WHERE agg_id = ?1", [e.id])?;
        conn.execute("DELETE FROM _aggregates WHERE agg_id = ?1", [e.id])?;
    }
    Ok(())
}

/// Every group of `info`, computed from the table's documents.
fn compute(conn: &Connection, table_number: u32, info: &AggregateInfo) -> anyhow::Result<BTreeMap<Vec<u8>, Counts>> {
    let mut groups: BTreeMap<Vec<u8>, Counts> = BTreeMap::new();
    super::storage::for_each_document(conn, table_number, |_, json| {
        let Value::Object(doc) = serde_json::from_str(json)? else { return Ok(()) };
        let (key, values) = contribution(&info.spec, &doc);
        let entry = groups.entry(key).or_insert_with(|| (0, vec![Sum::ZERO; values.len()]));
        entry.0 += 1;
        for (s, v) in entry.1.iter_mut().zip(values) {
            *s = s.add(v);
        }
        Ok(())
    })?;
    Ok(groups)
}

/// Recompute `info` from the documents.
fn backfill(conn: &Connection, table_number: u32, info: &AggregateInfo) -> anyhow::Result<()> {
    conn.execute("DELETE FROM aggregate_values WHERE agg_id = ?1", [info.id])?;
    let mut insert = conn.prepare("INSERT INTO aggregate_values (agg_id, key, count, sums) VALUES (?1, ?2, ?3, ?4)")?;
    for (key, (count, sums)) in compute(conn, table_number, info)? {
        insert.execute(params![info.id, key, count, sums_json(&sums)])?;
    }
    Ok(())
}

/// Recompute every aggregate (the key encoding changed).
pub(crate) fn rebuild_all(conn: &Connection) -> anyhow::Result<()> {
    for (number, infos) in load(conn)? {
        for info in infos {
            backfill(conn, number, &info)?;
        }
    }
    Ok(())
}

/// Recompute every aggregate of `tables` and compare with what is stored: a description of each difference.
pub(crate) fn verify(conn: &Connection, tables: &[std::sync::Arc<TableInfo>]) -> anyhow::Result<Vec<String>> {
    let mut problems = Vec::new();
    for t in tables {
        for info in &t.aggregates {
            let expected = compute(conn, t.number, info)?;
            let stored = stored_groups(conn, info.id)?;
            if expected.len() != stored.len() {
                problems.push(format!(
                    "aggregate `{}.{}`: {} groups, expected {}",
                    t.name,
                    info.name,
                    stored.len(),
                    expected.len()
                ));
                continue;
            }
            for ((ek, (ec, es)), (sk, (sc, ss))) in expected.iter().zip(&stored) {
                let close =
                    es.iter().zip(ss).all(|(a, b)| (a.as_f64() - b.as_f64()).abs() <= 1e-6 * a.as_f64().abs().max(1.0));
                if ek != sk || ec != sc || !close {
                    problems.push(format!("aggregate `{}.{}`: a group differs from the documents", t.name, info.name));
                    break;
                }
            }
        }
    }
    Ok(problems)
}

fn stored_groups(conn: &Connection, id: IndexId) -> anyhow::Result<Vec<(Vec<u8>, Counts)>> {
    let mut stmt = conn.prepare("SELECT key, count, sums FROM aggregate_values WHERE agg_id = ?1 ORDER BY key")?;
    let rows = stmt.query_map([id], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (key, count, sums) = row?;
        out.push((key, (count, parse_sums(&sums)?)));
    }
    Ok(out)
}

fn sums_json(sums: &[Sum]) -> String {
    Value::Array(sums.iter().map(|s| s.to_json()).collect()).to_string()
}

fn parse_sums(text: &str) -> anyhow::Result<Vec<Sum>> {
    let values: Vec<Value> = serde_json::from_str(text)?;
    Ok(values.iter().map(Sum::from_json).collect())
}

// ---------------------------------------------------------------------------------------------------------------------
// Maintenance
// ---------------------------------------------------------------------------------------------------------------------

/// A document's group key and values for `spec`.
fn contribution(spec: &AggregateSpec, doc: &Map<String, Value>) -> (Vec<u8>, Vec<Sum>) {
    let mut key = Vec::new();
    for f in &spec.group_fields {
        keys::encode_opt(doc.get(f), &mut key);
    }
    (key, spec.sum_fields.iter().map(|f| Sum::of(doc.get(f))).collect())
}

/// The aggregate changes of one document write (`WriteState::write`): computed before anything is written.
pub(crate) struct Writes {
    /// `(agg_id, key, count delta, sum deltas)`.
    deltas: Vec<(IndexId, Vec<u8>, i64, Vec<Sum>)>,
}

impl Writes {
    pub(crate) fn prepare(
        table: &TableInfo,
        old: Option<&Map<String, Value>>,
        new: Option<&Map<String, Value>>,
    ) -> Writes {
        let mut deltas = Vec::new();
        for info in &table.aggregates {
            let old = old.map(|d| contribution(&info.spec, d));
            let new = new.map(|d| contribution(&info.spec, d));
            match (old, new) {
                (Some((ok, ov)), Some((nk, nv))) if ok == nk => {
                    let diff: Vec<Sum> = nv.iter().zip(&ov).map(|(n, o)| n.add(o.neg())).collect();
                    if diff.iter().any(|d| !d.is_zero()) || ov != nv {
                        deltas.push((info.id, nk, 0, diff));
                    }
                }
                (old, new) => {
                    if let Some((k, v)) = old {
                        deltas.push((info.id, k, -1, v.into_iter().map(Sum::neg).collect()));
                    }
                    if let Some((k, v)) = new {
                        deltas.push((info.id, k, 1, v));
                    }
                }
            }
        }
        Writes { deltas }
    }

    /// Apply the changes in the write's transaction and record each changed group in `writes`.
    pub(crate) fn apply(self, conn: &Connection, writes: &mut Vec<(IndexId, Vec<u8>)>) -> anyhow::Result<()> {
        for (id, key, count, sums) in self.deltas {
            let current: Option<(i64, String)> = conn
                .prepare_cached("SELECT count, sums FROM aggregate_values WHERE agg_id = ?1 AND key = ?2")?
                .query_row(params![id, key], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            let (mut c, mut s) = match current {
                Some((c, s)) => (c, parse_sums(&s)?),
                None => (0, vec![Sum::ZERO; sums.len()]),
            };
            c += count;
            for (a, d) in s.iter_mut().zip(&sums) {
                *a = a.add(*d);
            }
            if c <= 0 {
                conn.prepare_cached("DELETE FROM aggregate_values WHERE agg_id = ?1 AND key = ?2")?
                    .execute(params![id, key])?;
            } else {
                conn.prepare_cached(
                    "INSERT INTO aggregate_values (agg_id, key, count, sums) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (agg_id, key) DO UPDATE SET count = excluded.count, sums = excluded.sums",
                )?
                .execute(params![id, key, c, sums_json(&s)])?;
            }
            writes.push((id, key));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------------------------------------------------

impl QueryDb {
    /// Read a live aggregate of `T` (`#[aggregate]`): `ctx.db.aggregate(Order::by_customer).get(|g| g.eq(Order::customer, c))`.
    pub fn aggregate<T: Table>(&self, aggregate: Aggregate<T>) -> AggregateQuery<'_, T> {
        AggregateQuery { db: self, table: T::NAME, name: aggregate.name(), _t: std::marker::PhantomData }
    }

    /// The aggregate `name` of `table` and its key range for `ops`.
    fn aggregate_range(&self, table: &str, name: &str, ops: &RangeOps) -> anyhow::Result<(AggregateInfo, KeyRange)> {
        let t = self.catalog().table(table)?;
        let info = t
            .aggregates
            .iter()
            .find(|a| a.name == name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("table `{table}` has no aggregate `{name}`"))?;
        let range = ops.to_range(&t.name, &info.spec.group_fields, &format!("aggregate `{name}`"))?;
        Ok((info, range))
    }

    fn aggregate_rows(
        &self,
        info: &AggregateInfo,
        range: &KeyRange,
        order: Order,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<GroupRow>> {
        let dir = if order == Order::Desc { "DESC" } else { "ASC" };
        let wanted = limit;
        let limit = limit.map_or(-1, |n| n as i64);
        let rows = self.with_conn(|c| {
            let sql = format!(
                "SELECT key, count, sums FROM aggregate_values WHERE agg_id = ?1 AND key >= ?2 AND (?3 IS NULL OR key < ?3)
                 ORDER BY key {dir} LIMIT ?4"
            );
            let mut stmt = c.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![info.id, range.start, range.end, limit], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (key, count, sums) = row?;
                out.push((key, count, parse_sums(&sums)?));
            }
            Ok(out)
        });
        // As for scans (`query::scan`): when the limit was reached, only the groups up to the last one returned were
        // read, and a write to a group past it can't change the result. On error, the whole range (always safe).
        let recorded = match (&rows, wanted) {
            (Ok(rows), Some(n)) if n > 0 && rows.len() >= n => {
                let last = &rows.last().expect("n > 0 rows").0;
                match order {
                    Order::Asc => {
                        KeyRange { start: range.start.clone(), end: Some(crate::keys::immediate_successor(last)) }
                    }
                    Order::Desc => KeyRange { start: last.clone(), end: range.end.clone() },
                }
            }
            _ => range.clone(),
        };
        self.record(info.id, recorded);
        rows
    }
}

/// A read of one aggregate ([`QueryDb::aggregate`]).
pub struct AggregateQuery<'a, T> {
    db: &'a QueryDb,
    table: &'static str,
    name: &'static str,
    _t: std::marker::PhantomData<fn() -> T>,
}

impl<T: Table> AggregateQuery<'_, T> {
    /// One group: an `eq` on every group field, in order (none for an aggregate without group fields). A group without
    /// documents reads as zero.
    pub async fn get(self, group: impl FnOnce(IndexRange<T>) -> IndexRange<T>) -> crate::Result<AggregateValue> {
        let ops = group(IndexRange::new()).into_ops();
        let (info, range) = self.db.aggregate_range(self.table, self.name, &ops)?;
        if !ops.is_exact(info.spec.group_fields.len()) {
            anyhow::bail!(
                "aggregate `{}`: get needs an `eq` on every group field ({}); use sum_over for a range",
                self.name,
                info.spec.group_fields.join(", ")
            );
        }
        let mut value = AggregateValue::zero(&info.spec);
        for (_, count, sums) in self.db.aggregate_rows(&info, &range, Order::Asc, Some(1))? {
            value.absorb(count, &sums);
        }
        Ok(value)
    }

    /// The whole table (every group combined).
    pub async fn total(self) -> crate::Result<AggregateValue> {
        self.sum_over(|g| g).await
    }

    /// The groups in a range over the group fields (`eq` on a prefix, then at most a lower and an upper bound on the
    /// next field), combined. Reads every group in the range: keep ranges narrow, or group by fewer fields.
    pub async fn sum_over(self, range: impl FnOnce(IndexRange<T>) -> IndexRange<T>) -> crate::Result<AggregateValue> {
        let ops = range(IndexRange::new()).into_ops();
        let (info, range) = self.db.aggregate_range(self.table, self.name, &ops)?;
        let mut value = AggregateValue::zero(&info.spec);
        for (_, count, sums) in self.db.aggregate_rows(&info, &range, Order::Asc, None)? {
            value.absorb(count, &sums);
        }
        Ok(value)
    }

    /// The groups in a range, in key order (`order`), at most `limit` of them.
    pub async fn groups(
        self,
        range: impl FnOnce(IndexRange<T>) -> IndexRange<T>,
        order: Order,
        limit: usize,
    ) -> crate::Result<Vec<AggregateGroup>> {
        let ops = range(IndexRange::new()).into_ops();
        let (info, range) = self.db.aggregate_range(self.table, self.name, &ops)?;
        let rows = self.db.aggregate_rows(&info, &range, order, Some(limit))?;
        // At most `limit` groups: the read set covers only what was returned (later keys can't change it).
        let mut out = Vec::with_capacity(rows.len());
        for (key, count, sums) in rows {
            let mut group = Vec::with_capacity(info.spec.group_fields.len());
            let mut rest = key.as_slice();
            for _ in &info.spec.group_fields {
                let (v, r) = keys::decode_value(rest).map_err(|e| anyhow::anyhow!("aggregate `{}`: {e}", self.name))?;
                group.push(v.unwrap_or(Value::Null));
                rest = r;
            }
            let mut value = AggregateValue::zero(&info.spec);
            value.absorb(count, &sums);
            out.push(AggregateGroup { group, value });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sums_stay_exact_until_a_float_joins() {
        assert_eq!(Sum::Int(2).add(Sum::Int(3)), Sum::Int(5));
        assert_eq!(Sum::Int(i64::MAX).add(Sum::Int(1)), Sum::Float(i64::MAX as f64 + 1.0));
        assert_eq!(Sum::Int(2).add(Sum::Float(0.5)), Sum::Float(2.5));
        assert_eq!(Sum::of(Some(&json!("x"))), Sum::ZERO, "non-numbers contribute nothing");
        assert_eq!(Sum::of(None), Sum::ZERO);
        assert_eq!(Sum::Int(i64::MIN).neg(), Sum::Float(-(i64::MIN as f64)));
    }

    #[test]
    fn a_write_that_changes_nothing_aggregated_is_no_change() {
        let spec = AggregateSpec { group_fields: vec!["g".into()], sum_fields: vec!["n".into()] };
        let table = TableInfo {
            name: "t".into(),
            number: 1,
            indexes: vec![],
            def: None,
            search_indexes: vec![],
            aggregates: vec![AggregateInfo { id: AGGREGATE_ID_BASE + 1, name: "a".into(), spec }],
        };
        let doc = |g: &str, n: i64, other: &str| json!({ "g": g, "n": n, "other": other }).as_object().unwrap().clone();
        assert!(Writes::prepare(&table, Some(&doc("x", 1, "a")), Some(&doc("x", 1, "b"))).deltas.is_empty());
        let same_group = Writes::prepare(&table, Some(&doc("x", 1, "a")), Some(&doc("x", 4, "a")));
        assert_eq!(same_group.deltas.len(), 1);
        assert_eq!((same_group.deltas[0].2, same_group.deltas[0].3.clone()), (0, vec![Sum::Int(3)]));
        let moved = Writes::prepare(&table, Some(&doc("x", 1, "a")), Some(&doc("y", 1, "a")));
        assert_eq!(moved.deltas.iter().map(|d| d.2).collect::<Vec<_>>(), vec![-1, 1]);
        assert_eq!(Writes::prepare(&table, None, Some(&doc("x", 2, "a"))).deltas[0].2, 1);
        assert_eq!(Writes::prepare(&table, Some(&doc("x", 2, "a")), None).deltas[0].3, vec![Sum::Int(-2)]);
    }
}
