//! Listening: plain HTTP, HTTPS with automatic Let's Encrypt certificates (`--https <domain>`), or
//! HTTPS with certificate files (`--tls-cert`/`--tls-key`).
//!
//! **`--https example.com[,www.example.com]`**: HTTPS on `--https-addr` (default `0.0.0.0:443`) with certificates
//! from Let's Encrypt, obtained and renewed by `rustls-acme` with the TLS-ALPN-01 challenge (on the HTTPS port
//! itself, so port 443 must be reachable from the internet and DNS must point at this host). Certificates and the
//! ACME account key are cached in `<data dir>/acme` (mode 0700; `--acme-cache` to change). `--acme-email` sets the
//! account's contact (expiry notices), `--acme-staging` uses Let's Encrypt's staging directory (untrusted
//! certificates, generous rate limits: use it to try things out). A plain-HTTP listener on `--http-redirect`
//! (default `0.0.0.0:80`; `off` disables it) answers every request with a `308` to the same path on
//! `https://<domain>`.
//!
//! **`--tls-cert cert.pem --tls-key key.pem`**: HTTPS on `--https-addr` with the given PEM certificate chain and
//! private key (PKCS#8, PKCS#1 or SEC1). Restart to pick up renewed files. No redirect listener unless
//! `--http-redirect <addr>` is given.
//!
//! Without either, the server listens on `--http` (plain HTTP), e.g. behind a reverse proxy that terminates TLS.

use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use axum::Router;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Redirect};
use futures::StreamExt;
use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::sync::watch;

/// The stop signal shared by a server's tasks.
type Stop = Arc<watch::Sender<bool>>;

async fn stopped(stop: &Stop) {
    let mut rx = stop.subscribe();
    let _ = rx.wait_for(|s| *s).await;
}

pub use rustls_acme::acme::{LETS_ENCRYPT_PRODUCTION_DIRECTORY, LETS_ENCRYPT_STAGING_DIRECTORY};

/// Automatic certificates from Let's Encrypt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcmeSettings {
    /// The domains to get a certificate for (the first is the canonical one for redirects).
    pub domains: Vec<String>,
    /// Contact emails of the ACME account.
    pub emails: Vec<String>,
    /// Use Let's Encrypt's staging directory.
    pub staging: bool,
    /// Where certificates and the account key are cached.
    pub cache_dir: PathBuf,
}

impl AcmeSettings {
    /// The ACME directory URL.
    pub fn directory_url(&self) -> &'static str {
        if self.staging { LETS_ENCRYPT_STAGING_DIRECTORY } else { LETS_ENCRYPT_PRODUCTION_DIRECTORY }
    }

    /// Check the domains and create the cache directory (mode 0700: it holds private keys).
    pub fn prepare(&self) -> crate::Result<()> {
        if self.domains.is_empty() {
            bail!("--https needs a domain name");
        }
        for d in &self.domains {
            let ok = !d.is_empty()
                && d.len() <= 253
                && d.contains('.')
                && d.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
                && d.parse::<std::net::IpAddr>().is_err();
            if !ok {
                bail!("--https: `{d}` is not a domain name (Let's Encrypt issues certificates for public DNS names only)");
            }
        }
        std::fs::create_dir_all(&self.cache_dir).with_context(|| format!("creating {}", self.cache_dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.cache_dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// The `rustls-acme` configuration.
    pub fn config(&self) -> rustls_acme::AcmeConfig<std::io::Error> {
        rustls_acme::AcmeConfig::new(self.domains.iter())
            .contact(self.emails.iter().map(|e| format!("mailto:{e}")))
            .cache(rustls_acme::caches::DirCache::new(self.cache_dir.clone()))
            .directory(self.directory_url())
    }
}

/// How the server listens.
#[derive(Debug, Clone)]
pub enum Listen {
    /// Plain HTTP.
    Http(SocketAddr),
    /// HTTPS with Let's Encrypt certificates, plus an optional redirect listener.
    Acme { https: SocketAddr, redirect: Option<SocketAddr>, acme: AcmeSettings },
    /// HTTPS with certificate files, plus an optional redirect listener.
    CertFiles { https: SocketAddr, redirect: Option<SocketAddr>, cert: PathBuf, key: PathBuf },
}

/// A rustls server configuration from PEM files (aws-lc-rs).
pub fn server_config_from_pem(cert: &Path, key: &Path) -> crate::Result<Arc<ServerConfig>> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("reading {}", cert.display()))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("parsing {}", cert.display()))?;
    if certs.is_empty() {
        bail!("{} contains no PEM certificate", cert.display());
    }
    let key = PrivateKeyDer::from_pem_file(key).with_context(|| format!("reading a private key from {}", key.display()))?;
    let config = ServerConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("the TLS certificate and key don't match or aren't supported")?;
    Ok(Arc::new(config))
}

/// A host name from a `Host` header (port removed), if it only has host name characters.
fn host_of(headers: &HeaderMap) -> Option<String> {
    let host = headers.get(axum::http::header::HOST)?.to_str().ok()?;
    let name = match host.strip_prefix('[') {
        Some(v6) => format!("[{}]", v6.split(']').next()?),
        None => host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host).to_string(),
    };
    (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '[' | ']' | ':')))
        .then_some(name)
}

/// The plain-HTTP router that redirects everything to HTTPS (308, same path and query). With `domains`, the target
/// host is the requested one if it is in the list, else the first; without, the requested host.
pub fn redirect_router(https_port: u16, domains: Vec<String>) -> Router {
    let domains = Arc::new(domains);
    Router::new().fallback(move |headers: HeaderMap, uri: Uri| {
        let domains = domains.clone();
        async move {
            let requested = host_of(&headers);
            let host = match (domains.first(), requested) {
                (Some(first), Some(h)) => {
                    if domains.iter().any(|d| d.eq_ignore_ascii_case(&h)) { h } else { first.clone() }
                }
                (Some(first), None) => first.clone(),
                (None, Some(h)) => h,
                (None, None) => return (axum::http::StatusCode::BAD_REQUEST, "missing Host header").into_response(),
            };
            let port = if https_port == 443 { String::new() } else { format!(":{https_port}") };
            let path = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
            Redirect::permanent(&format!("https://{host}{port}{path}")).into_response()
        }
    })
}

/// A started server: its addresses, and a way to stop it.
pub struct RunningServer {
    /// The main listener (HTTP or HTTPS).
    pub addr: SocketAddr,
    pub https: bool,
    /// The HTTP→HTTPS redirect listener.
    pub redirect_addr: Option<SocketAddr>,
    stop: Stop,
    tasks: Vec<tokio::task::JoinHandle<crate::Result<()>>>,
}

impl RunningServer {
    /// Stop accepting connections and let open requests finish (up to 10 s for HTTPS).
    pub fn shutdown(&self) {
        self.stop.send_replace(true);
    }

    /// Wait until the server stopped.
    pub async fn wait(self) -> crate::Result<()> {
        for task in self.tasks {
            task.await??;
        }
        Ok(())
    }

    /// Serve until `signal` resolves, then shut down gracefully.
    pub async fn run_until(self, signal: impl Future<Output = ()>) -> crate::Result<()> {
        let stop = self.stop.clone();
        let tasks = self.tasks;
        let mut all = futures::future::try_join_all(tasks.into_iter().map(|t| async move { t.await? }));
        tokio::select! {
            r = &mut all => { r?; return Ok(()); }
            _ = signal => {}
        }
        stop.send_replace(true);
        all.await?;
        Ok(())
    }
}

/// Resolves on Ctrl-C (SIGINT) or, on Unix, SIGTERM (what systemd and Docker send to stop a service).
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn std_listener(addr: SocketAddr) -> crate::Result<std::net::TcpListener> {
    let listener = std::net::TcpListener::bind(addr).with_context(|| format!("listening on {addr}"))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn spawn_plain(listener: tokio::net::TcpListener, router: Router, stop: Stop) -> tokio::task::JoinHandle<crate::Result<()>> {
    tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(async move { stopped(&stop).await })
            .await?;
        Ok(())
    })
}

/// Bind every listener of `listen` and start serving `router`.
pub async fn start(router: Router, listen: &Listen) -> crate::Result<RunningServer> {
    let stop = Arc::new(watch::channel(false).0);
    match listen {
        Listen::Http(addr) => {
            let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("listening on {addr}"))?;
            let addr = listener.local_addr()?;
            let task = spawn_plain(listener, router, stop.clone());
            Ok(RunningServer { addr, https: false, redirect_addr: None, stop, tasks: vec![task] })
        }
        Listen::Acme { https, redirect, acme } => {
            acme.prepare()?;
            let mut state = acme.config().state();
            let acceptor = state.axum_acceptor(state.default_rustls_config());
            tokio::spawn(async move {
                while let Some(event) = state.next().await {
                    match event {
                        Ok(ok) => tracing::info!("acme: {ok:?}"),
                        Err(err) => tracing::warn!("acme: {err:?}"),
                    }
                }
            });
            let listener = std_listener(*https)?;
            let addr = listener.local_addr()?;
            let handle = axum_server::Handle::new();
            let serve = axum_server::from_tcp(listener)?
                .acceptor(acceptor)
                .handle(handle.clone())
                .serve(router.into_make_service_with_connect_info::<SocketAddr>());
            start_tls(serve, handle, addr, *redirect, acme.domains.clone(), stop).await
        }
        Listen::CertFiles { https, redirect, cert, key } => {
            let config = server_config_from_pem(cert, key)?;
            let listener = std_listener(*https)?;
            let addr = listener.local_addr()?;
            let rustls = axum_server::tls_rustls::RustlsConfig::from_config(config);
            let handle = axum_server::Handle::new();
            let serve = axum_server::from_tcp(listener)?
                .acceptor(axum_server::tls_rustls::RustlsAcceptor::new(rustls))
                .handle(handle.clone())
                .serve(router.into_make_service_with_connect_info::<SocketAddr>());
            start_tls(serve, handle, addr, *redirect, Vec::new(), stop).await
        }
    }
}

/// Spawn an HTTPS server's future and its redirect listener; a stop signal shuts it down gracefully.
async fn start_tls(
    serve: impl Future<Output = std::io::Result<()>> + Send + 'static,
    handle: axum_server::Handle<SocketAddr>,
    addr: SocketAddr,
    redirect: Option<SocketAddr>,
    domains: Vec<String>,
    stop: Stop,
) -> crate::Result<RunningServer> {
    let mut tasks = Vec::new();
    let redirect_addr = match redirect {
        Some(r) => {
            let listener = tokio::net::TcpListener::bind(r).await.with_context(|| format!("listening on {r}"))?;
            let bound = listener.local_addr()?;
            tasks.push(spawn_plain(listener, redirect_router(addr.port(), domains), stop.clone()));
            Some(bound)
        }
        None => None,
    };
    let s2 = stop.clone();
    tokio::spawn(async move {
        stopped(&s2).await;
        handle.graceful_shutdown(Some(Duration::from_secs(10)));
    });
    tasks.push(tokio::spawn(async move {
        serve.await?;
        Ok(())
    }));
    Ok(RunningServer { addr, https: true, redirect_addr, stop, tasks })
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acme_settings() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = AcmeSettings {
            domains: vec!["example.com".into(), "www.example.com".into()],
            emails: vec!["ops@example.com".into()],
            staging: true,
            cache_dir: dir.path().join("acme"),
        };
        assert_eq!(s.directory_url(), LETS_ENCRYPT_STAGING_DIRECTORY);
        s.prepare().unwrap();
        assert!(s.cache_dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&s.cache_dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let _config = s.config(); // builds offline
        s.staging = false;
        assert_eq!(s.directory_url(), LETS_ENCRYPT_PRODUCTION_DIRECTORY);
        for bad in ["localhost", "127.0.0.1", "", "exa mple.com", "-"] {
            s.domains = vec![bad.into()];
            assert!(s.prepare().is_err(), "{bad}");
        }
    }

    #[test]
    fn hosts() {
        let mut h = HeaderMap::new();
        h.insert(axum::http::header::HOST, "example.com:8080".parse().unwrap());
        assert_eq!(host_of(&h).as_deref(), Some("example.com"));
        h.insert(axum::http::header::HOST, "[::1]:80".parse().unwrap());
        assert_eq!(host_of(&h).as_deref(), Some("[::1]"));
        h.insert(axum::http::header::HOST, "evil.com/x".parse().unwrap());
        assert_eq!(host_of(&h), None);
    }
}
