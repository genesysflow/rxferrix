//! Hand-written macro expansions (docs/CONTRACT.md §3) used by every integration test.
//!
//! `todos` spells out the expansions literally, exactly as the contract shows them. Everything else uses
//! `expand_fn!`, a `macro_rules!` stand-in that produces the same shape, to keep the boilerplate manageable.

// The contract's expansion names statics `__FERRIX_FN_<fn>` and binds `let out = ...` even for `()` outputs.
#![allow(dead_code, non_upper_case_globals, clippy::let_unit_value)]

pub mod items;
pub mod todos;

/// `impl SchedulableFunction` for mutations and actions, as the macros emit it.
#[macro_export]
macro_rules! schedulable_impl {
    (QueryFunction, $name:ident) => {};
    ($trait:ident, $name:ident) => {
        impl ::ferrix::SchedulableFunction for $name {}
    };
}

/// Same output as `#[query]` / `#[mutation]` / `#[action]` (§3.2), with the generated names passed explicitly
/// (macro_rules can't concatenate identifiers).
#[macro_export]
macro_rules! expand_fn {
    ($handler:ident, $trait:ident, $vis:ident, $name:ident, $args:ident, $stat:ident,
     ($($arg:ident : $ty:ty),* $(,)?) -> $out:ty) => {
        #[allow(non_camel_case_types)]
        #[derive(::ferrix::__private::serde::Serialize, ::ferrix::__private::serde::Deserialize, ::ferrix::__private::ts_rs::TS)]
        #[serde(crate = "::ferrix::__private::serde", rename_all = "camelCase")]
        #[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase", optional_fields = nullable)]
        pub struct $args { $(pub $arg: $ty),* }

        impl ::core::convert::From<($($ty,)*)> for $args {
            fn from(($($arg,)*): ($($ty,)*)) -> Self { Self { $($arg),* } }
        }

        #[allow(non_camel_case_types, dead_code)]
        pub struct $name {}

        impl ::ferrix::FunctionRef for $name {
            type Args = $args;
            type Output = $out;
            fn path() -> String { ::ferrix::__private::function_path(module_path!(), stringify!($name)) }
        }
        impl ::ferrix::$trait for $name {}
        $crate::schedulable_impl!($trait, $name);

        #[::ferrix::__private::linkme::distributed_slice(::ferrix::__private::FUNCTIONS)]
        #[linkme(crate = ::ferrix::__private::linkme)]
        static $stat: ::ferrix::__private::FunctionDef = ::ferrix::__private::FunctionDef {
            module_path: module_path!(),
            name: stringify!($name),
            visibility: ::ferrix::__private::Visibility::$vis,
            handler: ::ferrix::__private::Handler::$handler(|ctx, args| ::std::boxed::Box::pin(async move {
                #[allow(unused_variables)]
                let a: $args = ::ferrix::__private::decode_args(stringify!($name), args)?;
                let out = $name(&ctx, $(a.$arg),*).await?;
                ::ferrix::__private::encode_output(out)
            })),
            ts: || (::ferrix::__private::ts_inline::<$args>(), ::ferrix::__private::ts_inline::<$out>()),
        };
    };
}
