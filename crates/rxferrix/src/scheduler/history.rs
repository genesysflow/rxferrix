//! The job history: finished jobs moved out of `_scheduled_jobs` into `aux.db` (table `_job_history`).
//!
//! Finished jobs (success, failed, canceled) stay in `_scheduled_jobs` for `SchedulerOptions::history_after` (1 hour)
//! so subscriptions and `ctx.db.get(job)` see their outcome. Then the scheduler's cleanup (`super::runner`) copies each
//! one here (the whole document, as JSON) and deletes it from `data.db`, a batch at a time: the history is written and
//! committed first, so a crash in between leaves the job in both places, and the next cleanup writes the same row
//! again (`INSERT OR REPLACE` by id). The last job of every cron stays in `_scheduled_jobs` (the dashboard shows its
//! state). History rows are deleted `SchedulerOptions::retention` (7 days) after the job finished.
//!
//! Keeping the history in `aux.db` keeps `data.db` (and thus backups and replication snapshots) from growing with the
//! number of jobs run. It is not reactive: the dashboard reads it over HTTP (`GET /api/admin/job-history`), like the
//! execution log.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::tables::{JobState, ScheduledJob};
use crate::id::Id;
use crate::runtime::App;
use crate::schema::Doc;

pub(crate) const DDL: &str = "
CREATE TABLE IF NOT EXISTS _job_history (
    id TEXT PRIMARY KEY,             -- the job's id
    completed_at REAL NOT NULL,      -- ms since the unix epoch
    state TEXT NOT NULL,             -- success | failed | canceled
    path TEXT NOT NULL,
    doc TEXT NOT NULL                -- the `_scheduled_jobs` document, as JSON
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS _job_history_by_completed ON _job_history (completed_at, id);
CREATE INDEX IF NOT EXISTS _job_history_by_state ON _job_history (state, completed_at, id);
CREATE INDEX IF NOT EXISTS _job_history_by_path ON _job_history (path, completed_at, id);
";

/// Default and maximum [`HistoryFilter::limit`].
pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 500;

/// Filter for [`App::job_history`]. Every field is optional; `Default` returns the [`DEFAULT_LIMIT`] most recently
/// finished jobs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HistoryFilter {
    /// Only jobs that ended in this state.
    pub state: Option<JobState>,
    /// Only jobs of this function.
    pub path: Option<String>,
    /// At most this many jobs (default [`DEFAULT_LIMIT`], capped at [`MAX_LIMIT`]).
    pub limit: Option<usize>,
    /// Only jobs that come after this one, newest first: pass the `completedAt` and `_id` of the last job of the
    /// previous page (both, or neither).
    pub before_completed_at: Option<f64>,
    pub before_id: Option<String>,
}

/// Write `jobs` (finished) into the history, replacing rows with the same id. One transaction.
pub(crate) fn insert(conn: &mut Connection, jobs: &[Doc<ScheduledJob>]) -> anyhow::Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT OR REPLACE INTO _job_history (id, completed_at, state, path, doc) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for job in jobs {
            let completed_at = job.completed_at.unwrap_or(job.scheduled_at);
            let doc = serde_json::to_string(job)?;
            stmt.execute(params![job.id.to_string(), completed_at, job.state.as_str(), job.path, doc])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Delete the jobs that finished before `cutoff` (ms since the epoch). Returns how many.
pub(crate) fn trim(conn: &Connection, cutoff: f64) -> anyhow::Result<usize> {
    Ok(conn.execute("DELETE FROM _job_history WHERE completed_at < ?1", [cutoff])?)
}

fn parse(doc: String) -> rusqlite::Result<Doc<ScheduledJob>> {
    serde_json::from_str(&doc)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}

fn query(conn: &Connection, f: &HistoryFilter) -> anyhow::Result<Vec<Doc<ScheduledJob>>> {
    let mut sql = String::from("SELECT doc FROM _job_history WHERE 1 = 1");
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    // Appends `clause` with every `?` replaced by the number of the next parameter.
    let mut arg = |sql: &mut String, clause: &str, v: rusqlite::types::Value| {
        args.push(v);
        sql.push_str(&clause.replace('?', &format!("?{}", args.len())));
    };
    if let Some(state) = f.state {
        arg(&mut sql, " AND state = ?", state.as_str().to_string().into());
    }
    if let Some(path) = &f.path {
        arg(&mut sql, " AND path = ?", path.clone().into());
    }
    match (f.before_completed_at, &f.before_id) {
        (Some(at), Some(id)) => {
            arg(&mut sql, " AND (completed_at < ?", at.into());
            arg(&mut sql, " OR (completed_at = ?", at.into());
            arg(&mut sql, " AND id < ?))", id.clone().into());
        }
        (None, None) => {}
        _ => anyhow::bail!("pass both `beforeCompletedAt` and `beforeId`, or neither"),
    }
    let limit = f.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    arg(&mut sql, " ORDER BY completed_at DESC, id DESC LIMIT ?", (limit as i64).into());
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| parse(r.get(0)?))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

impl App {
    /// Finished jobs moved out of `_scheduled_jobs` (see [`crate::scheduler::history`]), most recently finished first.
    pub async fn job_history(&self, filter: HistoryFilter) -> crate::Result<Vec<Doc<ScheduledJob>>> {
        self.logs().with_reader(move |conn| query(conn, &filter)).await
    }

    /// The job `id` from the job history, if it is there.
    pub async fn job_history_get(&self, id: Id<ScheduledJob>) -> crate::Result<Option<Doc<ScheduledJob>>> {
        self.logs()
            .with_reader(move |conn| {
                Ok(conn
                    .query_row("SELECT doc FROM _job_history WHERE id = ?1", [id.to_string()], |r| parse(r.get(0)?))
                    .optional()?)
            })
            .await
    }
}
