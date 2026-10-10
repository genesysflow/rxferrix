//! Result diffs across languages: the server diffs in Rust (`ferrix::sync::diff`), clients apply in TypeScript
//! (`packages/client/src/diff.ts`). This writes a deterministic set of (old, new, diff) cases to
//! `packages/client/test/fixtures/diffs.json`, which the client's tests apply. The committed file must match what this
//! generates: run with `FERRIX_UPDATE_FIXTURES=1` after changing the diff algorithm.

use std::path::Path;

use ferrix::sync::diff;
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use serde_json::{Value, json};

fn leaf(rng: &mut StdRng) -> Value {
    match rng.random_range(0..6) {
        0 => Value::Null,
        1 => json!(rng.random_bool(0.5)),
        2 => json!(rng.random_range(-50..50)),
        3 => json!(rng.random_range(-50..50) as f64 / 4.0),
        4 => json!(["", "a", "é ✓", "quote \" and \\", "a longer string value, about forty chars"][rng.random_range(0..5)]),
        _ => json!(format!("s{}", rng.random_range(0..20))),
    }
}

fn value(rng: &mut StdRng, depth: u32) -> Value {
    if depth == 0 || rng.random_bool(0.3) {
        return leaf(rng);
    }
    if rng.random_bool(0.5) {
        Value::Array((0..rng.random_range(0..10)).map(|_| value(rng, depth - 1)).collect())
    } else {
        let keys = ["_id", "_creationTime", "a", "b", "title", "page", "isDone"];
        Value::Object((0..rng.random_range(0..5)).map(|_| (keys[rng.random_range(0..keys.len())].to_string(), value(rng, depth - 1))).collect())
    }
}

/// `v` with a few random edits, at any depth.
fn edit(rng: &mut StdRng, v: &Value) -> Value {
    match v {
        Value::Array(items) => {
            let mut items = items.clone();
            for _ in 0..rng.random_range(0..4) {
                let at = rng.random_range(0..=items.len());
                match rng.random_range(0..4) {
                    0 => items.insert(at, value(rng, 2)),
                    1 if at < items.len() => {
                        items.remove(at);
                    }
                    2 if at < items.len() => items[at] = edit(rng, &items[at]),
                    _ => {}
                }
            }
            Value::Array(items)
        }
        Value::Object(map) => {
            let mut map = map.clone();
            let keys: Vec<String> = map.keys().cloned().collect();
            for _ in 0..rng.random_range(0..3) {
                match rng.random_range(0..3) {
                    0 => {
                        map.insert(format!("k{}", rng.random_range(0..4)), value(rng, 2));
                    }
                    1 if !keys.is_empty() => {
                        map.remove(&keys[rng.random_range(0..keys.len())]);
                    }
                    _ if !keys.is_empty() => {
                        let k = &keys[rng.random_range(0..keys.len())];
                        if let Some(v) = map.get(k).cloned() {
                            map.insert(k.clone(), edit(rng, &v));
                        }
                    }
                    _ => {}
                }
            }
            Value::Object(map)
        }
        _ => value(rng, 1),
    }
}

fn post(i: i64) -> Value {
    json!({ "_id": format!("p{i}"), "_creationTime": 1000.5 + i as f64, "title": "hello world", "score": i, "done": false })
}

fn cases() -> Vec<Value> {
    let mut pairs: Vec<(Value, Value)> = Vec::new();
    // The shapes list queries take: a post added at the top (the oldest drops off), one updated, one deleted.
    let list: Vec<Value> = (0..20).map(post).collect();
    let mut added = list.clone();
    added.insert(0, post(99));
    added.pop();
    let mut updated = list.clone();
    updated[5]["done"] = json!(true);
    let mut deleted = list.clone();
    deleted.remove(10);
    for new in [added, updated, deleted] {
        pairs.push((Value::Array(list.clone()), Value::Array(new)));
    }
    pairs.push((json!({ "page": list.clone(), "isDone": false }), json!({ "page": list[1..].to_vec(), "isDone": true })));
    let mut rng = StdRng::seed_from_u64(7);
    let mut cases = Vec::new();
    let add = |old: Value, new: Value, cases: &mut Vec<Value>| {
        if let Some(d) = diff::diff(&old, &new) {
            assert_eq!(diff::apply(&old, &d).unwrap(), new, "Rust apply: old {old}, diff {d}");
            cases.push(json!({ "old": old, "new": new, "diff": d }));
        }
    };
    for (old, new) in pairs {
        add(old, new, &mut cases);
    }
    // Random pairs of related (mostly) or unrelated values; pairs that are replaced rather than diffed don't count.
    while cases.len() < 300 {
        let old = value(&mut rng, 3);
        let new = if rng.random_bool(0.8) { edit(&mut rng, &old) } else { value(&mut rng, 3) };
        add(old, new, &mut cases);
    }
    cases
}

#[test]
fn client_diff_fixtures_are_current() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/client/test/fixtures/diffs.json");
    let cases = cases();
    // One case per line: compact, and a changed case is a one-line diff.
    let lines: Vec<String> = cases.iter().map(|c| serde_json::to_string(c).unwrap()).collect();
    let text = format!("[\n{}\n]\n", lines.join(",\n"));
    if std::env::var_os("FERRIX_UPDATE_FIXTURES").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &text).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == text,
        "{} is stale: run `FERRIX_UPDATE_FIXTURES=1 cargo test -p ferrix --test it client_diff_fixtures`",
        path.display()
    );
}
