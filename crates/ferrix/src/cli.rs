//! Command line: `ferrix [serve] [--http ADDR] [--dir DIR] [--dev [--codegen-out DIR]]`, `ferrix functions` and
//! `ferrix codegen [--out DIR]`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::runtime::{App, AppOptions, registry};

#[derive(Parser, Debug)]
#[command(about = "A Ferrix backend", args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Options for `serve`, which runs when no subcommand is given.
    #[command(flatten)]
    serve: ServeArgs,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the server (default).
    Serve(ServeArgs),
    /// List registered functions with kind, visibility and TS signature.
    Functions,
    /// Write the TypeScript bindings (api.ts, dataModel.ts, index.ts) for the client packages.
    Codegen(CodegenArgs),
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
    /// Roll back mutations still running after this long, e.g. `5s`, `500ms`, `2m` (a bare number is seconds).
    /// Only enforced at `.await` points.
    #[arg(long, value_name = "DURATION", default_value = "5s", value_parser = parse_duration)]
    mutation_timeout: Duration,
    /// Start even if existing documents may not match changed table types (they are not validated).
    #[arg(long)]
    skip_schema_check: bool,
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
    _private: (),
}

impl Ferrix {
    pub fn new() -> Self {
        Ferrix::default()
    }

    /// Parse the command line and run it. Builds its own tokio runtime.
    pub fn run(self) -> crate::Result<()> {
        let cli = Cli::parse();
        match cli.command {
            Some(Command::Functions) => print_functions(),
            Some(Command::Codegen(args)) => codegen(&args.out),
            Some(Command::Serve(args)) => serve(args),
            None => serve(cli.serve),
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

fn serve(args: ServeArgs) -> crate::Result<()> {
    // stdout logging (filtered by RUST_LOG) plus the capture of function log lines.
    let stdout_filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,ferrix=info".into());
    let _ = crate::log::cli_subscriber(stdout_filter, std::io::stdout).try_init();
    if args.dev {
        let out = args.codegen_out.clone().or_else(|| std::env::var_os(CODEGEN_OUT_ENV).map(PathBuf::from));
        if let Some(out) = out.filter(|o| !o.as_os_str().is_empty()) {
            let changed = crate::codegen::write(&out)?;
            tracing::info!("codegen: {} ({} file(s) changed)", out.display(), changed.len());
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let options = AppOptions {
            dev: args.dev,
            mutation_timeout: args.mutation_timeout,
            skip_schema_check: args.skip_schema_check,
            ..Default::default()
        };
        let app = App::open(&args.dir, options).await?;
        let listener = tokio::net::TcpListener::bind(args.http).await?;
        tracing::info!(
            "ferrix listening on http://{} (data: {}, dev: {})",
            listener.local_addr()?,
            args.dir.display(),
            args.dev
        );
        crate::server::serve_with_shutdown(listener, app, async {
            let _ = tokio::signal::ctrl_c().await;
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
    fn serve_codegen_out_requires_dev() {
        let cli = parse(&["serve", "--dev", "--codegen-out", "gen"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Serve(ref a)) if a.codegen_out.as_deref() == Some(Path::new("gen"))));
        let cli = parse(&["--dev", "--codegen-out", "gen"]).unwrap();
        assert_eq!(cli.serve.codegen_out.as_deref(), Some(Path::new("gen")));
        assert!(parse(&["serve", "--codegen-out", "gen"]).is_err());
        assert!(parse(&["--codegen-out", "gen"]).is_err());
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
