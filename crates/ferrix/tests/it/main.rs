//! Integration tests. A single test binary so that every hand-written expansion in `common` is linked once.

mod common;

mod audit;
mod auth;
mod authkit;
mod bench;
mod catalog;
mod codegen;
mod contract;
mod dashboard;
mod hardening;
mod jwt;
mod macros;
mod model;
mod pagination;
mod query_engine;
mod readsets;
mod revocation;
mod storage;
mod scheduler;
mod search;
mod settings;
mod shared;
mod websocket;
