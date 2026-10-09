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
    let spec = BackupSpec { include_aux: args.with_aux, include_files: args.with_files, include_secret: args.include_secret };
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

/// `restore`: replace the data directory's database with a backup (offline).
#[derive(Args, Debug, Clone)]
pub struct RestoreArgs {
    /// The archive: a file, or s3://bucket/path/to/ferrix-….tar.zst.
    #[arg(required_unless_present = "cancel")]
    pub backup: Option<String>,
    /// Cancel the restore staged from the dashboard (it would otherwise be applied at the next start).
    #[arg(long, conflicts_with_all = ["backup", "skip_verify", "storage_dir"])]
    pub cancel: bool,
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
        let _lock = super::DirLock::acquire(&args.dir).context("cancel the staged restore from the dashboard while the server runs")?;
        match super::restore::cancel(&args.dir)? {
            true => println!("Canceled the staged restore of {}.", args.dir.display()),
            false => println!("No restore is staged in {}.", args.dir.display()),
        }
        return Ok(());
    }
    let given = args.backup.clone().context("which backup to restore?")?;
    if let Some(p) = super::restore::pending(&args.dir)? {
        anyhow::bail!(
            "a restore of {} is staged in {} (from the dashboard) and would replace this one at the next start; cancel \
             it first with `restore --cancel`",
            p.source,
            args.dir.display()
        );
    }
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
    let out = args.out.clone().unwrap_or_else(|| PathBuf::from(format!("export-{}", super::file_timestamp(crate::db::wall_clock_ms()))));
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
            println!("{} file blob(s) in {} ({} missing)", manifest.files, out.join("files").display(), manifest.missing_files);
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
        let import =
            ImportOptions { mode: args.mode, batch_size: args.batch_size, allow_system_tables: true, format: args.format };
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

        let ops = parse(&["--metrics", "--metrics-token", "t", "--backup-every", "6h", "--backup-keep", "3", "--backup-to", "s3://b/p"])
            .unwrap();
        let mut options = AppOptions::default();
        ops.apply(&mut options);
        assert!(options.ops.metrics.enabled && options.ops.metrics.token.as_deref() == Some("t"));
        assert_eq!(options.ops.backup_target.as_deref(), Some("s3://b/p"));
        assert_eq!(options.scheduler.crons.iter().map(|c| c.name()).collect::<Vec<_>>(), [backup::CRON_NAME]);

        // --https: ACME on :443, redirect from :80, cache in <dir>/acme, public URL.
        let ops = parse(&["--https", "Example.com,www.example.com", "--acme-email", "a@b.c", "--acme-staging"]).unwrap();
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
        assert!(matches!(ops.listen(http, dir).unwrap(), Listen::CertFiles { redirect: None, reload_every: Some(d), .. } if d == Duration::from_secs(30)));
        let ops = parse(&["--tls-cert", "c.pem", "--tls-key", "k.pem", "--tls-reload-every", "0"]).unwrap();
        assert!(matches!(ops.listen(http, dir).unwrap(), Listen::CertFiles { reload_every: None, .. }));
        let ops = parse(&["--tls-cert", "c.pem", "--tls-key", "k.pem", "--http-redirect", "127.0.0.1:8080"]).unwrap();
        assert!(matches!(ops.listen(http, dir).unwrap(), Listen::CertFiles { redirect: Some(_), .. }));
        assert!(parse(&["--tls-cert", "c.pem"]).is_err());
        assert!(parse(&["--tls-cert", "c.pem", "--tls-key", "k.pem", "--https", "example.com"]).is_err());
        assert!(parse(&["--https", "example.com", "--http-redirect", "nope"]).unwrap().listen(http, dir).is_err());
        assert!(parse(&["--backup-every", "soon"]).is_err());
    }
}
