//! The dev-mode determinism check ([`AppOptions::determinism_check`](crate::runtime::AppOptions::determinism_check)):
//! now and then a query is run a second time in the same snapshot, and a warning is logged if the result differs.
//!
//! A query's result must depend only on its arguments, the caller's identity and the data it reads: subscriptions,
//! the query cache and result diffs all assume a rerun at the same ts gives the same result. A query that reads
//! `SystemTime::now()`, draws random numbers or returns items in `HashMap` order breaks that without failing: its
//! subscribers get updates for nothing, and cached results are wrong. The warning goes to the server log and, as a
//! log line of the run, to the execution log and (in `--dev`) to the client.
//!
//! Runs that read the time (`ctx.now()`, signed URLs) are not checked: their results are known to depend on it (and
//! are never cached). A rerun is not an execution: it is neither logged nor counted in metrics, and its log lines are
//! dropped (stdout still shows them).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::error::QueryTimeout;

/// When each query was last checked (keys are registered query paths, so the map stays small).
pub(crate) struct DeterminismCheck {
    every: Duration,
    last: Mutex<HashMap<String, Instant>>,
}

impl DeterminismCheck {
    pub(crate) fn new(every: Duration) -> DeterminismCheck {
        DeterminismCheck { every, last: Mutex::new(HashMap::new()) }
    }

    /// True if `path` was not checked in the last `every`. Doesn't count as a check (see [`DeterminismCheck::start`]).
    pub(crate) fn due(&self, path: &str) -> bool {
        let last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        last.get(path).is_none_or(|t| t.elapsed() >= self.every)
    }

    /// Record a check of `path` starting now.
    pub(crate) fn start(&self, path: &str) {
        self.last.lock().unwrap_or_else(|e| e.into_inner()).insert(path.to_string(), Instant::now());
    }
}

/// What a run gave, as it reaches clients: the JSON text of the value, or the error message. `None` for a timeout,
/// which says nothing about determinism.
fn outcome(result: &crate::Result<Value>) -> Option<String> {
    match result {
        Ok(value) => Some(serde_json::to_string(value).unwrap_or_default()),
        Err(e) if e.downcast_ref::<QueryTimeout>().is_some() => None,
        Err(e) => Some(format!("error: {e:#}")),
    }
}

/// The warning for a query whose rerun at `ts` gave `second` after `first`, or `None` if they agree.
pub(crate) fn compare(
    path: &str,
    ts: u64,
    first: &crate::Result<Value>,
    second: &crate::Result<Value>,
) -> Option<String> {
    let (a, b) = (outcome(first)?, outcome(second)?);
    if a == b {
        return None;
    }
    let at = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    Some(format!(
        "query `{path}` gave a different result when run again at the same ts ({ts}), at byte {at}: {} then {}. A \
         query's result must depend only on its arguments and the data it reads: SystemTime::now(), random numbers \
         or HashMap order make its subscribers rerun and resend for nothing, and its cached results wrong. Use \
         ctx.now(), and a BTreeMap or a sort.",
        excerpt(&a, at),
        excerpt(&b, at)
    ))
}

/// About 40 bytes of `s` around byte `at`, with `…` where cut.
fn excerpt(s: &str, at: usize) -> String {
    const BEFORE: usize = 16;
    const AFTER: usize = 24;
    let mut start = at.saturating_sub(BEFORE).min(s.len());
    while !s.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (at + AFTER).min(s.len());
    while !s.is_char_boundary(end) {
        end += 1;
    }
    let head = if start > 0 { "…" } else { "" };
    let tail = if end < s.len() { "…" } else { "" };
    format!("`{head}{}{tail}`", &s[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn same_results_agree() {
        assert_eq!(compare("q", 1, &Ok(json!([1, 2])), &Ok(json!([1, 2]))), None);
        let err = || Err(anyhow::anyhow!("boom"));
        assert_eq!(compare("q", 1, &err(), &err()), None);
    }

    #[test]
    fn different_results_are_reported_with_where_they_differ() {
        let w = compare("items:list", 7, &Ok(json!(["apple", "pear"])), &Ok(json!(["pear", "apple"]))).unwrap();
        assert!(
            w.starts_with("query `items:list` gave a different result when run again at the same ts (7), at byte 2"),
            "{w}"
        );
        assert!(w.contains(r#"`["apple","pear"]` then `["pear","apple"]`"#), "{w}");
        let w = compare("q", 1, &Ok(json!(1)), &Err(anyhow::anyhow!("boom"))).unwrap();
        assert!(w.contains("`1` then `error: boom`"), "{w}");
    }

    #[test]
    fn timeouts_are_not_compared() {
        let timeout = || Err(QueryTimeout { path: "q".into(), timeout: Duration::from_secs(1) }.into());
        assert_eq!(compare("q", 1, &Ok(json!(1)), &timeout()), None);
        assert_eq!(compare("q", 1, &timeout(), &Ok(json!(1))), None);
    }

    #[test]
    fn excerpts_cut_at_char_boundaries() {
        let s = format!("{}é{}", "a".repeat(30), "b".repeat(40));
        let e = excerpt(&s, 31);
        assert!(e.starts_with("`…") && e.ends_with("…`"), "{e}");
        assert_eq!(excerpt("short", 2), "`short`");
    }

    #[test]
    fn due_once_per_interval() {
        let check = DeterminismCheck::new(Duration::from_secs(3600));
        assert!(check.due("a"));
        check.start("a");
        assert!(!check.due("a"));
        assert!(check.due("b"));
        let always = DeterminismCheck::new(Duration::ZERO);
        always.start("a");
        assert!(always.due("a"));
    }
}
