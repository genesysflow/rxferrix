//! DuckDB values to JSON (query results) and parameters to DuckDB values.

use data_encoding::BASE64;
use duckdb::types::{TimeUnit, Value as Db};
use serde_json::{Map, Number, Value, json};

use super::OlapValue;

pub(super) fn param(v: OlapValue) -> Db {
    match v {
        OlapValue::Null => Db::Null,
        OlapValue::Bool(b) => Db::Boolean(b),
        OlapValue::Int(i) => Db::BigInt(i),
        OlapValue::Float(f) => Db::Double(f),
        OlapValue::Text(s) => Db::Text(s),
        OlapValue::Bytes(b) => Db::Blob(b),
    }
}

fn float(f: f64) -> Value {
    Number::from_f64(f).map_or(Value::Null, Value::Number)
}

/// An integer as a JSON number (beyond i64/u64: as a float when it fits one, else a string).
fn wide(i: i128) -> Value {
    if let Ok(v) = i64::try_from(i) {
        json!(v)
    } else if let Ok(v) = u64::try_from(i) {
        json!(v)
    } else {
        float(i as f64)
    }
}

fn micros(unit: TimeUnit, v: i64) -> i64 {
    match unit {
        TimeUnit::Second => v.saturating_mul(1_000_000),
        TimeUnit::Millisecond => v.saturating_mul(1_000),
        TimeUnit::Microsecond => v,
        TimeUnit::Nanosecond => v / 1_000,
    }
}

/// A result value as JSON. `json_typed`: the column is DuckDB's `JSON` type, whose text is parsed.
pub(super) fn to_json(v: Db, json_typed: bool) -> Value {
    match v {
        Db::Null => Value::Null,
        Db::Boolean(b) => json!(b),
        Db::TinyInt(i) => json!(i),
        Db::SmallInt(i) => json!(i),
        Db::Int(i) => json!(i),
        Db::BigInt(i) => json!(i),
        Db::HugeInt(i) => wide(i),
        Db::UHugeInt(i) => i128::try_from(i).map_or_else(|_| float(i as f64), wide),
        Db::UTinyInt(i) => json!(i),
        Db::USmallInt(i) => json!(i),
        Db::UInt(i) => json!(i),
        Db::UBigInt(i) => json!(i),
        Db::Float(f) => float(f as f64),
        Db::Double(f) => float(f),
        Db::Decimal(d) => d.to_string().parse::<f64>().map_or_else(|_| json!(d.to_string()), float),
        Db::Timestamp(unit, v) => chrono::DateTime::from_timestamp_micros(micros(unit, v))
            .map_or(Value::Null, |t| json!(t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))),
        Db::Text(s) if json_typed => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        Db::Text(s) | Db::Enum(s) => Value::String(s),
        Db::Blob(b) | Db::Geometry(b) => json!({ "$bytes": BASE64.encode(&b) }),
        Db::Date32(days) => chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
            .and_then(|epoch| epoch.checked_add_signed(chrono::TimeDelta::days(days as i64)))
            .map_or(Value::Null, |d| json!(d.format("%Y-%m-%d").to_string())),
        Db::Time64(unit, v) => {
            let us = micros(unit, v);
            chrono::NaiveTime::from_num_seconds_from_midnight_opt(
                (us / 1_000_000) as u32,
                ((us % 1_000_000) * 1_000) as u32,
            )
            .map_or(Value::Null, |t| json!(t.format("%H:%M:%S%.f").to_string()))
        }
        Db::Interval { months, days, nanos } => json!({ "months": months, "days": days, "nanos": nanos }),
        Db::List(items) | Db::Array(items) => Value::Array(items.into_iter().map(|v| to_json(v, json_typed)).collect()),
        Db::Struct(fields) => {
            Value::Object(fields.iter().map(|(k, v)| (k.clone(), to_json(v.clone(), false))).collect::<Map<_, _>>())
        }
        Db::Map(entries) => {
            // String keys make an object; anything else a list of [key, value] pairs.
            let pairs: Vec<(Value, Value)> =
                entries.iter().map(|(k, v)| (to_json(k.clone(), false), to_json(v.clone(), false))).collect();
            if pairs.iter().all(|(k, _)| k.is_string()) {
                Value::Object(pairs.into_iter().map(|(k, v)| (k.as_str().unwrap_or_default().to_string(), v)).collect())
            } else {
                Value::Array(pairs.into_iter().map(|(k, v)| json!([k, v])).collect())
            }
        }
        Db::Union(inner) => to_json(*inner, json_typed),
        // Types added to DuckDB later: their debug text.
        other => Value::String(format!("{other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions() {
        assert_eq!(to_json(Db::HugeInt(5), false), json!(5));
        assert_eq!(to_json(Db::HugeInt(i128::MAX), false), json!(i128::MAX as f64));
        assert_eq!(to_json(Db::Double(f64::NAN), false), Value::Null);
        assert_eq!(
            to_json(Db::Timestamp(TimeUnit::Microsecond, 1_760_000_000_000_000), false),
            json!("2025-10-09T08:53:20Z")
        );
        assert_eq!(to_json(Db::Date32(1), false), json!("1970-01-02"));
        assert_eq!(to_json(Db::Text("{\"a\":1}".into()), true), json!({"a": 1}));
        assert_eq!(to_json(Db::Text("{\"a\":1}".into()), false), json!("{\"a\":1}"));
        assert_eq!(to_json(Db::Blob(vec![1, 2]), false), json!({"$bytes": "AQI="}));
    }
}
