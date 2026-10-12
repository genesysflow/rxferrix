//! Scheduled backups as a runtime setting (the `backups` row): how often, how many scheduled archives to keep, what
//! to include, and where backups go (the data directory's `backups/`, another directory on the server, or an
//! S3-compatible bucket with its own credentials). The dashboard's Backups page edits it through
//! `_system/settings:update { backups }`; `test_backup_destination` checks a destination before it is saved.
//!
//! **Applying.** At startup and after every change ([`apply`]), the `ferrix-backup` cron is declared, changed or
//! withdrawn at runtime (`App::set_runtime_cron`), and the destination is swapped in for scheduled backups, the
//! dashboard's manual backups, listings, downloads and restores (`crate::ops::backup::BackupTarget::for_app`).
//!
//! **Precedence.** The command line wins, part by part: `--backup-every` (with `--backup-keep`, `--backup-with-aux`,
//! `--backup-with-files`) fixes the schedule, `--backup-to` the destination; the view reports them in `locked` as
//! `backups.schedule` and `backups.destination`, and the stored values are kept but not used.
//!
//! **Secrets.** The S3 secret access key is the row's secret part (encrypted like the other settings secrets), shown
//! as `"••••"`, never part of the cron's arguments (the scheduled run reads the destination from the app). Without
//! an access key id, S3 uses the server's `AWS_*` environment, like `--backup-to s3://…`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{MASK, invalid, secret_from_input};
use crate::ops::backup::{BackupTarget, CRON_NAME, ScheduledBackup};
use crate::prelude::*;
use crate::runtime::App;

/// Shortest and longest schedule interval.
pub const MIN_EVERY_SECS: u64 = 60;
pub const MAX_EVERY_SECS: u64 = 30 * 24 * 3600;
/// Most scheduled archives kept.
pub const MAX_KEEP: u32 = 1000;

/// Where backups go.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", rename_all_fields = "camelCase")]
pub enum BackupDestination {
    /// `<data dir>/backups`.
    Default,
    /// An absolute directory on the server.
    Local { path: String },
    /// An S3-compatible bucket (AWS, R2, MinIO, …), archives under `prefix`.
    S3 {
        bucket: String,
        #[serde(default)]
        prefix: String,
        /// Default: `AWS_REGION` / `AWS_DEFAULT_REGION`, else `us-east-1`.
        #[serde(default)]
        region: Option<String>,
        /// A non-AWS endpoint (`https://<account>.r2.cloudflarestorage.com`, `http://minio:9000`).
        #[serde(default)]
        endpoint: Option<String>,
        /// None: the server's `AWS_*` environment credentials.
        #[serde(default)]
        access_key_id: Option<String>,
    },
}

/// The `backups` setting as stored (the secret is the row's encrypted part).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredBackups {
    /// None: no schedule (manual backups from the dashboard still go to the destination).
    pub every_secs: Option<u64>,
    pub keep: u32,
    #[serde(default)]
    pub include_aux: bool,
    #[serde(default)]
    pub include_files: bool,
    pub destination: BackupDestination,
    /// Decrypted; never part of `value`.
    #[serde(skip)]
    pub secret_access_key: Option<String>,
}

/// `backups` for `update` and `test_backup_destination`.
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase", optional_fields = nullable)]
pub struct BackupsInput {
    pub every_secs: Option<u64>,
    /// Default 7.
    pub keep: Option<u32>,
    #[serde(default)]
    pub include_aux: bool,
    #[serde(default)]
    pub include_files: bool,
    /// `{ kind: "default" }`, `{ kind: "local", path }` or `{ kind: "s3", bucket, prefix?, region?, endpoint?,
    /// accessKeyId? }`.
    #[ts(type = "any")]
    pub destination: BackupDestination,
    /// Omitted, null or `"••••"`: keep the stored one; `""`: none.
    #[serde(default)]
    pub secret_access_key: Option<String>,
}

/// `backups` as `get` returns it (the secret masked).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupsView {
    pub every_secs: Option<u64>,
    pub keep: u32,
    pub include_aux: bool,
    pub include_files: bool,
    pub destination: BackupDestination,
    pub secret_access_key: Option<String>,
}

/// What is in effect, settings and command line merged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupsEffective {
    /// Where backups go (`<dir>/backups`, a path, `s3://bucket/prefix`).
    pub target: String,
    pub every_secs: Option<u64>,
    pub keep: u32,
    pub include_aux: bool,
    pub include_files: bool,
    /// The server was built with S3 support (rxferrix's `s3` cargo feature).
    pub s3_available: bool,
}

impl StoredBackups {
    pub(crate) fn view(&self) -> BackupsView {
        BackupsView {
            every_secs: self.every_secs,
            keep: self.keep,
            include_aux: self.include_aux,
            include_files: self.include_files,
            destination: self.destination.clone(),
            secret_access_key: self.secret_access_key.as_ref().filter(|s| !s.is_empty()).map(|_| MASK.to_string()),
        }
    }

    /// The schedule as the cron runs it.
    fn schedule(&self) -> Option<(Duration, ScheduledBackup)> {
        let every = Duration::from_secs(self.every_secs?);
        let run = ScheduledBackup {
            to: None,
            keep: self.keep.max(1),
            include_aux: self.include_aux,
            include_files: self.include_files,
        };
        Some((every, run))
    }

    /// The archive store this destination describes.
    pub(crate) fn target(&self, data_dir: &std::path::Path) -> Result<BackupTarget> {
        match &self.destination {
            BackupDestination::Default => BackupTarget::default_for(data_dir),
            BackupDestination::Local { path } => BackupTarget::parse(path, data_dir),
            BackupDestination::S3 { bucket, prefix, region, endpoint, access_key_id } => {
                let location = format!("s3://{bucket}/{prefix}").trim_end_matches('/').to_string();
                let store = s3_store(bucket, region, endpoint, access_key_id, &self.secret_access_key)?;
                let mut target = BackupTarget::object_store(store, prefix.clone());
                target.location = location;
                Ok(target)
            }
        }
    }
}

#[cfg(feature = "s3")]
fn s3_store(
    bucket: &str,
    region: &Option<String>,
    endpoint: &Option<String>,
    access_key_id: &Option<String>,
    secret: &Option<String>,
) -> Result<std::sync::Arc<dyn object_store::ObjectStore>> {
    use anyhow::Context as _;
    use object_store::aws::AmazonS3Builder;
    let mut b = match access_key_id.as_deref().filter(|k| !k.is_empty()) {
        Some(key) => {
            AmazonS3Builder::new().with_access_key_id(key).with_secret_access_key(secret.clone().unwrap_or_default())
        }
        None => AmazonS3Builder::from_env(),
    }
    .with_bucket_name(bucket);
    let region = region
        .clone()
        .filter(|r| !r.is_empty())
        .or_else(|| std::env::var("AWS_REGION").ok())
        .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
        .unwrap_or_else(|| "us-east-1".to_string());
    b = b.with_region(region);
    if let Some(endpoint) = endpoint.as_deref().filter(|e| !e.is_empty()) {
        b = b.with_endpoint(endpoint).with_allow_http(endpoint.starts_with("http://"));
    }
    Ok(std::sync::Arc::new(b.build().with_context(|| format!("configuring S3 bucket `{bucket}`"))?))
}

#[cfg(not(feature = "s3"))]
fn s3_store(
    bucket: &str,
    _: &Option<String>,
    _: &Option<String>,
    _: &Option<String>,
    _: &Option<String>,
) -> Result<std::sync::Arc<dyn object_store::ObjectStore>> {
    anyhow::bail!("S3 backups (bucket `{bucket}`) need rxferrix's `s3` cargo feature")
}

/// Validate `input` into what is stored; `stored` supplies a kept secret.
pub(crate) fn from_input(input: BackupsInput, stored: Option<&StoredBackups>) -> Result<StoredBackups> {
    if let Some(every) = input.every_secs
        && !(MIN_EVERY_SECS..=MAX_EVERY_SECS).contains(&every)
    {
        return Err(invalid("backups: the interval must be between 1 minute and 30 days"));
    }
    let keep = input.keep.unwrap_or(7);
    if !(1..=MAX_KEEP).contains(&keep) {
        return Err(invalid(format!("backups: keep between 1 and {MAX_KEEP} scheduled backups")));
    }
    let trimmed = |s: Option<String>| s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let mut secret = None;
    let destination = match input.destination {
        BackupDestination::Default => BackupDestination::Default,
        BackupDestination::Local { path } => {
            let path = path.trim().to_string();
            if !std::path::Path::new(&path).is_absolute() || path.len() > 1024 {
                return Err(invalid("backups: the directory must be an absolute path on the server"));
            }
            BackupDestination::Local { path }
        }
        BackupDestination::S3 { bucket, prefix, region, endpoint, access_key_id } => {
            if !cfg!(feature = "s3") {
                return Err(invalid("backups: this server was built without S3 support (rxferrix's `s3` feature)"));
            }
            let bucket = bucket.trim().to_string();
            let bucket_ok = (3..=63).contains(&bucket.len())
                && bucket.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
                && bucket.starts_with(|c: char| c.is_ascii_alphanumeric())
                && bucket.ends_with(|c: char| c.is_ascii_alphanumeric());
            if !bucket_ok {
                return Err(invalid(format!("backups: `{bucket}` is not a valid bucket name")));
            }
            let prefix = prefix.trim().trim_matches('/').to_string();
            if prefix.len() > 512 || prefix.split('/').any(|p| p == ".." || p == ".") {
                return Err(invalid("backups: invalid key prefix"));
            }
            let region = trimmed(region);
            if region
                .as_ref()
                .is_some_and(|r| r.len() > 40 || !r.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
            {
                return Err(invalid("backups: invalid region"));
            }
            let endpoint = trimmed(endpoint);
            if let Some(e) = &endpoint {
                let url = url::Url::parse(e).map_err(|err| invalid(format!("backups: endpoint `{e}`: {err}")))?;
                if !matches!(url.scheme(), "http" | "https") || url.host().is_none() || !url.username().is_empty() {
                    return Err(invalid(format!("backups: endpoint `{e}` must be an http(s) URL")));
                }
            }
            let access_key_id = trimmed(access_key_id);
            if access_key_id.as_ref().is_some_and(|k| k.len() > 128) {
                return Err(invalid("backups: the access key id is too long"));
            }
            if access_key_id.is_some() {
                let kept = stored.and_then(|s| s.secret_access_key.as_ref());
                secret = secret_from_input(input.secret_access_key, kept);
                if secret.is_none() {
                    return Err(invalid("backups: a secret access key is needed with an access key id"));
                }
                if secret.as_ref().is_some_and(|s| s.len() > 512) {
                    return Err(invalid("backups: the secret access key is too long"));
                }
            }
            BackupDestination::S3 { bucket, prefix, region, endpoint, access_key_id }
        }
    };
    Ok(StoredBackups {
        every_secs: input.every_secs,
        keep,
        include_aux: input.include_aux,
        include_files: input.include_files,
        destination,
        secret_access_key: secret,
    })
}

/// `backups.schedule` / `backups.destination` when the command line fixes them.
pub(crate) fn locked(app: &App) -> Vec<String> {
    let mut out = Vec::new();
    if app.scheduler_state().is_declared(CRON_NAME) {
        out.push("backups.schedule".to_string());
    }
    if app.options().ops.backup_target.as_deref().is_some_and(|t| !t.trim().is_empty()) {
        out.push("backups.destination".to_string());
    }
    out
}

/// What is in effect now.
pub(crate) fn effective(app: &App) -> BackupsEffective {
    let target = match BackupTarget::for_app(app) {
        Ok(t) => t.location,
        Err(e) => format!("(unusable: {e:#})"),
    };
    let declared = app.options().scheduler.crons.iter().find(|c| c.name() == CRON_NAME);
    let stored = app.backup_settings();
    let (every_secs, keep, include_aux, include_files) = match (declared, &stored) {
        // The command line's schedule: its arguments are the `ScheduledBackup`.
        (Some(c), _) => {
            let s: Option<ScheduledBackup> =
                c.args_value().ok().and_then(|a| serde_json::from_value(a["schedule"].clone()).ok());
            let s = s.unwrap_or(ScheduledBackup { to: None, keep: 7, include_aux: false, include_files: false });
            (c.interval_secs(), s.keep, s.include_aux, s.include_files)
        }
        (None, Some(b)) => (b.every_secs, b.keep, b.include_aux, b.include_files),
        (None, None) => (None, 7, false, false),
    };
    BackupsEffective { target, every_secs, keep, include_aux, include_files, s3_available: cfg!(feature = "s3") }
}

/// Make the app use `stored` (at startup and after every change): the destination, and the `ferrix-backup` cron
/// unless the command line declares it.
pub(crate) async fn apply(app: &App, stored: Option<&StoredBackups>) -> Result<()> {
    app.set_backup_settings(stored.cloned());
    if app.scheduler_state().is_declared(CRON_NAME) {
        return Ok(());
    }
    let spec = stored.and_then(StoredBackups::schedule).map(|(every, run)| run.cron(every));
    app.set_runtime_cron(CRON_NAME, spec).await
}

/// Write a small object to `backups`' destination (the stored one when omitted), list it and delete it.
pub(crate) async fn test_destination(app: &App, stored: Option<&StoredBackups>) -> Result<String> {
    let target = match stored {
        Some(b) => b.target(app.dir())?,
        None => BackupTarget::for_app(app)?,
    };
    target.probe().await.map_err(|e| invalid(format!("{}: {e:#}", target.location)))?;
    Ok(target.location)
}
