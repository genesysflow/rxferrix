# Contributing to RxFerrix

Run the commands on this page from the framework repository root unless a step names another directory. For building an app with the framework, start with [Get started](docs/getting-started.md).

## Prepare a checkout

Install Rust and Bun, then build the client packages before running a web example or generating an app from the checkout:

```sh
bun install
bun run build
cargo install --path crates/rxferrix-cli
```

A generator built from this checkout defaults to the local Rust crate and packs the built client packages into the new app's `web/vendor` directory. Rebuild and regenerate those dependencies when testing framework package changes. Use `--published` to test registry packages or `--rxferrix-path DIR` to select another checkout.

The [todo](examples/todo/README.md) and [chat](examples/chat-react/README.md) READMEs include run commands. See the [design](docs/DESIGN.md), [Rust contract](docs/CONTRACT.md), and [protocol](docs/PROTOCOL.md) before changing runtime behavior or macro output.

## Repository layout

| Path | What |
|---|---|
| `crates/rxferrix` | The runtime: storage, query engine, reactivity, server, CLI, codegen, test helpers. |
| `crates/rxferrix-macros` | The `#[table]`, `#[query]`, `#[mutation]`, `#[action]`, `#[migration]`, … macros. |
| `crates/rxferrix-cli` | The `rxferrix` command (`rxferrix new`). |
| `packages/client` | `@rxferrix/client`: the TypeScript client (sync, optimistic updates, reconnects). |
| `packages/react`, `packages/svelte` | `@rxferrix/react` and `@rxferrix/svelte` bindings. |
| `packages/dashboard` | The admin dashboard (Svelte), embedded in the binary at `/_/`. |
| `examples/` | The todo and chat apps. |
| `deploy/` | systemd, Docker Compose and Kubernetes files. |
| `docs/` | The documentation site (VitePress): guide, operations, [API contract](docs/CONTRACT.md), [protocol](docs/PROTOCOL.md), [design](docs/DESIGN.md). |

## Development

```sh
cargo fmt --all --check                       # formatting (rustfmt); `cargo fmt --all` fixes it
cargo test --workspace                        # Rust tests: unit, property, reactivity model, WebSocket e2e
bun run typecheck && bun run test             # TypeScript packages, dashboard included
```

Run additional suites as needed for the area you change. From the repository root:

```sh
cargo test -p rxferrix --test it codegen_typechecks_with_tsc -- --ignored
cargo test -p rxferrix-cli --test new -- --ignored
cargo test -p rxferrix --features olap --lib --test it
bun run check:dashboard
```

The first two check generated TypeScript and complete generated apps. The analytics suite builds DuckDB and can take several minutes on a clean build. The release workflow runs these alongside the ordinary checks and browser suites.

For the dashboard browser suite, build `todo-server`, then run `bun e2e/dashboard.ts` inside `packages/dashboard`. For todo browser suites, follow their server requirements and run from the example web directory:

```sh
cd examples/todo/web
bun e2e/two-tabs.ts
bun e2e/reconnect.ts ../../../target/debug/todo-server <data-dir>
```

`two-tabs.ts` requires backend and Vite servers already running. `reconnect.ts` requires Vite but manages the backend itself. The chat suite starts both its servers; see its [README](examples/chat-react/README.md#run-tests).

Takeover tests run from the framework root after building the todo binary:

```sh
bun scripts/takeover-e2e.ts
bun scripts/docker-rollout-e2e.ts
bun scripts/k3s-rollout-e2e.ts
```

The latter two require Docker and the example image. For longer protocol fuzzing, use `FUZZ_SCALE=20 cargo test --release -p rxferrix --test it fuzz`.

Benchmarks: `cargo test --release -p rxferrix --test it bench_ -- --ignored --nocapture`.

The dashboard's built files are committed, so `cargo build` needs no JavaScript toolchain. After changing
`packages/dashboard` or the client packages, run `bun run build:dashboard` and commit `crates/rxferrix/dashboard-dist`
(`cargo test` fails while it is out of date). `cd packages/dashboard && bun run dev` serves it on :5174 for UI work.

The documentation site lives in `docs/` (VitePress): `bun run docs:dev` serves it with live reload, and
`bun run docs:build` builds it. A push to `master` that changes `docs/` publishes it to GitHub Pages
([`.github/workflows/docs.yml`](.github/workflows/docs.yml)).

## Releasing

The three crates and three npm packages share one version. Choose the next version, update the manifests, then commit and tag it. For example:

```sh
bun scripts/version.ts 0.10.0          # Cargo.toml, package.json files, CLIENT_VERSION, lockfiles, dashboard build
git commit -am "Version 0.10.0" && git tag v0.10.0 && git push origin master v0.10.0
```

Once the tag's CI run passes its gates, it publishes the crates to crates.io and the packages to npm (with
provenance), creates the GitHub release with the static binaries, and pushes the Docker image. A version with a
suffix (`0.10.0-rc.1`) goes to npm under the `next` tag. A version already published is skipped, so a run that
failed halfway can be rerun.

Publishing uses trusted publishing: no registry token is stored in the repository. Each crate and package names
this repository's `release.yml` as its trusted publisher, once:

- crates.io: for `rxferrix-macros`, `rxferrix` and `rxferrix-cli`, Settings → Trusted Publishing → Add: GitHub,
  owner `genesysflow`, repository `rxferrix`, workflow `release.yml`, no environment.
- npm: for `@rxferrix/client`, `@rxferrix/react` and `@rxferrix/svelte`, Settings → Trusted Publisher → GitHub
  Actions: organization or user `genesysflow`, repository `rxferrix`, workflow file `release.yml`, no environment.
