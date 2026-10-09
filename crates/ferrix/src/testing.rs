//! Test helpers for applications: an [`App`] on a temporary directory, with typed calls.
//!
//! ```ignore
//! let t = TestApp::new().await?;
//! let id = t.mutation::<add>(("hi".to_string(),)).await?;
//! let todos = t.query::<list>((None,)).await?;
//! t.advance_time(Duration::from_secs(60)).await?;   // runs the jobs and crons due within the next minute
//! ```
//!
//! The scheduler does not run in the background in a `TestApp` (unless [`TestApp::start_scheduler`] is called):
//! scheduled jobs run when the test calls [`TestApp::run_due_jobs`] or [`TestApp::advance_time`], so tests are
//! deterministic.

use std::net::SocketAddr;
use std::time::Duration;

use crate::runtime::{App, AppOptions, Identity};
use crate::{ActionFunction, MutationFunction, QueryFunction};

/// A fresh app on a temp dir (deleted on drop). Runs in dev mode so errors carry their real messages. Typed calls
/// may target internal functions too.
pub struct TestApp {
    app: App,
    dir: tempfile::TempDir,
    options: AppOptions,
}

impl TestApp {
    pub async fn new() -> crate::Result<TestApp> {
        Self::with_options(AppOptions { dev: true, ..Default::default() }).await
    }

    pub async fn with_options(options: AppOptions) -> crate::Result<TestApp> {
        let dir = tempfile::tempdir()?;
        let app = App::open(dir.path(), options.clone()).await?;
        // The scheduler's startup work (crons are reconciled into `_crons`), without its background loop.
        app.init_scheduler().await?;
        Ok(TestApp { app, dir, options })
    }

    /// Simulate a restart: close this app and open a new one on the same data directory with the same options (the
    /// test clock keeps its offset). Like a real restart, actions left in progress are marked failed and crons are
    /// reconciled. Anything still holding the old `App` (e.g. a server from [`TestApp::serve`]) keeps it alive.
    pub async fn restart(self) -> crate::Result<TestApp> {
        let options = self.options.clone();
        self.restart_with(options).await
    }

    /// [`TestApp::restart`] with different options, e.g. other crons (as after a redeploy).
    pub async fn restart_with(self, options: AppOptions) -> crate::Result<TestApp> {
        let TestApp { app, dir, .. } = self;
        let offset = app.db().clock().offset();
        drop(app);
        let app = App::open(dir.path(), options.clone()).await?;
        app.db().clock().advance(offset);
        app.init_scheduler().await?;
        Ok(TestApp { app, dir, options })
    }

    /// Run the scheduled jobs and crons that are due now, until none are (see [`App::run_due_jobs`]). Returns how
    /// many jobs ran.
    pub async fn run_due_jobs(&self) -> crate::Result<usize> {
        self.app.run_due_jobs().await
    }

    /// Move the app's clock forward by `by` (as seen by `ctx.now()` in mutations, `run_after` and the scheduler),
    /// then run whatever became due. Returns how many jobs ran.
    pub async fn advance_time(&self, by: Duration) -> crate::Result<usize> {
        self.app.db().clock().advance(by);
        self.app.scheduler_state().notify();
        self.run_due_jobs().await
    }

    /// Run the scheduler in the background, as `ferrix serve` does (e.g. for tests over a real socket).
    pub async fn start_scheduler(&self) -> crate::Result<()> {
        self.app.start_scheduler().await
    }

    /// A dev-mode app with this auth configuration.
    pub async fn with_auth(auth: crate::auth::AuthConfig) -> crate::Result<TestApp> {
        Self::with_options(AppOptions { dev: true, auth, ..Default::default() }).await
    }

    /// Run a query as `identity` (e.g. `Identity::user(id, None)`).
    pub async fn query_as<F: QueryFunction>(&self, identity: &Identity, args: impl Into<F::Args>) -> crate::Result<F::Output> {
        self.app.typed_query::<F>(args.into(), identity).await
    }

    /// Run a mutation as `identity`.
    pub async fn mutation_as<F: MutationFunction>(
        &self,
        identity: &Identity,
        args: impl Into<F::Args>,
    ) -> crate::Result<F::Output> {
        Ok(self.app.typed_mutation::<F>(args.into(), identity).await?.0)
    }

    /// Run an action as `identity`.
    pub async fn action_as<F: ActionFunction>(&self, identity: &Identity, args: impl Into<F::Args>) -> crate::Result<F::Output> {
        self.app.typed_action::<F>(args.into(), identity).await
    }

    /// Create a user in `_users` (email verified), with a password credential when `password` is given (so the
    /// user can also sign in with it), without going through sign-up: no auth method needs to be configured and no
    /// email is sent. Errors if the email is taken. Call functions as the user with [`TestApp::as_user`].
    pub async fn create_user(&self, email: &str, password: Option<&str>) -> crate::Result<crate::Id<crate::auth::User>> {
        use crate::auth::store;
        let email = store::normalize_email(email)?;
        let user = match password {
            Some(password) => {
                let hash = crate::auth::password::hash(password.to_string()).await?;
                let user = store::create_password_user(&self.app, email, hash, None).await??;
                store::mark_email_verified(&self.app, user).await?;
                user
            }
            None => {
                let lookup = email.clone();
                if store::read(&self.app, |db| async move { store::user_by_email(&db, &lookup).await }).await?.is_some() {
                    return Err(crate::UserError::new("email_in_use", "An account with this email already exists").into());
                }
                store::passwordless_user(&self.app, email, true).await??
            }
        };
        Ok(user)
    }

    /// Typed calls as `user` (`Identity::user(user, None)`): `t.as_user(ann).mutation::<send>((..)).await`.
    pub fn as_user(&self, user: crate::Id<crate::auth::User>) -> TestUser<'_> {
        TestUser { app: self, identity: Identity::user(user, None) }
    }

    pub fn app(&self) -> &App {
        &self.app
    }

    pub fn dir(&self) -> &std::path::Path {
        self.dir.path()
    }

    pub async fn query<F: QueryFunction>(&self, args: impl Into<F::Args>) -> crate::Result<F::Output> {
        self.app.query::<F>(args).await
    }

    pub async fn mutation<F: MutationFunction>(&self, args: impl Into<F::Args>) -> crate::Result<F::Output> {
        self.app.mutation::<F>(args).await
    }

    /// Like [`TestApp::mutation`], also returning the commit ts.
    pub async fn mutation_ts<F: MutationFunction>(&self, args: impl Into<F::Args>) -> crate::Result<(F::Output, u64)> {
        self.app.typed_mutation::<F>(args.into(), &Identity::default()).await
    }

    pub async fn action<F: ActionFunction>(&self, args: impl Into<F::Args>) -> crate::Result<F::Output> {
        self.app.action::<F>(args).await
    }

    /// Serve the app on `127.0.0.1:<random port>` in a background task; returns the address.
    pub async fn serve(&self) -> crate::Result<SocketAddr> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let app = self.app.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::server::serve(listener, app).await {
                tracing::error!("test server failed: {e}");
            }
        });
        Ok(addr)
    }
}

/// Typed calls as one user; see [`TestApp::as_user`].
pub struct TestUser<'a> {
    app: &'a TestApp,
    identity: Identity,
}

impl TestUser<'_> {
    /// The identity the calls run as.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub async fn query<F: QueryFunction>(&self, args: impl Into<F::Args>) -> crate::Result<F::Output> {
        self.app.query_as::<F>(&self.identity, args).await
    }

    pub async fn mutation<F: MutationFunction>(&self, args: impl Into<F::Args>) -> crate::Result<F::Output> {
        self.app.mutation_as::<F>(&self.identity, args).await
    }

    pub async fn action<F: ActionFunction>(&self, args: impl Into<F::Args>) -> crate::Result<F::Output> {
        self.app.action_as::<F>(&self.identity, args).await
    }
}
