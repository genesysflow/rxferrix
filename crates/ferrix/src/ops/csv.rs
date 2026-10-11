//! CSV import: the counterpart of the CSV export (`super::export::write_table`).
//!
//! **Reading** ([`CsvReader`]): RFC 4180. Fields are separated by commas, records end with CRLF or LF, a field in
//! double quotes may contain commas, quotes (doubled) and line breaks. A UTF-8 byte order mark is skipped, as are
//! blank lines. The first record is the header: column names, unique, with `_id` and `_creationTime` optional.
//!
//! **Types** ([`CsvDocs`]). The export writes strings as is, `null` and missing fields as empty cells, and every other
//! value as JSON. So a cell like `42` may be the number 42 or the string "42", and an empty cell `null` or `""`. The
//! table's shape decides: the TS type ferrix derives from the table's Rust type ([`crate::schema::TableDef::ts`]) is
//! read for each top-level field ([`FieldShape`]):
//!
//! - a field that can only be a string (`string`, `Id<…>`, string literals) takes the cell as is (an empty cell is
//!   `null` if the field is nullable, else `""`);
//! - a field that can't be a string (`number`, `boolean`, `Array<…>`, objects, `{ $bytes: string }`) parses the cell
//!   as JSON (an empty cell is `null`);
//! - otherwise (unions, named `#[object]` types, `any`) a cell that is valid JSON of a kind the field allows is
//!   parsed, anything else is a string.
//!
//! The document is then validated against the table's Rust type; when that fails and some cells were ambiguous, the
//! other reading of those cells is tried, field by field. What stays ambiguous after that: a nullable string field's
//! `""` and `null` are both exported as an empty cell, and are imported as `null`.

use std::collections::{BTreeMap, HashSet};
use std::io::BufRead;

use anyhow::bail;
use serde_json::{Map, Value};

use crate::schema::{CREATION_TIME_FIELD, ID_FIELD};

// ---------------------------------------------------------------------------------------------------------------------
// RFC 4180 reader
// ---------------------------------------------------------------------------------------------------------------------

/// Reads CSV records from a buffered reader.
pub struct CsvReader<R> {
    inner: R,
    /// Lines read so far (1-based line number of the last line read).
    line: usize,
    first: bool,
}

impl<R: BufRead> CsvReader<R> {
    pub fn new(inner: R) -> Self {
        CsvReader { inner, line: 0, first: true }
    }

    /// The next record and the line it starts on; `None` at the end. Blank lines are skipped.
    pub fn next_record(&mut self) -> crate::Result<Option<(usize, Vec<String>)>> {
        let mut buf = String::new();
        loop {
            buf.clear();
            if self.inner.read_line(&mut buf)? == 0 {
                return Ok(None);
            }
            self.line += 1;
            if self.first {
                self.first = false;
                if let Some(rest) = buf.strip_prefix('\u{feff}') {
                    buf = rest.to_string();
                }
            }
            if !buf.trim_end_matches(['\r', '\n']).is_empty() {
                break;
            }
        }
        let start = self.line;
        let mut fields = Vec::new();
        let mut field = String::new();
        let mut in_quotes = false;
        loop {
            let mut chars = buf.chars().peekable();
            while let Some(c) = chars.next() {
                if in_quotes {
                    if c == '"' {
                        if chars.peek() == Some(&'"') {
                            chars.next();
                            field.push('"');
                        } else {
                            in_quotes = false;
                        }
                    } else {
                        field.push(c);
                    }
                    continue;
                }
                match c {
                    ',' => fields.push(std::mem::take(&mut field)),
                    '"' if field.is_empty() => in_quotes = true,
                    '\r' if chars.peek() == Some(&'\n') || chars.peek().is_none() => {}
                    '\n' => {}
                    c => field.push(c),
                }
            }
            if !in_quotes {
                fields.push(field);
                return Ok(Some((start, fields)));
            }
            // A quoted field continues on the next line (the line break is part of the field, already pushed).
            buf.clear();
            if self.inner.read_line(&mut buf)? == 0 {
                bail!("line {start}: a quoted field is not closed before the end of the file");
            }
            self.line += 1;
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Field shapes from the table's TS type
// ---------------------------------------------------------------------------------------------------------------------

/// What JSON kinds a top-level field of a table admits, from its TS type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FieldShape {
    pub string: bool,
    pub null: bool,
    pub number: bool,
    pub boolean: bool,
    /// Arrays and objects.
    pub structured: bool,
    /// A named type, `any` or `unknown`: anything may fit.
    pub unknown: bool,
}

impl FieldShape {
    fn admits(&self, v: &Value) -> bool {
        self.unknown
            || match v {
                Value::Null => self.null,
                Value::Bool(_) => self.boolean,
                Value::Number(_) => self.number,
                Value::String(_) => self.string,
                Value::Array(_) | Value::Object(_) => self.structured,
            }
    }

    fn only_string(&self) -> bool {
        self.string && !self.number && !self.boolean && !self.structured && !self.unknown
    }
}

/// Remove `/* … */` and `// …` comments (doc comments in ts-rs output), outside string literals.
fn strip_comments(ts: &str) -> String {
    let mut out = String::with_capacity(ts.len());
    let mut chars = ts.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('*')) => {
                chars.next();
                let mut prev = ' ';
                for c in chars.by_ref() {
                    if prev == '*' && c == '/' {
                        break;
                    }
                    prev = c;
                }
                out.push(' ');
            }
            ('/', Some('/')) => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Split `s` at `sep` where it is not nested in brackets or a string literal.
fn split_top(s: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut in_string, mut start) = (0i32, false, 0);
    let mut prev = ' ';
    for (i, c) in s.char_indices() {
        if in_string {
            if c == '"' && prev != '\\' {
                in_string = false;
            }
        } else {
            match c {
                '"' => in_string = true,
                '{' | '[' | '(' | '<' => depth += 1,
                '}' | ']' | ')' | '>' => depth -= 1,
                c if c == sep && depth == 0 => {
                    parts.push(&s[start..i]);
                    start = i + c.len_utf8();
                }
                _ => {}
            }
        }
        prev = c;
    }
    parts.push(&s[start..]);
    parts
}

fn classify(ty: &str, shape: &mut FieldShape) {
    for alt in split_top(ty, '|').into_iter().map(str::trim).filter(|a| !a.is_empty()) {
        let alt = alt.trim_start_matches('(').trim_end_matches(')').trim();
        match alt {
            "string" => shape.string = true,
            "null" | "undefined" => shape.null = true,
            "number" | "bigint" => shape.number = true,
            "boolean" | "true" | "false" => shape.boolean = true,
            "any" | "unknown" => shape.unknown = true,
            a if a.starts_with('"') || a.starts_with('`') || a.starts_with("Id<") => shape.string = true,
            a if a.parse::<f64>().is_ok() => shape.number = true,
            a if a.starts_with('{')
                || a.starts_with('[')
                || a.starts_with("Array<")
                || a.starts_with("Record<")
                || a.ends_with("[]") =>
            {
                shape.structured = true
            }
            _ => shape.unknown = true,
        }
    }
}

/// The top-level fields of an inline TS object type (`{ text: string, done: boolean, … }`). Empty when the type
/// isn't an object literal (every column is then read without type information).
pub fn field_shapes(ts: &str) -> BTreeMap<String, FieldShape> {
    let ts = strip_comments(ts);
    let ts = ts.trim();
    let Some(body) = ts.strip_prefix('{').and_then(|t| t.strip_suffix('}')) else { return BTreeMap::new() };
    let mut out = BTreeMap::new();
    for entry in split_top(body, ',').into_iter().map(str::trim).filter(|e| !e.is_empty()) {
        let Some((name, ty)) = split_top(entry, ':').split_first().map(|(n, rest)| (*n, rest.join(":"))) else {
            continue;
        };
        if ty.is_empty() {
            continue;
        }
        let name = name.trim();
        let (name, optional) = match name.strip_suffix('?') {
            Some(n) => (n.trim(), true),
            None => (name, false),
        };
        let name = name.trim_matches('"').trim_matches('\'').to_string();
        let mut shape = FieldShape { null: optional, ..Default::default() };
        classify(&ty, &mut shape);
        out.insert(name, shape);
    }
    out
}

// ---------------------------------------------------------------------------------------------------------------------
// Records to documents
// ---------------------------------------------------------------------------------------------------------------------

/// A cell's value, and the other readings tried when the document doesn't validate.
fn cell_value(cell: &str, shape: Option<&FieldShape>) -> (Value, Vec<Value>) {
    let any = FieldShape { unknown: true, ..Default::default() };
    let shape = shape.unwrap_or(&any);
    let string_ok = shape.string || shape.unknown;
    if cell.is_empty() {
        if shape.only_string() && !shape.null {
            return (Value::String(String::new()), Vec::new());
        }
        let alts = if string_ok { vec![Value::String(String::new())] } else { Vec::new() };
        return (Value::Null, alts);
    }
    let text = Value::String(cell.to_string());
    if shape.only_string() {
        return (text, Vec::new());
    }
    // The exporter never JSON-encodes strings, so a cell that parses as a JSON string is text.
    match serde_json::from_str::<Value>(cell) {
        Ok(v) if !v.is_string() && (shape.admits(&v) || !string_ok) => {
            let alts = if string_ok { vec![text] } else { Vec::new() };
            (v, alts)
        }
        Ok(v) if !v.is_string() => (text, vec![v]),
        _ => (text, Vec::new()),
    }
}

/// A table's document check (`crate::schema::TableDef::validate`).
type Validate = fn(&Value) -> Result<(), String>;

/// Documents from a CSV file with a header: each record as the JSON object the JSONL import reads (`_id` and
/// `_creationTime` included when the columns are there).
pub struct CsvDocs<R> {
    reader: CsvReader<R>,
    columns: Vec<String>,
    shapes: BTreeMap<String, FieldShape>,
    validate: Option<Validate>,
    table_number: u32,
}

impl<R: BufRead> CsvDocs<R> {
    /// Read the header. `ts` is the table's TS type and `validate` its document check, when the table is defined in
    /// code (both guide the reading of ambiguous cells).
    pub fn new(reader: R, ts: Option<String>, validate: Option<Validate>, table_number: u32) -> crate::Result<Self> {
        let mut reader = CsvReader::new(reader);
        let Some((_, columns)) = reader.next_record()? else { bail!("the CSV file is empty (it needs a header row)") };
        let mut seen = HashSet::new();
        for c in &columns {
            if c.is_empty() {
                bail!("line 1: the header has an empty column name");
            }
            if !seen.insert(c.as_str()) {
                bail!("line 1: the header names `{c}` twice");
            }
        }
        let shapes = ts.as_deref().map(field_shapes).unwrap_or_default();
        Ok(CsvDocs { reader, columns, shapes, validate, table_number })
    }

    fn document(&self, cells: Vec<String>) -> crate::Result<Value> {
        if cells.len() != self.columns.len() {
            bail!("{} field(s), but the header has {} column(s)", cells.len(), self.columns.len());
        }
        let mut doc = Map::new();
        let mut alternatives: Vec<(String, Vec<Value>)> = Vec::new();
        for (column, cell) in self.columns.iter().zip(cells) {
            match column.as_str() {
                ID_FIELD => {
                    if !cell.is_empty() {
                        doc.insert(ID_FIELD.into(), Value::String(cell));
                    }
                }
                CREATION_TIME_FIELD => {
                    if !cell.is_empty() {
                        let n: f64 = cell
                            .trim()
                            .parse()
                            .map_err(|_| anyhow::anyhow!("`_creationTime` `{cell}` is not a number"))?;
                        let n = serde_json::Number::from_f64(n)
                            .ok_or_else(|| anyhow::anyhow!("`_creationTime` must be finite"))?;
                        doc.insert(CREATION_TIME_FIELD.into(), Value::Number(n));
                    }
                }
                _ => {
                    let (value, alts) = cell_value(&cell, self.shapes.get(column));
                    if !alts.is_empty() {
                        alternatives.push((column.clone(), alts));
                    }
                    doc.insert(column.clone(), value);
                }
            }
        }
        if let Some(validate) = self.validate
            && !alternatives.is_empty()
        {
            self.settle(&mut doc, &alternatives, validate);
        }
        Ok(Value::Object(doc))
    }

    /// The document doesn't validate: try the other readings of ambiguous cells, one field at a time (in the order
    /// serde reports them), keeping a change when the document validates or fails on another field.
    fn settle(&self, doc: &mut Map<String, Value>, alternatives: &[(String, Vec<Value>)], validate: Validate) {
        let check = |doc: &Map<String, Value>| {
            let mut full = doc.clone();
            full.entry(ID_FIELD.to_string())
                .or_insert_with(|| Value::String(crate::id::DocId::new_random(self.table_number).encode()));
            full.entry(CREATION_TIME_FIELD.to_string()).or_insert_with(|| Value::from(0.0));
            validate(&Value::Object(full))
        };
        let Err(mut error) = check(doc) else { return };
        for (field, alts) in alternatives {
            let original = doc.get(field).cloned().unwrap_or(Value::Null);
            for alt in alts {
                doc.insert(field.clone(), alt.clone());
                match check(doc) {
                    Ok(()) => return,
                    Err(e) if e != error => {
                        error = e;
                        break;
                    }
                    Err(_) => {
                        doc.insert(field.clone(), original.clone());
                    }
                }
            }
        }
    }

    /// The next document: `(line, document or the record's error)`; `None` at the end. Errors reading the file are
    /// the outer error.
    pub fn next_document(&mut self) -> crate::Result<Option<(usize, crate::Result<Value>)>> {
        let Some((line, cells)) = self.reader.next_record()? else { return Ok(None) };
        Ok(Some((line, self.document(cells))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read_all(text: &str) -> Vec<(usize, Vec<String>)> {
        let mut r = CsvReader::new(text.as_bytes());
        let mut out = Vec::new();
        while let Some(rec) = r.next_record().unwrap() {
            out.push(rec);
        }
        out
    }

    #[test]
    fn rfc4180() {
        let recs =
            read_all("\u{feff}a,b,c\r\n1,\"x,y\",\"say \"\"hi\"\"\"\r\n\r\n\"two\r\nlines\",,\" pad \"\nlast,\"\",z");
        let fields: Vec<Vec<&str>> = recs.iter().map(|(_, f)| f.iter().map(String::as_str).collect()).collect();
        assert_eq!(
            fields,
            vec![
                vec!["a", "b", "c"],
                vec!["1", "x,y", "say \"hi\""],
                vec!["two\r\nlines", "", " pad "],
                vec!["last", "", "z"],
            ]
        );
        assert_eq!(recs.iter().map(|(l, _)| *l).collect::<Vec<_>>(), [1, 2, 4, 6]);
        let mut bad = CsvReader::new("a,\"open\nstill".as_bytes());
        assert!(bad.next_record().is_err());
    }

    #[test]
    fn shapes_from_ts() {
        let ts = r#"{ /** The title. */ title: string, rank: number, done: boolean, owner: Id<"_users"> | null, tags: Array<string>, meta: { a: number, b: string | null, }, data: { $bytes: string }, kind: "a" | "b", view: MessageView, note?: string, mixed: string | number, }"#;
        let s = field_shapes(ts);
        assert!(s["title"].only_string() && !s["title"].null);
        assert!(s["rank"].number && !s["rank"].string);
        assert!(s["done"].boolean);
        assert!(s["owner"].only_string() && s["owner"].null);
        assert!(s["tags"].structured && s["meta"].structured && s["data"].structured);
        assert!(s["kind"].only_string());
        assert!(s["view"].unknown);
        assert!(s["note"].only_string() && s["note"].null);
        assert!(s["mixed"].string && s["mixed"].number);
        assert_eq!(s.len(), 11);
        assert!(field_shapes("SomeName").is_empty());
    }

    #[test]
    fn cells() {
        let string = FieldShape { string: true, ..Default::default() };
        let nullable = FieldShape { string: true, null: true, ..Default::default() };
        let number = FieldShape { number: true, ..Default::default() };
        let mixed = FieldShape { string: true, number: true, ..Default::default() };
        assert_eq!(cell_value("42", Some(&string)).0, json!("42"));
        assert_eq!(cell_value("true", Some(&string)).0, json!("true"));
        assert_eq!(cell_value("", Some(&string)).0, json!(""));
        assert_eq!(cell_value("", Some(&nullable)).0, Value::Null);
        assert_eq!(cell_value("42", Some(&number)).0, json!(42));
        assert_eq!(cell_value("4.5e3", Some(&number)).0, json!(4500.0));
        assert_eq!(cell_value("", Some(&number)).0, Value::Null);
        assert_eq!(cell_value("42", Some(&mixed)), (json!(42), vec![json!("42")]));
        assert_eq!(cell_value("forty", Some(&mixed)).0, json!("forty"));
        assert_eq!(cell_value("[1,\"a\"]", None).0, json!([1, "a"]));
        assert_eq!(cell_value("\"quoted\"", None).0, json!("\"quoted\""), "strings are never JSON-encoded");
        assert_eq!(cell_value("{oops", None).0, json!("{oops"));
    }
}
