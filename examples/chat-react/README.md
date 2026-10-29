# Chat example (React)

A multi-user chat app with a Rust backend and React 19 frontend. It demonstrates sign-in, channel access checks, live messages, pagination, optimistic updates, attachments, search, delayed reminders, aggregates, and optional SQL analytics.

## Run locally

Install Rust and Bun. From the framework repository root:

```sh
bun install
bun run build
cargo run -p chat-server -- serve --dev --http 127.0.0.1:8097 \
  --codegen-out examples/chat-react/web/src/rxferrix
```

In a second terminal, from the repository root:

```sh
cd examples/chat-react/web
bun run dev
```

Open http://localhost:5180 in two browser profiles, or a normal and private window, and create two accounts. Separate profiles keep their sign-in tokens independent.

The dashboard is at http://127.0.0.1:8097/_/; use the first-start setup link to create an administrator. Restart the backend after Rust changes to rebuild and regenerate the API.

## Configure the example

| Setting | Default / purpose |
| --- | --- |
| `RXFERRIX_URL` | Vite's backend target; `http://127.0.0.1:8097`. |
| `VITE_PORT` | Vite's port; `5180`. |
| `CHAT_APP_URL` | Allowed frontend auth return URL; `http://localhost:5180`. |
| `CHAT_WEB_DIR` | Optional directory of built frontend files served by the backend. |
| `RXFERRIX_AUTH_GITHUB_CLIENT_ID`, `RXFERRIX_AUTH_GITHUB_CLIENT_SECRET` | Enable GitHub sign-in when configured. |

Vite proxies `/api`, including the WebSocket. In `--dev`, file/upload URLs are relative to that origin unless `--public-url` is set. For GitHub, register `<public backend URL>/api/auth/oauth/github/callback`.

Without SMTP in development, email codes and links are printed to the backend log. The example also keeps a dev inbox for its browser tests. `dev:last_code` is a development-only action exposed through `devApi`; production mode refuses it.

Data uses `fx_data/` relative to the backend command's working directory. Pass a dedicated `--dir` when running this alongside another example.

## Try the features

Send messages between two accounts, join channels, upload an image, and search the channel. Use `/remind me in 10 seconds to stretch` for a scheduled reminder, or `/seed 200` to create history for pagination.

Set `CHAT_ANNOUNCEMENT` on **Dashboard → Environment**. It appears above channels and updates without a restart. This is a `ctx.env` app variable, separate from the process settings in the table above.

Channel functions enforce membership/readability and editing permissions. Expected errors such as `not_a_member`, `private_channel`, and `forbidden` are displayed in the UI. Internal reminder and maintenance jobs use trusted arguments rather than inheriting the user's identity.

## Enable Insights

Build with the analytics feature and enable the copy at runtime:

```sh
cargo run -p chat-server --features olap -- serve --dev --http 127.0.0.1:8097 \
  --olap --codegen-out examples/chat-react/web/src/rxferrix
```

Without analytics, the Insights view explains how to enable it. To generate demo traffic, from `examples/chat-react/web`:

```sh
bun e2e/demo-seed.ts http://127.0.0.1:8097 120
```

This creates demo users such as `ada@demo.test` with password `demo-password`, channels, messages, and reactions. It runs for two minutes and may be repeated. Use a disposable development data directory for this demo.

Insights uses actions over the asynchronous DuckDB copy, so it polls for reports rather than subscribing to SQL. Queries scope results to readable channels.

## Find the implementation

Server paths below are relative to `server/src/`; web paths are relative to `web/src/components/` unless indicated.

| Feature | Server | Web |
|---|---|---|
| Sign up / sign in, email code, GitHub | `server/src/main.rs` (`AuthConfig`), `dev.rs` (dev inbox, `dev_mailer`, `#[action(dev_only)]`) | `AuthScreen.tsx` (`useAuthActions`, `providers()`) |
| Profiles linked to `_users`, auto-join #general | `profiles.rs` (`ensure`) | `Chat.tsx` |
| Channels, join, invite, private channels | `channels.rs`, `model.rs` (`require_member`, `require_readable`) | `Sidebar.tsx`, `ChannelView.tsx`, `AccessError.tsx` |
| Messages newest first, infinite scroll | `messages.rs` (`list`: index `by_channel` + `Order::Desc`, `paginate`, `try_map` into `MessageView`) | `MessageList.tsx` (`usePaginatedQueryWithStatus`, `column-reverse` + IntersectionObserver) |
| View types (`#[object]`, a `Doc<T>` flattened in) | `messages.rs` (`MessageView`), `channels.rs` (`ChannelSummary`, `ChannelInfo`) | `lib.ts` (the generated `MessageView`, ...) |
| Send (optimistic), edit, delete (author only) | `messages.rs` (`send`, `edit`, `remove`) | `Composer.tsx` (`insertAtTop`), `MessageItem.tsx` |
| Reactions (optimistic) | `reactions.rs` | `MessageList.tsx` (`getAllPages` + `setQuery`) |
| Unread badges | `channels.rs` (`unread`: `_creation_time` bound after the channel, `count()`; `mark_read`; per-member `lastReadAt`) | `Sidebar.tsx`, `MessageList.tsx` |
| Image attachments with upload progress | `messages.rs` (`generate_upload_url`, `send`) | `Composer.tsx` (`uploadFile` + `onProgress`), `MessageItem.tsx` (`?thumb=`, falls back to the original) |
| Search | `messages.rs` (`search`, `#[search_index]` filtered by channel) | `SearchPanel.tsx` |
| Typing indicators | `typing.rs` (`start` + `run_after` expiry, hourly `prune` cron) | `TypingIndicator.tsx`, `Composer.tsx` |
| `/remind` | `messages.rs` (`run_after::<deliver_reminder>`) | `Composer.tsx` (shows the notice) |
| Connection state | | `ConnectionStatus.tsx` (`useConnectionState`) |
| Live counts: messages per channel, members, top posters | `model.rs` (`#[aggregate]`), `channels.rs` (`aggregate(..).get`, `groups`) | `Sidebar.tsx`, `ChannelView.tsx` |
| Announcement banner (an environment variable, live) | `announcement.rs` (`ctx.env.get("CHAT_ANNOUNCEMENT")`) | `Chat.tsx` (`Announcement`) |
| Insights: activity, busiest hours, rankings | `insights.rs` (an action using `ctx.olap.query_as`, scoped to readable channels) | `InsightsView.tsx` (polls `useAction`) |

## Serve the built frontend

From the repository root, after generating the API:

```sh
cd examples/chat-react/web
bun run build
cd ../../..
CHAT_WEB_DIR=examples/chat-react/web/dist cargo run -p chat-server -- \
  serve --http 127.0.0.1:8097 --auth-redirect-url http://127.0.0.1:8097
```

This example reads frontend files at runtime through `static_dir`; it does not embed them. Set the frontend return URL for the origin you actually use. See [Deploying](../../docs/operations/deploying.md) for public URLs and HTTPS.

## Run tests

From the repository root:

```sh
cargo test -p chat-server
cargo build -p chat-server
```

Then, from `examples/chat-react/web`:

```sh
bun run typecheck
bun e2e/chat.ts
```

The browser suite starts its own backend with a fresh data directory on :8097 and Vite on :5180. Stop existing processes on those ports first, or use `E2E_EXTERNAL=1` to test already-running development servers.

`CHAT_OLAP=1 bun e2e/chat.ts` includes analytics checks after `cargo build -p chat-server --features olap`. `SCREENSHOTS=/tmp/shots bun e2e/chat.ts` saves screenshots.

The suites cover permissions, unread state, pagination, typing expiry, reminders, uploads, search, live counts, announcements, and optional analytics. See [Contributing](../../CONTRIBUTING.md) for framework-wide checks.
