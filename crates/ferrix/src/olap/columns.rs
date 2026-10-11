//! The analytics copy's tables: columns from a table's shape (`crate::shape`), and the SQL that fills them from the
//! stored JSON documents (staged as text, converted by DuckDB itself: `TRY_CAST` for scalars, `json_transform` for
//! lists and objects, so a value that doesn't fit its column becomes `NULL` instead of failing the copy).

use std::collections::HashSet;

use serde_json::Value;

use crate::shape::{Shape, StructShape};

/// Bumped when the column mapping changes: every table's fingerprint changes with it, so each is rebuilt.
const MAPPING_VERSION: u32 = 1;

/// The system tables the copy includes (user tables are always included). The others hold secrets (password and
/// token hashes, settings) or admin-only history (`_audit_log`), which SQL in actions must not read.
pub const SYSTEM_TABLES: &[&str] =
    &["_users", "_accounts", "_storage", "_scheduled_jobs", "_crons", "_workflows", "_workflow_steps", "_migrations"];

/// Whether the copy includes the table `name`.
pub fn copied(name: &str) -> bool {
    !name.starts_with('_') || SYSTEM_TABLES.contains(&name)
}

/// One column: its name, DuckDB type, and the expression computing it from the staged `doc` (JSON text), `ts` (the
/// document's commit ts) and `extra` (unknown keys, for open shapes).
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    pub sql_type: String,
    pub expr: String,
}

/// A copied table.
#[derive(Debug, Clone, PartialEq)]
pub struct TablePlan {
    pub name: String,
    pub columns: Vec<Column>,
    /// Keys beyond the known fields may appear: they go to the `_extra` JSON column.
    pub open: bool,
    /// The document keys that have columns (for computing `_extra`).
    pub known: HashSet<String>,
    /// Changes whenever the columns do: a different fingerprint rebuilds the table.
    pub fingerprint: String,
}

impl TablePlan {
    pub fn new(name: &str, shape: &Shape) -> TablePlan {
        let mut columns = vec![
            Column { name: "_id".into(), sql_type: "VARCHAR".into(), expr: "json_extract_string(doc, '/_id')".into() },
            Column {
                name: "_creationTime".into(),
                sql_type: "DOUBLE".into(),
                expr: "TRY_CAST(json_extract_string(doc, '/_creationTime') AS DOUBLE)".into(),
            },
            Column { name: "_ts".into(), sql_type: "BIGINT".into(), expr: "ts".into() },
        ];
        let mut known: HashSet<String> = ["_id", "_creationTime"].into_iter().map(String::from).collect();
        let (fields, open) = match shape {
            Shape::Struct(s) => (s.fields.as_slice(), s.open),
            _ => (&[][..], true),
        };
        for (name, shape) in fields {
            if name.starts_with('_') || !known.insert(name.clone()) {
                continue; // a system field, or a duplicate (a flatten repeating a name)
            }
            columns.push(Column { name: name.clone(), sql_type: sql_type(shape, true), expr: expr(name, shape) });
        }
        if open {
            columns.push(Column { name: "_extra".into(), sql_type: "JSON".into(), expr: "extra::JSON".into() });
        }
        let fingerprint = format!(
            "v{MAPPING_VERSION}:{name}:{}",
            columns.iter().map(|c| format!("{}={}={}", c.name, c.sql_type, c.expr)).collect::<Vec<_>>().join(";")
        );
        TablePlan { name: name.to_string(), columns, open, known, fingerprint }
    }

    /// `CREATE TABLE <table> (...)`.
    pub fn create_sql(&self, table: &str) -> String {
        let cols: Vec<String> = self.columns.iter().map(|c| format!("{} {}", ident(&c.name), c.sql_type)).collect();
        format!("CREATE TABLE {} ({})", ident(table), cols.join(", "))
    }

    /// `INSERT INTO <table> SELECT ... FROM <stage> WHERE <filter>`.
    pub fn insert_sql(&self, table: &str, stage: &str, filter: &str) -> String {
        let exprs: Vec<String> = self.columns.iter().map(|c| c.expr.clone()).collect();
        format!("INSERT INTO {} SELECT {} FROM {stage} WHERE {filter}", ident(table), exprs.join(", "))
    }

    /// The unknown keys of a document, as JSON text (`None` when there are none or the shape is closed).
    pub fn extra(&self, doc: &str) -> Option<String> {
        if !self.open {
            return None;
        }
        let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(doc) else { return None };
        map.retain(|k, _| !self.known.contains(k));
        (!map.is_empty()).then(|| Value::Object(map).to_string())
    }
}

/// A quoted DuckDB identifier.
pub fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A SQL string literal.
pub fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A JSON pointer (`/a/b`, with `~` and `/` escaped) as a SQL literal.
fn pointer(keys: &[&str]) -> String {
    let mut p = String::new();
    for k in keys {
        p.push('/');
        p.push_str(&k.replace('~', "~0").replace('/', "~1"));
    }
    literal(&p)
}

fn closed(s: &StructShape) -> bool {
    !s.open && !s.fields.is_empty()
}

/// The DuckDB type of a value of `shape` (`top`: a column, where bytes are a BLOB; nested bytes stay JSON).
fn sql_type(shape: &Shape, top: bool) -> String {
    match shape {
        Shape::Bool => "BOOLEAN".into(),
        Shape::Int => "BIGINT".into(),
        Shape::UInt => "UBIGINT".into(),
        Shape::Float => "DOUBLE".into(),
        Shape::Text => "VARCHAR".into(),
        Shape::Bytes if top => "BLOB".into(),
        Shape::Bytes | Shape::Json => "JSON".into(),
        Shape::Optional(inner) => sql_type(inner, top),
        Shape::List(inner) => format!("{}[]", sql_type(inner, false)),
        Shape::Struct(s) if closed(s) => {
            let fields: Vec<String> = s.fields.iter().map(|(n, f)| format!("{} {}", ident(n), sql_type(f, false))).collect();
            format!("STRUCT({})", fields.join(", "))
        }
        Shape::Struct(_) => "JSON".into(),
    }
}

/// `json_transform`'s structure for a nested value of `shape`, as JSON text (it must produce exactly
/// `sql_type(shape, false)`; written by hand so struct fields keep their order, as in the column's type).
fn structure(shape: &Shape) -> String {
    let quoted = |s: &str| Value::String(s.to_string()).to_string();
    match shape {
        Shape::Bool => quoted("BOOLEAN"),
        Shape::Int => quoted("BIGINT"),
        Shape::UInt => quoted("UBIGINT"),
        Shape::Float => quoted("DOUBLE"),
        Shape::Text => quoted("VARCHAR"),
        Shape::Bytes | Shape::Json => quoted("JSON"),
        Shape::Optional(inner) => structure(inner),
        Shape::List(inner) => format!("[{}]", structure(inner)),
        Shape::Struct(s) if closed(s) => {
            let fields: Vec<String> = s.fields.iter().map(|(n, f)| format!("{}:{}", quoted(n), structure(f))).collect();
            format!("{{{}}}", fields.join(","))
        }
        Shape::Struct(_) => quoted("JSON"),
    }
}

/// The expression reading the top-level field `name` as `shape`.
fn expr(name: &str, shape: &Shape) -> String {
    let path = pointer(&[name]);
    match shape {
        Shape::Bool => format!("TRY_CAST(json_extract_string(doc, {path}) AS BOOLEAN)"),
        Shape::Int => format!("TRY_CAST(json_extract_string(doc, {path}) AS BIGINT)"),
        Shape::UInt => format!("TRY_CAST(json_extract_string(doc, {path}) AS UBIGINT)"),
        Shape::Float => format!("TRY_CAST(json_extract_string(doc, {path}) AS DOUBLE)"),
        Shape::Text => format!("json_extract_string(doc, {path})"),
        Shape::Bytes => format!("TRY(from_base64(json_extract_string(doc, {})))", pointer(&[name, "$bytes"])),
        Shape::Json => format!("json_extract(doc, {path})"),
        Shape::Optional(inner) => expr(name, inner),
        Shape::List(_) | Shape::Struct(_) if sql_type(shape, false) == "JSON" => format!("json_extract(doc, {path})"),
        Shape::List(_) | Shape::Struct(_) => {
            format!("json_transform(json_extract(doc, {path}), {})", literal(&structure(shape)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape() -> Shape {
        let mut inner = StructShape::default();
        inner.field("lat", Shape::Float);
        inner.field("label", Shape::Text.optional());
        let mut s = StructShape::default();
        s.field("text", Shape::Text);
        s.field("n", Shape::Int);
        s.field("done", Shape::Bool.optional());
        s.field("tags", Shape::List(Box::new(Shape::Text)));
        s.field("place", Shape::Struct(inner));
        s.field("raw", Shape::Bytes);
        s.field("any", Shape::Json);
        s.field("o'k/~x", Shape::Text);
        Shape::Struct(s)
    }

    #[test]
    fn columns_from_a_shape() {
        let plan = TablePlan::new("t", &shape());
        let cols: Vec<(&str, &str)> = plan.columns.iter().map(|c| (c.name.as_str(), c.sql_type.as_str())).collect();
        assert_eq!(
            cols,
            [
                ("_id", "VARCHAR"),
                ("_creationTime", "DOUBLE"),
                ("_ts", "BIGINT"),
                ("text", "VARCHAR"),
                ("n", "BIGINT"),
                ("done", "BOOLEAN"),
                ("tags", "VARCHAR[]"),
                ("place", "STRUCT(\"lat\" DOUBLE, \"label\" VARCHAR)"),
                ("raw", "BLOB"),
                ("any", "JSON"),
                ("o'k/~x", "VARCHAR"),
            ]
        );
        assert!(!plan.open);
        let get = |n: &str| plan.columns.iter().find(|c| c.name == n).unwrap().expr.clone();
        assert_eq!(get("place"), r#"json_transform(json_extract(doc, '/place'), '{"lat":"DOUBLE","label":"VARCHAR"}')"#);
        assert_eq!(get("raw"), "TRY(from_base64(json_extract_string(doc, '/raw/$bytes')))");
        assert_eq!(get("o'k/~x"), "json_extract_string(doc, '/o''k~1~0x')");
        assert!(plan.create_sql("t").ends_with(", \"o'k/~x\" VARCHAR)"), "{}", plan.create_sql("t"));
    }

    #[test]
    fn open_shapes_keep_unknown_keys() {
        let plan = TablePlan::new("t", &Shape::unknown());
        assert!(plan.open);
        assert_eq!(plan.columns.last().unwrap().name, "_extra");
        assert_eq!(plan.extra(r#"{"_id":"x","_creationTime":1,"a":1}"#).as_deref(), Some(r#"{"a":1}"#));
        assert_eq!(plan.extra(r#"{"_id":"x"}"#), None);
        assert_eq!(TablePlan::new("t", &shape()).extra(r#"{"zzz":1}"#), None, "closed: no _extra");
    }

    #[test]
    fn fingerprints_follow_the_columns() {
        let a = TablePlan::new("t", &shape());
        assert_eq!(a.fingerprint, TablePlan::new("t", &shape()).fingerprint);
        let mut s = StructShape::default();
        s.field("text", Shape::Int);
        assert_ne!(a.fingerprint, TablePlan::new("t", &Shape::Struct(s)).fingerprint);
    }

    #[test]
    fn which_tables_are_copied() {
        assert!(copied("messages"));
        assert!(copied("_users") && copied("_storage"));
        assert!(!copied("_credentials") && !copied("_sessions") && !copied("_settings") && !copied("_audit_log"));
    }
}
