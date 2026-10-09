//! Memcomparable (order-preserving) key encoding for index keys.
//!
//! Every value starts with a type tag, so values of different types order by tag:
//! `Undefined (missing) < Null < Int64 < Float64 < Bool < String < Array < Object`.
//! Each encoding is *self-delimiting* (no encoding is a proper prefix of another), which gives two properties the
//! query engine relies on:
//!
//! 1. Concatenations compare field by field: `enc(a1) ++ enc(a2) < enc(b1) ++ enc(b2)` exactly when `(a1, a2) <
//!    (b1, b2)` lexicographically.
//! 2. All keys whose first fields equal `v1..vk` form one contiguous byte range: the keys that start with the prefix
//!    `enc(v1) ++ … ++ enc(vk)`. That range ends at [`prefix_successor`] of the prefix.
//!
//! An index key is `enc(field_1) ++ … ++ enc(field_n) ++ id_bytes(16)`; the trailing id makes keys unique.

use serde_json::{Map, Number, Value};

pub const TAG_UNDEFINED: u8 = 0x01;
pub const TAG_NULL: u8 = 0x02;
pub const TAG_INT: u8 = 0x03;
pub const TAG_FLOAT: u8 = 0x04;
pub const TAG_BOOL: u8 = 0x05;
pub const TAG_STRING: u8 = 0x06;
pub const TAG_ARRAY: u8 = 0x07;
pub const TAG_OBJECT: u8 = 0x08;

const SIGN: u64 = 1 << 63;

/// A JSON number classified the way index keys see it.
pub enum NumberKind {
    Int(i64),
    Float(f64),
}

/// Ints are numbers that fit `i64`; everything else (including `u64 > i64::MAX`) is a float.
pub fn classify_number(n: &Number) -> NumberKind {
    match n.as_i64() {
        Some(i) => NumberKind::Int(i),
        None => NumberKind::Float(n.as_f64().unwrap_or(f64::NAN)),
    }
}

/// -0.0 becomes 0.0 and every NaN becomes the canonical NaN, so equal-looking floats encode identically.
pub fn normalize_float(f: f64) -> f64 {
    if f.is_nan() {
        f64::NAN
    } else if f == 0.0 {
        0.0
    } else {
        f
    }
}

fn float_bits(f: f64) -> u64 {
    let bits = normalize_float(f).to_bits();
    if bits & SIGN != 0 { !bits } else { bits ^ SIGN }
}

fn float_from_bits(enc: u64) -> f64 {
    let bits = if enc & SIGN != 0 { enc ^ SIGN } else { !enc };
    f64::from_bits(bits)
}

fn encode_str_body(s: &[u8], out: &mut Vec<u8>) {
    for &b in s {
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0x00, 0x00]);
}

/// Append the encoding of `v` to `out`.
pub fn encode_value(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.push(TAG_NULL),
        Value::Number(n) => match classify_number(n) {
            NumberKind::Int(i) => {
                out.push(TAG_INT);
                out.extend_from_slice(&((i as u64) ^ SIGN).to_be_bytes());
            }
            NumberKind::Float(f) => {
                out.push(TAG_FLOAT);
                out.extend_from_slice(&float_bits(f).to_be_bytes());
            }
        },
        Value::Bool(b) => {
            out.push(TAG_BOOL);
            out.push(*b as u8);
        }
        Value::String(s) => {
            out.push(TAG_STRING);
            encode_str_body(s.as_bytes(), out);
        }
        Value::Array(items) => {
            out.push(TAG_ARRAY);
            for item in items {
                encode_value(item, out);
            }
            out.push(0x00);
        }
        Value::Object(map) => {
            out.push(TAG_OBJECT);
            // serde_json's Map is a BTreeMap (no `preserve_order`), so iteration is already in sorted key order;
            // sort explicitly anyway so the encoding never depends on that feature flag.
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            for (k, val) in entries {
                out.push(TAG_STRING);
                encode_str_body(k.as_bytes(), out);
                encode_value(val, out);
            }
            out.push(0x00);
        }
    }
}

/// Append the encoding of an optional value; `None` is the "missing field" (Undefined) value.
pub fn encode_opt(v: Option<&Value>, out: &mut Vec<u8>) {
    match v {
        None => out.push(TAG_UNDEFINED),
        Some(v) => encode_value(v, out),
    }
}

/// Encode one value into a fresh buffer.
pub fn encode(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_value(v, &mut out);
    out
}

/// The index key of a document: its indexed field values (looked up by JSON name, missing = Undefined) followed by
/// the 16 id bytes.
pub fn index_key(doc: &Map<String, Value>, fields: &[String], id: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    for f in fields {
        encode_opt(doc.get(f), &mut out);
    }
    out.extend_from_slice(id);
    out
}

/// The smallest byte string greater than every string that starts with `prefix`, or `None` if there is none (an
/// empty prefix, or one made only of 0xFF bytes): then the range is unbounded above.
pub fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    while let Some(last) = out.pop() {
        if last != 0xFF {
            out.push(last + 1);
            return Some(out);
        }
    }
    None
}

/// The smallest byte string strictly greater than `key`.
pub fn immediate_successor(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len() + 1);
    out.extend_from_slice(key);
    out.push(0x00);
    out
}

/// A decoded key component: `None` = Undefined (missing field).
pub type Decoded = Option<Value>;

/// Decode one value from the front of `bytes`; returns the value and the remaining bytes.
pub fn decode_value(bytes: &[u8]) -> Result<(Decoded, &[u8]), String> {
    let (&tag, rest) = bytes.split_first().ok_or("unexpected end of key")?;
    match tag {
        TAG_UNDEFINED => Ok((None, rest)),
        TAG_NULL => Ok((Some(Value::Null), rest)),
        TAG_INT => {
            let (raw, rest) = take8(rest)?;
            Ok((Some(Value::from((raw ^ SIGN) as i64)), rest))
        }
        TAG_FLOAT => {
            let (raw, rest) = take8(rest)?;
            let f = float_from_bits(raw);
            let v = Number::from_f64(f).map(Value::Number).ok_or("non-finite float in key")?;
            Ok((Some(v), rest))
        }
        TAG_BOOL => {
            let (&b, rest) = rest.split_first().ok_or("unexpected end of key")?;
            Ok((Some(Value::Bool(b != 0)), rest))
        }
        TAG_STRING => {
            let (s, rest) = decode_str_body(rest)?;
            Ok((Some(Value::String(s)), rest))
        }
        TAG_ARRAY => {
            let mut items = Vec::new();
            let mut rest = rest;
            loop {
                match rest.first() {
                    None => return Err("unterminated array".into()),
                    Some(0x00) => return Ok((Some(Value::Array(items)), &rest[1..])),
                    Some(_) => {
                        let (v, r) = decode_value(rest)?;
                        items.push(v.ok_or("undefined inside array")?);
                        rest = r;
                    }
                }
            }
        }
        TAG_OBJECT => {
            let mut map = Map::new();
            let mut rest = rest;
            loop {
                match rest.first() {
                    None => return Err("unterminated object".into()),
                    Some(0x00) => return Ok((Some(Value::Object(map)), &rest[1..])),
                    Some(&TAG_STRING) => {
                        let (k, r) = decode_str_body(&rest[1..])?;
                        let (v, r) = decode_value(r)?;
                        map.insert(k, v.ok_or("undefined inside object")?);
                        rest = r;
                    }
                    Some(t) => return Err(format!("bad object key tag {t:#x}")),
                }
            }
        }
        t => Err(format!("bad tag {t:#x}")),
    }
}

fn take8(bytes: &[u8]) -> Result<(u64, &[u8]), String> {
    if bytes.len() < 8 {
        return Err("unexpected end of key".into());
    }
    let (head, rest) = bytes.split_at(8);
    Ok((u64::from_be_bytes(head.try_into().unwrap()), rest))
}

fn decode_str_body(bytes: &[u8]) -> Result<(String, &[u8]), String> {
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        match bytes.get(i) {
            None => return Err("unterminated string".into()),
            Some(0x00) => match bytes.get(i + 1) {
                Some(0x00) => break,
                Some(0xFF) => {
                    out.push(0);
                    i += 2;
                }
                _ => return Err("bad string escape".into()),
            },
            Some(&b) => {
                out.push(b);
                i += 1;
            }
        }
    }
    let s = String::from_utf8(out).map_err(|e| e.to_string())?;
    Ok((s, &bytes[i + 2..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;
    use std::cmp::Ordering;

    // ---- Reference order, defined independently of the byte encoding. ----

    fn rank(v: &Option<Value>) -> u8 {
        match v {
            None => 0,
            Some(Value::Null) => 1,
            Some(Value::Number(n)) => match classify_number(n) {
                NumberKind::Int(_) => 2,
                NumberKind::Float(_) => 3,
            },
            Some(Value::Bool(_)) => 4,
            Some(Value::String(_)) => 5,
            Some(Value::Array(_)) => 6,
            Some(Value::Object(_)) => 7,
        }
    }

    fn value_cmp(a: &Option<Value>, b: &Option<Value>) -> Ordering {
        let (ra, rb) = (rank(a), rank(b));
        if ra != rb {
            return ra.cmp(&rb);
        }
        match (a, b) {
            (None, None) | (Some(Value::Null), Some(Value::Null)) => Ordering::Equal,
            (Some(Value::Number(x)), Some(Value::Number(y))) => match (classify_number(x), classify_number(y)) {
                (NumberKind::Int(i), NumberKind::Int(j)) => i.cmp(&j),
                (NumberKind::Float(f), NumberKind::Float(g)) => normalize_float(f).total_cmp(&normalize_float(g)),
                _ => unreachable!(),
            },
            (Some(Value::Bool(x)), Some(Value::Bool(y))) => x.cmp(y),
            (Some(Value::String(x)), Some(Value::String(y))) => x.as_bytes().cmp(y.as_bytes()),
            (Some(Value::Array(x)), Some(Value::Array(y))) => {
                for (p, q) in x.iter().zip(y) {
                    let c = value_cmp(&Some(p.clone()), &Some(q.clone()));
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                x.len().cmp(&y.len())
            }
            (Some(Value::Object(x)), Some(Value::Object(y))) => {
                let mut xs: Vec<_> = x.iter().collect();
                let mut ys: Vec<_> = y.iter().collect();
                xs.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
                ys.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
                for ((kx, vx), (ky, vy)) in xs.iter().zip(&ys) {
                    let c = kx.as_bytes().cmp(ky.as_bytes());
                    if c != Ordering::Equal {
                        return c;
                    }
                    let c = value_cmp(&Some((*vx).clone()), &Some((*vy).clone()));
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                xs.len().cmp(&ys.len())
            }
            _ => unreachable!(),
        }
    }

    fn enc_opt(v: &Option<Value>) -> Vec<u8> {
        let mut out = Vec::new();
        encode_opt(v.as_ref(), &mut out);
        out
    }

    // ---- Generators. ----

    fn arb_string() -> impl Strategy<Value = String> {
        // Bias towards the interesting bytes: NUL (escaped), 0x7F, and multi-byte chars whose UTF-8 contains 0xFF-ish
        // high bytes.
        prop::collection::vec(
            prop_oneof![Just('\0'), Just('a'), Just('b'), Just('\u{7f}'), Just('\u{ff}'), Just('\u{10ffff}'), any::<char>()],
            0..6,
        )
        .prop_map(|cs| cs.into_iter().collect())
    }

    fn arb_float() -> impl Strategy<Value = f64> {
        prop_oneof![
            Just(0.0),
            Just(-0.0),
            Just(1.0),
            Just(-1.0),
            Just(f64::MIN_POSITIVE),
            Just(-f64::MIN_POSITIVE),
            Just(f64::MAX),
            Just(f64::MIN),
            Just(1e-310),
            any::<f64>().prop_filter("finite", |f| f.is_finite()),
        ]
    }

    fn arb_leaf() -> impl Strategy<Value = Value> {
        prop_oneof![
            Just(Value::Null),
            prop_oneof![Just(0i64), Just(-1), Just(1), Just(i64::MIN), Just(i64::MAX), any::<i64>()]
                .prop_map(Value::from),
            // u64 beyond i64::MAX: treated as a float.
            (i64::MAX as u64 + 1..=u64::MAX).prop_map(Value::from),
            arb_float().prop_map(|f| Value::Number(Number::from_f64(f).unwrap())),
            any::<bool>().prop_map(Value::Bool),
            arb_string().prop_map(Value::String),
        ]
    }

    fn arb_value() -> impl Strategy<Value = Value> {
        arb_leaf().prop_recursive(3, 24, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                prop::collection::btree_map(arb_string(), inner, 0..4)
                    .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
    }

    fn arb_opt_value() -> impl Strategy<Value = Option<Value>> {
        prop_oneof![1 => Just(None), 12 => arb_value().prop_map(Some)]
    }

    /// Values that round-trip exactly (no u64 > i64::MAX, no -0.0).
    fn is_roundtrippable(v: &Value) -> bool {
        match v {
            Value::Number(n) => match classify_number(n) {
                NumberKind::Int(_) => true,
                NumberKind::Float(f) => n.is_f64() && !(f == 0.0 && f.is_sign_negative()),
            },
            Value::Array(a) => a.iter().all(is_roundtrippable),
            Value::Object(o) => o.values().all(is_roundtrippable),
            _ => true,
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4000))]

        #[test]
        fn encoding_preserves_order(a in arb_opt_value(), b in arb_opt_value()) {
            prop_assert_eq!(enc_opt(&a).cmp(&enc_opt(&b)), value_cmp(&a, &b), "a={:?} b={:?}", a, b);
        }

        #[test]
        fn tuples_compare_lexicographically(
            a1 in arb_opt_value(), a2 in arb_opt_value(), b1 in arb_opt_value(), b2 in arb_opt_value()
        ) {
            let mut ka = enc_opt(&a1); ka.extend(enc_opt(&a2));
            let mut kb = enc_opt(&b1); kb.extend(enc_opt(&b2));
            let expected = value_cmp(&a1, &b1).then_with(|| value_cmp(&a2, &b2));
            prop_assert_eq!(ka.cmp(&kb), expected);
        }

        #[test]
        fn prefix_range_contains_exactly_matching_keys(
            p in arb_opt_value(), q in arb_opt_value(), rest in arb_opt_value(), id in any::<[u8; 16]>()
        ) {
            // Key = enc(q) ++ enc(rest) ++ id; it is in the range of prefix enc(p) iff q == p.
            let prefix = enc_opt(&p);
            let mut key = enc_opt(&q);
            key.extend(enc_opt(&rest));
            key.extend_from_slice(&id);
            let in_range = key.as_slice() >= prefix.as_slice()
                && prefix_successor(&prefix).is_none_or(|end| key < end);
            prop_assert_eq!(in_range, value_cmp(&p, &q) == Ordering::Equal);
        }

        #[test]
        fn decode_roundtrip(v in arb_value().prop_filter("exact", is_roundtrippable), tail in any::<[u8; 4]>()) {
            let mut bytes = encode(&v);
            bytes.extend_from_slice(&tail);
            let (decoded, rest) = decode_value(&bytes).unwrap();
            prop_assert_eq!(decoded, Some(v));
            prop_assert_eq!(rest, &tail[..]);
        }
    }

    #[test]
    fn type_tags_order() {
        let vals = [None, Some(json!(null)), Some(json!(5)), Some(json!(1.5)), Some(json!(false)), Some(json!("")), Some(json!([])), Some(json!({}))];
        for w in vals.windows(2) {
            assert!(enc_opt(&w[0]) < enc_opt(&w[1]), "{:?} < {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn negative_zero_and_large_u64() {
        assert_eq!(encode(&json!(-0.0)), encode(&json!(0.0)));
        // u64::MAX is a float (it doesn't fit i64) and sorts after every int... as a float, after all ints by tag.
        assert_eq!(encode(&json!(u64::MAX))[0], TAG_FLOAT);
        assert!(encode(&json!(i64::MAX)) < encode(&json!(u64::MAX)));
    }

    #[test]
    fn successors() {
        assert_eq!(prefix_successor(&[]), None);
        assert_eq!(prefix_successor(&[0xFF, 0xFF]), None);
        assert_eq!(prefix_successor(&[0x01, 0xFF]), Some(vec![0x02]));
        assert_eq!(prefix_successor(&[0x01, 0x02]), Some(vec![0x01, 0x03]));
        assert_eq!(prefix_successor(&[0x00]), Some(vec![0x01]));
        assert_eq!(immediate_successor(&[]), vec![0x00]);
        assert_eq!(immediate_successor(&[0xFF]), vec![0xFF, 0x00]);

        // Ids made of 0xFF bytes: a point range [id, id ++ 0x00) contains the id and nothing else.
        let id = [0xFFu8; 16];
        let succ = immediate_successor(&id);
        assert!(id.as_slice() < succ.as_slice());
        let mut bigger = id.to_vec();
        bigger.push(0x00);
        bigger.push(0x00);
        assert!(bigger.as_slice() >= succ.as_slice(), "extensions of id beyond id++0x00 are outside");

        // A prefix that is an encoded value followed by an all-0xFF id: keys with that value are inside.
        let mut key = encode(&json!("x"));
        let prefix = key.clone();
        key.extend_from_slice(&id);
        let end = prefix_successor(&prefix).unwrap();
        assert!(key.as_slice() >= prefix.as_slice() && key < end);
        // The next string value is outside.
        let mut next = encode(&json!("x\u{0}"));
        next.extend_from_slice(&[0u8; 16]);
        assert!(next >= end);
    }

    #[test]
    fn index_key_missing_fields_are_undefined() {
        let doc = json!({"a": 1}).as_object().unwrap().clone();
        let k = index_key(&doc, &["a".into(), "b".into()], &[7; 16]);
        let mut expected = encode(&json!(1));
        expected.push(TAG_UNDEFINED);
        expected.extend_from_slice(&[7; 16]);
        assert_eq!(k, expected);
    }
}
