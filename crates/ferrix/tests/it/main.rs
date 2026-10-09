//! Integration tests. A single test binary so that every hand-written expansion in `common` is linked once.

mod common;

mod auth;
mod bench;
mod catalog;
mod codegen;
mod contract;
mod dashboard;
mod hardening;
mod macros;
mod model;
mod pagination;
mod query_engine;
mod readsets;
mod storage;
mod scheduler;
mod search;
mod search_pages;
mod shared;
mod websocket;
