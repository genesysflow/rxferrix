//! The realtime sync protocol: wire messages, the per-connection worker, and the shared subscription manager.

pub(crate) mod interval;
pub mod protocol;
pub mod subscriptions;
pub mod worker;
