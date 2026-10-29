//! The `rxferrix` command. Apps are their own binaries (`cargo run -- serve`, `cargo run -- codegen`, ...); this one
//! creates them, and runs them in development:
//!
//! ```sh
//! cargo install --path crates/rxferrix-cli
//! rxferrix new my-app --web react      # or svelte, or none
//! cd my-app && rxferrix dev            # `cargo run -- serve --dev`, rebuilt and restarted when Rust code changes
//! ```

mod dev;
mod new;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(name = "rxferrix", version, about = "Creates RxFerrix apps")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create a new app in DIR: a Rust backend with a sample table, query and mutations, the `.cargo/config.toml` that
    /// builds SQLite for parallel queries, and optionally a web app.
    New(NewArgs),
    /// Run the app in the current directory with `serve --dev`, and rebuild and restart it when its Rust code changes.
    /// The running server keeps serving while the new version compiles, and after a failed build.
    Dev(DevArgs),
}

#[derive(clap::Args, Debug)]
struct DevArgs {
    /// Package to run, in a workspace with several (`cargo run --package`).
    #[arg(short, long)]
    package: Option<String>,
    /// Binary to run, in a package with several (`cargo run --bin`).
    #[arg(long)]
    bin: Option<String>,
    /// Build with optimizations.
    #[arg(long)]
    release: bool,
    /// More options for `serve`, after `--`: `rxferrix dev -- --http 127.0.0.1:9000`.
    #[arg(last = true, value_name = "SERVE_ARGS")]
    serve_args: Vec<String>,
}

#[derive(clap::Args, Debug)]
struct NewArgs {
    /// Directory to create (must not exist, or be empty). Its name is the crate name unless --name is given.
    dir: PathBuf,
    /// Crate and binary name (default: the directory's name).
    #[arg(long)]
    name: Option<String>,
    /// Web app to create in `web/`.
    #[arg(long, value_enum, default_value_t = Web::React)]
    web: Web,
    /// RxFerrix checkout to depend on (path dependencies; the web app gets the @rxferrix packages packed from it, which
    /// needs `bun`). Default: the checkout this command was built from, if it still exists; otherwise the published
    /// versions.
    #[arg(long, value_name = "DIR", conflicts_with = "published")]
    rxferrix_path: Option<PathBuf>,
    /// Depend on the published `rxferrix` crate and @rxferrix packages, even when built from a checkout.
    #[arg(long)]
    published: bool,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Web {
    React,
    Svelte,
    None,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::New(args) => {
            let source = if args.published {
                new::Source::Published
            } else if let Some(path) = args.rxferrix_path {
                new::Source::Checkout(path)
            } else {
                new::Source::default_for_this_build()
            };
            new::run(&new::Options { dir: args.dir, name: args.name, web: args.web, source })
        }
        Command::Dev(args) => dev::run(&dev::Options {
            package: args.package,
            bin: args.bin,
            release: args.release,
            serve_args: args.serve_args,
        }),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn new_defaults_to_react() {
        let cli = Cli::try_parse_from(["rxferrix", "new", "app"]).unwrap();
        let Command::New(args) = cli.command else { panic!("{:?}", cli.command) };
        assert_eq!(args.web, Web::React);
        assert!(Cli::try_parse_from(["rxferrix", "new", "app", "--published", "--rxferrix-path", "x"]).is_err());
    }

    #[test]
    fn dev_passes_serve_args_after_dashes() {
        let cli = Cli::try_parse_from(["rxferrix", "dev", "-p", "app", "--", "--http", "127.0.0.1:9000"]).unwrap();
        let Command::Dev(args) = cli.command else { panic!("{:?}", cli.command) };
        assert_eq!(args.package.as_deref(), Some("app"));
        assert_eq!(args.serve_args, ["--http", "127.0.0.1:9000"]);
        assert!(Cli::try_parse_from(["rxferrix", "dev", "--http", "127.0.0.1:9000"]).is_err());
    }
}
