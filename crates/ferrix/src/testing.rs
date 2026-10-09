//! Test helpers for applications: an [`App`] on a temporary directory, with typed calls.
//!
//! ```ignore
//! let t = TestApp::new().await?;
//! let id = t.mutation::<add>(("hi".to_string(),)).await?;
//! let todos = t.query::<list>((None,)).await?;
//! ```

use std::net::SocketAddr;

use crate::runtime::{App, AppOptions, Identity};
use crate::{ActionFunction, MutationFunction, QueryFunction};

/// A fresh app on a temp dir (deleted on drop). Runs in dev mode so errors carry their real messages. Typed calls
/// may target internal functions too.
pub struct TestApp {
    app: App,
    dir: tempfile::TempDir,
}

impl TestApp {
    pub async fn new() -> crate::Result<TestApp> {
        Self::with_options(AppOptions { dev: true, ..Default::default() }).await
    }

    pub async fn with_options(options: AppOptions) -> crate::Result<TestApp> {
        let dir = tempfile::tempdir()?;
        let app = App::open(dir.path(), options).await?;
        Ok(TestApp { app, dir })
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
