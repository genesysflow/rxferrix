# Ferrix

A single-binary reactive backend in Rust, deployed as one file.
You write typed `#[query]` / `#[mutation]` / `#[action]` functions in Rust. Clients subscribe with `useQuery` over a
WebSocket and get updated results automatically whenever a write affects them.

Status: Phases 0–3 and auth (Phase 5) are mostly done (see [PLAN.md](PLAN.md) §10). The dashboard, the scheduler,
files and search are not built yet.

## Layout

| Path | What |
|---|---|
| `crates/ferrix` | The runtime: SQLite storage, query engine, read sets, write log, sync worker, axum server, CLI, codegen, `TestApp`. |
| `crates/ferrix-macros` | `#[table]`, `#[query]`, `#[mutation]`, `#[action]`, `#[internal_*]`. |
| `packages/client` | `@ferrix/client`: framework-agnostic TS client (sync protocol, optimistic updates, reconnect). |
| `packages/react` | `@ferrix/react`: `FerrixProvider`, `useQuery`, `useMutation`, `useAction`. |
| `packages/svelte` | `@ferrix/svelte`: Svelte 5 runes bindings. |
| `examples/todo` | Rust backend and Svelte web app, with Playwright e2e scripts. |
| `docs/CONTRACT.md` | Rust API and the exact code each macro generates. |
| `docs/PROTOCOL.md` | WebSocket and HTTP protocol. |

## A backend in a nutshell

```rust
use ferrix::prelude::*;

#[table(name = "todos")]
#[index(by_done = [done])]
pub struct Todo { pub text: String, pub done: bool }

#[query]
pub async fn list(ctx: &QueryCtx, done: Option<bool>) -> Result<Vec<Doc<Todo>>> {
    let q = match done {
        Some(d) => ctx.db.query::<Todo>().with_index(Todo::by_done, |q| q.eq(Todo::done, d)),
        None => ctx.db.query::<Todo>(),
    };
    q.order(Order::Desc).take(500).await
}

#[mutation]
pub async fn add(ctx: &MutationCtx, text: String) -> Result<Id<Todo>> {
    ctx.db.insert(Todo { text, done: false }).await
}

fn main() -> ferrix::Result<()> { ferrix::Ferrix::new().run() }
```

Functions call each other through the runtime with turbofish: `ctx.run_mutation::<add>(("hi".into(),)).await?`.

## Auth

Enable sign-in methods in `main`, then use `ctx.auth` in functions:

```rust
use ferrix::auth::{AuthConfig, GitHub, User};

fn main() -> ferrix::Result<()> {
    ferrix::Ferrix::new()
        .auth(AuthConfig::password().magic_link().oauth(GitHub::from_env()).redirect_url("http://localhost:5173"))
        .run()
}

#[table(name = "notes")]
#[index(by_owner = [owner])]
pub struct Note { pub owner: Id<User>, pub text: String }

#[mutation]
pub async fn add_note(ctx: &MutationCtx, text: String) -> Result<Id<Note>> {
    let owner = ctx.auth.require_user()?;          // UserError "unauthenticated" when signed out
    ctx.db.insert(Note { owner, text }).await
}
```

On the client, `getAuth(client)` signs in and keeps the socket authenticated (tokens in `localStorage`, refreshed
automatically). React has `useFerrixAuth()`, `useAuthActions()` and `useCurrentUser()`; Svelte has `useAuth()`
and `useCurrentUser()`:

```ts
const auth = getAuth(client);
await auth.signIn({ email, password });           // or signUp, requestMagicLink + verifyCode, signInWithOAuth("github")
await auth.handleRedirectCallback();              // on the page OAuth and email links return to
```

Access tokens are 15-minute HS256 JWTs signed with a key from `<data dir>/auth_secret.key`; refresh tokens rotate
on every use. Email goes through `--smtp-url` (printed to the log in `--dev`). With no admin yet, `serve` prints a
one-time `/_/setup#token=…` link; `admin create <email>` works too. Details: [docs/PROTOCOL.md](docs/PROTOCOL.md)
("Auth") and [docs/CONTRACT.md](docs/CONTRACT.md) §1.2, §1.5.

## Running the example

Prerequisites: Rust via mise (`mise.toml`) and bun.

```sh
bun install && bun run build                                    # build the TS packages
cargo run -p todo-server -- serve --dev \
  --codegen-out examples/todo/web/src/lib/ferrix                # backend on :8090; keeps the generated TS API current
cd examples/todo/web && bun run dev                             # web app on :5173 (proxies /api to :8090)
```

Open http://localhost:5173 in two tabs. Other commands: `todo-server functions` lists the registered functions,
and `todo-server codegen --out <dir>` writes the TS API once.

## Tests

```sh
cargo test --workspace                                          # Rust: unit, proptests, model-based reactivity, WS e2e
cargo test -p ferrix --test it codegen_typechecks_with_tsc -- --ignored   # generated TS type-checks with tsc
bun run typecheck && bun run test                               # TS packages
cd examples/todo/web && bun e2e/two-tabs.ts                     # needs the backend and Vite running
bun e2e/reconnect.ts ../../../target/debug/todo-server <data dir>  # Vite running, backend NOT running
```
