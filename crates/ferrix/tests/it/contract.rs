//! Checks on the contract surface itself: registration through linkme, paths, TS signatures, auto traits.

use ferrix::__private::{FunctionKind, Visibility};
use ferrix::{ActionCtx, FunctionRef, MutationCtx, QueryCtx};

use crate::common::{items, todos};

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn contexts_are_send_sync() {
    assert_send_sync::<QueryCtx>();
    assert_send_sync::<MutationCtx>();
    assert_send_sync::<ActionCtx>();
}

#[test]
fn registry_lists_hand_expanded_functions() {
    let list = ferrix::registry().unwrap().list();
    let find = |path: &str| list.iter().find(|f| f.path == path).unwrap_or_else(|| panic!("{path} not registered"));

    assert_eq!(todos::list::path(), "common/todos:list");
    let f = find("common/todos:list");
    assert_eq!((f.kind, f.visibility), (FunctionKind::Query, Visibility::Public));
    assert_eq!(f.args_ts, "{ done?: boolean | null, }");
    assert_eq!(f.output_ts, "Array<Doc<\"todos\">>");

    let f = find("common/todos:add");
    assert_eq!(f.kind, FunctionKind::Mutation);
    assert_eq!(f.output_ts, "Id<\"todos\">");

    let f = find("common/todos:get");
    assert_eq!(f.args_ts, "{ id: Id<\"todos\">, }");
    assert_eq!(f.output_ts, "Doc<\"todos\"> | null");

    assert_eq!(find("common/todos:add_many").kind, FunctionKind::Action);
    assert_eq!(find("common/todos:count").visibility, Visibility::Internal);
    assert_eq!(items::scan::path(), "common/items:scan");

    let tables = ferrix::runtime::registered_tables();
    assert!(tables.iter().any(|t| t.name == "todos"));
    assert!(tables.iter().any(|t| t.name == "items"));
}
