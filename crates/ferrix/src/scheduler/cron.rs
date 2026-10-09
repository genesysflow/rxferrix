//! Crons: declared in `main` (`Ferrix::cron` / `Ferrix::interval`), reconciled into `_crons` at startup, and fired by
//! the runner, which enqueues an ordinary scheduled job for each run (one execution path for everything).
//!
//! - **Missed runs** (the server was down when a run was due) run **once** at startup: the stored `nextRun` is in
//!   the past, so the first tick enqueues one job and computes the next run from *now*, skipping the others.
//! - **No pile-up**: if the job of the previous run is still pending or in progress, the run is skipped (and logged).
//! - **Reconcile**: a cron whose schedule, function or args changed gets a fresh `nextRun`; rows of crons that are
//!   no longer declared are deleted (their already enqueued jobs stay).

use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use serde_json::Value;

use super::store::{self, NewJob};
use super::tables::{Cron, JobKind, ScheduledJob};
use crate::id::Id;
use crate::runtime::App;
use crate::SchedulableFunction;

/// A cron to declare: `CronSpec::cron::<f>("name", "0 */5 * * * *", args)` or `CronSpec::interval::<f>(..)`. Usually
/// built through `Ferrix::cron` / `Ferrix::interval` (or `SchedulerOptions::cron` / `interval`).
#[derive(Debug, Clone)]
pub struct CronSpec {
    name: String,
    schedule: Every,
    path: String,
    args: Result<Value, String>,
}

#[derive(Debug, Clone)]
enum Every {
    Expr(String),
    Interval(Duration),
}

impl CronSpec {
    /// Run `F` with `args` on a cron schedule: 6 fields with seconds (`sec min hour day-of-month month
    /// day-of-week`, an optional 7th is the year), evaluated in UTC. E.g. `"0 */5 * * * *"` (every 5 minutes),
    /// `"0 0 3 * * *"` (03:00 UTC daily), `"0 0 9 * * Mon-Fri"`.
    pub fn cron<F: SchedulableFunction>(name: impl Into<String>, expr: impl Into<String>, args: impl Into<F::Args>) -> Self {
        CronSpec::new::<F>(name.into(), Every::Expr(expr.into()), args.into())
    }

    /// Run `F` with `args` every `every`, the first time `every` after startup.
    pub fn interval<F: SchedulableFunction>(name: impl Into<String>, every: Duration, args: impl Into<F::Args>) -> Self {
        CronSpec::new::<F>(name.into(), Every::Interval(every), args.into())
    }

    fn new<F: SchedulableFunction>(name: String, schedule: Every, args: F::Args) -> Self {
        let args = serde_json::to_value(args).map_err(|e| e.to_string());
        CronSpec { name, schedule, path: F::path(), args }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

/// A validated [`CronSpec`].
pub(crate) struct ParsedCron {
    pub name: String,
    /// What `_crons.schedule` shows: the expression, or `every <duration>`.
    pub display: String,
    schedule: Parsed,
    pub path: String,
    pub kind: JobKind,
    pub args: Value,
}

enum Parsed {
    Expr(Box<cron::Schedule>),
    IntervalMs(f64),
}

/// `next_run` of a cron that will never run again (a cron expression with only past years).
const NEVER: f64 = f64::MAX;

impl ParsedCron {
    /// The first run strictly after `now` (ms since the epoch).
    pub fn next_after(&self, now: f64) -> f64 {
        match &self.schedule {
            Parsed::IntervalMs(ms) => now + ms,
            Parsed::Expr(schedule) => {
                let Some(from) = DateTime::<Utc>::from_timestamp_millis(now as i64) else { return NEVER };
                schedule.after(&from).next().map(|t| t.timestamp_millis() as f64).unwrap_or(NEVER)
            }
        }
    }
}

/// Validate the declared crons: unique non-empty names, valid expressions, positive intervals, serializable args,
/// and a registered mutation or action.
pub(crate) fn parse_all(specs: &[CronSpec]) -> crate::Result<Vec<ParsedCron>> {
    let mut out: Vec<ParsedCron> = Vec::with_capacity(specs.len());
    for spec in specs {
        let name = spec.name.trim().to_string();
        if name.is_empty() {
            bail!("a cron needs a name");
        }
        if out.iter().any(|c| c.name == name) {
            bail!("two crons are named `{name}`");
        }
        let (schedule, display) = match &spec.schedule {
            Every::Expr(expr) => {
                let expr = expr.trim();
                let fields = expr.split_whitespace().count();
                if !(6..=7).contains(&fields) {
                    bail!(
                        "cron `{name}`: `{expr}` has {fields} fields; use 6 (with seconds): `sec min hour day month weekday`, e.g. `0 */5 * * * *`"
                    );
                }
                let schedule = cron::Schedule::from_str(expr).with_context(|| format!("cron `{name}`: invalid expression `{expr}`"))?;
                (Parsed::Expr(Box::new(schedule)), expr.to_string())
            }
            Every::Interval(every) => {
                if every.is_zero() {
                    bail!("cron `{name}`: the interval must be longer than zero");
                }
                (Parsed::IntervalMs(every.as_secs_f64() * 1000.0), format!("every {}", format_duration(*every)))
            }
        };
        let args = spec.args.clone().map_err(|e| anyhow::anyhow!("cron `{name}`: args don't serialize: {e}"))?;
        let kind = store::kind_of(&spec.path).with_context(|| format!("cron `{name}`"))?;
        out.push(ParsedCron { name, display, schedule, path: spec.path.clone(), kind, args });
    }
    Ok(out)
}

/// `1h`, `90s`, `5m`, `1500ms`.
fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    match () {
        _ if ms.is_multiple_of(3_600_000) => format!("{}h", ms / 3_600_000),
        _ if ms.is_multiple_of(60_000) => format!("{}m", ms / 60_000),
        _ if ms.is_multiple_of(1000) => format!("{}s", ms / 1000),
        _ => format!("{ms}ms"),
    }
}

/// Make `_crons` match the declared crons (at startup).
pub(crate) async fn reconcile(app: &App, crons: &[ParsedCron], now: f64) -> crate::Result<()> {
    store::write(app, |db| async move {
        let existing = db.query::<Cron>().collect().await?;
        for row in &existing {
            if !crons.iter().any(|c| c.name == row.name) {
                tracing::info!(cron = %row.name, "removing a cron that is no longer declared");
                db.delete(row.id).await?;
            }
        }
        for c in crons {
            match existing.iter().find(|r| r.name == c.name) {
                Some(row) if row.schedule == c.display && row.path == c.path && row.args == c.args => {
                    // Unchanged: keep `nextRun`. If it passed while the server was down, the first tick runs it once.
                }
                Some(row) => {
                    let mut value = row.value.clone();
                    value.schedule = c.display.clone();
                    value.path = c.path.clone();
                    value.args = c.args.clone();
                    value.next_run = c.next_after(now);
                    db.replace(row.id, value).await?;
                }
                None => {
                    db.insert(Cron {
                        name: c.name.clone(),
                        schedule: c.display.clone(),
                        path: c.path.clone(),
                        args: c.args.clone(),
                        last_run: None,
                        next_run: c.next_after(now),
                        last_job: None,
                    })
                    .await?;
                }
            }
        }
        Ok(())
    })
    .await
}

/// Enqueue a job for every cron that is due at `now`; returns how many were enqueued.
pub(crate) async fn fire_due(app: &App, crons: &[ParsedCron], now: f64) -> crate::Result<usize> {
    if crons.is_empty() {
        return Ok(0);
    }
    let due = store::read(app, |db| async move {
        db.query::<Cron>().with_index(Cron::by_next_run, |q| q.lte(Cron::next_run, now)).collect().await
    })
    .await?;
    let mut fired = 0;
    for row in due {
        let Some(cron) = crons.iter().find(|c| c.name == row.name) else { continue };
        let enqueued: Option<Id<ScheduledJob>> = store::write(app, |db| async move {
            // Re-read under the writer lock: another dispatcher may have fired it already.
            let Some(current) = db.get(row.id).await? else { return Ok(None) };
            if current.next_run > now {
                return Ok(None);
            }
            let previous = match current.last_job {
                Some(job) => db.get(job).await?.filter(|j| !j.state.is_finished()),
                None => None,
            };
            let mut value = current.value;
            value.next_run = cron.next_after(now);
            let job = match previous {
                Some(previous) => {
                    tracing::warn!(
                        cron = %cron.name,
                        job = %previous.id,
                        "skipping a cron run: the previous run is still {}",
                        previous.state.as_str()
                    );
                    None
                }
                None => {
                    let job = NewJob {
                        path: cron.path.clone(),
                        kind: cron.kind,
                        args: cron.args.clone(),
                        run_at: now,
                        max_attempts: 1,
                        origin: Some(format!("cron:{}", cron.name)),
                    };
                    let id = store::insert(&db, job, now).await?;
                    value.last_run = Some(now);
                    value.last_job = Some(id);
                    Some(id)
                }
            };
            db.replace(row.id, value).await?;
            Ok(job)
        })
        .await?;
        fired += usize::from(enqueued.is_some());
    }
    Ok(fired)
}

/// When the next cron is due, if any.
pub(crate) async fn next_due(app: &App) -> crate::Result<Option<f64>> {
    let first = store::read(app, |db| async move { db.query::<Cron>().with_index(Cron::by_next_run, |q| q).first().await }).await?;
    Ok(first.map(|c| c.next_run).filter(|t| *t < NEVER))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(expr: &str) -> ParsedCron {
        ParsedCron {
            name: "t".into(),
            display: expr.into(),
            schedule: Parsed::Expr(Box::new(cron::Schedule::from_str(expr).unwrap())),
            path: "x".into(),
            kind: JobKind::Mutation,
            args: Value::Null,
        }
    }

    fn ms(s: &str) -> f64 {
        DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis() as f64
    }

    #[test]
    fn next_run_of_expressions() {
        let every5 = parsed("0 */5 * * * *");
        assert_eq!(every5.next_after(ms("2026-10-09T10:02:30Z")), ms("2026-10-09T10:05:00Z"));
        // Strictly after: a run exactly at `now` is the current one, not the next.
        assert_eq!(every5.next_after(ms("2026-10-09T10:05:00Z")), ms("2026-10-09T10:10:00Z"));
        let daily = parsed("0 0 3 * * *");
        assert_eq!(daily.next_after(ms("2026-10-09T03:00:00.500Z")), ms("2026-10-10T03:00:00Z"));
        let weekdays = parsed("0 0 9 * * Mon-Fri");
        assert_eq!(weekdays.next_after(ms("2026-10-09T12:00:00Z")), ms("2026-10-12T09:00:00Z"), "Friday → Monday");
        let past = parsed("0 0 0 1 1 * 2020");
        assert_eq!(past.next_after(ms("2026-10-09T12:00:00Z")), NEVER);
    }

    #[test]
    fn durations_render_compactly() {
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h");
        assert_eq!(format_duration(Duration::from_secs(300)), "5m");
        assert_eq!(format_duration(Duration::from_secs(90)), "90s");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1500ms");
    }
}
