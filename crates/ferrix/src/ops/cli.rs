//! Command line of the operations features: the flags `serve` takes ([`OpsServeArgs`]) and the `backup`, `restore`,
//! `export` and `import` commands. `crate::cli` wires them in.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use clap::Args;

use super::backup::{self, BackupSpec, BackupTarget, RestoreOptions, ScheduledBackup, SnapshotSource};
use super::export::{self, ExportFormat, ExportOptions, ImportMode, ImportOptions};
use super::tls::{AcmeSettings, Listen};
use crate::runtime::{App, AppOptions};
use crate::storage::{StorageBackend, StorageConfig};

/// Operations flags of `serve`: metrics, scheduled backups, TLS.
#[derive(Args, Debug, Clone)]
pub struct OpsServeArgs {
    /// Serve Prometheus metrics at /metrics.
    #[arg(long, env = "FERRIX_METRICS")]
    pub metrics: bool,
    /// With --metrics: require `Authorization: Bearer <TOKEN>` on /metrics.
    #[arg(long, env = "FERRIX_METRICS_TOKEN", value_name = "TOKEN", hide_env_values = true)]
    pub metrics_token: Option<String>,

    /// Take a backup this often, e.g. `6h`, `1d`, `30m` (runs as the cron `ferrix-backup`).
    #[arg(long, value_name = "DURATION", value_parser = super::parse_duration)]
    pub backup_every: Option<Duration>,
    /// With --backup-every: how many scheduled backups to keep.
    #[arg(long, value_name = "N", default_value_t = 7)]
    pub backup_keep: u32,
    /// Where backups go (scheduled ones and those made from the dashboard): a directory, file:///dir or
    /// s3://bucket/prefix (credentials from the AWS_* variables). Default: <dir>/backups.
    #[arg(long, env = "FERRIX_BACKUP_TO", value_name = "LOCATION")]
    pub backup_to: Option<String>,
    /// Include aux.db (the execution log) in scheduled backups.
    #[arg(long)]
    pub backup_with_aux: bool,
    /// Include the blobs of stored files (local storage only) in scheduled backups.
    #[arg(long)]
    pub backup_with_files: bool,

    /// Replicate continuously to a directory, file:///dir or s3://bucket/prefix: every commit is journaled and
    /// shipped in segments, with periodic snapshots, for point-in-time restores (`restore --from`).
    #[arg(long, env = "FERRIX_REPLICATE_TO", value_name = "LOCATION")]
    pub replicate_to: Option<String>,
    /// With --replicate-to: ship new commits this often (e.g. `1s`, `10s`).
    #[arg(long, value_name = "DURATION", default_value = "1s", value_parser = super::parse_duration)]
    pub replicate_every: Duration,
    /// With --replicate-to: take a snapshot this often (e.g. `24h`, `6h`).
    #[arg(long, value_name = "DURATION", default_value = "24h", value_parser = super::parse_duration)]
    pub snapshot_every: Duration,
    /// With --replicate-to: how far back point-in-time restores reach (older snapshots and segments are deleted).
    #[arg(long, value_name = "DURATION", default_value = "7d", value_parser = super::parse_duration)]
    pub replicate_keep: Duration,
    /// With --replicate-to: journal kept locally while the target is unreachable before it is dropped (a new
    /// generation starts when the target is back), e.g. `1GB`, `200MB`.
    #[arg(long, value_name = "SIZE", default_value = "1GB", value_parser = crate::cli::parse_size)]
    pub replicate_max_pending: u64,

    /// Keep an analytics copy of the data in DuckDB (`<data dir>/olap.duckdb`) for read-only SQL from actions
    /// (`ctx.olap`). Needs a binary built with the `olap` feature.
    #[arg(long, env = "FERRIX_OLAP")]
    pub olap: bool,
    /// With --olap: DuckDB's memory limit, e.g. `2GB` (default: 80% of RAM).
    #[arg(long, value_name = "SIZE", requires = "olap", value_parser = crate::cli::parse_size)]
    pub olap_memory: Option<u64>,
    /// With --olap: DuckDB's worker threads (default: one per core).
    #[arg(long, value_name = "N", requires = "olap")]
    pub olap_threads: Option<usize>,
    /// With --olap: interrupt an analytics query after this long.
    #[arg(long, value_name = "DURATION", default_value = "30s", value_parser = super::parse_duration)]
    pub olap_timeout: Duration,
    /// With --olap: past this much journal waiting to be applied, rebuild the copy instead of following it.
    #[arg(long, value_name = "SIZE", default_value = "512MB", value_parser = crate::cli::parse_size)]
    pub olap_max_lag: u64,

    /// Serve HTTPS for DOMAIN (comma-separated for several) with automatic Let's Encrypt certificates.
    #[arg(long, value_name = "DOMAIN", value_delimiter = ',', env = "FERRIX_HTTPS")]
    pub https: Vec<String>,
    /// With --https or --tls-cert: the HTTPS address.
    #[arg(long, value_name = "ADDR", default_value = "0.0.0.0:443")]
    pub https_addr: SocketAddr,
    /// With --https or --tls-cert: a plain-HTTP address that redirects to HTTPS, or `off`. Default: 0.0.0.0:80 with
    /// --https, off with --tls-cert.
    #[arg(long, value_name = "ADDR|off")]
    pub http_redirect: Option<String>,
    /// With --https: contact email for the Let's Encrypt account (repeatable).
    #[arg(long, env = "FERRIX_ACME_EMAIL", value_name = "EMAIL")]
    pub acme_email: Vec<String>,
    /// With --https: use Let's Encrypt's staging directory (untrusted test certificates).
    #[arg(long)]
    pub acme_staging: bool,
    /// With --https: where certificates are cached. Default: <dir>/acme.
    #[arg(long, value_name = "DIR")]
    pub acme_cache: Option<PathBuf>,
    /// Serve HTTPS with this PEM certificate chain (with --tls-key).
    #[arg(long, value_name = "PEM", requires = "tls_key", conflicts_with = "https")]
    pub tls_cert: Option<PathBuf>,
    /// The PEM private key of --tls-cert.
    #[arg(long, value_name = "PEM", requires = "tls_cert")]
    pub tls_key: Option<PathBuf>,
    /// With --tls-cert: re-read the certificate and key this often and use them for new connections when they
    /// changed (renewals without a restart; a pair that doesn't load is ignored and logged). `0` turns it off.
    #[arg(long, value_name = "DURATION", default_value = "30s", value_parser = super::parse_duration)]
    pub tls_reload_every: Duration,
    /// Let administrators restart the server to apply a restore staged from the dashboard: it shuts down gracefully
    /// and exits with CODE, for a supervisor to start it again (systemd: `Restart=on-failure` or
    /// `RestartForceExitStatus=CODE`; Docker: a restart policy). Without it, staged restores wait for a manual restart.
    #[arg(long, env = "FERRIX_RESTART_EXIT_CODE", value_name = "CODE", value_parser = clap::value_parser!(i32).range(1..=255))]
    pub restart_exit_code: Option<i32>,
}

impl OpsServeArgs {
    /// Apply metrics and backup settings to the app's options (scheduled backups become a cron).
    pub fn apply(&self, options: &mut AppOptions) {
        if self.metrics {
            options.ops.metrics.enabled = true;
        }
        if let Some(token) = self.metrics_token.clone().filter(|t| !t.is_empty()) {
            options.ops.metrics.token = Some(token);
        }
        if let Some(to) = self.backup_to.clone().filter(|t| !t.trim().is_empty()) {
            options.ops.backup_target = Some(to);
        }
        if self.restart_exit_code.is_some() {
            options.ops.restart = super::restore::RestartHandle::new(self.restart_exit_code);
        }
        if let Some(to) = self.replicate_to.clone().filter(|t| !t.trim().is_empty()) {
            let mut replication = super::replicate::ReplicationOptions::new(to);
            replication.every = self.replicate_every.max(Duration::from_millis(10));
            replication.snapshot_every = self.snapshot_every.max(Duration::from_secs(60));
            replication.keep = self.replicate_keep;
            replication.max_pending_bytes = self.replicate_max_pending;
            options.ops.replication = Some(replication);
        }
        if self.olap {
            options.olap.enabled = true;
            options.olap.memory_limit = self.olap_memory;
            options.olap.threads = self.olap_threads;
            options.olap.query_timeout = self.olap_timeout.max(Duration::from_millis(100));
            options.olap.max_lag_bytes = self.olap_max_lag;
        }
        if let Some(every) = self.backup_every {
            let schedule = ScheduledBackup {
                to: None,
                keep: self.backup_keep.max(1),
                include_aux: self.backup_with_aux,
                include_files: self.backup_with_files,
            };
            options.scheduler.crons.push(schedule.cron(every));
        }
    }

    fn redirect(&self, default_on: bool) -> crate::Result<Option<SocketAddr>> {
        match self.http_redirect.as_deref().map(str::trim) {
            Some("off") | Some("") => Ok(None),
            Some(addr) => Ok(Some(addr.parse().with_context(|| format!("--http-redirect: invalid address `{addr}`"))?)),
            None if default_on => Ok(Some(SocketAddr::from(([0, 0, 0, 0], 80)))),
            None => Ok(None),
        }
    }

    fn domains(&self) -> Vec<String> {
        self.https.iter().map(|d| d.trim().to_ascii_lowercase()).filter(|d| !d.is_empty()).collect()
    }

    /// How to listen: plain HTTP on `http`, or one of the TLS modes.
    pub fn listen(&self, http: SocketAddr, dir: &Path) -> crate::Result<Listen> {
        let domains = self.domains();
        if !domains.is_empty() {
            return Ok(Listen::Acme {
                https: self.https_addr,
                redirect: self.redirect(true)?,
                acme: AcmeSettings {
                    domains,
                    emails: self.acme_email.clone(),
                    staging: self.acme_staging,
                    cache_dir: self.acme_cache.clone().unwrap_or_else(|| dir.join("acme")),
                },
            });
        }
        if let (Some(cert), Some(key)) = (&self.tls_cert, &self.tls_key) {
            return Ok(Listen::CertFiles {
                https: self.https_addr,
                redirect: self.redirect(false)?,
                cert: cert.clone(),
                key: key.clone(),
                reload_every: (!self.tls_reload_every.is_zero()).then_some(self.tls_reload_every),
            });
        }
        Ok(Listen::Http(http))
    }

    /// The public URL implied by `--https` (`https://<first domain>[:port]`).
    pub fn public_url(&self) -> Option<String> {
        let domain = self.domains().into_iter().next()?;
        let port = self.https_addr.port();
        Some(if port == 443 { format!("https://{domain}") } else { format!("https://{domain}:{port}") })
    }
}

fn runtime() -> crate::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().enable_all().build()?)
}

fn storage_config(flag: Option<&str>, mut config: StorageConfig) -> crate::Result<StorageConfig> {
    if let Some(location) = flag.filter(|l| !l.trim().is_empty()) {
        config.backend = StorageBackend::parse(location)?;
    }
    Ok(config)
}

// ---------------------------------------------------------------------------------------------------------------------
// backup / restore
// ---------------------------------------------------------------------------------------------------------------------

/// `backup`: take a consistent snapshot (works while the server runs).
#[derive(Args, Debug, Clone)]
pub struct BackupArgs {
    /// Data directory.
    #[arg(long, default_value = "fx_data")]
    pub dir: PathBuf,
    /// Write the archive to this file. Default: <dir>/backups/ferrix-<time>.tar.zst (or --to).
    #[arg(long, value_name = "FILE", conflicts_with = "to")]
    pub out: Option<PathBuf>,
    /// Store the archive in a directory, file:///dir or s3://bucket/prefix instead.
    #[arg(long, value_name = "LOCATION")]
    pub to: Option<String>,
    /// Include aux.db (the execution log).
    #[arg(long)]
    pub with_aux: bool,
    /// Include the blobs of stored files (local storage only).
    #[arg(long)]
    pub with_files: bool,
    /// Include auth_secret.key. Anyone with the archive can then forge sessions and upload URLs.
    #[arg(long)]
    pub include_secret: bool,
    /// Where uploaded files are stored, if not <dir>/storage (as for `serve`).
    #[arg(long, env = "FERRIX_STORAGE", value_name = "LOCATION")]
    pub storage: Option<String>,
}

pub fn run_backup(args: BackupArgs, storage: StorageConfig) -> crate::Result<()> {
    let storage = storage_config(args.storage.as_deref(), storage)?;
    let source = SnapshotSource::new(&args.dir, &storage);
    let spec =
        BackupSpec { include_aux: args.with_aux, include_files: args.with_files, include_secret: args.include_secret };
    if spec.include_secret {
        eprintln!(
            "warning: the archive includes auth_secret.key: anyone holding it can forge sessions and upload URLs. Keep it safe."
        );
    }
    let now = crate::db::wall_clock_ms();
    let name = backup::archive_name(now, false);
    match (&args.out, &args.to) {
        (Some(out), _) => {
            let manifest = backup::write_archive(&source, &spec, out, now)?;
            println!("Wrote {} (ts {}, {} bytes)", out.display(), manifest.ts, std::fs::metadata(out)?.len());
        }
        (None, to) => {
            let target = match to {
                Some(t) => BackupTarget::parse(t, &args.dir)?,
                None => BackupTarget::default_for(&args.dir)?,
            };
            let work = tempfile::Builder::new().prefix(".ferrix-backup-").tempdir_in(&args.dir)?;
            let local = work.path().join(&name);
            let manifest = backup::write_archive(&source, &spec, &local, now)?;
            let bytes = std::fs::metadata(&local)?.len();
            let name = backup::archive_name_with_ts(now, false, manifest.ts);
            runtime()?.block_on(target.upload_file(&local, &name))?;
            println!("Wrote {name} to {} (ts {}, {bytes} bytes)", target.location, manifest.ts);
        }
    }
    Ok(())
}

/// `restore`: replace the data directory's database with a backup, or with a point in time from a replication
/// target (offline).
#[derive(Args, Debug, Clone)]
pub struct RestoreArgs {
    /// The archive: a file, or s3://bucket/path/to/ferrix-….tar.zst.
    #[arg(required_unless_present_any = ["cancel", "from"])]
    pub backup: Option<String>,
    /// Cancel the restore staged from the dashboard (it would otherwise be applied at the next start).
    #[arg(long, conflicts_with_all = ["backup", "skip_verify", "storage_dir", "from"])]
    pub cancel: bool,
    /// Restore a point in time from a replication target (what `serve --replicate-to` wrote): a directory,
    /// file:///dir or s3://bucket/prefix. Default point: the newest replicated commit.
    #[arg(long, value_name = "LOCATION", conflicts_with = "backup")]
    pub from: Option<String>,
    /// With --from: the commit ts to restore.
    #[arg(long, value_name = "TS", requires = "from", conflicts_with = "to_time")]
    pub to_ts: Option<u64>,
    /// With --from: the time to restore (RFC 3339, e.g. 2026-10-09T12:00:00Z): the state after the last commit
    /// at or before it.
    #[arg(long, value_name = "TIME", requires = "from", value_parser = super::parse_time)]
    pub to_time: Option<f64>,
    /// With --from: the generation to restore from (default: the newest, or the one live at --to-time).
    #[arg(long, value_name = "ID", requires = "from")]
    pub generation: Option<String>,
    /// With --from: list the generations and restorable ranges of the target, and restore nothing.
    #[arg(long, requires = "from")]
    pub list: bool,
    /// Data directory.
    #[arg(long, default_value = "fx_data")]
    pub dir: PathBuf,
    /// Where to put restored file blobs. Default: the local storage directory (<dir>/storage).
    #[arg(long, value_name = "DIR")]
    pub storage_dir: Option<PathBuf>,
    /// Don't open the restored database with this binary's tables to check its schema and indexes.
    #[arg(long)]
    pub skip_verify: bool,
}

pub fn run_restore(args: RestoreArgs, storage: StorageConfig) -> crate::Result<()> {
    let storage_dir = args.storage_dir.clone().or_else(|| backup::local_storage_dir(&args.dir, &storage.backend));
    let options = RestoreOptions { storage_dir, skip_verify: args.skip_verify };
    if args.cancel {
        let _lock = super::DirLock::acquire(&args.dir)
            .context("cancel the staged restore from the dashboard while the server runs")?;
        match super::restore::cancel(&args.dir)? {
            true => println!("Canceled the staged restore of {}.", args.dir.display()),
            false => println!("No restore is staged in {}.", args.dir.display()),
        }
        return Ok(());
    }
    if let Some(p) = super::restore::pending(&args.dir)?.filter(|_| !args.list) {
        anyhow::bail!(
            "a restore of {} is staged in {} (from the dashboard) and would replace this one at the next start; cancel \
             it first with `restore --cancel`",
            p.source,
            args.dir.display()
        );
    }
    if let Some(from) = &args.from {
        return runtime()?.block_on(restore_from_replica(&args, from, &options));
    }
    let given = args.backup.clone().context("which backup to restore?")?;
    runtime()?.block_on(async {
        std::fs::create_dir_all(&args.dir)?;
        let work = tempfile::Builder::new().prefix(".ferrix-download-").tempdir_in(&args.dir)?;
        let archive = match given.strip_prefix("s3://") {
            Some(rest) => {
                let (location, name) = rest.rsplit_once('/').context("expected s3://bucket/path/ferrix-….tar.zst")?;
                let target = BackupTarget::parse(&format!("s3://{location}"), &args.dir)?;
                let local = work.path().join(name);
                target.download(name, &local).await?;
                local
            }
            None => PathBuf::from(&given),
        };
        let report = backup::restore(&archive, &args.dir, &options).await?;
        println!(
            "Restored {} into {} (snapshot ts {}, taken {}).",
            given,
            args.dir.display(),
            report.manifest.ts,
            super::file_timestamp(report.manifest.created_at)
        );
        if let Some(previous) = report.previous {
            println!("The previous database files are in {} (delete them once you're happy).", previous.display());
        }
        if report.blobs_restored > 0 {
            println!("Restored {} file blob(s).", report.blobs_restored);
        }
        Ok(())
    })
}

fn time_str(ms: f64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms as i64)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| "?".into())
}

/// `restore --from <replica> [--to-ts N | --to-time T] [--generation G] [--list]`.
async fn restore_from_replica(args: &RestoreArgs, from: &str, options: &RestoreOptions) -> crate::Result<()> {
    std::fs::create_dir_all(&args.dir)?;
    let target = BackupTarget::parse(from, &args.dir)?;
    if args.list {
        let generations = super::replicate::list_generations(&target).await?;
        if generations.is_empty() {
            println!("No replicated data in {}.", target.location);
        }
        for g in &generations {
            let (Some(from_ts), Some(to_ts)) = (g.oldest_ts(), g.newest_ts()) else {
                println!("generation {} (started {}): no snapshot, not restorable", g.id, time_str(g.created_at));
                continue;
            };
            println!(
                "generation {} (started {}): ts {from_ts}..={to_ts}, {} snapshot(s), {} segment(s)",
                g.id,
                time_str(g.created_at),
                g.snapshots.len(),
                g.segments.len()
            );
            for s in &g.snapshots {
                println!("  snapshot {} (ts {}, taken {})", s.name, s.ts, time_str(s.created_at));
            }
        }
        return Ok(());
    }
    let point =
        super::replicate::PointInTime { to_ts: args.to_ts, to_time: args.to_time, generation: args.generation.clone() };
    let (report, info) = super::replicate::restore_point_in_time(&target, &args.dir, &point, options).await?;
    println!(
        "Restored ts {} (committed {}) of generation {} into {}: snapshot ts {} plus {} replayed commit(s) from {} segment(s).",
        info.ts,
        time_str(info.time),
        info.generation,
        args.dir.display(),
        info.snapshot_ts,
        info.replayed,
        info.segments
    );
    if !info.reached {
        println!("The point asked for is beyond the newest replicated commit; restored that one instead.");
    }
    if let Some(previous) = report.previous {
        println!("The previous database files are in {} (delete them once you're happy).", previous.display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------------
// export / import
// ---------------------------------------------------------------------------------------------------------------------

/// `export`: write tables as JSONL or CSV (works while the server runs).
#[derive(Args, Debug, Clone)]
pub struct ExportArgs {
    /// Data directory.
    #[arg(long, default_value = "fx_data")]
    pub dir: PathBuf,
    /// Tables to export (comma-separated; system tables allowed). Default: every app table.
    #[arg(long, value_delimiter = ',')]
    pub tables: Vec<String>,
    #[arg(long, value_enum, default_value = "jsonl")]
    pub format: ExportFormat,
    /// Output directory. Default: export-<time>.
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,
    /// Don't copy the blobs of exported `_storage` documents.
    #[arg(long)]
    pub no_files: bool,
    /// Where uploaded files are stored, if not <dir>/storage (as for `serve`).
    #[arg(long, env = "FERRIX_STORAGE", value_name = "LOCATION")]
    pub storage: Option<String>,
}

pub fn run_export(args: ExportArgs, storage: StorageConfig) -> crate::Result<()> {
    let storage = storage_config(args.storage.as_deref(), storage)?;
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("export-{}", super::file_timestamp(crate::db::wall_clock_ms()))));
    let options = ExportOptions {
        tables: (!args.tables.is_empty()).then(|| args.tables.clone()),
        format: args.format,
        include_files: !args.no_files,
    };
    runtime()?.block_on(async {
        let blobs = export::BlobStore::from_config(&storage, &args.dir).ok();
        let manifest = export::export_to_dir(&args.dir, blobs, &out, &options).await?;
        for t in &manifest.tables {
            println!("{:<24} {:>8} document(s)  {}", t.name, t.documents, out.join(&t.file).display());
        }
        if manifest.files > 0 || manifest.missing_files > 0 {
            println!(
                "{} file blob(s) in {} ({} missing)",
                manifest.files,
                out.join("files").display(),
                manifest.missing_files
            );
        }
        println!("Exported at ts {} into {}", manifest.ts, out.display());
        Ok(())
    })
}

/// `import`: load JSONL or CSV through the normal write path (offline: the server must not be running).
#[derive(Args, Debug, Clone)]
pub struct ImportArgs {
    /// An export directory, a directory of <table>.jsonl / <table>.csv files, or one .jsonl or .csv file.
    pub path: PathBuf,
    /// Data directory.
    #[arg(long, default_value = "fx_data")]
    pub dir: PathBuf,
    #[arg(long, value_enum, default_value = "insert")]
    pub mode: ImportMode,
    /// With a single file: the table (default: the file name without its extension). With a directory: only this
    /// table.
    #[arg(long)]
    pub table: Option<String>,
    /// jsonl or csv (as `export --format csv` writes it: a header, values typed with the table's shape). Default:
    /// from the file extension (.csv is CSV), or the export's manifest.
    #[arg(long, value_enum)]
    pub format: Option<ExportFormat>,
    /// Documents per transaction.
    #[arg(long, default_value_t = export::DEFAULT_BATCH_SIZE)]
    pub batch_size: usize,
    /// Where uploaded files are stored, if not <dir>/storage (as for `serve`).
    #[arg(long, env = "FERRIX_STORAGE", value_name = "LOCATION")]
    pub storage: Option<String>,
}

pub fn run_import(args: ImportArgs, mut options: AppOptions) -> crate::Result<()> {
    options.storage = storage_config(args.storage.as_deref(), options.storage)?;
    let _lock = super::DirLock::acquire(&args.dir).context(
        "import runs offline; to import into a running server use POST /api/admin/import/{table} (docs/CONTRACT.md §4.2)",
    )?;
    runtime()?.block_on(async {
        let app = App::open(&args.dir, options).await?;
        let import = ImportOptions {
            mode: args.mode,
            batch_size: args.batch_size,
            allow_system_tables: true,
            format: args.format,
        };
        let report = export::import_path(&app, &args.path, args.table.as_deref(), &import).await?;
        for t in &report.tables {
            println!(
                "{:<24} {:>8} document(s) written ({} replaced, {} deleted first)",
                t.table, t.documents, t.replaced, t.deleted
            );
        }
        if report.files > 0 || report.missing_files > 0 {
            println!("{} file blob(s) stored ({} documents without a blob)", report.files, report.missing_files);
        }
        if report.remapped_ids {
            println!("Ids were rewritten: some tables have different numbers in this database.");
        }
        app.logs().flush().await?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser, Debug)]
    struct T {
        #[command(flatten)]
        ops: OpsServeArgs,
    }

    fn parse(args: &[&str]) -> Result<OpsServeArgs, clap::Error> {
        T::try_parse_from(std::iter::once("app").chain(args.iter().copied())).map(|t| t.ops)
    }

    #[test]
    fn serve_flags() {
        let http: SocketAddr = "127.0.0.1:8090".parse().unwrap();
        let dir = Path::new("data");
        // Defaults: plain HTTP, no metrics, no scheduled backups.
        let ops = parse(&[]).unwrap();
        assert!(matches!(ops.listen(http, dir).unwrap(), Listen::Http(a) if a == http));
        let mut options = AppOptions::default();
        ops.apply(&mut options);
        assert!(!options.ops.metrics.enabled && options.scheduler.crons.is_empty() && ops.public_url().is_none());

        let ops = parse(&[
            "--metrics",
            "--metrics-token",
            "t",
            "--backup-every",
            "6h",
            "--backup-keep",
            "3",
            "--backup-to",
            "s3://b/p",
        ])
        .unwrap();
        let mut options = AppOptions::default();
        ops.apply(&mut options);
        assert!(options.ops.metrics.enabled && options.ops.metrics.token.as_deref() == Some("t"));
        assert_eq!(options.ops.backup_target.as_deref(), Some("s3://b/p"));
        assert_eq!(options.scheduler.crons.iter().map(|c| c.name()).collect::<Vec<_>>(), [backup::CRON_NAME]);

        // --https: ACME on :443, redirect from :80, cache in <dir>/acme, public URL.
        let ops =
            parse(&["--https", "Example.com,www.example.com", "--acme-email", "a@b.c", "--acme-staging"]).unwrap();
        let Listen::Acme { https, redirect, acme } = ops.listen(http, dir).unwrap() else { panic!("acme") };
        assert_eq!(https, "0.0.0.0:443".parse().unwrap());
        assert_eq!(redirect, Some("0.0.0.0:80".parse().unwrap()));
        assert_eq!(acme.domains, ["example.com", "www.example.com"]);
        assert!(acme.staging && acme.cache_dir == dir.join("acme") && acme.emails == ["a@b.c"]);
        assert_eq!(ops.public_url().as_deref(), Some("https://example.com"));
        let ops = parse(&["--https", "example.com", "--https-addr", "0.0.0.0:8443", "--http-redirect", "off"]).unwrap();
        assert!(matches!(ops.listen(http, dir).unwrap(), Listen::Acme { redirect: None, .. }));
        assert_eq!(ops.public_url().as_deref(), Some("https://example.com:8443"));

        // Certificate files: both needed, not with --https; no redirect unless asked.
        let ops = parse(&["--tls-cert", "c.pem", "--tls-key", "k.pem"]).unwrap();
        assert!(
            matches!(ops.listen(http, dir).unwrap(), Listen::CertFiles { redirect: None, reload_every: Some(d), .. } if d == Duration::from_secs(30))
        );
        let ops = parse(&["--tls-cert", "c.pem", "--tls-key", "k.pem", "--tls-reload-every", "0"]).unwrap();
        assert!(matches!(ops.listen(http, dir).unwrap(), Listen::CertFiles { reload_every: None, .. }));
        let ops = parse(&["--tls-cert", "c.pem", "--tls-key", "k.pem", "--http-redirect", "127.0.0.1:8080"]).unwrap();
        assert!(matches!(ops.listen(http, dir).unwrap(), Listen::CertFiles { redirect: Some(_), .. }));
        assert!(parse(&["--tls-cert", "c.pem"]).is_err());
        assert!(parse(&["--tls-cert", "c.pem", "--tls-key", "k.pem", "--https", "example.com"]).is_err());
        assert!(parse(&["--https", "example.com", "--http-redirect", "nope"]).unwrap().listen(http, dir).is_err());
        assert!(parse(&["--backup-every", "soon"]).is_err());

        // Replication: off by default; the flags fill `ops.replication`.
        assert!(options.ops.replication.is_none());
        let ops = parse(&[
            "--replicate-to",
            "s3://b/r",
            "--replicate-every",
            "10s",
            "--snapshot-every",
            "6h",
            "--replicate-keep",
            "3d",
            "--replicate-max-pending",
            "200MB",
        ])
        .unwrap();
        let mut options = AppOptions::default();
        ops.apply(&mut options);
        let r = options.ops.replication.unwrap();
        assert_eq!(
            (r.target.as_str(), r.every, r.snapshot_every, r.keep),
            ("s3://b/r", Duration::from_secs(10), Duration::from_secs(6 * 3600), Duration::from_secs(3 * 86400))
        );
        assert_eq!(r.max_pending_bytes, 200 << 20);
        let defaults = parse(&["--replicate-to", "/r"]).unwrap();
        assert_eq!(
            (defaults.replicate_every, defaults.snapshot_every, defaults.replicate_keep),
            (Duration::from_secs(1), Duration::from_secs(86400), Duration::from_secs(7 * 86400))
        );
    }
}
