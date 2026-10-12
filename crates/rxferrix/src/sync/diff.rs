//! Result diffs: what a client needs to turn the result it has into the new one (docs/PROTOCOL.md, "Result diffs").
//!
//! A diff is a JSON value, applied to the client's copy of the previous result:
//!
//! * `{"$r": value}`: replace with `value`.
//! * `{"$a": [op, …]}`: edit an array. The ops walk the old array from the start: a positive integer `n` keeps the
//!   next `n` elements, a negative integer `-n` deletes them, an array inserts its elements, and `{"$p": diff}` patches
//!   the next element with `diff`. Elements after the last op are kept.
//! * `{"$o": {"s": {key: value, …}, "d": [key, …], "p": {key: diff, …}}}`: edit an object: set, delete and patch keys
//!   (each part optional).
//!
//! Arrays are compared with Myers' algorithm over their elements (hashed, then confirmed equal), so the usual change to a
//! list query (a document added, removed or updated) costs about one document, not the whole list. A changed element
//! becomes a patch when that is shorter than the element itself. [`diff`] gives up (`None`) when the edit distance is
//! large; the caller then sends the full value.

use std::hash::{Hash, Hasher};

use serde_json::{Map, Value, json};

/// Arrays further apart than this many inserted plus deleted elements are replaced instead of diffed.
const MAX_EDITS: usize = 256;

/// The diff from `old` to `new`, or `None` if replacing is clearly better (an unrelated value, or arrays too far apart).
pub fn diff(old: &Value, new: &Value) -> Option<Value> {
    match (old, new) {
        (Value::Array(a), Value::Array(b)) => array_diff(a, b),
        (Value::Object(a), Value::Object(b)) => Some(object_diff(a, b)),
        _ => None,
    }
}

/// Apply `diff` to `old` (the reference implementation of what clients do). Errors if the diff doesn't fit `old`:
/// the client's copy is not what the server diffed against.
pub fn apply(old: &Value, diff: &Value) -> anyhow::Result<Value> {
    let Value::Object(d) = diff else { anyhow::bail!("a diff must be an object") };
    if let Some(v) = d.get("$r") {
        return Ok(v.clone());
    }
    if let Some(Value::Array(ops)) = d.get("$a") {
        let Value::Array(old) = old else { anyhow::bail!("array diff applied to a non-array") };
        let mut out = Vec::with_capacity(old.len());
        let mut i = 0usize;
        for op in ops {
            match op {
                Value::Number(n) => {
                    let n = n.as_i64().ok_or_else(|| anyhow::anyhow!("bad op {n}"))?;
                    let count = n.unsigned_abs() as usize;
                    anyhow::ensure!(n != 0 && i + count <= old.len(), "op {n} at {i} runs past {} elements", old.len());
                    if n > 0 {
                        out.extend_from_slice(&old[i..i + count]);
                    }
                    i += count;
                }
                Value::Array(items) => out.extend(items.iter().cloned()),
                Value::Object(p) => {
                    let sub = p.get("$p").ok_or_else(|| anyhow::anyhow!("bad array op {op}"))?;
                    let element = old.get(i).ok_or_else(|| anyhow::anyhow!("patch past the end"))?;
                    out.push(apply(element, sub)?);
                    i += 1;
                }
                _ => anyhow::bail!("bad array op {op}"),
            }
        }
        out.extend_from_slice(&old[i..]);
        return Ok(Value::Array(out));
    }
    if let Some(Value::Object(o)) = d.get("$o") {
        let Value::Object(old) = old else { anyhow::bail!("object diff applied to a non-object") };
        let mut out = old.clone();
        if let Some(Value::Array(keys)) = o.get("d") {
            for key in keys {
                let key = key.as_str().ok_or_else(|| anyhow::anyhow!("bad key {key}"))?;
                anyhow::ensure!(out.remove(key).is_some(), "deleting missing key {key}");
            }
        }
        if let Some(Value::Object(set)) = o.get("s") {
            for (k, v) in set {
                out.insert(k.clone(), v.clone());
            }
        }
        if let Some(Value::Object(patch)) = o.get("p") {
            for (k, sub) in patch {
                let current = old.get(k).ok_or_else(|| anyhow::anyhow!("patching missing key {k}"))?;
                out.insert(k.clone(), apply(current, sub)?);
            }
        }
        return Ok(Value::Object(out));
    }
    anyhow::bail!("unknown diff {diff}")
}

fn encoded_len(v: &Value) -> usize {
    serde_json::to_string(v).map_or(usize::MAX, |s| s.len())
}

/// A diff for a changed element or key value: a patch if one exists and is shorter than sending `new` whole.
fn patch_or_none(old: &Value, new: &Value) -> Option<Value> {
    let sub = diff(old, new)?;
    (encoded_len(&sub) < encoded_len(new)).then_some(sub)
}

fn object_diff(old: &Map<String, Value>, new: &Map<String, Value>) -> Value {
    let (mut set, mut del, mut patch) = (Map::new(), Vec::new(), Map::new());
    for key in old.keys() {
        if !new.contains_key(key) {
            del.push(Value::String(key.clone()));
        }
    }
    for (key, value) in new {
        match old.get(key) {
            Some(current) if current == value => {}
            Some(current) => match patch_or_none(current, value) {
                Some(sub) => {
                    patch.insert(key.clone(), sub);
                }
                None => {
                    set.insert(key.clone(), value.clone());
                }
            },
            None => {
                set.insert(key.clone(), value.clone());
            }
        }
    }
    let mut o = Map::new();
    if !set.is_empty() {
        o.insert("s".into(), Value::Object(set));
    }
    if !del.is_empty() {
        o.insert("d".into(), Value::Array(del));
    }
    if !patch.is_empty() {
        o.insert("p".into(), Value::Object(patch));
    }
    json!({ "$o": o })
}

fn element_hash(v: &Value) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    serde_json::to_vec(v).unwrap_or_default().hash(&mut h);
    h.finish()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edit {
    Keep,
    Delete,
    Insert,
}

/// The shortest edit script from `a` to `b` (Myers, O((N+M)·D)), or `None` past [`MAX_EDITS`].
fn edit_script(a: &[Value], b: &[Value]) -> Option<Vec<Edit>> {
    let (ha, hb): (Vec<u64>, Vec<u64>) = (a.iter().map(element_hash).collect(), b.iter().map(element_hash).collect());
    let eq = |x: usize, y: usize| ha[x] == hb[y] && a[x] == b[y];
    // Common prefix and suffix first: most changes touch a few elements at one end or in the middle.
    let mut start = 0;
    while start < a.len() && start < b.len() && eq(start, start) {
        start += 1;
    }
    let mut end = 0;
    while end < a.len() - start && end < b.len() - start && eq(a.len() - 1 - end, b.len() - 1 - end) {
        end += 1;
    }
    let (n, m) = (a.len() - start - end, b.len() - start - end);
    let max = (n + m).min(MAX_EDITS);
    let offset = max as isize;
    let mut v = vec![0usize; 2 * max + 2];
    let mut trace: Vec<Vec<usize>> = Vec::new();
    let mut found = false;
    'search: for d in 0..=max as isize {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let idx = (k + offset) as usize;
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) { v[idx + 1] } else { v[idx - 1] + 1 };
            let mut y = (x as isize - k) as usize;
            while x < n && y < m && eq(start + x, start + y) {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x >= n && y >= m {
                found = true;
                break 'search;
            }
            k += 2;
        }
    }
    if !found {
        return None;
    }
    // Walk the trace back from (n, m) to build the script.
    let mut edits = Vec::with_capacity(n + m);
    let (mut x, mut y) = (n as isize, m as isize);
    for d in (0..trace.len() as isize).rev() {
        let v = &trace[d as usize];
        let k = x - y;
        let idx = |k: isize| (k + offset) as usize;
        let prev_k = if k == -d || (k != d && v[idx(k - 1)] < v[idx(k + 1)]) { k + 1 } else { k - 1 };
        let prev_x = v[idx(prev_k)] as isize;
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            edits.push(Edit::Keep);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            edits.push(if x == prev_x { Edit::Insert } else { Edit::Delete });
        }
        x = prev_x;
        y = prev_y;
    }
    edits.reverse();
    let mut script = vec![Edit::Keep; start];
    script.extend(edits);
    script.extend(std::iter::repeat_n(Edit::Keep, end));
    Some(script)
}

fn array_diff(a: &[Value], b: &[Value]) -> Option<Value> {
    let script = edit_script(a, b)?;
    let mut ops: Vec<Value> = Vec::new();
    let (mut i, mut j, mut s) = (0usize, 0usize, 0usize);
    while s < script.len() {
        let run = script[s..].iter().take_while(|e| **e == script[s]).count();
        match script[s] {
            Edit::Keep => {
                ops.push(json!(run));
                i += run;
                j += run;
                s += run;
            }
            Edit::Delete | Edit::Insert => {
                // A run of deletes and inserts (in any order): pair them up front to back, patching where that is
                // shorter, then delete or insert the rest.
                let changes = script[s..].iter().take_while(|e| **e != Edit::Keep).count();
                let deletes = script[s..s + changes].iter().filter(|e| **e == Edit::Delete).count();
                let inserts = changes - deletes;
                let mut pending_insert: Vec<Value> = Vec::new();
                let mut pending_delete = 0i64;
                let flush = |ops: &mut Vec<Value>, del: &mut i64, ins: &mut Vec<Value>| {
                    if *del > 0 {
                        ops.push(json!(-*del));
                        *del = 0;
                    }
                    if !ins.is_empty() {
                        ops.push(Value::Array(std::mem::take(ins)));
                    }
                };
                for p in 0..deletes.max(inserts) {
                    match (p < deletes, p < inserts) {
                        (true, true) => match patch_or_none(&a[i + p], &b[j + p]) {
                            Some(sub) => {
                                flush(&mut ops, &mut pending_delete, &mut pending_insert);
                                ops.push(json!({ "$p": sub }));
                            }
                            None => {
                                pending_delete += 1;
                                pending_insert.push(b[j + p].clone());
                            }
                        },
                        (true, false) => pending_delete += 1,
                        (false, true) => pending_insert.push(b[j + p].clone()),
                        (false, false) => unreachable!(),
                    }
                }
                flush(&mut ops, &mut pending_delete, &mut pending_insert);
                i += deletes;
                j += inserts;
                s += changes;
            }
        }
    }
    // Trailing keeps are implied.
    if matches!(ops.last(), Some(Value::Number(n)) if n.as_i64().is_some_and(|n| n > 0)) {
        ops.pop();
    }
    debug_assert_eq!((i, j), (a.len(), b.len()));
    Some(json!({ "$a": ops }))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn roundtrip(old: &Value, new: &Value) -> Option<Value> {
        let d = diff(old, new)?;
        let applied = apply(old, &d).unwrap_or_else(|e| panic!("apply failed: {e}\nold {old}\nnew {new}\ndiff {d}"));
        assert_eq!(&applied, new, "old {old}\ndiff {d}");
        Some(d)
    }

    fn doc(id: &str, n: i64) -> Value {
        json!({ "_id": id, "_creationTime": 1.5, "title": "a post title of some length", "score": n, "done": false })
    }

    #[test]
    fn a_new_document_at_the_top_of_a_list_costs_one_document() {
        let old: Vec<Value> = (0..20).map(|i| doc(&format!("d{i}"), i)).collect();
        let mut new = old.clone();
        new.insert(0, doc("new", 99));
        new.pop();
        let d = roundtrip(&Value::Array(old), &Value::Array(new.clone())).unwrap();
        assert_eq!(d, json!({ "$a": [[doc("new", 99)], 19, -1] }));
    }

    #[test]
    fn an_updated_document_is_patched() {
        let old: Vec<Value> = (0..20).map(|i| doc(&format!("d{i}"), i)).collect();
        let mut new = old.clone();
        new[7]["done"] = json!(true);
        let d = roundtrip(&Value::Array(old), &Value::Array(new)).unwrap();
        assert_eq!(d, json!({ "$a": [7, { "$p": { "$o": { "s": { "done": true } } } }] }));
    }

    #[test]
    fn objects_and_nested_pages() {
        let old = json!({ "page": [doc("a", 1), doc("b", 2)], "isDone": false, "continueCursor": "x", "gone": 1 });
        let new = json!({ "page": [doc("a", 1), doc("c", 3)], "isDone": true, "continueCursor": "x", "added": [1] });
        roundtrip(&old, &new).unwrap();
        assert!(diff(&json!(1), &json!(2)).is_none());
        assert!(diff(&json!([1]), &json!({})).is_none());
    }

    #[test]
    fn far_apart_arrays_are_replaced() {
        let old: Vec<Value> = (0..400).map(|i| json!(i)).collect();
        let new: Vec<Value> = (1000..1400).map(|i| json!(i)).collect();
        assert!(diff(&Value::Array(old), &Value::Array(new)).is_none());
    }

    #[test]
    fn apply_rejects_diffs_that_do_not_fit() {
        assert!(apply(&json!([1, 2]), &json!({ "$a": [3] })).is_err());
        assert!(apply(&json!([1, 2]), &json!({ "$a": [-3] })).is_err());
        assert!(apply(&json!({ "a": 1 }), &json!({ "$o": { "d": ["b"] } })).is_err());
        assert!(apply(&json!({ "a": 1 }), &json!({ "$o": { "p": { "b": { "$r": 1 } } } })).is_err());
        assert!(apply(&json!(1), &json!({ "$a": [] })).is_err());
    }

    fn leaf() -> impl Strategy<Value = Value> {
        prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            (-5i64..5).prop_map(|n| json!(n)),
            "[a-c]{0,3}".prop_map(Value::String),
        ]
    }

    fn value() -> impl Strategy<Value = Value> {
        leaf().prop_recursive(4, 48, 6, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..8).prop_map(Value::Array),
                prop::collection::btree_map("[a-d]", inner, 0..5).prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
    }

    /// A value and an edited copy of it (so diffs between related values are exercised, not just unrelated ones).
    fn related() -> impl Strategy<Value = (Value, Value)> {
        (value(), value(), prop::collection::vec((0usize..8, 0usize..3), 0..6)).prop_map(|(old, extra, edits)| {
            let mut new = old.clone();
            for (at, kind) in edits {
                if let Value::Array(items) = &mut new {
                    let at = at.min(items.len());
                    match kind {
                        0 => items.insert(at, extra.clone()),
                        1 if at < items.len() => {
                            items.remove(at);
                        }
                        _ if at < items.len() => items[at] = extra.clone(),
                        _ => {}
                    }
                } else if let Value::Object(map) = &mut new {
                    let key = ["a", "b", "c", "d"][at % 4].to_string();
                    match kind {
                        0 => {
                            map.insert(key, extra.clone());
                        }
                        1 => {
                            map.remove(&key);
                        }
                        _ => {
                            if let Some(v) = map.get_mut(&key) {
                                *v = extra.clone();
                            }
                        }
                    }
                }
            }
            (old, new)
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        #[test]
        fn diff_then_apply_gives_the_new_value((old, new) in related()) {
            roundtrip(&old, &new);
        }

        #[test]
        fn unrelated_values_roundtrip_too(old in value(), new in value()) {
            roundtrip(&old, &new);
        }
    }
}
