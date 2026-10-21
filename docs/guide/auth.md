# Auth

Enable sign-in methods in `main`, then use `ctx.auth` in functions:

```rust
use rxferrix::auth::{AuthConfig, GitHub, User};
use rxferrix::prelude::*;

fn main() -> rxferrix::Result<()> {
    rxferrix::RxFerrix::new()
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
automatically). React has `useRxFerrixAuth()`, `useAuthActions()` and `useCurrentUser()`; Svelte has `useAuth()`
and `useCurrentUser()`:

```ts
const auth = getAuth(client);
await auth.signIn({ email, password });           // or signUp, requestMagicLink + verifyCode, signInWithOAuth("github")
await auth.handleRedirectCallback();              // on the page OAuth and email links return to
await auth.changePassword(current, newPassword);   // signed in; ends the user's other sessions
```

Access tokens are HS256 JWTs (15 minutes by default) signed with a key derived from `<data dir>/auth_secret.key`;
refresh tokens rotate on every use. Signing out, revoking a session from the dashboard, deleting a user or reusing a
rotated refresh token (past a 10-second grace, `refresh_reuse_grace`) ends the sockets of that session **at once**
(`AuthError`, then the client continues signed out), not when the access token expires. Email goes through
`--smtp-url` (without it, printed to the log in `--dev`). With no admin yet, `serve` prints a
one-time `/_/setup#token=…` link; `admin create <email>` works too.

OAuth presets: `GitHub`, `Google`, `Discord` and `Apple` (Sign in with Apple), plus `Oidc` for any OpenID Connect
issuer; an email the provider hasn't verified is never stored or used to link accounts; each has `from_env()` (`RXFERRIX_AUTH_<PROVIDER>_CLIENT_ID`, …;
`Oidc::from_env(id)` also reads `RXFERRIX_AUTH_<ID>_ISSUER`) and can also
be set up in the dashboard's Settings. For Apple, give the Services ID, team id, key id and the `.p8` private key
(`RXFERRIX_AUTH_APPLE_PRIVATE_KEY` or `_PRIVATE_KEY_FILE`): rxferrix signs and rotates the short-lived client secret
itself, accepts Apple's `form_post` callback (register `https://<public url>/api/auth/oauth/apple/callback`; the
public URL must be https), verifies the ID token against Apple's keys, and keeps the name Apple sends on the first
sign-in only. The key is a secret: keep it out of source control (the dashboard stores it encrypted).

**Two-factor authentication.** Any user (and any administrator) can add an authenticator app (TOTP: Google
Authenticator, 1Password, Authy, …) to their account; from then on every sign-in method stops at a second step until
a code (or one of the 8 recovery codes shown at setup) is entered. Nothing to configure on the server (optionally
`.totp_issuer("My App")`, the name the authenticator shows; the public URL's host by default):

```ts
// Enrolling (signed in; accounts with a password confirm it):
const { qrSvg, secret } = await auth.setupTotp({ password });   // show the QR code (an <svg>) or the secret
const { recoveryCodes } = await auth.confirmTotp(firstCode);     // show the codes once; auth.getMfaStatus() from then on
// Signing in:
const r = await auth.signIn({ email, password });                 // also verifyCode, resetPassword, handleRedirectCallback
if (r.mfaRequired) await auth.verifyMfa(r.mfaToken, codeFromApp); // or a recovery code
// Later: auth.disableMfa(code), auth.regenerateRecoveryCodes(code)
```

Five wrong codes lock the factor for five minutes, a code works once, and the secret is stored encrypted with the
server's key. A user who lost both the app and the recovery codes is rescued from the dashboard's Users page
("Reset 2FA"); an administrator with `admin disable-2fa <email>`. Administrators turn on their own second factor on
the dashboard's Account page.

**Clerk, Auth0, Supabase or any OIDC provider** can sign users in instead (or as well): rxferrix verifies their JWTs
against the provider's JWKS (RS256, ES256, EdDSA; `iss`, `aud`, `exp`, `nbf`) and creates a `_users` row for each new
subject, so `ctx.auth.user_id()` and `Id<User>` work the same for every sign-in method:

```rust
.auth(AuthConfig::default()
    .jwt(JwtProvider::new("https://clerk.example.com").any_audience())          // JWKS found by OIDC discovery
    .jwt(JwtProvider::new("https://tenant.auth0.com/").audience("https://api.example.com")))
```

```ts
// Hand the provider's token getter to the client; it refreshes before `exp` and after an AuthError. A getter that
// throws (offline at page load) is retried with backoff; the user stays signed in meanwhile.
client.setAuth(({ forceRefresh }) => clerk.session!.getToken({ skipCache: forceRefresh }));
```

**Runtime settings.** Administrators can change OAuth providers, SMTP, the redirect allowlist, sign-ups, token
lifetimes and CORS origins without a restart (`_system/settings:*`). Secrets are encrypted at rest with a key derived
from `auth_secret.key` and never shown again. Whatever the code, the command line or the environment configures wins
over these settings. Every administrator write is recorded in an audit log (`_audit_log`), and admins can act as a
user through a short-lived, audited session. Details: [docs/PROTOCOL.md](../PROTOCOL.md) ("Auth") and
[docs/CONTRACT.md](../CONTRACT.md) §1.2, §1.5, §4.1.4, §4.1.5.

**Users from another system.** Import `_users` and `_credentials` with `<app> import --mode insert` (system tables
can be imported from the command line). Besides argon2id, `_credentials.passwordHash` may hold a bcrypt hash
(`$2a$`, `$2b$`, `$2y$`: PocketBase, Laravel, Devise, Supabase, ...): those users sign in with their old password,
and the hash is replaced with an argon2id one on that sign-in.

**Email templates.** The verification, password reset and sign-in emails are plain text by default. In the
dashboard's **Settings → Email templates**, give any of them a subject and an HTML body (with a live preview); a
plain-text version is made from the HTML. Placeholders: `{link}` (the link back to your app; empty without a redirect
URL), `{code}`, `{expires}` (e.g. `15 minutes`), `{app_name}` (set there, else the display name of the SMTP sender) and
`{email}`. Values are HTML-escaped in the body, and a template must contain `{code}` or `{link}`.

**Sending your own email.** Actions send through the same mailer as the auth emails (`--smtp-url`, `AuthConfig::mailer`
or SMTP from the dashboard's settings, and the dev log mailer in `--dev`):

```rust
use rxferrix::auth::Email;

#[action]
pub async fn send_confirmation(ctx: &ActionCtx, to: String, text: String, html: String) -> Result<()> {
    ctx.send_email(Email::new(to, "Your booking", text).html(html)).await   // `email_not_configured` without a mailer
}
```

From a mutation, schedule the action (`ctx.scheduler.run_after::<send_confirmation>(Duration::ZERO, ..)`), so the
email goes out only if the mutation commits; a workpool adds retries.

**Reacting to sign-ins.** `AuthConfig::on_sign_in` runs after each successful user sign-in that starts a session
(password, sign-up, password reset, magic link, OAuth), e.g. to keep a login history:

```rust
AuthConfig::password().on_sign_in(|app, sign_in| async move {
    // sign_in.user, sign_in.session, sign_in.method ("password", "oauth:google", ...), sign_in.client (ip, user agent)
    if let Err(e) = app.mutation::<audit::record_sign_in>((sign_in.user, sign_in.method.to_string())).await {
        tracing::warn!("recording a sign-in failed: {e:#}");
    }
})
```

The user is already signed in when it runs, and the response waits for it: keep it short.

