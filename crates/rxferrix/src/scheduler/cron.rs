//! Crons: declared in `main` (`RxFerrix::cron` / `RxFerrix::interval`), reconciled into `_crons` at startup, and fired by
//! the runner, which enqueues an ordinary scheduled job for each run (one execution path for everything).
//!
//! - **Missed runs** (the server was down when a run was due) run **once** at startup: the stored `nextRun` is in
//!   the past, so the first tick enqueues one job and computes the next run from *now*, skipping the others.
//! - **No pile-up**: if the job of the previous run is still pending or in progress, the run is skipped (and logged).
//! - **Reconcile**: a cron whose schedule, time zone, function or args changed gets a fresh `nextRun`; rows of crons
//!   that are no longer declared are deleted (their already enqueued jobs stay).
//! - **Time zones** ([`CronSpec::timezone`]): expressions are evaluated in local time of an IANA zone, see
//!   `next_in_zone` for daylight saving time.
//! - **System crons** (names starting with `_`, reserved) are declared by rxferrix itself, e.g. `_storage_sweep`, the
//!   storage orphan sweep (`system_crons`). They are ordinary `_crons` rows and jobs; the dashboard marks them.

use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, bail};
use chrono::{DateTime, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use cron::TimeUnitSpec;
use serde_json::Value;

use super::store::{self, NewJob};
use super::tables::{Cron, JobKind, ScheduledJob};
use crate::SchedulableFunction;
use crate::id::Id;
use crate::runtime::{App, AppOptions};

/// A cron to declare: `CronSpec::cron::<f>("name", "0 */5 * * * *", args)` or `CronSpec::interval::<f>(..)`. Usually
/// built through `RxFerrix::cron` / `RxFerrix::cron_tz` / `RxFerrix::interval` (or the same methods on `SchedulerOptions`).
#[derive(Debug, Clone)]
pub struct CronSpec {
    name: String,
    schedule: Every,
    path: String,
    args: Result<Value, String>,
    timezone: Option<String>,
    system: bool,
}

#[derive(Debug, Clone)]
enum Every {
    Expr(String),
    Interval(Duration),
}

impl CronSpec {
    /// Run `F` with `args` on a cron schedule: 6 fields with seconds (`sec min hour day-of-month month
    /// day-of-week`, an optional 7th is the year), evaluated in UTC unless a [`CronSpec::timezone`] is set. E.g.
    /// `"0 */5 * * * *"` (every 5 minutes), `"0 0 3 * * *"` (03:00 daily), `"0 0 9 * * Mon-Fri"`.
    pub fn cron<F: SchedulableFunction>(
        name: impl Into<String>,
        expr: impl Into<String>,
        args: impl Into<F::Args>,
    ) -> Self {
        CronSpec::new::<F>(name.into(), Every::Expr(expr.into()), args.into())
    }

    /// [`CronSpec::cron`] evaluated in the IANA time zone `tz`, e.g. `"Europe/Bucharest"`: see
    /// [`CronSpec::timezone`].
    pub fn cron_tz<F: SchedulableFunction>(
        name: impl Into<String>,
        expr: impl Into<String>,
        tz: impl Into<String>,
        args: impl Into<F::Args>,
    ) -> Self {
        CronSpec::cron::<F>(name, expr, args).timezone(tz)
    }

    /// Run `F` with `args` every `every`, the first time `every` after startup.
    pub fn interval<F: SchedulableFunction>(
        name: impl Into<String>,
        every: Duration,
        args: impl Into<F::Args>,
    ) -> Self {
        CronSpec::new::<F>(name.into(), Every::Interval(every), args.into())
    }

    /// Evaluate the expression in the IANA time zone `tz` (e.g. `"Europe/Bucharest"`, `"America/New_York"`; `"UTC"`
    /// is the default) instead of UTC. Daylight saving time:
    /// - **Spring forward** (a skipped hour): a run whose local time doesn't exist that day runs later by the length
    ///   of the gap (`0 30 2 * * *` in `America/New_York` runs at 03:30 on that day), so a daily job never misses a
    ///   day.
    /// - **Fall back** (a repeated hour): a run whose local time occurs twice runs once, at the first occurrence; only
    ///   expressions that match every hour (hour field `*`) also run in the repeated hour, since they are about
    ///   elapsed time rather than a time of day.
    ///
    /// An unknown zone, or a time zone on an interval, fails `App::open`.
    pub fn timezone(mut self, tz: impl Into<String>) -> Self {
        self.timezone = Some(tz.into());
        self
    }

    fn new<F: SchedulableFunction>(name: String, schedule: Every, args: F::Args) -> Self {
        let args = serde_json::to_value(args).map_err(|e| e.to_string());
        CronSpec { name, schedule, path: F::path(), args, timezone: None, system: false }
    }

    /// A rxferrix system cron (its name starts with `_`).
    fn system(mut self) -> Self {
        self.system = true;
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The function's arguments (an error if they didn't serialize).
    pub(crate) fn args_value(&self) -> Result<&Value, &str> {
        self.args.as_ref().map_err(String::as_str)
    }

    /// The interval in whole seconds, for an interval cron.
    pub(crate) fn interval_secs(&self) -> Option<u64> {
        match self.schedule {
            Every::Interval(d) => Some(d.as_secs()),
            Every::Expr(_) => None,
        }
    }
}

/// The crons rxferrix declares itself, from the app's options: `_storage_sweep` (the storage orphan sweep, every
/// `StorageConfig::sweep_interval`).
pub(crate) fn system_crons(options: &AppOptions) -> Vec<CronSpec> {
    let mut crons = Vec::new();
    if let Some(every) = options.storage.sweep_interval {
        crons.push(CronSpec::interval::<crate::storage::sweep_orphans>(SWEEP_CRON, every, ()).system());
    }
    crons
}

/// The name of the storage orphan sweep's system cron.
pub const SWEEP_CRON: &str = "_storage_sweep";

/// A validated [`CronSpec`].
#[derive(Clone)]
pub(crate) struct ParsedCron {
    pub name: String,
    /// What `_crons.schedule` shows: the expression, or `every <duration>`.
    pub display: String,
    /// The IANA zone name, `None` for UTC.
    pub timezone: Option<String>,
    schedule: Parsed,
    pub path: String,
    pub kind: JobKind,
    pub args: Value,
}

#[derive(Clone)]
enum Parsed {
    Expr { schedule: Box<cron::Schedule>, tz: Option<Tz> },
    IntervalMs(f64),
}

/// `next_run` of a cron that will never run again (a cron expression with only past years).
const NEVER: f64 = f64::MAX;

impl ParsedCron {
    /// The first run strictly after `now` (ms since the epoch).
    pub fn next_after(&self, now: f64) -> f64 {
        match &self.schedule {
            Parsed::IntervalMs(ms) => now + ms,
            Parsed::Expr { schedule, tz: None } => {
                let Some(from) = DateTime::<Utc>::from_timestamp_millis(now as i64) else { return NEVER };
                schedule.after(&from).next().map(|t| t.timestamp_millis() as f64).unwrap_or(NEVER)
            }
            Parsed::Expr { schedule, tz: Some(tz) } => next_in_zone(schedule, *tz, now),
        }
    }
}

/// How far around a daylight saving change local times are looked at (offset changes are at most a few hours).
const DST_WINDOW: chrono::TimeDelta = chrono::TimeDelta::hours(3);
/// Upper bound on the candidates [`next_in_zone`] looks at (an every-second expression near a change).
const MAX_CANDIDATES: usize = 200_000;

/// The first run of `schedule`, read as local wall-clock times in `tz`, strictly after `now`.
///
/// The cron crate enumerates matching *local* times (fed as naive times); each is mapped to instants: a local time
/// that exists once maps to that instant; one in a spring-forward gap maps to the instant it would have with the
/// offset before the gap (later by the gap's length); one in a fall-back fold maps to its first occurrence, plus the
/// second one when the expression matches every hour. A second occurrence can be in the future while its local time
/// has passed, so near an offset change the enumeration starts [`DST_WINDOW`] early.
fn next_in_zone(schedule: &cron::Schedule, tz: Tz, now: f64) -> f64 {
    let Some(now_utc) = DateTime::<Utc>::from_timestamp_millis(now as i64) else { return NEVER };
    let offset_at = |t: DateTime<Utc>| tz.offset_from_utc_datetime(&t.naive_utc()).fix();
    let local = now_utc.with_timezone(&tz).naive_local();
    let stable =
        offset_at(now_utc - DST_WINDOW) == offset_at(now_utc) && offset_at(now_utc + DST_WINDOW) == offset_at(now_utc);
    let start = if stable { local } else { local - DST_WINDOW };
    let every_hour = schedule.hours().is_all();
    let mut best: Option<DateTime<Utc>> = None;
    for candidate in schedule.after(&start.and_utc()).take(MAX_CANDIDATES) {
        let [first, second] = local_instants(tz, candidate.naive_utc(), every_hour);
        for t in [first, second].into_iter().flatten() {
            if t > now_utc && best.is_none_or(|b| t < b) {
                best = Some(t);
            }
        }
        // First occurrences are monotonic in local time, and a second occurrence is later than its first: past the
        // first candidate whose first occurrence is in the future, nothing can be earlier.
        if first.is_some_and(|t| t > now_utc) {
            break;
        }
    }
    best.map(|t| t.timestamp_millis() as f64).unwrap_or(NEVER)
}

/// The instants at which local time `local` in `tz` runs (see [`next_in_zone`]).
fn local_instants(tz: Tz, local: NaiveDateTime, both_in_fold: bool) -> [Option<DateTime<Utc>>; 2] {
    match tz.from_local_datetime(&local) {
        chrono::LocalResult::Single(t) => [Some(t.with_timezone(&Utc)), None],
        chrono::LocalResult::Ambiguous(first, second) => {
            [Some(first.with_timezone(&Utc)), both_in_fold.then(|| second.with_timezone(&Utc))]
        }
        chrono::LocalResult::None => {
            // In a gap: use the offset in effect before it.
            for hours in [1, 2, 3, 6, 12, 24] {
                if let chrono::LocalResult::Single(before) =
                    tz.from_local_datetime(&(local - chrono::TimeDelta::hours(hours)))
                {
                    let offset = before.offset().fix().local_minus_utc();
                    return [Some((local - chrono::TimeDelta::seconds(offset.into())).and_utc()), None];
                }
            }
            [None, None]
        }
    }
}

/// Validate the declared crons: unique non-empty names (`_` is reserved for system crons), valid expressions and time
/// zones, positive intervals, serializable args, and a registered mutation or action.
pub(crate) fn parse_all(specs: &[CronSpec]) -> crate::Result<Vec<ParsedCron>> {
    let mut out: Vec<ParsedCron> = Vec::with_capacity(specs.len());
    for spec in specs {
        let name = spec.name.trim().to_string();
        if name.is_empty() {
            bail!("a cron needs a name");
        }
        if name.starts_with('_') && !spec.system {
            bail!("cron `{name}`: names starting with `_` are reserved for rxferrix's own crons");
        }
        if out.iter().any(|c| c.name == name) {
            bail!("two crons are named `{name}`");
        }
        let (schedule, display, timezone) = match &spec.schedule {
            Every::Expr(expr) => {
                let expr = expr.trim();
                let fields = expr.split_whitespace().count();
                if !(6..=7).contains(&fields) {
                    bail!(
                        "cron `{name}`: `{expr}` has {fields} fields; use 6 (with seconds): `sec min hour day month weekday`, e.g. `0 */5 * * * *`"
                    );
                }
                let schedule = cron::Schedule::from_str(expr)
                    .with_context(|| format!("cron `{name}`: invalid expression `{expr}`"))?;
                let tz = match spec.timezone.as_deref().map(str::trim) {
                    None => None,
                    Some(zone) => Some(zone.parse::<Tz>().map_err(|_| {
                        anyhow::anyhow!(
                            "cron `{name}`: unknown time zone `{zone}` (use an IANA name such as `Europe/Bucharest`)"
                        )
                    })?),
                };
                // UTC is the default: stored as no zone.
                let tz = tz.filter(|tz| *tz != Tz::UTC);
                let timezone = tz.map(|tz| tz.name().to_string());
                (Parsed::Expr { schedule: Box::new(schedule), tz }, expr.to_string(), timezone)
            }
            Every::Interval(every) => {
                if every.is_zero() {
                    bail!("cron `{name}`: the interval must be longer than zero");
                }
                if spec.timezone.is_some() {
                    bail!("cron `{name}`: an interval has no time zone (only cron expressions do)");
                }
                (Parsed::IntervalMs(every.as_secs_f64() * 1000.0), format!("every {}", format_duration(*every)), None)
            }
        };
        let args = spec.args.clone().map_err(|e| anyhow::anyhow!("cron `{name}`: args don't serialize: {e}"))?;
        let kind = store::kind_of(&spec.path).with_context(|| format!("cron `{name}`"))?;
        out.push(ParsedCron { name, display, timezone, schedule, path: spec.path.clone(), kind, args });
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
            upsert(&db, existing.iter().find(|r| r.name == c.name), c, now).await?;
        }
        Ok(())
    })
    .await
}

/// Make the `_crons` row of `name` match `cron` (`None`: delete it): a cron declared or withdrawn at runtime
/// (`App::set_runtime_cron`).
pub(crate) async fn reconcile_one(app: &App, name: &str, cron: Option<&ParsedCron>, now: f64) -> crate::Result<()> {
    store::write(app, |db| async move {
        let row = db.query::<Cron>().with_index(Cron::by_name, |q| q.eq(Cron::name, name.to_string())).first().await?;
        match (cron, row) {
            (Some(c), row) => upsert(&db, row.as_ref(), c, now).await,
            (None, Some(row)) => db.delete(row.id).await,
            (None, None) => Ok(()),
        }
    })
    .await
}

/// Insert `c`'s row, or update `row` if the schedule, zone, function or args changed (a change gets a fresh
/// `nextRun`; an unchanged row keeps it, so if it passed while the server was down, the first tick runs it once).
async fn upsert(
    db: &crate::db::query::MutationDb,
    row: Option<&crate::schema::Doc<Cron>>,
    c: &ParsedCron,
    now: f64,
) -> crate::Result<()> {
    match row {
        Some(row)
            if row.schedule == c.display && row.timezone == c.timezone && row.path == c.path && row.args == c.args => {}
        Some(row) => {
            let mut value = row.value.clone();
            value.schedule = c.display.clone();
            value.timezone = c.timezone.clone();
            value.path = c.path.clone();
            value.args = c.args.clone();
            value.next_run = c.next_after(now);
            db.replace(row.id, value).await?;
        }
        None => {
            db.insert(Cron {
                name: c.name.clone(),
                schedule: c.display.clone(),
                timezone: c.timezone.clone(),
                path: c.path.clone(),
                args: c.args.clone(),
                last_run: None,
                next_run: c.next_after(now),
                last_job: None,
            })
            .await?;
        }
    }
    Ok(())
}

/// Enqueue a run of the cron in `row` now, in the caller's transaction (`_system/scheduler:run_cron_now`): skipped
/// (`Ok(None)`) while its previous run is still pending or in progress, unless `force`. Updates `lastRun` /
/// `lastJob`, not `nextRun`.
pub(crate) async fn enqueue_now(
    db: &crate::db::query::MutationDb,
    row: crate::schema::Doc<Cron>,
    now: f64,
    force: bool,
) -> crate::Result<Option<Id<ScheduledJob>>> {
    if !force
        && let Some(job) = row.last_job
        && db.get(job).await?.is_some_and(|j| !j.state.is_finished())
    {
        return Ok(None);
    }
    let kind = store::kind_of(&row.path)?;
    let mut value = row.value;
    let job = NewJob {
        path: value.path.clone(),
        kind,
        args: value.args.clone(),
        run_at: now,
        max_attempts: 1,
        pool: None,
        on_complete: None,
        backoff: None,
        origin: Some(format!("cron:{}", value.name)),
    };
    let id = store::insert(db, job, now).await?;
    value.last_run = Some(now);
    value.last_job = Some(id);
    db.replace(row.id, value).await?;
    Ok(Some(id))
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
                        pool: None,
                        on_complete: None,
                        backoff: None,
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
    let first =
        store::read(app, |db| async move { db.query::<Cron>().with_index(Cron::by_next_run, |q| q).first().await })
            .await?;
    Ok(first.map(|c| c.next_run).filter(|t| *t < NEVER))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed_in(expr: &str, tz: Option<&str>) -> ParsedCron {
        ParsedCron {
            name: "t".into(),
            display: expr.into(),
            timezone: tz.map(String::from),
            schedule: Parsed::Expr {
                schedule: Box::new(cron::Schedule::from_str(expr).unwrap()),
                tz: tz.map(|tz| tz.parse().unwrap()),
            },
            path: "x".into(),
            kind: JobKind::Mutation,
            args: Value::Null,
        }
    }

    fn parsed(expr: &str) -> ParsedCron {
        parsed_in(expr, None)
    }

    fn ms(s: &str) -> f64 {
        DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis() as f64
    }

    /// The next `n` runs after `from`, as RFC 3339 UTC strings.
    fn runs(cron: &ParsedCron, from: &str, n: usize) -> Vec<String> {
        let mut at = ms(from);
        (0..n)
            .map(|_| {
                at = cron.next_after(at);
                DateTime::<Utc>::from_timestamp_millis(at as i64).unwrap().format("%Y-%m-%dT%H:%MZ").to_string()
            })
            .collect()
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
        assert_eq!(
            parsed_in("0 0 0 1 1 * 2020", Some("Europe/Bucharest")).next_after(ms("2026-10-09T12:00:00Z")),
            NEVER
        );
    }

    // Europe/Bucharest in 2026: EET (+2) until Sunday 29 March 03:00 local (01:00Z), which becomes 04:00 EEST (+3);
    // EEST until Sunday 25 October 04:00 local (01:00Z), which becomes 03:00 EET again (03:00-03:59 happens twice).

    #[test]
    fn time_zones_outside_dst_changes() {
        let nine = parsed_in("0 0 9 * * *", Some("Europe/Bucharest"));
        assert_eq!(runs(&nine, "2026-10-09T12:00:00Z", 2), ["2026-10-10T06:00Z", "2026-10-11T06:00Z"], "09:00 EEST");
        assert_eq!(runs(&nine, "2026-12-01T12:00:00Z", 1), ["2026-12-02T07:00Z"], "09:00 EET");
        let weekdays = parsed_in("0 0 9 * * Mon-Fri", Some("America/New_York"));
        assert_eq!(
            runs(&weekdays, "2026-10-09T14:00:00Z", 1),
            ["2026-10-12T13:00Z"],
            "Friday 10:00 EDT → Monday 09:00"
        );
    }

    #[test]
    fn spring_forward_runs_skipped_local_times_later_by_the_gap() {
        // 03:30 doesn't exist on 29 March: that day's run is at 04:30 EEST, and no day is skipped.
        let daily = parsed_in("0 30 3 * * *", Some("Europe/Bucharest"));
        assert_eq!(
            runs(&daily, "2026-03-27T12:00:00Z", 4),
            ["2026-03-28T01:30Z", "2026-03-29T01:30Z", "2026-03-30T00:30Z", "2026-03-31T00:30Z"]
        );
        // Hourly at :30: 02:30 EET, then 03:30 (shifted to 04:30, the same instant as the real 04:30, one run), 05:30.
        let hourly = parsed_in("0 30 * * * *", Some("Europe/Bucharest"));
        assert_eq!(
            runs(&hourly, "2026-03-28T23:00:00Z", 4),
            ["2026-03-28T23:30Z", "2026-03-29T00:30Z", "2026-03-29T01:30Z", "2026-03-29T02:30Z"]
        );
        // America/New_York, 8 March: 02:30 doesn't exist; runs at 03:30 EDT (07:30Z).
        let ny = parsed_in("0 30 2 * * *", Some("America/New_York"));
        assert_eq!(runs(&ny, "2026-03-07T12:00:00Z", 2), ["2026-03-08T07:30Z", "2026-03-09T06:30Z"]);
    }

    #[test]
    fn fall_back_runs_a_time_of_day_once_and_hourly_schedules_in_both_hours() {
        // 03:30 happens twice on 25 October: the daily run is at the first one (03:30 EEST = 00:30Z) only.
        let daily = parsed_in("0 30 3 * * *", Some("Europe/Bucharest"));
        assert_eq!(
            runs(&daily, "2026-10-24T12:00:00Z", 3),
            ["2026-10-25T00:30Z", "2026-10-26T01:30Z", "2026-10-27T01:30Z"]
        );
        // From inside the repeated hour (03:10 EET, the second time): the next run is tomorrow's.
        assert_eq!(runs(&daily, "2026-10-25T01:10:00Z", 1), ["2026-10-26T01:30Z"]);
        // A time-of-day schedule in the repeated hour (every 15 minutes from 03:00 to 03:45): first occurrences only.
        let in_three = parsed_in("0 */15 3 * * *", Some("Europe/Bucharest"));
        assert_eq!(
            runs(&in_three, "2026-10-24T23:59:00Z", 5),
            ["2026-10-25T00:00Z", "2026-10-25T00:15Z", "2026-10-25T00:30Z", "2026-10-25T00:45Z", "2026-10-26T01:00Z"]
        );
        // An every-hour schedule follows elapsed time: 03:30 EEST and 03:30 EET both run.
        let hourly = parsed_in("0 30 * * * *", Some("Europe/Bucharest"));
        assert_eq!(
            runs(&hourly, "2026-10-24T22:00:00Z", 5),
            ["2026-10-24T22:30Z", "2026-10-24T23:30Z", "2026-10-25T00:30Z", "2026-10-25T01:30Z", "2026-10-25T02:30Z"]
        );
        // Starting inside the repeated hour finds the second occurrence.
        assert_eq!(runs(&hourly, "2026-10-25T01:05:00Z", 2), ["2026-10-25T01:30Z", "2026-10-25T02:30Z"]);
        let quarter = parsed_in("0 */15 * * * *", Some("Europe/Bucharest"));
        let quarters = runs(&quarter, "2026-10-25T00:40:00Z", 6);
        assert_eq!(
            quarters,
            [
                "2026-10-25T00:45Z",
                "2026-10-25T01:00Z",
                "2026-10-25T01:15Z",
                "2026-10-25T01:30Z",
                "2026-10-25T01:45Z",
                "2026-10-25T02:00Z"
            ],
            "no gap in an every-15-minutes schedule"
        );
    }

    #[test]
    fn durations_render_compactly() {
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h");
        assert_eq!(format_duration(Duration::from_secs(300)), "5m");
        assert_eq!(format_duration(Duration::from_secs(90)), "90s");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1500ms");
    }
}
