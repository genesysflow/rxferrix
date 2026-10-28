//! Command line: `rxferrix [serve] [--http ADDR] [--dir DIR] [--dev [--codegen-out DIR]] [auth options]
//! [--storage LOCATION] [--max-upload-size SIZE] [--storage-private] [operations flags]`,
//! `rxferrix functions`, `rxferrix codegen [--out DIR]`, `rxferrix admin create|reset-password|disable-2fa <email>`,
//! `rxferrix migrate [--name NAME] [--dry-run] [--restart]` / `rxferrix migrate status` (`crate::migrations`), and the
//! operations commands `backup`, `restore`, `export`, `import` (flags and commands in `crate::ops::cli`).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use clap::{Args, Parser, Subcommand};
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::SchedulableFunction;
use crate::auth::{AuthConfig, SmtpConfig};
use crate::runtime::{App, AppOptions, registry};
use crate::scheduler::SchedulerOptions;
use crate::storage::{StorageBackend, StorageConfig};

#[derive(Parser, Debug)]
#[command(about = "A RxFerrix backend", args_conflicts_with_subcommands = true)]
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
    /// Manage environment variables (`ctx.env`) offline, e.g. before the first start; the dashboard's Environment
    /// page changes them on a running server.
    #[command(subcommand)]
    Env(EnvCommand),
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
    /// Check that this version can open the data (works while a server runs): opens a copy of the database exactly
    /// as `serve --takeover` would (index builds, schema check), reports, and deletes the copy.
    Check(CheckArgs),
    /// Exit 0 if the server answers its health endpoint, 1 otherwise: a container health check (the image has no
    /// curl).
    Health(HealthArgs),
}

/// `serve --takeover[=WHEN]`.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum TakeoverMode {
    /// Take over the running server; fail if there is none.
    Require,
    /// Take over if a server is running, else start normally.
    Auto,
}

/// `serve --after-handover`.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum AfterHandover {
    Exit,
    Wait,
}

#[derive(Args, Debug, Clone)]
struct HealthArgs {
    /// The server's health endpoint.
    #[arg(default_value = "http://127.0.0.1:8090/api/health")]
    url: String,
}

#[derive(Args, Debug, Clone)]
struct CheckArgs {
    /// Data directory.
    #[arg(long, default_value = "fx_data")]
    dir: PathBuf,
}

#[derive(Args, Debug, Clone)]
#[command(args_conflicts_with_subcommands = true)]
struct MigrateArgs {
    #[command(subcommand)]
    command: Option<MigrateCommand>,
    /// Run only this migration (default: every migration that isn't done, dependencies first, then in name order,
    /// stopping at a failure).
    #[arg(long)]
    name: Option<String>,
    /// Run over every document and report what would change, without writing anything.
    #[arg(long)]
    dry_run: bool,
    /// Start the migration over from the first document, even if it is done.
    #[arg(long, requires = "name")]
    restart: bool,
    /// Run the migration even if some it runs after (`#[migration(after = [..])]`) are not done.
    #[arg(long, requires = "name")]
    force: bool,
    /// Data directory.
    #[arg(long, default_value = "fx_data")]
    dir: PathBuf,
    /// Format of the progress log lines (default: pretty; the report itself is plain text).
    #[arg(long, env = "RXFERRIX_LOG_FORMAT", value_enum, value_name = "FORMAT")]
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
    /// Remove an administrator's second factor (they lost their authenticator and recovery codes), so they can sign
    /// in with their password alone and set it up again.
    #[command(name = "disable-2fa")]
    Disable2fa(AdminEmailArgs),
}

#[derive(Args, Debug, Clone)]
struct AdminEmailArgs {
    email: String,
    /// Data directory.
    #[arg(long, default_value = "fx_data")]
    dir: PathBuf,
}

#[derive(Subcommand, Debug)]
enum EnvCommand {
    /// List the variables' names (and values with --values).
    List {
        /// Print `NAME=value` lines.
        #[arg(long)]
        values: bool,
        /// Data directory.
        #[arg(long, default_value = "fx_data")]
        dir: PathBuf,
    },
    /// Set a variable. Without VALUE, the value is read from stdin (one line), so it stays out of the shell history.
    Set {
        name: String,
        value: Option<String>,
        /// Data directory.
        #[arg(long, default_value = "fx_data")]
        dir: PathBuf,
    },
    /// Remove a variable.
    Unset {
        name: String,
        /// Data directory.
        #[arg(long, default_value = "fx_data")]
        dir: PathBuf,
    },
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
pub const CODEGEN_OUT_ENV: &str = "RXFERRIX_CODEGEN_OUT";

#[derive(Args, Debug, Clone)]
struct CodegenArgs {
    /// Output directory, e.g. `../web/src/rxferrix`. Only files whose content changed are rewritten.
    #[arg(long, value_name = "DIR", default_value = "rxferrix-generated")]
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
    /// With --dev: run codegen into DIR on startup (like `rxferrix codegen --out DIR`). Defaults to the
    /// RXFERRIX_CODEGEN_OUT environment variable; without either, no codegen runs.
    #[arg(long, value_name = "DIR", requires = "dev")]
    codegen_out: Option<PathBuf>,
    /// Public base URL of this server, used for OAuth callback URLs and the admin setup link
    /// (default: http://<--http address>).
    #[arg(long, env = "RXFERRIX_PUBLIC_URL", value_name = "URL")]
    public_url: Option<String>,
    /// App URL that auth may redirect to after OAuth sign-in and from email links (repeatable; an origin allows all
    /// of its paths). The first one is the default target.
    #[arg(long = "auth-redirect-url", env = "RXFERRIX_AUTH_REDIRECT_URLS", value_delimiter = ',', value_name = "URL")]
    auth_redirect_urls: Vec<String>,
    /// SMTP server for auth emails, e.g. smtps://user:pass@smtp.example.com:465. In --dev without SMTP, emails are
    /// printed to the log instead.
    #[arg(long, env = "RXFERRIX_SMTP_URL", value_name = "URL", hide_env_values = true)]
    smtp_url: Option<String>,
    /// Sender address for auth emails, e.g. "My App <noreply@example.com>".
    #[arg(long, env = "RXFERRIX_SMTP_FROM", value_name = "ADDRESS")]
    smtp_from: Option<String>,
    /// Take client IPs (for rate limiting) from the last X-Forwarded-For entry. Only behind a reverse proxy; overrides
    /// the trusted proxy headers set in the dashboard.
    #[arg(long, env = "RXFERRIX_TRUST_PROXY")]
    trust_proxy: bool,
    /// The reverse proxy's address (an IP or a CIDR range such as 10.0.0.0/8; repeatable, or comma-separated): proxy
    /// headers count only on requests from it, so clients reaching the server directly can't pick their IP.
    #[arg(long, env = "RXFERRIX_TRUST_PROXY_FROM", value_name = "ADDRESS", value_delimiter = ',')]
    trust_proxy_from: Vec<String>,
    /// Roll back mutations still running after this long, e.g. `5s`, `500ms`, `2m` (a bare number is seconds).
    /// Only enforced at `.await` points.
    #[arg(long, value_name = "DURATION", default_value = "5s", value_parser = parse_duration)]
    mutation_timeout: Duration,
    /// Fail queries still running after this long, e.g. `30s`. Only enforced at `.await` points.
    #[arg(long, value_name = "DURATION", default_value = "30s", value_parser = parse_duration)]
    query_timeout: Duration,
    /// Memory for cached results of queries nobody watches any more, e.g. `64MB`, `1GB` (0 keeps none). They answer
    /// the next subscriber, HTTP query or `ctx.run_query` without running, until a write changes them.
    #[arg(long, env = "RXFERRIX_QUERY_CACHE_SIZE", value_name = "SIZE", default_value = "64MB")]
    #[arg(value_parser = parse_size)]
    query_cache_size: u64,
    /// How long a cached result of a query nobody watches is kept, e.g. `30m`, `2h`.
    #[arg(long, env = "RXFERRIX_QUERY_CACHE_TTL", value_name = "DURATION", default_value = "30m")]
    #[arg(value_parser = parse_duration)]
    query_cache_ttl: Duration,
    /// Wait for each commit to reach the disk before answering (SQLite synchronous=FULL). Without it a process crash
    /// loses nothing, but a power loss or OS crash may lose the last commits. One fsync covers a batch of mutations.
    #[arg(long, env = "RXFERRIX_FSYNC_COMMITS")]
    fsync_commits: bool,
    /// Memory for the page cache of the database connection all writes go through, e.g. `32MB`, `128MB`. Taken as
    /// it is used. Writes that change many index entries (many indexes, or several documents per mutation) are the
    /// ones it speeds up.
    #[arg(long, env = "RXFERRIX_WRITER_CACHE_SIZE", value_name = "SIZE", default_value = "32MB")]
    #[arg(value_parser = parse_size)]
    writer_cache_size: u64,
    /// How many queries (and other reads) can run at once: the number of read-only database connections. Default:
    /// one and a half per core. A query holds one for its whole run.
    #[arg(long, env = "RXFERRIX_READERS", value_name = "N", value_parser = clap::value_parser!(u16).range(1..))]
    readers: Option<u16>,
    /// Start even if existing documents may not match changed table types (they are not validated).
    #[arg(long)]
    skip_schema_check: bool,
    /// Run the migrations that aren't done (`#[migration]`, dependencies first, then in name order) in the background
    /// after starting.
    #[arg(long, env = "RXFERRIX_MIGRATE")]
    migrate: bool,
    /// Take over from the server running on this data directory with no downtime: copy the database, open the copy
    /// with this version (index builds, schema check), catch up, then take over (its listening sockets too, in the
    /// same network namespace) and swap the copy in. If this version can't serve the data, it exits and the running
    /// server carries on. `--takeover=auto`: take over if a server is running, else start normally (containers).
    #[arg(long, env = "RXFERRIX_TAKEOVER", value_enum, num_args = 0..=1, default_missing_value = "require", value_name = "WHEN")]
    takeover: Option<TakeoverMode>,
    /// What this server does once a new process took over from it: `exit` (default), or `wait`: stay alive, serving
    /// nothing and holding nothing, until SIGTERM (containers and pods, so the orchestrator doesn't restart it while it
    /// replaces it).
    #[arg(long, env = "RXFERRIX_AFTER_HANDOVER", value_enum, default_value = "exit", value_name = "WHAT")]
    after_handover: AfterHandover,
    /// Where uploaded files are stored: a directory (default: <dir>/storage), file:///dir, or s3://bucket/prefix
    /// (S3, R2, MinIO; credentials, region and endpoint from the standard AWS_* environment variables).
    #[arg(long, env = "RXFERRIX_STORAGE", value_name = "LOCATION")]
    storage: Option<String>,
    /// Largest accepted upload, e.g. `100MB` (default), `2GB`, `512KB`, or a number of bytes.
    #[arg(long, env = "RXFERRIX_MAX_UPLOAD_SIZE", value_name = "SIZE", value_parser = parse_size)]
    max_upload_size: Option<u64>,
    /// Private file storage: plain /api/storage/<id> URLs are refused (403); files are served only through signed,
    /// expiring URLs (ctx.storage.get_signed_url).
    #[arg(long, env = "RXFERRIX_STORAGE_PRIVATE")]
    storage_private: bool,
    /// Keep deleted files' contents this long, e.g. `7d`, so restores to before a deletion find them (they are gone
    /// for everything else at once). Default: the `--replicate-keep` window when replicating, otherwise off.
    #[arg(long, env = "RXFERRIX_STORAGE_KEEP_DELETED", value_name = "DURATION", value_parser = parse_duration)]
    storage_keep_deleted: Option<Duration>,
    /// How log lines are written to stdout: `json` (one object per line), `pretty` or `compact`. Default: pretty on a
    /// terminal, json otherwise. `RUST_LOG` filters them either way.
    #[arg(long, env = "RXFERRIX_LOG_FORMAT", value_enum, value_name = "FORMAT")]
    log_format: Option<crate::log::LogFormat>,
    /// Metrics, scheduled backups and TLS (`crate::ops::cli`).
    #[command(flatten)]
    ops: crate::ops::cli::OpsServeArgs,
}

/// `"100MB"`, `"2GB"`, `"512KB"`, `"1.5MB"` (powers of 1024), or a bare number of bytes.
pub(crate) fn parse_size(s: &str) -> Result<u64, String> {
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
    let n: f64 = num.parse().map_err(|_| format!("invalid duration `{s}` (examples: 500ms, 5s, 2m, 1h)"))?;
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

/// Entry point for application binaries: `fn main() -> rxferrix::Result<()> { RxFerrix::new().run() }`.
#[derive(Debug, Default)]
pub struct RxFerrix {
    auth: AuthConfig,
    storage: StorageConfig,
    scheduler: SchedulerOptions,
    web: crate::ops::WebOptions,
    guard: Option<crate::guard::Guard>,
    on_start: Vec<StartHook>,
}

/// What [`RxFerrix::on_start`] runs.
struct StartHook(Box<dyn FnOnce(App) -> futures::future::BoxFuture<'static, crate::Result<()>> + Send>);

impl std::fmt::Debug for StartHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StartHook(<fn>)")
    }
}

impl RxFerrix {
    /// Serve the app's frontend from a directory (read at runtime; relative to the working directory), with SPA
    /// fallback to `index.html` (see [`crate::ops::web`]).
    pub fn static_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.web.static_files = Some(crate::ops::StaticFiles::Dir(dir.into()));
        self
    }

    /// Serve the app's frontend from files embedded in the binary:
    /// `#[derive(rxferrix::rust_embed::Embed)] #[folder = "../web/dist/"] #[crate_path = "rxferrix::rust_embed"] struct
    /// Assets;` then `.embed_static::<Assets>()` (see [`crate::ops::web`]).
    pub fn embed_static<A: rust_embed::RustEmbed>(mut self) -> Self {
        self.web.static_files = Some(crate::ops::StaticFiles::Embedded(A::get));
        self
    }

    /// Add axum routes (webhooks, custom endpoints) next to RxFerrix's own; they must not overlap `/api/*`, `/_/*` or
    /// `/metrics`. Repeated calls add up (with [`app_routes`](Self::app_routes) too).
    pub fn routes(self, routes: axum::Router) -> Self {
        self.app_routes(routes.with_state(()))
    }

    /// Like [`routes`](Self::routes), for handlers that take the running [`App`](crate::App) as
    /// `axum::extract::State<App>`: run functions (`app.mutation::<record_payment>(..)`, internal ones included), mint
    /// sessions (`app.auth().sign_in_as(..)`), ... A webhook can verify its signature on the raw body, then hand the
    /// event to a mutation.
    pub fn app_routes(mut self, routes: axum::Router<crate::App>) -> Self {
        self.web.routes = Some(match self.web.routes.take() {
            Some(existing) => existing.merge(routes),
            None => routes,
        });
        self
    }

    pub fn new() -> Self {
        RxFerrix::default()
    }

    /// Configure authentication: `RxFerrix::new().auth(AuthConfig::password().oauth(GitHub::from_env()))`.
    /// Command line flags (`--public-url`, `--auth-redirect-url`, `--smtp-url`, ...) add to or override it.
    pub fn auth(mut self, auth: AuthConfig) -> Self {
        self.auth = auth;
        self
    }

    /// Configure file storage: `RxFerrix::new().storage(StorageConfig::s3("bucket", "uploads").max_upload_size(..))`.
    /// `--storage` and `--max-upload-size` override it.
    pub fn storage(mut self, storage: StorageConfig) -> Self {
        self.storage = storage;
        self
    }

    /// Run `F` with `args` on a cron schedule (6 fields with seconds, UTC): `.cron::<cleanup>("cleanup",
    /// "0 0 3 * * *", ())`. See [`crate::scheduler::CronSpec::cron`].
    pub fn cron<F: SchedulableFunction>(
        mut self,
        name: impl Into<String>,
        expr: impl Into<String>,
        args: impl Into<F::Args>,
    ) -> Self {
        self.scheduler = self.scheduler.cron::<F>(name, expr, args);
        self
    }

    /// [`RxFerrix::cron`] evaluated in an IANA time zone, DST included: `.cron_tz::<report>("report", "0 0 9 * * *",
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
    pub fn interval<F: SchedulableFunction>(
        mut self,
        name: impl Into<String>,
        every: Duration,
        args: impl Into<F::Args>,
    ) -> Self {
        self.scheduler = self.scheduler.interval::<F>(name, every, args);
        self
    }

    /// Declare a workpool: `.workpool(Workpool::new("emails").max_parallelism(5).retries(3))`, then
    /// `ctx.scheduler.pool("emails").enqueue::<send_email>(args)`. See [`crate::scheduler::Workpool`].
    pub fn workpool(mut self, pool: crate::scheduler::Workpool) -> Self {
        self.scheduler.pools.push(pool);
        self
    }

    /// Scheduler settings (concurrency, job retention, retry backoff). Crons and workpools declared with
    /// [`RxFerrix::cron`] / [`RxFerrix::interval`] / [`RxFerrix::workpool`] are kept, before or after this call.
    pub fn scheduler(mut self, options: SchedulerOptions) -> Self {
        let declared = std::mem::take(&mut self.scheduler.crons);
        let pools = std::mem::take(&mut self.scheduler.pools);
        self.scheduler = options;
        self.scheduler.crons.splice(0..0, declared);
        self.scheduler.pools.splice(0..0, pools);
        self
    }

    /// Check every call a client makes to a public function before it runs, and refuse it with an error: one place
    /// for an access policy instead of a check in each function. See [`crate::guard`] for what it covers and sees.
    ///
    /// ```ignore
    /// RxFerrix::new().guard(|ctx, function| async move {
    ///     if function.is::<access::status>() || function.is::<invites::accept>() {
    ///         return Ok(());
    ///     }
    ///     let user = ctx.auth.require_user()?;
    ///     if !access::granted(&ctx.db, user).await? {
    ///         return Err(UserError::new("access_required", "Ask for an invite first").into());
    ///     }
    ///     Ok(())
    /// })
    /// ```
    ///
    /// Only `serve` uses it; tests set [`AppOptions::guard`] (`TestApp::with_options`).
    pub fn guard<F, Fut>(mut self, guard: F) -> Self
    where
        F: Fn(crate::guard::GuardCtx, crate::guard::GuardedFunction) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = crate::Result<()>> + Send + 'static,
    {
        self.guard = Some(crate::guard::Guard::new(guard));
        self
    }

    /// Run `hook` when `serve` starts, once the app is open and before the scheduler runs or the server accepts a
    /// connection: start background services that live as long as the server, clear state a previous run left
    /// behind. An error stops `serve` before it listens. Repeated calls run in order.
    ///
    /// Run a service with [`App::spawn`]: it should end once [`App::closed`] resolves, and `serve` waits for it (a
    /// while) before it lets go of the database, so the service can still write and has released what it holds (a
    /// port) before the next server starts:
    ///
    /// ```ignore
    /// RxFerrix::new().on_start(|app| async move {
    ///     app.mutation::<voice::clear_stale>(()).await?;
    ///     let media = MediaServer::bind(media_port()).await?;   // a port in use stops the server here
    ///     let closed = app.clone();
    ///     app.spawn(async move { media.run_until(closed.closed()).await });
    ///     Ok(())
    /// })
    /// ```
    ///
    /// With `serve --takeover`, the hooks run after the old server has stopped (that is when the new one opens the
    /// database), so its services have let go of their ports; a hook that fails there leaves no server running. Other
    /// commands (`migrate`, `admin`, ...) and `TestApp` don't run the hooks.
    pub fn on_start<F, Fut>(mut self, hook: F) -> Self
    where
        F: FnOnce(App) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = crate::Result<()>> + Send + 'static,
    {
        self.on_start.push(StartHook(Box::new(move |app| Box::pin(hook(app)))));
        self
    }

    /// Parse the command line and run it. Builds its own tokio runtime.
    pub fn run(self) -> crate::Result<()> {
        let cli = Cli::parse();
        match cli.command {
            Some(Command::Functions) => print_functions(),
            Some(Command::Codegen(args)) => codegen(&args.out),
            Some(Command::Admin(cmd)) => admin(cmd, self.auth),
            Some(Command::Env(cmd)) => env(cmd, self.auth),
            Some(Command::Backup(args)) => crate::ops::cli::run_backup(args, self.storage),
            Some(Command::Restore(args)) => crate::ops::cli::run_restore(args, self.storage),
            Some(Command::Export(args)) => crate::ops::cli::run_export(args, self.storage),
            Some(Command::Import(args)) => crate::ops::cli::run_import(
                args,
                AppOptions { auth: self.auth, storage: self.storage, scheduler: self.scheduler, ..Default::default() },
            ),
            Some(Command::Migrate(args)) => migrate(args, self.auth, self.storage, self.scheduler),
            Some(Command::Check(args)) => check(args),
            Some(Command::Health(args)) => health(args),
            Some(Command::Serve(args)) => {
                serve(args, self.auth, self.storage, self.scheduler, self.web, self.guard, self.on_start)
            }
            None => serve(cli.serve, self.auth, self.storage, self.scheduler, self.web, self.guard, self.on_start),
        }
    }
}

fn print_functions() -> crate::Result<()> {
    for f in registry()?.list() {
        println!(
            "{:<8} {:<8} {}  (args: {}) => {}",
            f.kind.as_str(),
            f.visibility.as_str(),
            f.path,
            f.args_ts,
            f.output_ts
        );
    }
    Ok(())
}

fn codegen(out: &Path) -> crate::Result<()> {
    let changed = crate::codegen::write(out)?;
    println!("rxferrix codegen: wrote {} ({} file(s) changed)", out.display(), changed.len());
    Ok(())
}

fn health(args: HealthArgs) -> crate::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let ok = rt.block_on(async {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(3)).build()?;
        anyhow::Ok(client.get(&args.url).send().await.map(|r| r.status().is_success()).unwrap_or(false))
    })?;
    if !ok {
        eprintln!("unhealthy: {}", args.url);
        std::process::exit(1);
    }
    Ok(())
}

fn check(args: CheckArgs) -> crate::Result<()> {
    let _ = crate::log::cli_subscriber(
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        std::io::stderr,
        crate::log::LogFormat::Pretty,
    )
    .try_init();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let report = rt.block_on(crate::ops::takeover::check(&args.dir))?;
    println!(
        "OK: this version opens the data in {} (at ts {}, in {:.1?})",
        args.dir.display(),
        report.ts,
        report.elapsed
    );
    if !report.migrating.is_empty() {
        println!("migrating (documents waiting for their #[migration]): {}", report.migrating.join(", "));
    }
    Ok(())
}

fn admin(cmd: AdminCommand, auth: AuthConfig) -> crate::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    if let AdminCommand::Disable2fa(args) = cmd {
        return rt.block_on(async move {
            let app = App::open(&args.dir, AppOptions { auth, ..Default::default() }).await?;
            let email = crate::auth::normalize_email(&args.email)?;
            if !app.auth().disable_admin_mfa(&app, &email).await? {
                anyhow::bail!("no administrator with email {email}");
            }
            println!("Removed the second factor of {email}; their next sign-in needs the password only");
            Ok(())
        });
    }
    let (args, create) = match cmd {
        AdminCommand::Create(a) => (a, true),
        AdminCommand::ResetPassword(a) => (a, false),
        AdminCommand::Disable2fa(_) => unreachable!("handled above"),
    };
    let password = match args.password {
        Some(p) => p,
        None => read_password()?,
    };
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

fn migrate(
    args: MigrateArgs,
    auth: AuthConfig,
    storage: StorageConfig,
    scheduler: SchedulerOptions,
) -> crate::Result<()> {
    let stdout_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "warn,rxferrix::migrations=info".into());
    let format = args.log_format.unwrap_or(crate::log::LogFormat::Pretty);
    let _ = crate::log::cli_subscriber(stdout_filter, std::io::stdout, format).try_init();
    let dir = match &args.command {
        Some(MigrateCommand::Status { dir }) => dir.clone(),
        None => args.dir.clone(),
    };
    // Offline: refuse while a server holds the data directory (`crate::ops::lock`). Opening the app writes (a
    // migration left `running` becomes `pending`, the catalog is reconciled), which would disturb a running server;
    // its dashboard shows the live state instead.
    let _lock = match crate::ops::DirLock::acquire(&dir) {
        Ok(lock) => lock,
        Err(e) if matches!(args.command, Some(MigrateCommand::Status { .. })) => anyhow::bail!(
            "{e}\n(while the server runs, its dashboard's Migrations page shows the migrations' live state)"
        ),
        Err(e) => return Err(e),
    };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let app = App::open(&dir, AppOptions { auth, storage, scheduler, ..Default::default() }).await?;
        if matches!(args.command, Some(MigrateCommand::Status { .. })) {
            print_migration_status(&app).await?;
            return Ok(());
        }
        let options = crate::migrations::RunOptions {
            dry_run: args.dry_run,
            restart: args.restart,
            force: args.force,
            ..Default::default()
        };
        let reports = match &args.name {
            Some(name) => vec![app.run_migration_named(name, options).await?],
            None => app.run_pending_migrations(options).await?,
        };
        if reports.is_empty() {
            println!("rxferrix migrate: nothing to do (no migration is pending)");
        }
        let mut failed = false;
        for r in &reports {
            println!("{}", describe_report(r));
            failed |= r.outcome == crate::migrations::RunOutcome::Failed;
        }
        if failed {
            anyhow::bail!("a migration failed; fix it and run `migrate` again to resume");
        }
        if reports.iter().any(|r| r.outcome == crate::migrations::RunOutcome::Blocked) {
            anyhow::bail!("a migration is waiting for another one that is not done");
        }
        Ok(())
    })
}

/// One report, as `rxferrix migrate` prints it.
fn describe_report(r: &crate::migrations::MigrationReport) -> String {
    use crate::migrations::RunOutcome;
    let mut s = format!("{} (table `{}`): ", r.name, r.table);
    s.push_str(&match r.outcome {
        RunOutcome::AlreadyDone => "already done".to_string(),
        RunOutcome::AlreadyRunning => "already running elsewhere".to_string(),
        RunOutcome::Blocked => "blocked".to_string(),
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
        let mut line = format!(
            "{:<10} {}  (table `{}`, {}/{} processed",
            m.state.as_str(),
            m.name,
            m.table,
            m.processed,
            m.documents
        );
        if !m.after.is_empty() {
            line.push_str(&format!(", after {}", m.after.join(", ")));
        }
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
fn env(cmd: EnvCommand, auth: AuthConfig) -> crate::Result<()> {
    let dir = match &cmd {
        EnvCommand::List { dir, .. } | EnvCommand::Set { dir, .. } | EnvCommand::Unset { dir, .. } => dir.clone(),
    };
    // Offline: a running server wouldn't see the change (`crate::ops::lock`).
    let _lock = crate::ops::DirLock::acquire(&dir)?;
    let value = match &cmd {
        EnvCommand::Set { value: None, .. } => Some(read_line("Value: ")?),
        _ => None,
    };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let app = App::open(&dir, AppOptions { auth, ..Default::default() }).await?;
        let cli = crate::auth::Identity::anonymous();
        match cmd {
            EnvCommand::List { values, .. } => {
                for (name, value) in crate::env::list_all(&app).await? {
                    if values {
                        println!("{name}={value}");
                    } else {
                        println!("{name}");
                    }
                }
            }
            EnvCommand::Set { name, value: given, .. } => {
                let value = given.or(value).unwrap_or_default();
                let (changed, _) =
                    app.typed_mutation::<crate::env::put>((name.clone(), Some(value)).into(), &cli).await?;
                println!("{}", if changed { format!("Set {name}") } else { format!("{name} already had that value") });
            }
            EnvCommand::Unset { name, .. } => {
                let (changed, _) = app.typed_mutation::<crate::env::put>((name.clone(), None).into(), &cli).await?;
                println!("{}", if changed { format!("Removed {name}") } else { format!("{name} is not set") });
            }
        }
        Ok(())
    })
}

/// One line from stdin (prompting with `prompt` on a terminal), without the line ending.
fn read_line(prompt: &str) -> crate::Result<String> {
    use std::io::{BufRead, IsTerminal, Write};
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        eprint!("{prompt}");
        std::io::stderr().flush()?;
    }
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

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
    let public_url =
        args.public_url.clone().or_else(|| auth.public_url.clone()).or_else(|| args.ops.public_url()).unwrap_or_else(
            || {
                let addr = if args.http.ip().is_unspecified() {
                    SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), args.http.port())
                } else {
                    args.http
                };
                format!("http://{addr}")
            },
        );
    auth.public_url = Some(public_url);
    for url in args.auth_redirect_urls.iter().map(|u| u.trim()).filter(|u| !u.is_empty()) {
        if !auth.redirect_urls.iter().any(|u| u == url) {
            auth.redirect_urls.push(url.to_string());
        }
    }
    let smtp = args.smtp_url.clone().map(|url| {
        let from = args.smtp_from.clone().or_else(|| auth.smtp.as_ref().map(|s| s.from.clone()));
        SmtpConfig::new(url, from.unwrap_or_else(|| "rxferrix <noreply@localhost>".into()))
    });
    if smtp.is_some() {
        auth.smtp = smtp;
    } else if let (Some(from), Some(s)) = (&args.smtp_from, auth.smtp.as_mut()) {
        s.from = from.clone();
    }
    if args.trust_proxy {
        auth.trust_proxy = true;
    }
    if !args.trust_proxy_from.is_empty() {
        auth.trusted_proxies = args.trust_proxy_from.clone();
    }
    auth
}

/// Whether a public URL was given explicitly (`--public-url` / `RXFERRIX_PUBLIC_URL`, `AuthConfig::public_url`, or a
/// TLS domain), before [`auth_config`] fills in the default `http://<--http address>`.
fn explicit_public_url(args: &ServeArgs, auth: &AuthConfig) -> bool {
    args.public_url.is_some() || auth.public_url.is_some() || args.ops.public_url().is_some()
}

/// The builder's storage config with `--storage` / `--max-upload-size` / `--storage-private` applied on top.
///
/// In `--dev`, unless a public URL was set explicitly (`explicit_public_url`, or `StorageConfig::public_url`), file
/// and upload URLs are root-relative (`/api/storage/<id>`), so an app served by a dev server that proxies `/api`
/// (Vite) loads and uploads files through the proxy, same-origin, instead of from the backend's own address.
fn storage_config(
    args: &ServeArgs,
    mut storage: StorageConfig,
    explicit_public_url: bool,
) -> crate::Result<StorageConfig> {
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
    if let Some(keep) = args.storage_keep_deleted {
        storage.keep_deleted = Some(keep);
    }
    Ok(storage)
}

/// How long a stopping server keeps serving with `Connection: close` before it stops accepting.
const DRAIN_GRACE: Duration = Duration::from_millis(100);
/// How long a stopping server waits for the tasks of `App::spawn` to end before it drops them.
const SERVICES_GRACE: Duration = Duration::from_secs(10);

fn serve(
    args: ServeArgs,
    auth: AuthConfig,
    storage: StorageConfig,
    scheduler: SchedulerOptions,
    web: crate::ops::WebOptions,
    guard: Option<crate::guard::Guard>,
    on_start: Vec<StartHook>,
) -> crate::Result<()> {
    // stdout logging (filtered by RUST_LOG) plus the capture of function log lines.
    let stdout_filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,rxferrix=info".into());
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
    // The data directory's lock, set once taken. It outlives the runtime: dropping the runtime waits for its blocking
    // tasks and drops every task, so all connections to the database are closed before a next server (a takeover
    // waiting for this lock) moves `data.db` and its WAL.
    let mut dir_lock = None;
    let lock_slot = &mut dir_lock;
    let outcome = rt.block_on(async move {
        let dir_lock = lock_slot;
        let explicit = explicit_public_url(&args, &auth);
        if !explicit && !args.dev && (args.ops.tls_cert.is_some() || auth.oauth.iter().any(|p| p.is_configured())) {
            // The default `http://<--http address>` is wrong for every link and OAuth callback.
            tracing::warn!(
                "no public URL set (--public-url / RXFERRIX_PUBLIC_URL): email links, file URLs and OAuth callbacks \
                 use http://{}, which browsers outside this machine can't reach",
                args.http
            );
        }
        let auth = auth_config(&args, auth);
        let storage = storage_config(&args, storage, explicit)?;
        let public_url = auth.public_url.clone().unwrap_or_default();
        let mut options = AppOptions {
            dev: args.dev,
            mutation_timeout: args.mutation_timeout,
            query_timeout: args.query_timeout,
            skip_schema_check: args.skip_schema_check,
            query_cache: crate::runtime::QueryCacheOptions {
                idle_ttl: args.query_cache_ttl,
                max_bytes: usize::try_from(args.query_cache_size).unwrap_or(usize::MAX),
            },
            fsync_commits: args.fsync_commits,
            writer_cache_size: args.writer_cache_size,
            readers: args.readers.map_or_else(crate::db::default_readers, usize::from),
            determinism_check: args.dev.then_some(crate::runtime::DEV_DETERMINISM_CHECK),
            auth,
            storage,
            scheduler,
            guard,
            ..Default::default()
        };
        args.ops.apply(&mut options);
        let listen = args.ops.listen(args.http, &args.dir)?;
        // Held while serving: `restore` refuses to run under a server. A takeover gets it from the old process.
        let takeover = match args.takeover {
            Some(TakeoverMode::Require) => true,
            Some(TakeoverMode::Auto) => {
                let running = crate::ops::takeover::server_running(&args.dir).await;
                if !running {
                    tracing::info!("no server running on this data directory: starting normally");
                }
                running
            }
            None => false,
        };
        let (lock, inherited) = if takeover {
            // What can be checked before the running server hands over is checked now: after the handover, a
            // failure leaves nothing serving.
            listen.check().context("--takeover: this server couldn't listen as configured")?;
            if let Some(replication) = &options.ops.replication {
                replication.build_target(&args.dir).context("--takeover: --replicate-to")?;
            }
            let incoming = crate::ops::takeover::take_over(&args.dir, &listen.addrs()).await?;
            options.prechecked_schema = incoming.migrating;
            tracing::info!(from = incoming.from_pid, "took over the data directory");
            (incoming.lock, incoming.listeners)
        } else {
            let lock = crate::ops::DirLock::acquire(&args.dir)?;
            crate::ops::takeover::recover(&args.dir)?;
            (lock, Vec::new())
        };
        // Released only once the runtime is gone (below), with every connection to the database closed.
        *dir_lock = Some(lock);
        // A restore staged from the dashboard replaces the database now, before anything opens it.
        let storage_dir = crate::ops::backup::local_storage_dir(&args.dir, &options.storage.backend);
        let dir = args.dir.clone();
        if let Some(report) =
            tokio::task::spawn_blocking(move || crate::ops::restore::apply_pending(&dir, storage_dir)).await??
        {
            // A takeover checked the database it copied; this one replaced it (staged on the old server meanwhile,
            // and checked by that binary): check it again.
            options.prechecked_schema = None;
            tracing::warn!(
                ts = report.manifest.ts,
                previous = ?report.previous,
                blobs = report.blobs_restored,
                "restored the staged backup"
            );
        }
        let restart = options.ops.restart.clone();
        let app = App::open(&args.dir, options).await?;
        for hook in on_start {
            (hook.0)(app.clone()).await.context("an on_start hook failed")?;
        }
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
                tracing::warn!(
                    ?pending,
                    "migrations are waiting; run them with `serve --migrate`, `migrate` or the dashboard"
                );
            }
        }
        let mut router = crate::ops::web::router(app.clone(), &web);
        if listen.is_tls() && !args.ops.no_hsts {
            router = crate::ops::tls::with_hsts(router);
        }
        let server = crate::ops::tls::start_with(router, &listen, inherited).await?;
        // A later version can take over from this process (`serve --takeover`, `crate::ops::takeover`).
        #[cfg(unix)]
        let control = match crate::ops::takeover::Control::start(
            app.clone(),
            &args.dir,
            server.listener_fds(),
            server.listener_addrs(),
        )
        .await
        {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("no zero-downtime takeover possible: {e:#}");
                None
            }
        };
        let handed_over = std::sync::atomic::AtomicBool::new(false);
        let scheme = if server.https { "https" } else { "http" };
        tracing::info!(
            "rxferrix listening on {scheme}://{} (data: {}, dev: {})",
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
        crate::ops::takeover::sd_notify(&format!("READY=1\nMAINPID={}", std::process::id()));
        let pauser = server.pauser();
        server
            .run_until(async {
                #[cfg(unix)]
                let handover = async {
                    match &control {
                        Some(c) => c.handover_requested().await,
                        None => std::future::pending().await,
                    }
                };
                #[cfg(not(unix))]
                let handover = std::future::pending::<()>();
                tokio::select! {
                    _ = crate::ops::tls::shutdown_signal() => tracing::info!("shutting down"),
                    _ = restart.requested() => tracing::warn!("restarting: shutting down to apply the staged restore"),
                    _ = handover => {
                        handed_over.store(true, std::sync::atomic::Ordering::SeqCst);
                        tracing::info!("handing over to the new process");
                    }
                }
                crate::ops::takeover::sd_notify("STOPPING=1");
                // Stop accepting but keep serving the open connections a moment with `Connection: close`, so
                // keep-alive clients finish and reconnect instead of racing the shutdown; then close WebSockets (1012)
                // and stop.
                app.begin_drain();
                // If the new process holds the listening sockets now, new connections wait there for it: leave them
                // open. Otherwise (a normal stop, or a new process in another container, on its own address) close
                // them, so new connections are refused and a proxy retries them on the new server.
                #[cfg(unix)]
                let taken = control.as_ref().is_some_and(|c| c.sockets_taken());
                #[cfg(not(unix))]
                let taken = false;
                if taken {
                    pauser.stop_accepting();
                } else {
                    pauser.close();
                }
                tokio::time::sleep(DRAIN_GRACE).await;
                app.begin_close();
                // A backup or replication snapshot still copying the database would hold it open long after this.
                crate::ops::backup::interrupt_snapshots();
            })
            .await?;
        // Services end on `begin_close`; they may still write, and must let go of what they hold before the next server.
        app.stop_tasks(SERVICES_GRACE).await;
        if !handed_over.load(std::sync::atomic::Ordering::SeqCst) {
            // The last commits reach the replication target before the process exits. (After a handover the
            // journal goes to the new process, which ships them.)
            crate::ops::replicate::flush(&app, Duration::from_secs(15)).await;
        }
        app.logs().flush().await?;
        // Nothing may commit after this process lets go of the data directory.
        let last_ts = app.db().seal().await?;
        tracing::info!(ts = last_ts, "stopped");
        let wait = handed_over.load(std::sync::atomic::Ordering::SeqCst) && args.after_handover == AfterHandover::Wait;
        Ok::<_, crate::Error>((restart.is_requested().then(|| restart.exit_code()).flatten(), wait))
    });
    drop(rt);
    drop(dir_lock);
    let (exit_code, wait) = outcome?;
    if wait {
        // Everything is released (lock, sockets, database, control socket): our port refuses connections, so health
        // checks fail and a proxy retries elsewhere what it sends here, but this process stays alive until it is told
        // to go, so an orchestrator doesn't start it again (it would take over from the new one).
        tracing::info!("handed over to the new process; waiting for SIGTERM");
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(crate::ops::tls::shutdown_signal());
        return Ok(());
    }
    // An administrator asked for a restart (`--restart-exit-code`): the supervisor starts the server again.
    if let Some(code) = exit_code {
        std::process::exit(code);
    }
    Ok(())
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
        assert!(matches!(cli.command, Some(Command::Codegen(ref a)) if a.out == Path::new("rxferrix-generated")));
        let cli = parse(&["codegen", "--out", "../web/src/rxferrix"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Codegen(ref a)) if a.out == Path::new("../web/src/rxferrix")));
    }

    #[test]
    fn admin_and_auth_args() {
        let cli = parse(&["admin", "create", "a@b.c", "--password", "pw", "--dir", "d"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Admin(AdminCommand::Create(ref a)))
            if a.email == "a@b.c" && a.password.as_deref() == Some("pw") && a.dir == Path::new("d")));
        let cli = parse(&["admin", "disable-2fa", "a@b.c", "--dir", "d"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Admin(AdminCommand::Disable2fa(ref a))) if a.email == "a@b.c"));
        let cli = parse(&["admin", "reset-password", "a@b.c"]).unwrap();
        assert!(
            matches!(cli.command, Some(Command::Admin(AdminCommand::ResetPassword(ref a))) if a.password.is_none())
        );

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
        assert!(
            matches!(cli.command, Some(Command::Serve(ref a)) if a.codegen_out.as_deref() == Some(Path::new("gen")))
        );
        let cli = parse(&["--dev", "--codegen-out", "gen"]).unwrap();
        assert_eq!(cli.serve.codegen_out.as_deref(), Some(Path::new("gen")));
        assert!(parse(&["serve", "--codegen-out", "gen"]).is_err());
        assert!(parse(&["--codegen-out", "gen"]).is_err());
    }

    #[test]
    fn storage_flags() {
        let cli =
            parse(&["serve", "--storage", "s3://bucket/some/prefix", "--max-upload-size", "2GB", "--storage-private"])
                .unwrap();
        let Some(Command::Serve(args)) = cli.command else { panic!("expected serve") };
        let storage = storage_config(&args, StorageConfig::default(), false).unwrap();
        assert!(
            matches!(storage.backend, StorageBackend::S3 { ref bucket, ref prefix } if bucket == "bucket" && prefix == "some/prefix")
        );
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
        assert!(
            matches!(cli.command, Some(Command::Migrate(ref a)) if a.command.is_none() && a.name.is_none() && !a.dry_run)
        );
        let cli = parse(&["migrate", "--name", "todos:add", "--dry-run", "--dir", "d"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Migrate(ref a))
            if a.name.as_deref() == Some("todos:add") && a.dry_run && a.dir == Path::new("d")));
        let cli = parse(&["migrate", "status", "--dir", "d"]).unwrap();
        assert!(
            matches!(cli.command, Some(Command::Migrate(MigrateArgs { command: Some(MigrateCommand::Status { ref dir }), .. }))
            if dir == Path::new("d"))
        );
        assert!(parse(&["migrate", "--restart"]).is_err(), "--restart needs --name");
        let cli = parse(&["serve", "--migrate"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Serve(ref a)) if a.migrate));
    }

    #[test]
    fn log_format_flag() {
        use crate::log::LogFormat;
        assert_eq!(
            parse(&["serve", "--log-format", "json"]).unwrap().command.map(|c| match c {
                Command::Serve(a) => a.log_format,
                _ => None,
            }),
            Some(Some(LogFormat::Json))
        );
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
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(7 * 86400));
        assert!(parse_duration("5w").is_err() && parse_duration("x").is_err() && parse_duration("-1s").is_err());
        assert!(parse(&["--mutation-timeout", "soon"]).is_err());
    }

    #[test]
    fn query_cache_flags() {
        let cli = parse(&["serve"]).unwrap();
        let Some(Command::Serve(a)) = cli.command else { panic!("serve") };
        assert_eq!((a.query_cache_size, a.query_cache_ttl), (64 << 20, Duration::from_secs(30 * 60)));
        let cli = parse(&["serve", "--query-cache-size", "1GB", "--query-cache-ttl", "2h"]).unwrap();
        let Some(Command::Serve(a)) = cli.command else { panic!("serve") };
        assert_eq!((a.query_cache_size, a.query_cache_ttl), (1 << 30, Duration::from_secs(7200)));
        assert!(parse(&["serve", "--query-cache-size", "lots"]).is_err());
        assert!(!a.fsync_commits);
        let Some(Command::Serve(a)) = parse(&["serve", "--fsync-commits"]).unwrap().command else { panic!("serve") };
        assert!(a.fsync_commits);
        assert_eq!(a.writer_cache_size, crate::db::DEFAULT_WRITER_CACHE_SIZE);
        let args = ["serve", "--writer-cache-size", "128MB"];
        let Some(Command::Serve(a)) = parse(&args).unwrap().command else { panic!("serve") };
        assert_eq!(a.writer_cache_size, 128 << 20);
        assert_eq!(a.readers, None);
        let Some(Command::Serve(a)) = parse(&["serve", "--readers", "24"]).unwrap().command else { panic!("serve") };
        assert_eq!(a.readers, Some(24));
        assert!(parse(&["serve", "--readers", "0"]).is_err());
    }

    #[test]
    fn olap_flags() {
        let mut options = AppOptions::default();
        let Some(Command::Serve(a)) = parse(&["serve"]).unwrap().command else { panic!("serve") };
        a.ops.apply(&mut options);
        assert!(!options.olap.enabled);
        let args = [
            "serve",
            "--olap",
            "--olap-memory",
            "2GB",
            "--olap-threads",
            "3",
            "--olap-timeout",
            "5s",
            "--olap-max-lag",
            "64MB",
        ];
        let Some(Command::Serve(a)) = parse(&args).unwrap().command else { panic!("serve") };
        a.ops.apply(&mut options);
        let o = &options.olap;
        assert_eq!(
            (o.enabled, o.memory_limit, o.threads, o.query_timeout, o.max_lag_bytes),
            (true, Some(2 << 30), Some(3), Duration::from_secs(5), 64 << 20)
        );
        assert!(parse(&["serve", "--olap-memory", "1GB"]).is_err(), "needs --olap");
    }
}
