//! The `_migrations` system table: one row per migration that has been started at least once. A declared migration
//! without a row is pending. An ordinary table (docs/CONTRACT.md §4), so the dashboard and subscriptions see runs
//! progress live.
//!
//! Field comments are plain `//` comments: ts-rs would copy doc comments into generated TS.

use serde::{Deserialize, Serialize};

use crate::prelude::*;

/// Where a migration is: `pending` → `running` → `done`, or `failed` / `canceled` (both resumable from the cursor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub enum MigrationState {
    /// Not started, or interrupted by a restart (then `cursor` says where it resumes).
    Pending,
    /// A runner is working through it.
    Running,
    /// Every document was processed. Running it again is a no-op (unless restarted from scratch).
    Done,
    /// A document failed (`error`, `failedDocument`); the batch with that document was rolled back.
    Failed,
    /// Stopped from the dashboard or `cancel`; the next run resumes from the cursor.
    Canceled,
}

impl MigrationState {
    pub fn as_str(self) -> &'static str {
        match self {
            MigrationState::Pending => "pending",
            MigrationState::Running => "running",
            MigrationState::Done => "done",
            MigrationState::Failed => "failed",
            MigrationState::Canceled => "canceled",
        }
    }
}

impl std::fmt::Display for MigrationState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The progress of one migration (`_migrations`).
#[table(name = "_migrations")]
#[index(by_name = [name])]
pub struct MigrationRecord {
    // The migration's stable id (`#[migration(name = "...")]`, or its function path).
    pub name: String,
    // The table it migrates.
    pub table: String,
    #[ts(type = "\"pending\" | \"running\" | \"done\" | \"failed\" | \"canceled\"")]
    pub state: MigrationState,
    // Where the next batch starts: a pagination cursor over the table's `by_creation_time` index; null = the start.
    pub cursor: Option<String>,
    // Documents processed so far in this run (each exactly once).
    pub processed: u64,
    // Documents patched, replaced or deleted so far.
    pub changed: u64,
    pub started_at: Option<f64>,
    // Set when it is done.
    pub finished_at: Option<f64>,
    // Why it failed (cut to 4 KiB).
    pub error: Option<String>,
    // The id of the document it failed on, if it failed on one.
    pub failed_document: Option<String>,
    // The schema check of the table after its last pending migration finished: "passed", or the failure report.
    // Null when the table didn't need one (its documents already matched when the server started).
    pub schema_check: Option<String>,
}
