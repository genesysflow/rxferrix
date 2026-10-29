use serde_json::json;

use super::*;

fn update(query_set: u64, ts: u64, removed: Vec<QueryId>, changed: &[(QueryId, i64)]) -> Update {
    let changed = changed
        .iter()
        .map(|(q, v)| {
            (
                *q,
                StateModification::QueryUpdated { query_id: *q, value: json!(v).into(), base: None, log_lines: vec![] },
            )
        })
        .collect();
    Update { query_set, identity: 0, ts, removed, changed }
}

#[test]
fn the_cache_answers_only_at_or_past_its_frontier() {
    let cache = QueryCache::new(true);
    let identity = Identity::default();
    let key = QueryKey { path: "q:f".into(), args: canonical_args(&json!({})).into(), scope: Scope::Any };
    let result = |n: i64| {
        let value = QueryValue::new(json!(n));
        Arc::new(CachedResult { hash: [0; 32], outcome: Ok(value), log_lines: vec![] })
    };
    cache.publish(key.clone(), 5, result(1));
    cache.frontier.store(7, Ordering::Release);
    let get = |latest| cache.get("q:f", &json!({}), &identity, latest).map(|h| h.value.value().clone());
    assert_eq!(get(7), Some(json!(1)), "valid from its run (5) through the frontier (7)");
    assert_eq!(get(6), Some(json!(1)));
    assert_eq!(get(8), None, "a commit past the frontier may have invalidated it");
    assert_eq!(get(4), None, "computed after the ts asked for");
    cache.unpublish(&key);
    assert_eq!(get(7), None);
    // Another identity's scope doesn't match a session-scoped entry.
    let mine = Identity { session_id: Some("s1".into()), ..Default::default() };
    let session_key = QueryKey { scope: Scope::of(AuthDeps::Session, &mine), ..key.clone() };
    cache.publish(session_key, 5, result(2));
    assert_eq!(cache.get("q:f", &json!({}), &mine, 7).map(|h| h.value.value().clone()), Some(json!(2)));
    assert!(cache.get("q:f", &json!({}), &Identity::default(), 7).is_none());
}

#[test]
fn mailbox_merges_updates_without_version_steps() {
    let (mailbox, mut doorbell) = Mailbox::new();
    assert!(mailbox.push(update(1, 5, vec![3], &[(1, 10)]), true));
    assert!(mailbox.push(update(1, 6, vec![], &[(1, 11), (2, 20)]), false));
    assert!(mailbox.push(update(1, 8, vec![], &[(2, 21)]), false));
    assert!(mailbox.push(update(2, 8, vec![1], &[]), true));
    assert!(mailbox.push(update(2, 9, vec![], &[(2, 22)]), false));
    assert!(doorbell.try_recv().is_ok());
    let (updates, overflowed) = mailbox.take();
    assert!(!overflowed);
    assert_eq!(updates, vec![update(1, 8, vec![3], &[(1, 11), (2, 21)]), update(2, 9, vec![1], &[(2, 22)])]);
    assert_eq!(
        updates[0].clone().into_modifications(),
        vec![
            StateModification::QueryRemoved { query_id: 3 },
            StateModification::QueryUpdated { query_id: 1, value: json!(11).into(), base: None, log_lines: vec![] },
            StateModification::QueryUpdated { query_id: 2, value: json!(21).into(), base: None, log_lines: vec![] },
        ]
    );
    assert!(mailbox.is_empty());
}

#[test]
fn mailbox_overflows_on_too_many_version_steps() {
    let (mailbox, _doorbell) = Mailbox::new();
    for v in 0..MAILBOX_LIMIT as u64 {
        assert!(mailbox.push(update(v + 1, 1, vec![], &[]), true));
    }
    // Commit-driven updates still merge...
    assert!(mailbox.push(update(MAILBOX_LIMIT as u64, 2, vec![], &[(1, 1)]), false));
    // ...but one more step is too many.
    assert!(!mailbox.push(update(MAILBOX_LIMIT as u64 + 1, 2, vec![], &[]), true));
    let (updates, overflowed) = mailbox.take();
    assert!(overflowed);
    assert_eq!(updates.len(), MAILBOX_LIMIT);
}

#[test]
fn scopes() {
    let anon = Identity::default();
    let alice = Identity { user_id: Some("a".into()), admin_id: None, session_id: Some("s1".into()) };
    let alice2 = Identity { session_id: Some("s2".into()), ..alice.clone() };
    assert_eq!(Scope::of(AuthDeps::None, &anon), Scope::of(AuthDeps::None, &alice));
    assert_ne!(Scope::of(AuthDeps::Principal, &anon), Scope::of(AuthDeps::Principal, &alice));
    assert_eq!(Scope::of(AuthDeps::Principal, &alice), Scope::of(AuthDeps::Principal, &alice2));
    assert_ne!(Scope::of(AuthDeps::Session, &alice), Scope::of(AuthDeps::Session, &alice2));
    for deps in [AuthDeps::None, AuthDeps::Principal, AuthDeps::Session] {
        assert_eq!(Scope::of(deps, &alice).deps(), deps);
    }
}
