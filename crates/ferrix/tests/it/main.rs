//! Integration tests. A single test binary so that every hand-written expansion in `common` is linked once.

mod common;

mod audit;
mod auth;
mod authkit;
mod bench;
mod bytes;
mod catalog;
mod codegen;
mod contract;
mod dashboard;
mod diff_fixtures;
mod dx;
mod group_commit;
mod hardening;
mod index_order;
mod jwt;
mod macros;
mod migrations;
mod model;
mod oauth_presets;
mod ops;
mod pagination;
mod query_engine;
mod readsets;
mod replication;
mod revocation;
mod storage;
mod vector_queries;
mod scheduler;
mod search;
mod search_pages;
mod settings;
mod shared;
mod stemming;
mod websocket;
