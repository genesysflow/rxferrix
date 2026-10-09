# Ferrix Chat (React)

A Slack-lite that exercises most of Ferrix: auth (password, email code, optional GitHub), reactive queries and
paginated queries, optimistic updates, file storage, full-text search, the scheduler and crons, and per-channel
authorization. The backend is `server/` (crate `chat-server`), the React 19 + Vite app is `web/`.

## Run it

Prerequisites: Rust via mise and bun, from the repository root.

```sh
bun install && bun run build                          # build the TS packages
cargo run -p chat-server -- serve --dev \
  --http 127.0.0.1:8097 --public-url http://localhost:5180 \
  --codegen-out examples/chat-react/web/src/ferrix     # backend on :8097; regenerates the TS API on start
cd examples/chat-react/web && bun run dev              # app on http://localhost:5180 (proxies /api to :8097)
```

Open http://localhost:5180 in two browser profiles (or one normal and one private window) and sign up twice.

- `--public-url` makes file URLs (`ctx.storage.get_url`) and upload URLs point at the Vite origin, so images and
  uploads go through the same `/api` proxy as everything else. Without it they point at :8097 directly, which also
  works.
- Ports: `FERRIX_URL` (the backend Vite proxies to, default `http://127.0.0.1:8097`) and `VITE_PORT` (default 5180).
- In `--dev` without `--smtp-url`, sign-in emails (codes and magic links) are printed to the backend log. The example
  also keeps them in memory so the e2e test can read codes with the `dev:last_code` action; that action refuses to
  run in any other mode.
- GitHub sign-in shows up when `FERRIX_AUTH_GITHUB_CLIENT_ID` and `FERRIX_AUTH_GITHUB_CLIENT_SECRET` are set (callback
  URL: `<public url>/api/auth/oauth/github/callback`).
- Slash commands: `/remind me in 10 seconds to stretch` (also `minutes`, `hours`), and `/seed 200`, which adds 200
  backdated sample messages through an action so there is history to scroll through.
- The admin dashboard is at http://127.0.0.1:8097/_/ (the backend prints a one-time setup link on first start).

## What's where

| Feature | Server | Web |
|---|---|---|
| Sign up / sign in, email code, GitHub | `main.rs` (`AuthConfig`), `dev.rs` (dev inbox) | `AuthScreen.tsx` (`useAuthActions`, `useFerrixAuth`) |
| Profiles linked to `_users`, auto-join #general | `profiles.rs` (`ensure`) | `Chat.tsx` |
| Channels, join, invite, private channels | `channels.rs`, `model.rs` (`require_member`, `require_readable`) | `Sidebar.tsx`, `ChannelView.tsx`, `AccessError.tsx` |
| Messages newest first, infinite scroll | `messages.rs` (`list`, index `by_channel_sent`, `paginate`) | `MessageList.tsx` (`usePaginatedQuery`, `column-reverse` + IntersectionObserver) |
| Send (optimistic), edit, delete (author only) | `messages.rs` (`send`, `edit`, `remove`) | `Composer.tsx` (`insertAtTop`), `MessageItem.tsx` |
| Reactions (optimistic) | `reactions.rs` | `MessageList.tsx` (`getAllPages` + `setQuery`) |
| Unread badges | `channels.rs` (`unread`, `mark_read`; per-member `lastReadAt`) | `Sidebar.tsx`, `MessageList.tsx` |
| Image attachments with upload progress | `messages.rs` (`generate_upload_url`, `send`) | `Composer.tsx` (`uploadFile` + `onProgress`), `MessageItem.tsx` (`?thumb=`, falls back to the original) |
| Search | `messages.rs` (`search`, `#[search_index]` filtered by channel) | `SearchPanel.tsx` |
| Typing indicators | `typing.rs` (`start` + `run_after` expiry, hourly `prune` cron) | `TypingIndicator.tsx`, `Composer.tsx` |
| `/remind` | `messages.rs` (`run_after::<deliver_reminder>`) | `Composer.tsx` (shows the notice) |
| Connection state | | `ConnectionStatus.tsx` (`useConnectionState`) |

Every function starts with `ctx.auth.require_user()`; channel access fails with the `UserError` codes
`not_a_member`, `private_channel`, `forbidden` (editing someone else's message), which the UI shows.

## Tests

```sh
cargo test -p chat-server                 # TestApp: authorization, unread counts, pagination order, typing expiry, /remind
cargo build -p chat-server
cd examples/chat-react/web
bun run typecheck
bun e2e/chat.ts                           # starts its own backend (fresh data dir) on :8097 and Vite on :5180
SCREENSHOTS=/tmp/shots bun e2e/chat.ts    # ...and saves chat-*.png screenshots
E2E_EXTERNAL=1 bun e2e/chat.ts            # against a backend (--dev) and Vite that are already running
```

The e2e drives three users in separate browser contexts: two sign up with passwords, the third signs in with an
emailed code. It covers live messages, reactions, edits and deletes, unread badges, attachments, search, infinite
scroll over 200 seeded messages, a typing indicator that appears and expires, a `/remind` that arrives a few seconds
later, and a private channel that a non-member can't read (in the UI and over the HTTP API) until invited.
