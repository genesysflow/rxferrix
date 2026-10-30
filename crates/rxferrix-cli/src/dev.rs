//! `rxferrix dev`: runs the app in the current directory like `cargo run -- serve --dev`, and rebuilds and restarts it
//! when its Rust code changes. The running server keeps serving while the new version compiles, and a failed build
//! leaves it running; clients reconnect to the new server by themselves.
//!
//! Watched: the `Cargo.toml` and the source directories (of libraries and binaries, not tests, benches or examples) of
//! the app and of every local crate it builds with (workspace members and path dependencies), as `cargo metadata`
//! lists them. Only `.rs` files and `Cargo.toml` count, so the database, the generated TypeScript API and the web app
//! don't trigger builds.
//!
//! The server starts through `cargo run` (after `cargo build`, so it doesn't compile again), which applies the `[env]`
//! of `.cargo/config.toml`, such as `RXFERRIX_CODEGEN_OUT`.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use serde_json::Value;

/// Quiet time after a change before building: editors and formatters write several files, or one file several times.
const SETTLE: Duration = Duration::from_millis(200);
/// How often to check whether the server exited by itself.
const POLL: Duration = Duration::from_millis(250);
/// How long a stopping server gets before it is killed (it gives its own background work 10 s).
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Options {
    pub package: Option<String>,
    pub bin: Option<String>,
    pub release: bool,
    /// After `serve --dev`.
    pub serve_args: Vec<String>,
}

enum Event {
    Changed(PathBuf),
    Stop,
}

enum Build {
    /// Something was compiled: the server must restart.
    Compiled,
    /// Everything was up to date.
    Fresh,
    Failed,
}

pub fn run(opts: &Options) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| format!("current directory: {e}"))?;
    let (tx, rx) = mpsc::channel();
    let stop = tx.clone();
    // Ctrl+C reaches cargo and the server too (same process group): they stop by themselves, and this waits for them.
    ctrlc::set_handler(move || {
        let _ = stop.send(Event::Stop);
    })
    .map_err(|e| format!("can't handle Ctrl+C: {e}"))?;
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        // Not `Access`: the compiler reading the sources would start the next build.
        if matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)) {
            for path in event.paths.into_iter().filter(|p| is_source(p)) {
                let _ = tx.send(Event::Changed(path));
            }
        }
    })
    .map_err(|e| format!("can't watch files: {e}"))?;
    let mut watched = Vec::new();
    update_watches(&mut watcher, &mut watched, watch_list(&metadata()?, opts.package.as_deref()));
    let dirs: Vec<_> = watched.iter().map(|d| relative(&d.path, &cwd)).collect();
    note(&format!("watching {}", dirs.join(", ")));

    let mut server: Option<Child> = None;
    let mut build_now = true;
    loop {
        if build_now {
            build_now = false;
            match build(opts)? {
                Build::Fresh if server.is_some() => note("up to date; the server keeps running"),
                Build::Compiled | Build::Fresh => {
                    if server.is_some() {
                        note("restarting the server");
                    }
                    stop_server(server.take());
                    server = Some(start(opts)?);
                }
                Build::Failed if server.is_some() => {
                    note("build failed; the server keeps running the previous version")
                }
                Build::Failed => note("build failed; waiting for changes"),
            }
        }
        match rx.recv_timeout(POLL) {
            Ok(Event::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Event::Changed(first)) => {
                let Some(paths) = settle(&rx, first) else { break };
                if paths.iter().any(|p| p.file_name().is_some_and(|n| n == "Cargo.toml")) {
                    // Dependencies may have changed, and with them the local crates to watch.
                    match metadata() {
                        Ok(metadata) => {
                            update_watches(&mut watcher, &mut watched, watch_list(&metadata, opts.package.as_deref()))
                        }
                        Err(e) => note(&e),
                    }
                }
                let more = match paths.len() {
                    1 => String::new(),
                    n => format!(" (and {} more)", n - 1),
                };
                note(&format!("{}{more} changed; rebuilding", relative(&paths[0], &cwd)));
                build_now = true;
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(child) = &mut server
                    && let Ok(Some(status)) = child.try_wait()
                {
                    note(&format!("the server exited ({status}); waiting for changes"));
                    server = None;
                }
            }
        }
    }
    stop_server(server.take());
    Ok(())
}

/// Whether a change to `path` calls for a build: Rust sources and manifests, but not editors' backup, swap and lock
/// files (`.main.rs.swp`, `main.rs~`, `.#main.rs`, `#main.rs#`).
fn is_source(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { return false };
    !name.starts_with('.') && !name.starts_with('#') && (name.ends_with(".rs") || name == "Cargo.toml")
}

/// Collects the changes that follow `first` until none came for [`SETTLE`]. None when told to stop meanwhile.
fn settle(rx: &Receiver<Event>, first: PathBuf) -> Option<Vec<PathBuf>> {
    let mut paths = vec![first];
    loop {
        match rx.recv_timeout(SETTLE) {
            Ok(Event::Changed(path)) => {
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
            Ok(Event::Stop) | Err(RecvTimeoutError::Disconnected) => return None,
            Err(RecvTimeoutError::Timeout) => return Some(paths),
        }
    }
}

/// `cargo SUBCOMMAND` with the package, binary and profile to run.
fn cargo(opts: &Options, subcommand: &str) -> Command {
    let mut cmd = Command::new("cargo");
    cmd.arg(subcommand);
    if let Some(package) = &opts.package {
        cmd.args(["--package", package]);
    }
    if let Some(bin) = &opts.bin {
        cmd.args(["--bin", bin]);
    }
    if opts.release {
        cmd.arg("--release");
    }
    cmd
}

fn build(opts: &Options) -> Result<Build, String> {
    // Diagnostics and progress still go to the terminal (stderr); stdout has a JSON message per compiled crate.
    let mut child = cargo(opts, "build")
        .arg("--message-format=json-render-diagnostics")
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("can't run cargo: {e}"))?;
    let stdout = child.stdout.take().expect("piped");
    let mut compiled = false;
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        compiled |= is_compiled_artifact(&line);
    }
    let status = child.wait().map_err(|e| format!("cargo build: {e}"))?;
    Ok(match (status.success(), compiled) {
        (false, _) => Build::Failed,
        (true, true) => Build::Compiled,
        (true, false) => Build::Fresh,
    })
}

/// Whether a line of `cargo build --message-format=json` reports a crate that was compiled (not reused).
fn is_compiled_artifact(line: &str) -> bool {
    serde_json::from_str::<Value>(line).is_ok_and(|m| m["reason"] == "compiler-artifact" && m["fresh"] == false)
}

fn start(opts: &Options) -> Result<Child, String> {
    // On Unix `cargo run` execs the app, so the child is the server itself.
    cargo(opts, "run")
        .args(["--quiet", "--", "serve", "--dev"])
        .args(&opts.serve_args)
        .spawn()
        .map_err(|e| format!("can't run cargo: {e}"))
}

/// Asks the server to stop as on Ctrl+C (SIGTERM), so it closes WebSockets with "reconnect" and releases the data
/// directory, and waits for it; kills it if it takes longer than [`STOP_TIMEOUT`].
fn stop_server(server: Option<Child>) {
    let Some(mut child) = server else { return };
    if !matches!(child.try_wait(), Ok(None)) {
        return;
    }
    #[cfg(unix)]
    // SAFETY: kill(2) has no memory effects. The child hasn't been reaped yet, so its pid can't belong to another
    // process.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            _ => return,
        }
    }
    note("the server didn't stop in time; killing it");
    let _ = child.kill();
    let _ = child.wait();
}

fn metadata() -> Result<Value, String> {
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1"])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("can't run cargo: {e}"))?;
    if !out.status.success() {
        return Err("cargo metadata failed".into());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("cargo metadata: {e}"))
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct WatchDir {
    path: PathBuf,
    recursive: bool,
}

/// The directories to watch, from `cargo metadata`: for every local package the app (`package`, else the package in
/// the current directory, else every workspace member) builds with, its manifest's directory (not recursive: it
/// holds `Cargo.toml`, `build.rs`, and the data and web directories) and the directories of its library's and
/// binaries' root files (recursive). Nested directories are left out.
fn watch_list(metadata: &Value, package: Option<&str>) -> Vec<WatchDir> {
    let packages: HashMap<&str, &Value> =
        array(&metadata["packages"]).filter_map(|p| Some((p["id"].as_str()?, p))).collect();
    let members: Vec<&str> = array(&metadata["workspace_members"]).filter_map(Value::as_str).collect();
    let named =
        package.and_then(|name| members.iter().copied().find(|id| packages.get(id).is_some_and(|p| p["name"] == name)));
    let mut stack = match (named, metadata["resolve"]["root"].as_str()) {
        (Some(id), _) => vec![id],
        (None, Some(id)) if package.is_none() => vec![id],
        _ => members,
    };
    // Each package's dependencies, without dev-dependencies (they aren't in the app).
    let deps: HashMap<&str, Vec<&str>> = array(&metadata["resolve"]["nodes"])
        .filter_map(|node| {
            let built = array(&node["deps"])
                .filter(|dep| array(&dep["dep_kinds"]).any(|k| k["kind"] != "dev"))
                .filter_map(|dep| dep["pkg"].as_str())
                .collect();
            Some((node["id"].as_str()?, built))
        })
        .collect();

    let mut dirs = Vec::new();
    let mut seen = HashSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        stack.extend(deps.get(id).into_iter().flatten().copied());
        let Some(package) = packages.get(id) else { continue };
        // Registry and git packages don't change.
        if !package["source"].is_null() {
            continue;
        }
        let Some(root) = package["manifest_path"].as_str().and_then(|m| Path::new(m).parent()) else { continue };
        dirs.push(WatchDir { path: root.to_owned(), recursive: false });
        for target in array(&package["targets"]) {
            let skip = array(&target["kind"])
                .filter_map(Value::as_str)
                .any(|kind| matches!(kind, "test" | "bench" | "example" | "custom-build"));
            let dir = target["src_path"].as_str().and_then(|p| Path::new(p).parent());
            if let Some(dir) = dir
                && !skip
                && dir != root
            {
                dirs.push(WatchDir { path: dir.to_owned(), recursive: true });
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    let recursive: Vec<PathBuf> = dirs.iter().filter(|d| d.recursive).map(|d| d.path.clone()).collect();
    dirs.retain(|d| !recursive.iter().any(|r| d.path.starts_with(r) && (d.path != *r || !d.recursive)));
    dirs
}

fn array(value: &Value) -> impl Iterator<Item = &Value> {
    value.as_array().into_iter().flatten()
}

fn update_watches(watcher: &mut RecommendedWatcher, current: &mut Vec<WatchDir>, wanted: Vec<WatchDir>) {
    for dir in current.iter().filter(|d| !wanted.contains(d)) {
        let _ = watcher.unwatch(&dir.path);
    }
    for dir in wanted.iter().filter(|d| !current.contains(d)) {
        let mode = if dir.recursive { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
        if let Err(e) = watcher.watch(&dir.path, mode) {
            note(&format!("can't watch {}: {e}", dir.path.display()));
        }
    }
    *current = wanted;
}

fn relative(path: &Path, cwd: &Path) -> String {
    match path.strip_prefix(cwd) {
        Ok(p) if p.as_os_str().is_empty() => ".".into(),
        Ok(p) => p.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

fn note(message: &str) {
    eprintln!("[rxferrix dev] {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources() {
        for path in ["src/main.rs", "/a/b/build.rs", "Cargo.toml"] {
            assert!(is_source(Path::new(path)), "{path}");
        }
        for path in ["src/.main.rs.swp", "src/main.rs~", "src/.#main.rs", "src/#main.rs#", "src/4913", "Cargo.lock"] {
            assert!(!is_source(Path::new(path)), "{path}");
        }
        assert!(!is_source(Path::new("web/src/rxferrix/api.ts")));
    }

    #[test]
    fn compiled_artifacts() {
        assert!(is_compiled_artifact(r#"{"reason":"compiler-artifact","fresh":false,"executable":null}"#));
        assert!(!is_compiled_artifact(r#"{"reason":"compiler-artifact","fresh":true}"#));
        assert!(!is_compiled_artifact(r#"{"reason":"build-finished","success":true}"#));
        assert!(!is_compiled_artifact("not json"));
    }

    fn package(id: &str, name: &str, dir: &str, local: bool, targets: &[(&str, &str)]) -> Value {
        let targets: Vec<Value> =
            targets.iter().map(|(kind, src)| serde_json::json!({"kind": [kind], "src_path": src})).collect();
        serde_json::json!({
            "id": id,
            "name": name,
            "manifest_path": format!("{dir}/Cargo.toml"),
            "source": if local { Value::Null } else { "registry+https://github.com/rust-lang/crates.io-index".into() },
            "targets": targets,
        })
    }

    fn dep(pkg: &str, kind: Option<&str>) -> Value {
        serde_json::json!({"pkg": pkg, "dep_kinds": [{"kind": kind}]})
    }

    fn dir(path: &str, recursive: bool) -> WatchDir {
        WatchDir { path: path.into(), recursive }
    }

    #[test]
    fn watches_local_crates_the_app_builds_with() {
        let metadata = serde_json::json!({
            "packages": [
                package("app", "app", "/w/app", true, &[
                    ("bin", "/w/app/src/main.rs"),
                    ("custom-build", "/w/app/build.rs"),
                    ("test", "/w/app/tests/it.rs"),
                ]),
                package("other", "other", "/w/other", true, &[("bin", "/w/other/src/main.rs")]),
                package("fx", "rxferrix", "/fx/crates/rxferrix", true, &[
                    ("lib", "/fx/crates/rxferrix/src/lib.rs"),
                    ("bin", "/fx/crates/rxferrix/src/bin/tool.rs"),
                    ("example", "/fx/crates/rxferrix/examples/e.rs"),
                ]),
                package("macros", "rxferrix-macros", "/fx/crates/rxferrix-macros", true, &[
                    ("proc-macro", "/fx/crates/rxferrix-macros/src/lib.rs"),
                ]),
                package("helper", "helper", "/w/helper", true, &[("lib", "/w/helper/src/lib.rs")]),
                package("serde", "serde", "/registry/serde", false, &[("lib", "/registry/serde/src/lib.rs")]),
            ],
            "workspace_members": ["app", "other"],
            "resolve": {
                "root": "app",
                "nodes": [
                    {"id": "app", "deps": [dep("fx", None), dep("serde", None), dep("helper", Some("dev"))]},
                    {"id": "other", "deps": []},
                    {"id": "fx", "deps": [dep("macros", None), dep("serde", None)]},
                    {"id": "macros", "deps": []},
                    {"id": "helper", "deps": []},
                    {"id": "serde", "deps": []},
                ],
            },
        });
        let app = [
            dir("/fx/crates/rxferrix", false),
            dir("/fx/crates/rxferrix/src", true),
            dir("/fx/crates/rxferrix-macros", false),
            dir("/fx/crates/rxferrix-macros/src", true),
            dir("/w/app", false),
            dir("/w/app/src", true),
        ];
        assert_eq!(watch_list(&metadata, None), app);
        assert_eq!(watch_list(&metadata, Some("other")), [dir("/w/other", false), dir("/w/other/src", true)]);

        // A virtual workspace: every member.
        let mut workspace = metadata.clone();
        workspace["resolve"]["root"] = Value::Null;
        let all = watch_list(&workspace, None);
        assert!(app.iter().all(|d| all.contains(d)) && all.contains(&dir("/w/other/src", true)), "{all:?}");
    }
}
