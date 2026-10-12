//! Hand-written macro expansions (docs/CONTRACT.md §3) used by every integration test.
//!
//! `todos` spells out the expansions literally, exactly as the contract shows them. Everything else uses
//! `expand_fn!`, a `macro_rules!` stand-in that produces the same shape, to keep the boilerplate manageable.

// The contract's expansion names statics `__RXFERRIX_FN_<fn>` and binds `let out = ...` even for `()` outputs.
#![allow(dead_code, non_upper_case_globals, clippy::let_unit_value)]

pub mod items;
pub mod todos;

/// `impl SchedulableFunction` for mutations and actions, as the macros emit it.
#[macro_export]
macro_rules! schedulable_impl {
    (QueryFunction, $name:ident) => {};
    ($trait:ident, $name:ident) => {
        impl ::rxferrix::SchedulableFunction for $name {}
    };
}

/// Same output as `#[query]` / `#[mutation]` / `#[action]` (§3.2), with the generated names passed explicitly
/// (macro_rules can't concatenate identifiers).
#[macro_export]
macro_rules! expand_fn {
    ($handler:ident, $trait:ident, $vis:ident, $name:ident, $args:ident, $stat:ident,
     ($($arg:ident : $ty:ty),* $(,)?) -> $out:ty) => {
        #[allow(non_camel_case_types)]
        #[derive(::rxferrix::__private::serde::Serialize, ::rxferrix::__private::serde::Deserialize, ::rxferrix::__private::ts_rs::TS)]
        #[serde(crate = "::rxferrix::__private::serde", rename_all = "camelCase")]
        #[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase", optional_fields = nullable)]
        pub struct $args { $(pub $arg: $ty),* }

        impl ::core::convert::From<($($ty,)*)> for $args {
            fn from(($($arg,)*): ($($ty,)*)) -> Self { Self { $($arg),* } }
        }

        #[allow(non_camel_case_types, dead_code)]
        pub struct $name {}

        impl ::rxferrix::FunctionRef for $name {
            type Args = $args;
            type Output = $out;
            fn path() -> String { ::rxferrix::__private::function_path(module_path!(), stringify!($name)) }
        }
        impl ::rxferrix::$trait for $name {}
        $crate::schedulable_impl!($trait, $name);

        #[::rxferrix::__private::linkme::distributed_slice(::rxferrix::__private::FUNCTIONS)]
        #[linkme(crate = ::rxferrix::__private::linkme)]
        static $stat: ::rxferrix::__private::FunctionDef = ::rxferrix::__private::FunctionDef {
            module_path: module_path!(),
            name: stringify!($name),
            visibility: ::rxferrix::__private::Visibility::$vis,
            dev_only: false,
            handler: ::rxferrix::__private::Handler::$handler(|ctx, args| ::std::boxed::Box::pin(async move {
                #[allow(unused_variables)]
                let a: $args = ::rxferrix::__private::decode_args(stringify!($name), args)?;
                let out = $name(&ctx, $(a.$arg),*).await?;
                ::rxferrix::__private::encode_output(out)
            })),
            ts: || (::rxferrix::__private::ts_inline::<$args>(), ::rxferrix::__private::ts_output::<$out>()),
        };
    };
}

/// Checks a `Transition`'s wire form (the text a socket sends) against its values: every result diff, applied to what
/// the client had (`results`, before this Transition), must give the new value. Returns how many diffs it carried.
pub fn check_wire_diffs(
    msg: &rxferrix::sync::protocol::ServerMessage,
    results: &std::collections::BTreeMap<u64, Result<serde_json::Value, String>>,
) -> usize {
    use rxferrix::sync::protocol::{ServerMessage, StateModification};
    let ServerMessage::Transition { modifications, .. } = msg else { return 0 };
    let wire: serde_json::Value = serde_json::from_str(&msg.to_text()).expect("a Transition is valid JSON");
    let mut diffs = 0;
    for (m, w) in modifications.iter().zip(wire["modifications"].as_array().expect("modifications")) {
        let StateModification::QueryUpdated { query_id, value, .. } = m else { continue };
        assert_eq!(w["queryId"], serde_json::json!(query_id));
        match w.get("diff") {
            Some(diff) => {
                let Some(Ok(had)) = results.get(query_id) else {
                    panic!("query {query_id}: a diff, but the client has no result to apply it to")
                };
                let applied = rxferrix::sync::diff::apply(had, diff)
                    .unwrap_or_else(|e| panic!("query {query_id}: diff {diff} doesn't apply to {had}: {e}"));
                assert_eq!(&applied, value.value(), "query {query_id}: diff {diff} applied to {had}");
                diffs += 1;
            }
            None => assert_eq!(&w["value"], value.value(), "query {query_id}: value on the wire"),
        }
    }
    diffs
}
