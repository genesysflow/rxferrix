//! Command line: `ferrix [serve] [--http ADDR] [--dir DIR] [--dev [--codegen-out DIR]] [auth options]
//! [--storage LOCATION] [--max-upload-size SIZE] [--storage-private] [operations flags]`,
//! `ferrix functions`, `ferrix codegen [--out DIR]`, `ferrix admin create|reset-password <email>`,
//! `ferrix migrate [--name NAME] [--dry-run] [--restart]` / `ferrix migrate status` (`crate::migrations`), and the
//! operations commands `backup`, `restore`, `export`, `import` (flags and commands in `crate::ops::cli`).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::auth::{AuthConfig, SmtpConfig};
use crate::runtime::{App, AppOptions, registry};
use crate::storage::{StorageBackend, StorageConfig};
use crate::scheduler::SchedulerOptions;
use crate::SchedulableFunction;

#[derive(Parser, Debug)]
#[command(about = "A Ferrix backend", args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Options for `serve`, which runs when no subcommand is given.
    #[command(flatten)]
    serve: ServeArgs,
}

// Parsed once at startup: the size difference between variants doesn't matter.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand, Debug)]
enum Command {
    /// Run the server (default).
    Serve(ServeArgs),
    /// List registered functions with kind, visibility and TS signature.
    Functions,
    /// Write the TypeScript bindings (api.ts, dataModel.ts, index.ts) for the client packages.
    Codegen(CodegenArgs),
    /// Manage dashboard administrators.
    #[command(subcommand)]
    Admin(AdminCommand),
    /// Take a consistent backup (.tar.zst) of the data directory; works while the server runs.
    Backup(crate::ops::cli::BackupArgs),
    /// Restore a backup into the data directory (offline: refuses while a server uses it).
    Restore(crate::ops::cli::RestoreArgs),
    /// Export tables as JSONL or CSV; works while the server runs.
    Export(crate::ops::cli::ExportArgs),
    /// Import JSONL or CSV (an export directory or files) through the normal write path (offline).
    Import(crate::ops::cli::ImportArgs),
    /// Run data migrations (`#[migration]`), or show their status with `migrate status`. Stop the server first, or
    /// use `serve --migrate` / the dashboard to migrate a running server.
    Migrate(MigrateArgs),
}

#[derive(Args, Debug, Clone)]
#[command(args_conflicts_with_subcommands = true)]
struct MigrateArgs {
    #[command(subcommand)]
    command: Option<MigrateCommand>,
    /// Run only this migration (default: every migration that isn't done, in name order, stopping at a failure).
    #[arg(long)]
    name: Option<String>,
    /// Run over every document and report what would change, without writing anything.
    #[arg(long)]
    dry_run: bool,
    /// Start the migration over from the first document, even if it is done.
    #[arg(long, requires = "name")]
    restart: bool,
    /// Data directory.
    #[arg(long, default_value = "fx_data")]
    dir: PathBuf,
    /// Format of the progress log lines (default: pretty; the report itself is plain text).
    #[arg(long, env = "FERRIX_LOG_FORMAT", value_enum, value_name = "FORMAT")]
    log_format: Option<crate::log::LogFormat>,
}

#[derive(Subcommand, Debug, Clone)]
enum MigrateCommand {
    /// List the migrations with their state and progress.
    Status {
        /// Data directory.
        #[arg(long, default_value = "fx_data")]
        dir: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
enum AdminCommand {
    /// Create an administrator. Asks for the password (read from stdin) unless --password is given.
    Create(AdminArgs),
    /// Set an administrator's password and end their sessions.
    ResetPassword(AdminArgs),
}

#[derive(Args, Debug, Clone)]
struct AdminArgs {
    email: String,
    /// The password. It ends up in shell history and process lists; prefer the prompt.
    #[arg(long)]
    password: Option<String>,
    /// Data directory.
    #[arg(long, default_value = "fx_data")]
    dir: PathBuf,
}

/// Environment variable naming the codegen output directory for `serve --dev` (same as `--codegen-out`).
pub const CODEGEN_OUT_ENV: &str = "FERRIX_CODEGEN_OUT";

#[derive(Args, Debug, Clone)]
struct CodegenArgs {
    /// Output directory, e.g. `../web/src/ferrix`. Only files whose content changed are rewritten.
    #[arg(long, value_name = "DIR", default_value = "ferrix-generated")]
    out: PathBuf,
}

#[derive(Args, Debug, Clone)]
struct ServeArgs {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8090")]
    http: SocketAddr,
    /// Data directory.
    #[arg(long, default_value = "fx_data")]
    dir: PathBuf,
    /// Development mode: real error messages are sent to clients.
    #[arg(long)]
    dev: bool,
    /// With --dev: run codegen into DIR on startup (like `ferrix codegen --out DIR`). Defaults to the
    /// FERRIX_CODEGEN_OUT environment variable; without either, no codegen runs.
    #[arg(long, value_name = "DIR", requires = "dev")]
    codegen_out: Option<PathBuf>,
    /// Public base URL of this server, used for OAuth callback URLs and the admin setup link
    /// (default: http://<--http address>).
    #[arg(long, env = "FERRIX_PUBLIC_URL", value_name = "URL")]
    public_url: Option<String>,
    /// App URL that auth may redirect to after OAuth sign-in and from email links (repeatable; an origin allows all
    /// of its paths). The first one is the default target.
    #[arg(long = "auth-redirect-url", env = "FERRIX_AUTH_REDIRECT_URLS", value_delimiter = ',', value_name = "URL")]
    auth_redirect_urls: Vec<String>,
    /// SMTP server for auth emails, e.g. smtps://user:pass@smtp.example.com:465. In --dev without SMTP, emails are
    /// printed to the log instead.
    #[arg(long, env = "FERRIX_SMTP_URL", value_name = "URL", hide_env_values = true)]
    smtp_url: Option<String>,
    /// Sender address for auth emails, e.g. "My App <noreply@example.com>".
    #[arg(long, env = "FERRIX_SMTP_FROM", value_name = "ADDRESS")]
    smtp_from: Option<String>,
    /// Take client IPs (for rate limiting) from the last X-Forwarded-For entry. Only behind a reverse proxy.
    #[arg(long, env = "FERRIX_TRUST_PROXY")]
    trust_proxy: bool,
    /// Roll back mutations still running after this long, e.g. `5s`, `500ms`, `2m` (a bare number is seconds).
    /// Only enforced at `.await` points.
    #[arg(long, value_name = "DURATION", default_value = "5s", value_parser = parse_duration)]
    mutation_timeout: Duration,
    /// Start even if existing documents may not match changed table types (they are not validated).
    #[arg(long)]
    skip_schema_check: bool,
    /// Run the migrations that aren't done (`#[migration]`, in name order) in the background after starting.
    #[arg(long, env = "FERRIX_MIGRATE")]
    migrate: bool,
    /// Where uploaded files are stored: a directory (default: <dir>/storage), file:///dir, or s3://bucket/prefix
    /// (S3, R2, MinIO; credentials, region and endpoint from the standard AWS_* environment variables).
    #[arg(long, env = "FERRIX_STORAGE", value_name = "LOCATION")]
    storage: Option<String>,
    /// Largest accepted upload, e.g. `100MB` (default), `2GB`, `512KB`, or a number of bytes.
    #[arg(long, env = "FERRIX_MAX_UPLOAD_SIZE", value_name = "SIZE", value_parser = parse_size)]
    max_upload_size: Option<u64>,
    /// Private file storage: plain /api/storage/<id> URLs are refused (403); files are served only through signed,
    /// expiring URLs (ctx.storage.get_signed_url).
    #[arg(long, env = "FERRIX_STORAGE_PRIVATE")]
    storage_private: bool,
    /// How log lines are written to stdout: `json` (one object per line), `pretty` or `compact`. Default: pretty on a
    /// terminal, json otherwise. `RUST_LOG` filters them either way.
    #[arg(long, env = "FERRIX_LOG_FORMAT", value_enum, value_name = "FORMAT")]
    log_format: Option<crate::log::LogFormat>,
    /// Metrics, scheduled backups and TLS (`crate::ops::cli`).
    #[command(flatten)]
    ops: crate::ops::cli::OpsServeArgs,
}

/// `"100MB"`, `"2GB"`, `"512KB"`, `"1.5MB"` (powers of 1024), or a bare number of bytes.
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = (&s[..split], s[split..].trim());
    let n: f64 = num.parse().map_err(|_| format!("invalid size `{s}` (examples: 100MB, 2GB, 512KB)"))?;
    let factor: u64 = match unit.to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1 << 10,
        "M" | "MB" | "MIB" => 1 << 20,
        "G" | "GB" | "GIB" => 1 << 30,
        _ => return Err(format!("invalid size unit in `{s}` (use B, KB, MB or GB)")),
    };
    Ok((n * factor as f64) as u64)
}

/// `"500ms"`, `"5s"`, `"2m"`, or a bare number of seconds.
fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, unit) = match s.find(|c: char| !(c.is_ascii_digit() || c == '.')) {
        Some(i) => (&s[..i], s[i..].trim()),
        None => (s, "s"),
    };
    let n: f64 = num.parse().map_err(|_| format!("invalid duration `{s}` (examples: 500ms, 5s, 2m)"))?;
    let secs = match unit {
        "ms" => n / 1000.0,
        "s" => n,
        "m" => n * 60.0,
        _ => return Err(format!("invalid duration unit in `{s}` (use ms, s or m)")),
    };
    Duration::try_from_secs_f64(secs).map_err(|e| format!("invalid duration `{s}`: {e}"))
}

/// Entry point for application binaries: `fn main() -> ferrix::Result<()> { Ferrix::new().run() }`.
#[derive(Debug, Default)]
pub struct Ferrix {
    auth: AuthConfig,
    storage: StorageConfig,
    scheduler: SchedulerOptions,
    web: crate::ops::WebOptions,
}

impl Ferrix {
    /// Serve the app's frontend from a directory (read at runtime; relative to the working directory), with SPA
    /// fallback to `index.html` (see [`crate::ops::web`]).
    pub fn static_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.web.static_files = Some(crate::ops::StaticFiles::Dir(dir.into()));
        self
    }

    /// Serve the app's frontend from files embedded in the binary:
    /// `#[derive(ferrix::rust_embed::Embed)] #[folder = "../web/dist/"] #[crate_path = "ferrix::rust_embed"] struct
    /// Assets;` then `.embed_static::<Assets>()` (see [`crate::ops::web`]).
    pub fn embed_static<A: rust_embed::RustEmbed>(mut self) -> Self {
        self.web.static_files = Some(crate::ops::StaticFiles::Embedded(A::get));
        self
    }

    /// Add axum routes (webhooks, custom endpoints) next to Ferrix's own; they must not overlap `/api/*`, `/_/*` or
    /// `/metrics`. Repeated calls add up.
    pub fn routes(mut self, routes: axum::Router) -> Self {
        self.web.routes = Some(match self.web.routes.take() {
            Some(existing) => existing.merge(routes),
            None => routes,
        });
        self
    }

    pub fn new() -> Self {
        Ferrix::default()
    }

    /// Configure authentication: `Ferrix::new().auth(AuthConfig::password().oauth(GitHub::from_env()))`.
    /// Command line flags (`--public-url`, `--auth-redirect-url`, `--smtp-url`, ...) add to or override it.
    pub fn auth(mut self, auth: AuthConfig) -> Self {
        self.auth = auth;
        self
    }

    /// Configure file storage: `Ferrix::new().storage(StorageConfig::s3("bucket", "uploads").max_upload_size(..))`.
    /// `--storage` and `--max-upload-size` override it.
    pub fn storage(mut self, storage: StorageConfig) -> Self {
        self.storage = storage;
        self
    }

    /// Run `F` with `args` on a cron schedule (6 fields with seconds, UTC): `.cron::<cleanup>("cleanup",
    /// "0 0 3 * * *", ())`. See [`crate::scheduler::CronSpec::cron`].
    pub fn cron<F: SchedulableFunction>(mut self, name: impl Into<String>, expr: impl Into<String>, args: impl Into<F::Args>) -> Self {
        self.scheduler = self.scheduler.cron::<F>(name, expr, args);
        self
    }

    /// [`Ferrix::cron`] evaluated in an IANA time zone, DST included: `.cron_tz::<report>("report", "0 0 9 * * *",
    /// "Europe/Bucharest", ())`. See [`crate::scheduler::CronSpec::timezone`].
    pub fn cron_tz<F: SchedulableFunction>(
        mut self,
        name: impl Into<String>,
        expr: impl Into<String>,
        tz: impl Into<String>,
        args: impl Into<F::Args>,
    ) -> Self {
        self.scheduler = self.scheduler.cron_tz::<F>(name, expr, tz, args);
        self
    }

    /// Run `F` with `args` every `every`: `.interval::<sync>("sync", Duration::from_secs(300), ())`.
    pub fn interval<F: SchedulableFunction>(mut self, name: impl Into<String>, every: Duration, args: impl Into<F::Args>) -> Self {
        self.scheduler = self.scheduler.interval::<F>(name, every, args);
        self
    }

    /// Scheduler settings (concurrency, job retention, retry backoff). Crons declared with [`Ferrix::cron`] /
    /// [`Ferrix::interval`] are kept, before or after this call.
    pub fn scheduler(mut self, options: SchedulerOptions) -> Self {
        let declared = std::mem::take(&mut self.scheduler.crons);
        self.scheduler = options;
        self.scheduler.crons.splice(0..0, declared);
        self
    }

    /// Parse the command line and run it. Builds its own tokio runtime.
    pub fn run(self) -> crate::Result<()> {
        let cli = Cli::parse();
        match cli.command {
            Some(Command::Functions) => print_functions(),
            Some(Command::Codegen(args)) => codegen(&args.out),
            Some(Command::Admin(cmd)) => admin(cmd, self.auth),
            Some(Command::Backup(args)) => crate::ops::cli::run_backup(args, self.storage),
            Some(Command::Restore(args)) => crate::ops::cli::run_restore(args, self.storage),
            Some(Command::Export(args)) => crate::ops::cli::run_export(args, self.storage),
            Some(Command::Import(args)) => crate::ops::cli::run_import(
                args,
                AppOptions { auth: self.auth, storage: self.storage, scheduler: self.scheduler, ..Default::default() },
            ),
            Some(Command::Migrate(args)) => migrate(args, self.auth, self.storage, self.scheduler),
            Some(Command::Serve(args)) => serve(args, self.auth, self.storage, self.scheduler, self.web),
            None => serve(cli.serve, self.auth, self.storage, self.scheduler, self.web),
        }
    }
}

fn print_functions() -> crate::Result<()> {
    for f in registry()?.list() {
        println!("{:<8} {:<8} {}  (args: {}) => {}", f.kind.as_str(), f.visibility.as_str(), f.path, f.args_ts, f.output_ts);
    }
    Ok(())
}

fn codegen(out: &Path) -> crate::Result<()> {
    let changed = crate::codegen::write(out)?;
    println!("ferrix codegen: wrote {} ({} file(s) changed)", out.display(), changed.len());
    Ok(())
}

fn admin(cmd: AdminCommand, auth: AuthConfig) -> crate::Result<()> {
    let (args, create) = match cmd {
        AdminCommand::Create(a) => (a, true),
        AdminCommand::ResetPassword(a) => (a, false),
    };
    let password = match args.password {
        Some(p) => p,
        None => read_password()?,
    };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let app = App::open(&args.dir, AppOptions { auth, ..Default::default() }).await?;
        let email = crate::auth::normalize_email(&args.email)?;
        if create {
            app.auth().create_admin(&app, &email, password).await?;
            println!("Created administrator {email}");
        } else {
            app.auth().reset_admin_password(&app, &email, password).await?;
            println!("Updated the password of {email} and ended their sessions");
        }
        Ok(())
    })
}

fn migrate(args: MigrateArgs, auth: AuthConfig, storage: StorageConfig, scheduler: SchedulerOptions) -> crate::Result<()> {
    let stdout_filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn,ferrix::migrations=info".into());
    let format = args.log_format.unwrap_or(crate::log::LogFormat::Pretty);
    let _ = crate::log::cli_subscriber(stdout_filter, std::io::stdout, format).try_init();
    let dir = match &args.command {
        Some(MigrateCommand::Status { dir }) => dir.clone(),
        None => args.dir.clone(),
    };
    // Running migrations is offline: refuse while a server holds the data directory (`crate::ops::lock`).
    let _lock = match &args.command {
        Some(MigrateCommand::Status { .. }) => None,
        None => Some(crate::ops::DirLock::acquire(&dir)?),
    };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let app = App::open(&dir, AppOptions { auth, storage, scheduler, ..Default::default() }).await?;
        if matches!(args.command, Some(MigrateCommand::Status { .. })) {
            print_migration_status(&app).await?;
            return Ok(());
        }
        let options = crate::migrations::RunOptions { dry_run: args.dry_run, restart: args.restart, ..Default::default() };
        let reports = match &args.name {
            Some(name) => vec![app.run_migration_named(name, options).await?],
            None => app.run_pending_migrations(options).await?,
        };
        if reports.is_empty() {
            println!("ferrix migrate: nothing to do (no migration is pending)");
        }
        let mut failed = false;
        for r in &reports {
            println!("{}", describe_report(r));
            failed |= r.outcome == crate::migrations::RunOutcome::Failed;
        }
        if failed {
            anyhow::bail!("a migration failed; fix it and run `migrate` again to resume");
        }
        Ok(())
    })
}

/// One report, as `ferrix migrate` prints it.
fn describe_report(r: &crate::migrations::MigrationReport) -> String {
    use crate::migrations::RunOutcome;
    let mut s = format!("{} (table `{}`): ", r.name, r.table);
    s.push_str(&match r.outcome {
        RunOutcome::AlreadyDone => "already done".to_string(),
        RunOutcome::AlreadyRunning => "already running elsewhere".to_string(),
        RunOutcome::DryRun => format!("dry run: would change {} of {} document(s)", r.changed, r.processed),
        outcome => format!(
            "{}: {} batch(es), {} document(s) processed, {} changed",
            serde_json::to_value(outcome).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default(),
            r.batches,
            r.processed,
            r.changed
        ),
    });
    if let Some(e) = &r.error {
        match &r.failed_document {
            Some(doc) => s.push_str(&format!("\n  failed on document {doc}: {e}")),
            None => s.push_str(&format!("\n  error: {e}")),
        }
    }
    if let Some(check) = &r.schema_check {
        s.push_str(&format!("\n  schema check of `{}`: {check}", r.table));
    }
    for ex in &r.examples {
        let after = ex.after.as_ref().map_or("(deleted)".to_string(), |v| v.to_string());
        s.push_str(&format!("\n  {}:\n    before: {}\n    after:  {after}", ex.id, ex.before));
    }
    s
}

async fn print_migration_status(app: &App) -> crate::Result<()> {
    let migrations = app.migration_summaries().await?;
    if migrations.is_empty() {
        println!("no migrations are declared");
    }
    for m in migrations {
        let mut line =
            format!("{:<10} {}  (table `{}`, {}/{} processed", m.state.as_str(), m.name, m.table, m.processed, m.documents);
        if !m.declared {
            line.push_str(", no longer declared");
        }
        line.push(')');
        if let Some(e) = &m.error {
            line.push_str(&format!("\n           error: {e}"));
        }
        if let Some(check) = &m.schema_check {
            line.push_str(&format!("\n           schema check: {check}"));
        }
        println!("{line}");
    }
    for table in app.db().catalog().migrating_tables() {
        println!("table `{table}` has documents that don't match its type until its migrations have run");
    }
    Ok(())
}

/// Read a password from stdin (one line). Prompts on stderr when stdin is a terminal (the input is echoed).
fn read_password() -> crate::Result<String> {
    use std::io::{BufRead, IsTerminal, Write};
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        eprint!("Password: ");
        std::io::stderr().flush()?;
    }
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    let password = line.trim_end_matches(['\r', '\n']).to_string();
    if password.is_empty() {
        anyhow::bail!("no password given");
    }
    Ok(password)
}

/// The builder's auth config with the command line's options applied on top.
fn auth_config(args: &ServeArgs, mut auth: AuthConfig) -> AuthConfig {
    let public_url = args.public_url.clone().or_else(|| auth.public_url.clone()).or_else(|| args.ops.public_url()).unwrap_or_else(|| {
        let addr = if args.http.ip().is_unspecified() {
            SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), args.http.port())
        } else {
            args.http
        };
        format!("http://{addr}")
    });
    auth.public_url = Some(public_url);
    for url in args.auth_redirect_urls.iter().map(|u| u.trim()).filter(|u| !u.is_empty()) {
        if !auth.redirect_urls.iter().any(|u| u == url) {
            auth.redirect_urls.push(url.to_string());
        }
    }
    let smtp = args.smtp_url.clone().map(|url| {
        let from = args.smtp_from.clone().or_else(|| auth.smtp.as_ref().map(|s| s.from.clone()));
        SmtpConfig::new(url, from.unwrap_or_else(|| "ferrix <noreply@localhost>".into()))
    });
    if smtp.is_some() {
        auth.smtp = smtp;
    } else if let (Some(from), Some(s)) = (&args.smtp_from, auth.smtp.as_mut()) {
        s.from = from.clone();
    }
    if args.trust_proxy {
        auth.trust_proxy = true;
    }
    auth
}

/// Whether a public URL was given explicitly (`--public-url` / `FERRIX_PUBLIC_URL`, `AuthConfig::public_url`, or a
/// TLS domain), before [`auth_config`] fills in the default `http://<--http address>`.
fn explicit_public_url(args: &ServeArgs, auth: &AuthConfig) -> bool {
    args.public_url.is_some() || auth.public_url.is_some() || args.ops.public_url().is_some()
}

/// The builder's storage config with `--storage` / `--max-upload-size` / `--storage-private` applied on top.
///
/// In `--dev`, unless a public URL was set explicitly (`explicit_public_url`, or `StorageConfig::public_url`), file
/// and upload URLs are root-relative (`/api/storage/<id>`), so an app served by a dev server that proxies `/api`
/// (Vite) loads and uploads files through the proxy, same-origin, instead of from the backend's own address.
fn storage_config(args: &ServeArgs, mut storage: StorageConfig, explicit_public_url: bool) -> crate::Result<StorageConfig> {
    if args.dev && !explicit_public_url && storage.public_url.is_none() {
        storage.public_url = Some(String::new());
    }
    if let Some(location) = args.storage.as_deref().filter(|l| !l.trim().is_empty()) {
        storage.backend = StorageBackend::parse(location)?;
    }
    if let Some(max) = args.max_upload_size {
        storage.max_upload_size = max;
    }
    if args.storage_private {
        storage.private = true;
    }
    Ok(storage)
}

fn serve(
    args: ServeArgs,
    auth: AuthConfig,
    storage: StorageConfig,
    scheduler: SchedulerOptions,
    web: crate::ops::WebOptions,
) -> crate::Result<()> {
    // stdout logging (filtered by RUST_LOG) plus the capture of function log lines.
    let stdout_filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,ferrix=info".into());
    let format = args.log_format.unwrap_or_else(crate::log::LogFormat::default_for_stdout);
    let _ = crate::log::cli_subscriber(stdout_filter, std::io::stdout, format).try_init();
    if args.dev {
        let out = args.codegen_out.clone().or_else(|| std::env::var_os(CODEGEN_OUT_ENV).map(PathBuf::from));
        if let Some(out) = out.filter(|o| !o.as_os_str().is_empty()) {
            let changed = crate::codegen::write(&out)?;
            tracing::info!("codegen: {} ({} file(s) changed)", out.display(), changed.len());
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let explicit = explicit_public_url(&args, &auth);
        let auth = auth_config(&args, auth);
        let storage = storage_config(&args, storage, explicit)?;
        let public_url = auth.public_url.clone().unwrap_or_default();
        let mut options = AppOptions {
            dev: args.dev,
            mutation_timeout: args.mutation_timeout,
            skip_schema_check: args.skip_schema_check,
            auth,
            storage,
            scheduler,
            ..Default::default()
        };
        args.ops.apply(&mut options);
        let listen = args.ops.listen(args.http, &args.dir)?;
        // Held while serving: `restore` refuses to run under a server.
        let _lock = crate::ops::DirLock::acquire(&args.dir)?;
        let app = App::open(&args.dir, options).await?;
        app.start_scheduler().await?;
        if args.migrate {
            let app = app.clone();
            tokio::spawn(async move {
                match app.run_pending_migrations(Default::default()).await {
                    Ok(reports) => reports.iter().for_each(|r| tracing::info!("migration {}", describe_report(r))),
                    Err(e) => tracing::error!("running migrations failed: {e:#}"),
                }
            });
        } else {
            let pending: Vec<String> = app
                .migration_summaries()
                .await?
                .into_iter()
                .filter(|m| m.declared && m.state != crate::migrations::MigrationState::Done)
                .map(|m| m.name)
                .collect();
            if !pending.is_empty() {
                tracing::warn!(?pending, "migrations are waiting; run them with `serve --migrate`, `migrate` or the dashboard");
            }
        }
        let server = crate::ops::tls::start(crate::ops::web::router(app.clone(), &web), &listen).await?;
        let scheme = if server.https { "https" } else { "http" };
        tracing::info!(
            "ferrix listening on {scheme}://{} (data: {}, dev: {})",
            server.addr,
            args.dir.display(),
            args.dev
        );
        if let Some(redirect) = server.redirect_addr {
            tracing::info!("redirecting http://{redirect} to https");
        }
        if let Some(token) = app.auth().create_setup_token(&app).await? {
            // Printed to the console rather than logged: a one-time secret, valid until used or until restart.
            println!(
                "\nNo administrator exists yet. Create one at:\n\n    {}/_/setup#token={token}\n\n\
                 (or run `admin create <email>`). The link works once and only until the server restarts.\n",
                public_url.trim_end_matches('/')
            );
        }
        server
            .run_until(async {
                crate::ops::tls::shutdown_signal().await;
                tracing::info!("shutting down");
            })
            .await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("app").chain(args.iter().copied()))
    }

    #[test]
    fn codegen_args() {
        let cli = parse(&["codegen"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Codegen(ref a)) if a.out == Path::new("ferrix-generated")));
        let cli = parse(&["codegen", "--out", "../web/src/ferrix"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Codegen(ref a)) if a.out == Path::new("../web/src/ferrix")));
    }

    #[test]
    fn admin_and_auth_args() {
        let cli = parse(&["admin", "create", "a@b.c", "--password", "pw", "--dir", "d"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Admin(AdminCommand::Create(ref a)))
            if a.email == "a@b.c" && a.password.as_deref() == Some("pw") && a.dir == Path::new("d")));
        let cli = parse(&["admin", "reset-password", "a@b.c"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Admin(AdminCommand::ResetPassword(ref a))) if a.password.is_none()));

        let cli = parse(&[
            "serve",
            "--auth-redirect-url",
            "http://a.test",
            "--auth-redirect-url",
            "http://b.test/x",
            "--public-url",
            "https://api.test",
        ])
        .unwrap();
        let Some(Command::Serve(args)) = cli.command else { panic!("expected serve") };
        let auth = auth_config(&args, AuthConfig::password().redirect_url("http://a.test"));
        assert_eq!(auth.redirect_urls, vec!["http://a.test".to_string(), "http://b.test/x".to_string()]);
        assert_eq!(auth.public_url.as_deref(), Some("https://api.test"));

        let cli = parse(&["serve", "--http", "0.0.0.0:9000"]).unwrap();
        let Some(Command::Serve(args)) = cli.command else { panic!("expected serve") };
        assert_eq!(auth_config(&args, AuthConfig::default()).public_url.as_deref(), Some("http://127.0.0.1:9000"));
    }

    #[test]
    fn serve_codegen_out_requires_dev() {
        let cli = parse(&["serve", "--dev", "--codegen-out", "gen"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Serve(ref a)) if a.codegen_out.as_deref() == Some(Path::new("gen"))));
        let cli = parse(&["--dev", "--codegen-out", "gen"]).unwrap();
        assert_eq!(cli.serve.codegen_out.as_deref(), Some(Path::new("gen")));
        assert!(parse(&["serve", "--codegen-out", "gen"]).is_err());
        assert!(parse(&["--codegen-out", "gen"]).is_err());
    }

    #[test]
    fn storage_flags() {
        let cli = parse(&["serve", "--storage", "s3://bucket/some/prefix", "--max-upload-size", "2GB", "--storage-private"]).unwrap();
        let Some(Command::Serve(args)) = cli.command else { panic!("expected serve") };
        let storage = storage_config(&args, StorageConfig::default(), false).unwrap();
        assert!(matches!(storage.backend, StorageBackend::S3 { ref bucket, ref prefix } if bucket == "bucket" && prefix == "some/prefix"));
        assert_eq!(storage.max_upload_size, 2 << 30);
        assert!(storage.private);
        assert_eq!(storage.public_url, None, "not in --dev: the public URL (default: the --http address) applies");
        // Without flags the builder's config stands.
        let cli = parse(&[]).unwrap();
        let storage = storage_config(&cli.serve, StorageConfig::local("/srv/files").max_upload_size(5), false).unwrap();
        assert!(matches!(storage.backend, StorageBackend::Local(Some(_))) && storage.max_upload_size == 5);
        assert_eq!(parse_size("100MB").unwrap(), 100 << 20);
        assert_eq!(parse_size("512kb").unwrap(), 512 << 10);
        assert_eq!(parse_size("1.5M").unwrap(), 3 << 19);
        assert_eq!(parse_size("1234").unwrap(), 1234);
        assert!(parse_size("5TB").is_err() && parse_size("big").is_err());
        let cli = parse(&["--storage", "gs://x"]).unwrap();
        assert!(storage_config(&cli.serve, StorageConfig::default(), false).is_err());
    }

    #[test]
    fn dev_storage_urls_are_root_relative_unless_a_public_url_is_set() {
        let serve_args = |args: &[&str]| match parse(args).unwrap().command {
            Some(Command::Serve(a)) => a,
            _ => panic!("expected serve"),
        };
        let storage = |args: &ServeArgs, auth: &AuthConfig, storage: StorageConfig| {
            storage_config(args, storage, explicit_public_url(args, auth)).unwrap().public_url
        };
        // --dev alone: root-relative (an empty base URL).
        let dev = serve_args(&["serve", "--dev", "--http", "127.0.0.1:8097"]);
        assert_eq!(storage(&dev, &AuthConfig::default(), StorageConfig::default()).as_deref(), Some(""));
        // An explicit public URL (flag, builder, storage config) wins.
        let flag = serve_args(&["serve", "--dev", "--public-url", "http://localhost:5180"]);
        assert_eq!(storage(&flag, &AuthConfig::default(), StorageConfig::default()), None);
        let builder = AuthConfig::default().public_url("https://app.test");
        assert_eq!(storage(&dev, &builder, StorageConfig::default()), None);
        let own = StorageConfig::default().public_url("https://cdn.test");
        assert_eq!(storage(&dev, &AuthConfig::default(), own).as_deref(), Some("https://cdn.test"));
        // Not --dev: unchanged (absolute URLs from the default public URL).
        let prod = serve_args(&["serve", "--http", "127.0.0.1:8097"]);
        assert_eq!(storage(&prod, &AuthConfig::default(), StorageConfig::default()), None);
    }

    #[test]
    fn migrate_args() {
        let cli = parse(&["migrate"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Migrate(ref a)) if a.command.is_none() && a.name.is_none() && !a.dry_run));
        let cli = parse(&["migrate", "--name", "todos:add", "--dry-run", "--dir", "d"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Migrate(ref a))
            if a.name.as_deref() == Some("todos:add") && a.dry_run && a.dir == Path::new("d")));
        let cli = parse(&["migrate", "status", "--dir", "d"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Migrate(MigrateArgs { command: Some(MigrateCommand::Status { ref dir }), .. }))
            if dir == Path::new("d")));
        assert!(parse(&["migrate", "--restart"]).is_err(), "--restart needs --name");
        let cli = parse(&["serve", "--migrate"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Serve(ref a)) if a.migrate));
    }

    #[test]
    fn log_format_flag() {
        use crate::log::LogFormat;
        assert_eq!(parse(&["serve", "--log-format", "json"]).unwrap().command.map(|c| match c {
            Command::Serve(a) => a.log_format,
            _ => None,
        }), Some(Some(LogFormat::Json)));
        assert_eq!(parse(&["--log-format", "compact"]).unwrap().serve.log_format, Some(LogFormat::Compact));
        assert_eq!(parse(&["--log-format", "pretty"]).unwrap().serve.log_format, Some(LogFormat::Pretty));
        assert!(parse(&["--log-format", "xml"]).is_err());
        // Tests don't run on a terminal: the default is JSON.
        use std::io::IsTerminal;
        if !std::io::stdout().is_terminal() {
            assert_eq!(LogFormat::default_for_stdout(), LogFormat::Json);
        }
    }

    #[test]
    fn mutation_timeout_and_schema_check_flags() {
        let cli = parse(&[]).unwrap();
        assert_eq!(cli.serve.mutation_timeout, crate::runtime::DEFAULT_MUTATION_TIMEOUT);
        assert!(!cli.serve.skip_schema_check);
        let cli = parse(&["serve", "--mutation-timeout", "250ms", "--skip-schema-check"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Serve(ref a))
            if a.mutation_timeout == Duration::from_millis(250) && a.skip_schema_check));
        assert_eq!(parse_duration("2").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_duration("1.5s").unwrap(), Duration::from_millis(1500));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert!(parse_duration("5h").is_err() && parse_duration("x").is_err() && parse_duration("-1s").is_err());
        assert!(parse(&["--mutation-timeout", "soon"]).is_err());
    }
}
