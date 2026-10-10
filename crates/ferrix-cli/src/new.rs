//! `ferrix new`: writes an app from the templates in `templates/`.
//!
//! The point of starting from here rather than an empty crate is `.cargo/config.toml`: the SQLite flags it sets can
//! only come from the app's own Cargo config (a dependency can't set them), and without them queries don't run in
//! parallel. [`render`] is pure (every file as text); [`run`] writes them and, for a checkout, packs the @ferrix
//! packages for the web app.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Web;

/// Where the app gets Ferrix from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The `ferrix` crate and @ferrix packages from crates.io and npm, at this command's version.
    Published,
    /// A Ferrix checkout (the repository root): a path dependency, and @ferrix packages packed from it.
    Checkout(PathBuf),
}

impl Source {
    /// The checkout this binary was built from, if it is still there; otherwise the published versions.
    pub fn default_for_this_build() -> Source {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        match root.canonicalize() {
            Ok(root) if is_checkout(&root) => Source::Checkout(root),
            _ => Source::Published,
        }
    }
}

pub struct Options {
    pub dir: PathBuf,
    pub name: Option<String>,
    pub web: Web,
    pub source: Source,
}

/// The JS packages a web app depends on, besides `@ferrix/client`.
fn binding_package(web: Web) -> Option<&'static str> {
    match web {
        Web::React => Some("react"),
        Web::Svelte => Some("svelte"),
        Web::None => None,
    }
}

pub fn run(opts: &Options) -> Result<(), String> {
    if opts.dir.exists() {
        let mut entries = std::fs::read_dir(&opts.dir).map_err(|e| format!("{}: {e}", opts.dir.display()))?;
        if entries.next().is_some() {
            return Err(format!("{} already exists and is not empty", opts.dir.display()));
        }
    }
    let name = match &opts.name {
        Some(name) => name.clone(),
        None => dir_name(&opts.dir)?,
    };
    validate_name(&name)?;

    let source = match &opts.source {
        Source::Checkout(path) => {
            let root = path.canonicalize().map_err(|e| format!("{}: {e}", path.display()))?;
            if !is_checkout(&root) {
                return Err(format!(
                    "{} is not a Ferrix checkout (no crates/ferrix or packages/client)",
                    root.display()
                ));
            }
            Source::Checkout(root)
        }
        Source::Published => Source::Published,
    };
    // Checked before writing anything, so a failure leaves no half-created app behind.
    let packed = match (&source, binding_package(opts.web)) {
        (Source::Checkout(root), Some(binding)) => Some(check_packages_built(root, binding)?),
        _ => None,
    };

    let files = render(&name, opts.web, &source, packed.as_ref())?;
    for (path, contents) in &files {
        let path = opts.dir.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::write(&path, contents).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    if let Source::Checkout(root) = &source {
        // The versions the checkout is tested with (Cargo drops what the app doesn't use on its first build).
        let lock = root.join("Cargo.lock");
        if lock.is_file() {
            std::fs::copy(&lock, opts.dir.join("Cargo.lock")).map_err(|e| format!("{}: {e}", lock.display()))?;
        }
    }
    if let (Source::Checkout(root), Some(packed)) = (&source, &packed) {
        let vendor = opts.dir.join("web/vendor");
        std::fs::create_dir_all(&vendor).map_err(|e| format!("{}: {e}", vendor.display()))?;
        for package in &packed.packages {
            pack(&root.join("packages").join(&package.dir), &vendor, &package.tarball)?;
        }
    }

    let from = match &source {
        Source::Published => format!("ferrix {}", env!("CARGO_PKG_VERSION")),
        Source::Checkout(root) => format!("Ferrix from {}", root.display()),
    };
    let dir = opts.dir.display();
    println!("Created {name} in {dir} ({from}).\n\nNext:\n  cd {dir}\n  cargo run -- serve --dev");
    if opts.web != Web::None {
        println!("\n  # in a second terminal, once the server has started (it writes web/src/ferrix)");
        println!("  cd {dir}/web && bun install && bun run dev");
    }
    Ok(())
}

/// The @ferrix packages packed into `web/vendor` from a checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packed {
    pub packages: Vec<PackedPackage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedPackage {
    /// `@ferrix/client`.
    pub name: String,
    /// Its directory under `packages/`.
    pub dir: String,
    /// The file `bun pm pack` writes: `ferrix-client-0.1.0.tgz`.
    pub tarball: String,
}

/// Errors unless `packages/client` and `packages/<binding>` have a built `dist/` (`bun pm pack` packs what is there).
fn check_packages_built(root: &Path, binding: &str) -> Result<Packed, String> {
    let mut packages = Vec::new();
    for dir in ["client", binding] {
        let package_dir = root.join("packages").join(dir);
        if !package_dir.join("dist/index.js").is_file() {
            return Err(format!(
                "@ferrix/{dir} in {} isn't built: run `bun install && bun run build` there first",
                root.display()
            ));
        }
        let manifest = package_dir.join("package.json");
        let text = std::fs::read_to_string(&manifest).map_err(|e| format!("{}: {e}", manifest.display()))?;
        let json: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", manifest.display()))?;
        let version = json["version"].as_str().ok_or_else(|| format!("{}: no version", manifest.display()))?;
        packages.push(PackedPackage {
            name: format!("@ferrix/{dir}"),
            dir: dir.to_string(),
            tarball: format!("ferrix-{dir}-{version}.tgz"),
        });
    }
    Ok(Packed { packages })
}

fn pack(package_dir: &Path, vendor: &Path, tarball: &str) -> Result<(), String> {
    let output = Command::new("bun")
        .args(["pm", "pack", "--destination"])
        .arg(vendor)
        .current_dir(package_dir)
        .output()
        .map_err(|e| format!("running `bun pm pack` (packs the @ferrix packages from the checkout): {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "`bun pm pack` in {} failed:\n{}{}",
            package_dir.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    if !vendor.join(tarball).is_file() {
        return Err(format!("`bun pm pack` in {} didn't write {tarball}", package_dir.display()));
    }
    Ok(())
}

fn is_checkout(root: &Path) -> bool {
    root.join("crates/ferrix/Cargo.toml").is_file() && root.join("packages/client/package.json").is_file()
}

fn dir_name(dir: &Path) -> Result<String, String> {
    let absolute = std::path::absolute(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // The last component rather than `file_name()`: also names `my-app/.` and `my-app/`.
    let last = absolute.components().rfind(|c| !matches!(c, std::path::Component::CurDir));
    match last {
        Some(std::path::Component::Normal(name)) => {
            name.to_str().map(str::to_string).ok_or_else(|| format!("{} is not valid UTF-8", dir.display()))
        }
        _ => Err(format!("can't name an app after {}; pass --name", dir.display())),
    }
}

/// A Cargo package name that is also a fine binary name, and isn't one of the crates the app depends on.
pub fn validate_name(name: &str) -> Result<(), String> {
    let starts_with_letter = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
    let valid_chars = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !starts_with_letter || !valid_chars {
        return Err(format!(
            "`{name}` can't be a crate name: use letters, digits, `-` and `_`, starting with a letter (or pass --name)"
        ));
    }
    let reserved = ["ferrix", "ferrix-macros", "ferrix_macros", "test", "std", "core", "alloc", "proc_macro"];
    if reserved.contains(&name) {
        return Err(format!("`{name}` is taken (a crate the app depends on, or a built-in one); pick another name"));
    }
    Ok(())
}

const CARGO_CONFIG: &str = include_str!("../templates/cargo_config.toml");
const CARGO_CONFIG_WEB: &str = include_str!("../templates/cargo_config_web.toml");

/// Every file of a new app, as (path relative to the app, contents). `packed` is set when the web app depends on
/// tarballs packed from a checkout.
pub fn render(
    name: &str,
    web: Web,
    source: &Source,
    packed: Option<&Packed>,
) -> Result<Vec<(PathBuf, String)>, String> {
    let fill = |template: &str| template.replace("{{name}}", name);
    let ferrix_dep = match source {
        Source::Published => toml_string(env!("CARGO_PKG_VERSION")),
        Source::Checkout(root) => {
            let path = root.join("crates/ferrix");
            let path = path.to_str().ok_or_else(|| format!("{} is not valid UTF-8", path.display()))?;
            format!("{{ path = {} }}", toml_string(path))
        }
    };
    let mut files = vec![
        (".gitignore", include_str!("../templates/gitignore").to_string()),
        ("Cargo.toml", fill(include_str!("../templates/server/Cargo.toml")).replace("{{ferrix_dep}}", &ferrix_dep)),
        ("src/tasks.rs", include_str!("../templates/server/src/tasks.rs").to_string()),
    ];
    let mut readme = fill(include_str!("../templates/README.md"));
    if web == Web::None {
        files.push((".cargo/config.toml", CARGO_CONFIG.to_string()));
        files.push(("src/main.rs", fill(include_str!("../templates/server/src/main.rs"))));
        readme = readme.replace("{{web_blurb}}", "").replace("{{web_steps}}", "").replace("{{build_steps}}", "");
    } else {
        files.push((".cargo/config.toml", format!("{CARGO_CONFIG}{CARGO_CONFIG_WEB}")));
        files.push(("src/main.rs", fill(include_str!("../templates/server/src/main_web.rs"))));
        let mut steps = include_str!("../templates/README_web.md").to_string();
        if let (Source::Checkout(root), Some(_)) = (source, packed) {
            let _ = write!(
                steps,
                "\n`web/vendor` holds the @ferrix packages, packed from the Ferrix checkout at {}. To pick up changes \
                 made there: `bun run build` in the checkout, `bun pm pack --destination <this app>/web/vendor` in \
                 each of its `packages/client` and `packages/{}`, then `bun install --force` here in `web/`.\n",
                root.display(),
                binding_package(web).unwrap_or_default()
            );
        }
        readme = readme
            .replace("{{web_blurb}}", " and the web app in `web/`")
            .replace("{{web_steps}}", &steps)
            .replace("{{build_steps}}", "(cd web && bun install && bun run build)   # embedded into the binary\n");
    }
    files.push(("README.md", readme));

    let web_files: &[(&str, &str)] = match web {
        Web::React => &[
            ("web/index.html", include_str!("../templates/react/index.html")),
            ("web/vite.config.ts", include_str!("../templates/react/vite.config.ts")),
            ("web/tsconfig.json", include_str!("../templates/react/tsconfig.json")),
            ("web/src/main.tsx", include_str!("../templates/react/src/main.tsx")),
            ("web/src/App.tsx", include_str!("../templates/react/src/App.tsx")),
            ("web/src/styles.css", include_str!("../templates/react/src/styles.css")),
        ],
        Web::Svelte => &[
            ("web/index.html", include_str!("../templates/svelte/index.html")),
            ("web/vite.config.ts", include_str!("../templates/svelte/vite.config.ts")),
            ("web/tsconfig.json", include_str!("../templates/svelte/tsconfig.json")),
            ("web/src/main.ts", include_str!("../templates/svelte/src/main.ts")),
            ("web/src/App.svelte", include_str!("../templates/svelte/src/App.svelte")),
            ("web/src/app.css", include_str!("../templates/svelte/src/app.css")),
        ],
        Web::None => &[],
    };
    files.extend(web_files.iter().map(|(path, template)| (*path, fill(template))));
    if web != Web::None {
        files.push(("web/package.json", package_json(name, web, packed)));
    }
    Ok(files.into_iter().map(|(path, contents)| (PathBuf::from(path), contents)).collect())
}

/// The versions match `examples/*/web/package.json` (a test checks).
fn package_json(name: &str, web: Web, packed: Option<&Packed>) -> String {
    let published = format!("^{}", env!("CARGO_PKG_VERSION"));
    let spec = |package: &str| match packed.and_then(|p| p.packages.iter().find(|p| p.name == package)) {
        Some(p) => format!("file:./vendor/{}", p.tarball),
        None => published.clone(),
    };
    type Deps<'a> = &'a [(&'a str, &'a str)];
    let (typecheck, mut deps, dev_deps): (&str, Vec<(String, String)>, Deps) = match web {
        Web::React => (
            "tsc --noEmit",
            vec![
                ("@ferrix/client".into(), spec("@ferrix/client")),
                ("@ferrix/react".into(), spec("@ferrix/react")),
                ("react".into(), "^19.3.0".into()),
                ("react-dom".into(), "^19.3.0".into()),
            ],
            &[
                ("@types/react", "^19.3.0"),
                ("@types/react-dom", "^19.3.0"),
                ("@vitejs/plugin-react", "^6.1.2"),
                ("typescript", "^6.0.3"),
                ("vite", "^8.3.4"),
            ],
        ),
        Web::Svelte => (
            "svelte-check --tsconfig ./tsconfig.json --threshold error",
            vec![("@ferrix/client".into(), spec("@ferrix/client")), ("@ferrix/svelte".into(), spec("@ferrix/svelte"))],
            &[
                ("@sveltejs/vite-plugin-svelte", "^7.3.1"),
                ("svelte", "^5.57.2"),
                ("svelte-check", "^4.7.6"),
                ("typescript", "^6.0.3"),
                ("vite", "^8.3.4"),
            ],
        ),
        Web::None => unreachable!("no web app"),
    };
    deps.sort();
    let q = |s: &str| serde_json::to_string(s).expect("a string serializes");
    let object = |entries: &mut dyn Iterator<Item = (&str, &str)>| {
        let body: Vec<String> = entries.map(|(k, v)| format!("    {}: {}", q(k), q(v))).collect();
        format!("{{\n{}\n  }}", body.join(",\n"))
    };
    let mut out = format!(
        "{{\n  \"name\": {},\n  \"private\": true,\n  \"type\": \"module\",\n  \"scripts\": {},\n",
        q(&format!("{name}-web")),
        object(&mut [("dev", "vite"), ("build", "vite build"), ("typecheck", typecheck)].into_iter()),
    );
    let _ = writeln!(out, "  \"dependencies\": {},", object(&mut deps.iter().map(|(k, v)| (k.as_str(), v.as_str()))));
    let _ = write!(out, "  \"devDependencies\": {}", object(&mut dev_deps.iter().copied()));
    if packed.is_some() {
        // @ferrix/react (or svelte) depends on @ferrix/client too; without this, that dependency would go to npm.
        let client = spec("@ferrix/client");
        let _ = write!(out, ",\n  \"overrides\": {}", object(&mut [("@ferrix/client", client.as_str())].into_iter()));
    }
    out.push_str("\n}\n");
    out
}

/// A TOML basic string.
fn toml_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn file<'a>(files: &'a [(PathBuf, String)], path: &str) -> &'a str {
        &files.iter().find(|(p, _)| p == Path::new(path)).unwrap_or_else(|| panic!("no {path}")).1
    }

    /// The value of `LIBSQLITE3_FLAGS = "..."` in a Cargo config.
    fn sqlite_flags(config: &str) -> &str {
        let line = config.lines().find(|l| l.starts_with("LIBSQLITE3_FLAGS")).expect("LIBSQLITE3_FLAGS line");
        line.split('"').nth(1).expect("a quoted value")
    }

    #[test]
    fn the_sqlite_flags_are_the_ones_this_repository_builds_and_ferrix_recommends() {
        let ours = std::fs::read_to_string(repo_root().join(".cargo/config.toml")).unwrap();
        let flags = sqlite_flags(CARGO_CONFIG);
        assert_eq!(flags, sqlite_flags(&ours));
        // The startup warning (`configure_sqlite`) quotes them for apps that lack this file.
        let db = std::fs::read_to_string(repo_root().join("crates/ferrix/src/db/mod.rs")).unwrap();
        assert!(db.contains(&format!("LIBSQLITE3_FLAGS=\\\"{}\\\"", flags.replace('"', "\\\""))), "warning text");
    }

    #[test]
    fn every_app_gets_the_cargo_config() {
        for web in [Web::React, Web::Svelte, Web::None] {
            let files = render("app", web, &Source::Published, None).unwrap();
            let config = file(&files, ".cargo/config.toml");
            assert_eq!(sqlite_flags(config), sqlite_flags(CARGO_CONFIG));
            assert_eq!(config.contains("FERRIX_CODEGEN_OUT"), web != Web::None, "{web:?}");
            // Every placeholder was filled.
            for (path, contents) in &files {
                assert!(!contents.contains("{{"), "{} has an unfilled placeholder", path.display());
            }
        }
    }

    #[test]
    fn published_and_checkout_dependencies() {
        let files = render("app", Web::React, &Source::Published, None).unwrap();
        let version = env!("CARGO_PKG_VERSION");
        assert!(file(&files, "Cargo.toml").contains(&format!("ferrix = \"{version}\"")));
        let package: serde_json::Value = serde_json::from_str(file(&files, "web/package.json")).unwrap();
        assert_eq!(package["dependencies"]["@ferrix/react"], format!("^{version}"));
        assert!(package.get("overrides").is_none());

        let root = PathBuf::from("/src/fer\"rix");
        let packed = Packed {
            packages: ["client", "svelte"]
                .map(|dir| PackedPackage {
                    name: format!("@ferrix/{dir}"),
                    dir: dir.into(),
                    tarball: format!("ferrix-{dir}-9.9.9.tgz"),
                })
                .to_vec(),
        };
        let files = render("app", Web::Svelte, &Source::Checkout(root), Some(&packed)).unwrap();
        assert!(file(&files, "Cargo.toml").contains(r#"ferrix = { path = "/src/fer\"rix/crates/ferrix" }"#));
        let package: serde_json::Value = serde_json::from_str(file(&files, "web/package.json")).unwrap();
        assert_eq!(package["name"], "app-web");
        assert_eq!(package["dependencies"]["@ferrix/svelte"], "file:./vendor/ferrix-svelte-9.9.9.tgz");
        assert_eq!(package["overrides"]["@ferrix/client"], "file:./vendor/ferrix-client-9.9.9.tgz");
        assert!(file(&files, "README.md").contains("web/vendor"));
    }

    #[test]
    fn web_dependency_versions_match_the_examples() {
        let mut known = std::collections::BTreeMap::new();
        for example in ["todo", "chat-react"] {
            let path = repo_root().join(format!("examples/{example}/web/package.json"));
            let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            for section in ["dependencies", "devDependencies"] {
                for (k, v) in json[section].as_object().unwrap() {
                    known.insert(k.clone(), v.as_str().unwrap().to_string());
                }
            }
        }
        for web in [Web::React, Web::Svelte] {
            let files = render("app", web, &Source::Published, None).unwrap();
            let package: serde_json::Value = serde_json::from_str(file(&files, "web/package.json")).unwrap();
            for section in ["dependencies", "devDependencies"] {
                for (k, v) in package[section].as_object().unwrap() {
                    if k.starts_with("@ferrix/") {
                        continue;
                    }
                    assert_eq!(Some(v.as_str().unwrap()), known.get(k).map(String::as_str), "{web:?} {k}");
                }
            }
        }
    }

    #[test]
    fn names() {
        for ok in ["app", "my-app", "my_app2", "A"] {
            validate_name(ok).unwrap();
        }
        for bad in ["", "2app", "-app", "my app", "ferrix", "test", "ünï"] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
        assert_eq!(dir_name(Path::new("/tmp/my-app/.")).unwrap(), "my-app");
        assert_eq!(dir_name(Path::new("/tmp/my-app/")).unwrap(), "my-app");
        assert!(dir_name(Path::new("/")).is_err());
    }
}
