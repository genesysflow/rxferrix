# Auth

Enable sign-in methods in `main`, then use `ctx.auth` in functions:

```rust
use ferrix::auth::{AuthConfig, GitHub, User};
use ferrix::prelude::*;

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

Access tokens are HS256 JWTs (15 minutes by default) signed with a key derived from `<data dir>/auth_secret.key`;
refresh tokens rotate on every use. Signing out, revoking a session from the dashboard, deleting a user or reusing a
rotated refresh token (past a 10-second grace, `refresh_reuse_grace`) ends the sockets of that session **at once**
(`AuthError`, then the client continues signed out), not when the access token expires. Email goes through
`--smtp-url` (without it, printed to the log in `--dev`). With no admin yet, `serve` prints a
one-time `/_/setup#token=…` link; `admin create <email>` works too.

OAuth presets: `GitHub`, `Google`, `Discord` (only a Discord-verified email is used) and `Apple` (Sign in with Apple),
plus `Oidc` for any OpenID Connect issuer; each has `from_env()` (`FERRIX_AUTH_<PROVIDER>_CLIENT_ID`, …;
`Oidc::from_env(id)` also reads `FERRIX_AUTH_<ID>_ISSUER`) and can also
be set up in the dashboard's Settings. For Apple, give the Services ID, team id, key id and the `.p8` private key
(`FERRIX_AUTH_APPLE_PRIVATE_KEY` or `_PRIVATE_KEY_FILE`): ferrix signs and rotates the short-lived client secret
itself, accepts Apple's `form_post` callback (register `https://<public url>/api/auth/oauth/apple/callback`; the
public URL must be https), verifies the ID token against Apple's keys, and keeps the name Apple sends on the first
sign-in only. The key is a secret: keep it out of source control (the dashboard stores it encrypted).

**Clerk, Auth0, Supabase or any OIDC provider** can sign users in instead (or as well): ferrix verifies their JWTs
against the provider's JWKS (RS256, ES256, EdDSA; `iss`, `aud`, `exp`, `nbf`) and creates a `_users` row for each new
subject, so `ctx.auth.user_id()` and `Id<User>` work the same for every sign-in method:

```rust
.auth(AuthConfig::default()
    .jwt(JwtProvider::new("https://clerk.example.com").any_audience())          // JWKS found by OIDC discovery
    .jwt(JwtProvider::new("https://tenant.auth0.com/").audience("https://api.example.com")))
```

```ts
// Hand the provider's token getter to the client; it refreshes before `exp` and after an AuthError.
client.setAuth(({ forceRefresh }) => clerk.session!.getToken({ skipCache: forceRefresh }));
```

**Runtime settings.** Administrators can change OAuth providers, SMTP, the redirect allowlist, sign-ups, token
lifetimes and CORS origins without a restart (`_system/settings:*`). Secrets are encrypted at rest with a key derived
from `auth_secret.key` and never shown again. Whatever the code, the command line or the environment configures wins
over these settings. Every administrator write is recorded in an audit log (`_audit_log`), and admins can act as a
user through a short-lived, audited session. Details: [docs/PROTOCOL.md](../PROTOCOL.md) ("Auth") and
[docs/CONTRACT.md](../CONTRACT.md) §1.2, §1.5, §4.1.4, §4.1.5.
