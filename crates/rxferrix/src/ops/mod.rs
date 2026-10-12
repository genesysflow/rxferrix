//! Operations (docs/CONTRACT.md §4.4): backups and restore, export and import, Prometheus metrics, serving the app's frontend,
//! TLS, and the data directory lock.
//!
//! - [`backup`]: consistent online snapshots (`VACUUM INTO`) packed as `.tar.zst` archives, backup targets (a
//!   directory or an S3-compatible bucket), scheduled backups with rotation, and the offline `restore`.
//! - [`export`]: JSONL and CSV export of tables, and JSONL and CSV import through the normal write path ([`csv`]:
//!   reading CSV with the table's types).
//! - [`restore`]: staged restores (validated while the server runs, applied at the next start) and restart requests.
//! - [`replicate`]: continuous replication (the change journal shipped as segments, periodic snapshots, retention)
//!   and point-in-time restores.
//! - [`metrics`]: counters and the Prometheus text format of `GET /metrics`.
//! - [`http`]: the admin endpoints (`/api/admin/backups*`, `/api/admin/export/{table}`, `/api/admin/import/{table}`)
//!   and `/metrics`.
//! - [`web`]: the app's own frontend (`RxFerrix::static_dir`, `RxFerrix::embed_static`) and extra routes
//!   (`RxFerrix::routes`).
//! - [`tls`]: `--https <domain>` (Let's Encrypt via `rustls-acme`), `--tls-cert/--tls-key`, the HTTP→HTTPS redirect.
//! - [`lock`]: `<dir>/ferrix.lock`, held by `serve` (and `import`), so `restore` can refuse to run under a server.
//! - [`cli`]: the `backup`, `restore`, `export` and `import` commands and the operations flags of `serve`.

pub mod backup;
pub mod cli;
pub mod csv;
pub mod export;
pub mod http;
pub mod lock;
pub mod metrics;
pub mod replicate;
pub mod restore;
pub mod takeover;
pub mod tls;
pub mod web;

use std::time::Duration;

pub use backup::{BackupInfo, BackupSpec, BackupTarget, ScheduledBackup};
pub use export::{ExportFormat, ImportMode, ImportReport};
pub use lock::DirLock;
pub use metrics::MetricsOptions;
pub use web::{StaticFiles, WebOptions};

/// Operations settings of an [`App`](crate::App) (`AppOptions::ops`).
#[derive(Debug, Clone, Default)]
pub struct OpsOptions {
    /// `GET /metrics` (off by default; `--metrics`, `--metrics-token`).
    pub metrics: MetricsOptions,
    /// Where backups go: the admin endpoints list, create, download and delete backups there, and scheduled backups
    /// write there. `None` = `<data dir>/backups`. A directory, `file:///dir` or `s3://bucket/prefix` (`--backup-to`).
    pub backup_target: Option<String>,
    /// Whether (and with which exit code) an administrator may ask the server to restart to apply a staged restore
    /// (`--restart-exit-code`; [`restore`]).
    pub restart: restore::RestartHandle,
    /// Continuous replication and point-in-time restores (`--replicate-to`; [`replicate`]). Turns the change journal
    /// on from the first commit.
    pub replication: Option<replicate::ReplicationOptions>,
}

/// `"500ms"`, `"30s"`, `"5m"`, `"6h"`, `"7d"`, or a bare number of seconds.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, unit) = match s.find(|c: char| !(c.is_ascii_digit() || c == '.')) {
        Some(i) => (&s[..i], s[i..].trim()),
        None => (s, "s"),
    };
    let n: f64 = num.parse().map_err(|_| format!("invalid duration `{s}` (examples: 30s, 5m, 6h, 1d)"))?;
    let secs = match unit {
        "ms" => n / 1000.0,
        "s" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        "d" => n * 86400.0,
        _ => return Err(format!("invalid duration unit in `{s}` (use ms, s, m, h or d)")),
    };
    Duration::try_from_secs_f64(secs).map_err(|e| format!("invalid duration `{s}`: {e}"))
}

/// `2026-10-09T12:00:00.123Z` → `20261009T120000.123Z`: a UTC timestamp that sorts lexicographically and is safe in
/// file names.
pub fn file_timestamp(ms: f64) -> String {
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms as i64).unwrap_or_default();
    dt.format("%Y%m%dT%H%M%S%.3fZ").to_string()
}

/// The inverse of [`file_timestamp`] (ms since the epoch).
pub fn parse_file_timestamp(s: &str) -> Option<f64> {
    let dt = chrono::NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%S%.3fZ").ok()?;
    Some(dt.and_utc().timestamp_millis() as f64)
}

/// An RFC 3339 time (`2026-10-09T12:00:00Z`, `2026-10-09T14:00:00.250+02:00`) or ms since the epoch, as ms.
pub fn parse_time(s: &str) -> Result<f64, String> {
    let s = s.trim();
    if let Ok(ms) = s.parse::<f64>() {
        return Ok(ms);
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|t| t.timestamp_millis() as f64)
        .map_err(|e| format!("invalid time `{s}` (RFC 3339, e.g. 2026-10-09T12:00:00Z): {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_timestamps() {
        assert_eq!(parse_duration("6h").unwrap(), Duration::from_secs(6 * 3600));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("1.5m").unwrap(), Duration::from_secs(90));
        assert!(parse_duration("3w").is_err() && parse_duration("soon").is_err());
        assert_eq!(file_timestamp(0.0), "19700101T000000.000Z");
        assert_eq!(file_timestamp(1_760_011_200_123.0), "20251009T120000.123Z");
        assert_eq!(parse_file_timestamp("20251009T120000.123Z"), Some(1_760_011_200_123.0));
        assert_eq!(parse_file_timestamp("nope"), None);
        assert_eq!(parse_time("2025-10-09T14:00:00.123+02:00").unwrap(), 1_760_011_200_123.0);
        assert_eq!(parse_time("1760011200123").unwrap(), 1_760_011_200_123.0);
        assert!(parse_time("yesterday").is_err());
    }
}
