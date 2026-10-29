# Authentication and authorization

Authentication identifies the caller. Authorization decides which data that caller may use. Configure sign-in methods in `main.rs`, then enforce access in your public queries, mutations, and actions.

## Enable sign-in

```rust
use rxferrix::auth::AuthConfig;

fn main() -> rxferrix::Result<()> {
    rxferrix::RxFerrix::new()
        .auth(AuthConfig::password()
            .magic_link()
            .redirect_url("http://localhost:5173"))
        .run()
}
```

`password()` enables email/password accounts. `magic_link()` adds email-code/link sign-in. The redirect URL is the frontend users return to after following a link or completing OAuth.

In development without SMTP, auth email codes and links appear in the backend log. For delivery, configure `--smtp-url` and `--smtp-from`, a custom mailer, or the dashboard's SMTP settings.

Set the backend's `--public-url` in production so email links and OAuth callbacks use its public address. Keep allowed frontend return URLs in `redirect_url(...)`, `--auth-redirect-url`, or dashboard settings.

## Sign in from a client

The core auth manager stores browser tokens in `localStorage`, refreshes them, and authenticates the client's WebSocket:

```ts
import { getAuth } from "@rxferrix/client";

const auth = getAuth(client);
const result = await auth.signIn({ email, password });
if (result.mfaRequired) {
  await auth.verifyMfa(result.mfaToken, codeFromAuthenticator);
}
```

`client` is the app's existing `RxFerrixClient`. Call `auth.handleRedirectCallback()` on the page receiving OAuth or email redirects, and handle a possible MFA challenge there too.

| Task | Client method |
| --- | --- |
| Create a password account | `signUp({ email, password })` |
| Sign in | `signIn({ email, password })` |
| Send and verify an email code | `requestMagicLink(...)`, then `verifyCode(...)` |
| Start OAuth | `signInWithOAuth("github")` |
| Sign out | `signOut()` |
| Change a password while signed in | `changePassword(currentPassword, newPassword)` |
| Recover a password | `requestPasswordReset(...)`, then `resetPassword(...)` |

React exposes state through `useRxFerrixAuth()`, actions through `useAuthActions()`, and the user's document through `useCurrentUser()`. Svelte combines state and actions in `useAuth()` and also provides `useCurrentUser()`. See [Clients](/guide/clients).

## Check access to each document

This example stores notes owned by an app user:

```rust
use rxferrix::auth::User;
use rxferrix::prelude::*;

#[table(name = "notes")]
#[index(by_owner = [owner])]
pub struct Note {
    pub owner: Id<User>,
    pub text: String,
}

#[query]
pub async fn list(ctx: &QueryCtx) -> Result<Vec<Doc<Note>>> {
    let owner = ctx.auth.require_user()?;
    ctx.db.query::<Note>()
        .with_index(Note::by_owner, |q| q.eq(Note::owner, owner))
        .take(100)
        .await
}

#[mutation]
pub async fn add(ctx: &MutationCtx, text: String) -> Result<Id<Note>> {
    let owner = ctx.auth.require_user()?;
    ctx.db.insert(Note { owner, text }).await
}

#[mutation]
pub async fn remove(ctx: &MutationCtx, id: Id<Note>) -> Result<()> {
    let owner = ctx.auth.require_user()?;
    let note = ctx.db.get(id).await?.or_user("not_found", "Note not found")?;
    ensure_user!(note.owner == owner, "forbidden", "You cannot delete this note");
    ctx.db.delete(id).await
}
```

`require_user()` rejects signed-out callers with `unauthenticated`. The list selects that user's rows; deletion checks ownership explicitly. Knowing an ID never grants access by itself.

Authorization reads are tracked along with a subscribed query's other reads, so changing access data can invalidate the result. Use internal functions for operations that only trusted server code should call.

## Apply an app-wide guard

A guard can enforce a common requirement before every client call to your public functions:

```rust
rxferrix::RxFerrix::new()
    .guard(|ctx, _function| async move {
        ctx.auth.require_user()?;
        Ok(())
    })
    .run()
```

The callback can also read access data through `ctx.db` or exempt selected functions with `function.is::<module::function>()`. Its reads use the call's snapshot/transaction and participate in query invalidation.

Guards apply to client calls to app public functions. Internal functions, built-in `_system/*` functions, scheduled jobs, action-to-function calls, trusted `App` calls, and the admin function runner bypass them. Keep document-specific checks in the functions that need them. In tests, set `AppOptions::guard` with `Guard::new(...)`; `TestApp::as_user` exercises client access checks.

## Add OAuth

Add a provider to the auth configuration, for example `.oauth(rxferrix::auth::GitHub::from_env())`.

Presets are `GitHub`, `Google`, `Discord`, and `Apple`; `Oidc` supports other OpenID Connect issuers. `from_env()` reads `RXFERRIX_AUTH_<PROVIDER>_CLIENT_ID` and related variables. `Oidc::from_env(id)` also reads `RXFERRIX_AUTH_<ID>_ISSUER`. Providers can also be configured through the dashboard.

Register the provider callback as `https://your-backend/api/auth/oauth/<provider>/callback`. Accounts are linked by email only when the provider verifies that address.

For Apple, configure the Services ID, team ID, key ID, and `.p8` private key (`RXFERRIX_AUTH_APPLE_PRIVATE_KEY` or `_PRIVATE_KEY_FILE`). RxFerrix creates and rotates the client secret. Apple requires an HTTPS public callback and uses `form_post`; the name supplied on the first sign-in is retained. Store the private key outside source control or in encrypted dashboard settings.

## Use an external identity provider

To accept JWTs from Clerk, Auth0, Supabase, or another issuer:

```rust
use rxferrix::auth::{AuthConfig, JwtProvider};

let auth = AuthConfig::default().jwt(
    JwtProvider::new("https://tenant.auth0.com/")
        .audience("https://api.example.com")
);
```

Pass this configuration to `.auth(auth)`. RxFerrix discovers the issuer's JWKS and validates signatures and claims. Supported algorithms are RS256, ES256, and EdDSA. Each new subject gets a `_users` row, so app code continues using `Id<User>`.

Give the core client a token getter from your provider:

```ts
client.setAuth(({ forceRefresh }) =>
  clerk.session!.getToken({ skipCache: forceRefresh })
);
```

The client refreshes before token expiry and after rejection. A getter that throws is retried with backoff. If your issuer does not use an audience claim, configure `.any_audience()` explicitly.

## Add two-factor authentication

Users and administrators can enroll a TOTP authenticator without extra server infrastructure. Accounts with an email address must verify it first. `.totp_issuer("My App")` changes the name shown by authenticator apps.

```ts
const { qrSvg, secret } = await auth.setupTotp({ password });
// Show the QR code or secret, then ask for the first code.
const { recoveryCodes } = await auth.confirmTotp(firstCode);
// Show the recovery codes once and ask the user to save them.
```

Every enabled sign-in method then returns an MFA challenge until a TOTP code or recovery code is verified. `getMfaStatus()`, `disableMfa(code)`, and `regenerateRecoveryCodes(code)` manage enrollment. Five wrong codes lock the factor for five minutes; a code cannot be reused.

Administrators can reset a user's lost factor on **Users**. For an administrator who lost both the authenticator and recovery codes, use the app's `admin disable-2fa EMAIL` command. Administrators manage their own enrollment on **Account**.

## Control account creation and sign-in hooks

`signups(false)` disables sign-ups. `allowed_domains(["example.com"])` and `allowed_emails(["guest@example.net"])` restrict accepted addresses. With an allowlist, password accounts must verify their email before sign-in and OAuth must supply a verified address.

For invitations or another custom policy, use `before_sign_up(|app, attempt| async move { ... })`. The attempt contains the email, verification state, method, name, and optional frontend return URL. Return `UserError` to reject creation with a code the client can handle. The hook is a check; perform app-side invitation acceptance after sign-in because account creation can still fail after the check passes.

`on_sign_in(|app, sign_in| async move { ... })` runs after a successful session-creating sign-in. It receives the user, session, method, and client information. Keep it short: the response waits for it. Use an internal mutation to record app-side login history.

## Send email and customize templates

Actions can use the same configured mailer as auth:

```rust
use rxferrix::auth::Email;
use rxferrix::prelude::*;

#[internal_action]
pub async fn send_confirmation(ctx: &ActionCtx, to: String, text: String) -> Result<()> {
    ctx.send_email(Email::new(to, "Your booking", text)).await
}
```

Use `.html(html)` for an HTML body. Without a mailer this returns `email_not_configured`. Schedule email actions from mutations so a rolled-back mutation does not send mail; use retries only with an appropriate duplicate-delivery strategy.

In **Settings → Email templates**, customize the subject and HTML for verification, password reset, and sign-in. Placeholders are `{link}`, `{code}`, `{expires}`, `{app_name}`, and `{email}`. A template must include `{code}` or `{link}`; values are HTML-escaped and a plain-text alternative is generated.

## Sessions, settings, and user removal

Built-in access tokens last 15 minutes by default; refresh tokens rotate. Sign-out, session revocation, user deletion, or detected refresh-token reuse revokes affected sockets promptly. The refresh reuse grace period is 10 seconds by default. External tokens have no local session row; deleting their provisioned user also revokes their sockets.

The data directory's `auth_secret.key` signs built-in sessions and encrypts secrets. Preserve it when moving or restoring the app.

Dashboard administrators can update providers, SMTP, redirects, sign-up settings, token lifetimes, and CORS without restarting. Values explicitly fixed in code, flags, or environment take precedence. Admin changes and impersonation sessions are audited.

Mutations can call `ctx.users.delete(user)`, `revoke_sessions(user)`, or `revoke_session(session)` in their transaction. These helpers do not check app permissions: protect the calling mutation. Revoking sessions does not ban a user who can sign in again; store and enforce a ban in your access policy.

For imported users, `_credentials.passwordHash` accepts bcrypt hashes (`$2a$`, `$2b$`, `$2y$`) as well as argon2id. A successful bcrypt sign-in upgrades the hash to argon2id. Use the offline import command for system-table imports.

See the [auth API contract](/CONTRACT#_1-5-auth-configuration-and-services) and [auth endpoints](/PROTOCOL#auth) for full signatures and wire formats.
