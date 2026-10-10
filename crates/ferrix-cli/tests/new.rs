//! `ferrix new`, end to end: creates an app of each kind from this checkout, builds it with cargo, checks that its
//! SQLite is built for parallel readers (no startup warning), and for web apps lets `serve --dev` write the generated
//! API, then installs, typechecks and builds the web app with bun.
//!
//! Ignored by default (it builds whole apps, and bun downloads packages):
//!
//! ```sh
//! cargo test -p ferrix-cli --test new -- --ignored
//! ```
//!
//! The apps build into `target/tests/ferrix-new`, so only the first run compiles Ferrix's dependencies.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// A command with none of the environment that this repository's `.cargo/config.toml` gives the test process: an app
/// must get it from its own config.
fn command(program: &str, dir: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.current_dir(dir)
        .env_remove("LIBSQLITE3_FLAGS")
        .env_remove("FERRIX_CODEGEN_OUT")
        .env("CARGO_TARGET_DIR", repo_root().join("target/tests/ferrix-new"));
    cmd
}

fn cargo(dir: &Path) -> Command {
    command(&std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()), dir)
}

#[track_caller]
fn ok(what: &str, output: Output) -> String {
    let log = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "{what} failed:\n{log}");
    log
}

fn create(parent: &Path, name: &str, web: &str) -> PathBuf {
    let dir = parent.join(name);
    let output = Command::new(env!("CARGO_BIN_EXE_ferrix"))
        .args(["new", "--web", web, "--ferrix-path"])
        .arg(repo_root())
        .arg(&dir)
        .output()
        .unwrap();
    ok("ferrix new", output);
    dir
}

/// Builds and runs the app once (a command that opens the database), and checks SQLite didn't warn.
fn build_and_check_sqlite(app: &Path) {
    let output = cargo(app).args(["run", "-q", "--", "migrate", "status", "--dir"]).arg(app.join("fx_test")).output();
    let log = ok("cargo run -- migrate status", output.unwrap());
    assert!(!log.contains("ENABLE_MEMORY_MANAGEMENT"), "SQLite built without the app's flags:\n{log}");
}

/// Runs `cargo run -- serve --dev` until it has written `web/src/ferrix` (through `FERRIX_CODEGEN_OUT` in the app's
/// `.cargo/config.toml`).
fn codegen_through_serve_dev(app: &Path) {
    let api = app.join("web/src/ferrix/api.ts");
    let mut server = cargo(app)
        .args(["run", "-q", "--", "serve", "--dev", "--http", "127.0.0.1:0", "--dir"])
        .arg(app.join("fx_test"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !api.exists() && start.elapsed() < Duration::from_secs(120) {
        if let Some(status) = server.try_wait().unwrap() {
            panic!("serve --dev exited ({status}) before writing {}", api.display());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = server.kill();
    let _ = server.wait();
    let api = std::fs::read_to_string(&api).expect("serve --dev wrote web/src/ferrix/api.ts");
    assert!(api.contains("tasks"), "the generated API has the template's functions:\n{api}");
}

fn bun(web: &Path, args: &[&str]) {
    let output = command("bun", web).args(args).output().expect("failed to run bun (is it installed?)");
    ok(&format!("bun {}", args.join(" ")), output);
}

#[test]
#[ignore = "builds generated apps with cargo and bun; run with --ignored"]
fn new_apps_build_and_typecheck() {
    let tmp = tempfile::tempdir().unwrap();
    // The web apps depend on @ferrix packages packed from the checkout: they need a built dist/.
    bun(&repo_root(), &["run", "build"]);

    let plain = create(tmp.path(), "plain-app", "none");
    assert!(!plain.join("web").exists());
    build_and_check_sqlite(&plain);

    for web in ["react", "svelte"] {
        let app = create(tmp.path(), &format!("{web}-app"), web);
        build_and_check_sqlite(&app);
        codegen_through_serve_dev(&app);
        let web_dir = app.join("web");
        bun(&web_dir, &["install"]);
        bun(&web_dir, &["run", "typecheck"]);
        bun(&web_dir, &["run", "build"]);
        assert!(web_dir.join("dist/index.html").is_file(), "{web}: vite build wrote dist/index.html");
    }
}
