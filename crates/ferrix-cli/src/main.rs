//! The `ferrix` command. Apps are their own binaries (`cargo run -- serve`, `cargo run -- codegen`, ...); this one only
//! creates them:
//!
//! ```sh
//! cargo install --path crates/ferrix-cli
//! ferrix new my-app --web react      # or svelte, or none
//! ```

mod new;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(name = "ferrix", version, about = "Creates Ferrix apps")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create a new app in DIR: a Rust backend with a sample table, query and mutations, the `.cargo/config.toml` that
    /// builds SQLite for parallel queries, and optionally a web app.
    New(NewArgs),
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
    /// Ferrix checkout to depend on (path dependencies; the web app gets the @ferrix packages packed from it, which
    /// needs `bun`). Default: the checkout this command was built from, if it still exists; otherwise the published
    /// versions.
    #[arg(long, value_name = "DIR", conflicts_with = "published")]
    ferrix_path: Option<PathBuf>,
    /// Depend on the published `ferrix` crate and @ferrix packages, even when built from a checkout.
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
            } else if let Some(path) = args.ferrix_path {
                new::Source::Checkout(path)
            } else {
                new::Source::default_for_this_build()
            };
            new::run(&new::Options { dir: args.dir, name: args.name, web: args.web, source })
        }
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
        let cli = Cli::try_parse_from(["ferrix", "new", "app"]).unwrap();
        let Command::New(args) = cli.command;
        assert_eq!(args.web, Web::React);
        assert!(Cli::try_parse_from(["ferrix", "new", "app", "--published", "--ferrix-path", "x"]).is_err());
    }
}
