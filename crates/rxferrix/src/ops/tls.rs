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
//! private key (PKCS#8, PKCS#1 or SEC1). No redirect listener unless `--http-redirect <addr>` is given. The files are
//! re-read every `--tls-reload-every` (default 30 s; `0` turns it off): when their content changed and they make a
//! valid certificate and key pair, new connections get the new certificate (the rustls configuration is swapped
//! atomically; open connections keep theirs and are not dropped). If the new files don't load (a half-written
//! renewal, a key that doesn't match), the current certificate stays and an error is logged once per change, so a
//! renewal tool that writes the two files one after the other is picked up as soon as both are in place.
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
                && d.split('.').all(|l| {
                    !l.is_empty() && l.len() <= 63 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                })
                && d.parse::<std::net::IpAddr>().is_err();
            if !ok {
                bail!(
                    "--https: `{d}` is not a domain name (Let's Encrypt issues certificates for public DNS names only)"
                );
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

impl Listen {
    /// The addresses it listens on, by role (`main`, `redirect`).
    pub fn addrs(&self) -> Vec<(String, SocketAddr)> {
        match self {
            Listen::Http(addr) => vec![("main".to_string(), *addr)],
            Listen::Acme { https, redirect, .. } | Listen::CertFiles { https, redirect, .. } => {
                let mut out = vec![("main".to_string(), *https)];
                out.extend(redirect.map(|r| ("redirect".to_string(), r)));
                out
            }
        }
    }
}

/// How the server listens.
#[derive(Debug, Clone)]
pub enum Listen {
    /// Plain HTTP.
    Http(SocketAddr),
    /// HTTPS with Let's Encrypt certificates, plus an optional redirect listener.
    Acme { https: SocketAddr, redirect: Option<SocketAddr>, acme: AcmeSettings },
    /// HTTPS with certificate files, plus an optional redirect listener. With `reload_every`, the files are checked
    /// that often and a changed, valid pair replaces the certificate without a restart.
    CertFiles {
        https: SocketAddr,
        redirect: Option<SocketAddr>,
        cert: PathBuf,
        key: PathBuf,
        reload_every: Option<Duration>,
    },
}

impl Listen {
    /// Whether this serves HTTPS.
    pub fn is_tls(&self) -> bool {
        !matches!(self, Listen::Http(_))
    }
}

/// A digest of the certificate and key files' contents.
fn files_digest(cert: &Path, key: &Path) -> std::io::Result<[u8; 32]> {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    for path in [cert, key] {
        let bytes = std::fs::read(path)?;
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    Ok(h.finalize().into())
}

/// Re-read `cert` and `key` every `every` until `stop`; swap in a changed, valid pair (see the module docs).
fn spawn_cert_reloader(
    rustls: axum_server::tls_rustls::RustlsConfig,
    cert: PathBuf,
    key: PathBuf,
    every: Duration,
    stop: Stop,
) {
    tokio::spawn(async move {
        // What is loaded now, and the last content that failed (to log each bad change once).
        let mut loaded = files_digest(&cert, &key).ok();
        // `Some(None)`: the files couldn't be read.
        let mut failed: Option<Option<[u8; 32]>> = None;
        let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = stopped(&stop) => return,
                _ = ticks.tick() => {}
            }
            let (c, k) = (cert.clone(), key.clone());
            let attempt =
                tokio::task::spawn_blocking(move || -> (Option<[u8; 32]>, crate::Result<Arc<ServerConfig>>) {
                    match files_digest(&c, &k) {
                        Ok(d) => (Some(d), server_config_from_pem(&c, &k)),
                        Err(e) => (None, Err(anyhow::Error::from(e).context("reading the certificate files"))),
                    }
                })
                .await;
            let Ok((digest, config)) = attempt else { continue };
            if digest.is_some() && digest == loaded {
                failed = None;
                continue;
            }
            match config {
                Ok(config) => {
                    rustls.reload_from_config(config);
                    loaded = digest;
                    failed = None;
                    tracing::info!(cert = %cert.display(), "tls: loaded the new certificate");
                }
                Err(e) => {
                    if failed != Some(digest) {
                        tracing::error!(
                            cert = %cert.display(),
                            "tls: the changed certificate files don't load; keeping the current certificate: {e:#}"
                        );
                    }
                    failed = Some(digest);
                }
            }
        }
    });
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
    let key =
        PrivateKeyDer::from_pem_file(key).with_context(|| format!("reading a private key from {}", key.display()))?;
    let config = ServerConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("the TLS certificate and key don't match or aren't supported")?;
    Ok(Arc::new(config))
}

/// `Strict-Transport-Security` on every response that doesn't set its own: for a year, so browsers go straight to
/// HTTPS (`includeSubDomains` is left to the operator: other hosts of the domain may not serve HTTPS).
pub fn with_hsts(router: Router) -> Router {
    router.layer(axum::middleware::map_response(|mut res: axum::response::Response| async move {
        res.headers_mut()
            .entry(axum::http::header::STRICT_TRANSPORT_SECURITY)
            .or_insert(axum::http::HeaderValue::from_static("max-age=31536000"));
        res
    }))
}

/// The DNS names of a PEM certificate file's leaf certificate (its subject alternative names, lowercase), for the
/// HTTP→HTTPS redirect; empty when the file can't be read or has none.
fn cert_dns_names(cert: &Path) -> Vec<String> {
    let Some(Ok(leaf)) = CertificateDer::pem_file_iter(cert).ok().and_then(|mut certs| certs.next()) else {
        return Vec::new();
    };
    let Ok(leaf) = webpki::EndEntityCert::try_from(&leaf) else { return Vec::new() };
    leaf.valid_dns_names().map(str::to_ascii_lowercase).collect()
}

/// Whether `host` is `name`, or matches it as a wildcard (`*.example.com` matches `a.example.com`, one label).
fn host_matches(name: &str, host: &str) -> bool {
    match name.strip_prefix("*.") {
        Some(parent) => {
            host.split_once('.').is_some_and(|(label, rest)| !label.is_empty() && rest.eq_ignore_ascii_case(parent))
        }
        None => name.eq_ignore_ascii_case(host),
    }
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

/// The plain-HTTP router that redirects everything to HTTPS (308, same path and query). With `domains` (the
/// `--https` domains, or the certificate's names; `*.` wildcards match one label), the target host is the requested
/// one if it is in the list, else the first name that isn't a wildcard; without, the requested host.
pub fn redirect_router(https_port: u16, domains: Vec<String>) -> Router {
    let domains = Arc::new(domains);
    Router::new().fallback(move |headers: HeaderMap, uri: Uri| {
        let domains = domains.clone();
        async move {
            let requested = host_of(&headers);
            let fallback = domains.iter().find(|d| !d.starts_with("*.")).cloned();
            let host = match requested {
                _ if domains.is_empty() => match requested {
                    Some(h) => h,
                    None => return (axum::http::StatusCode::BAD_REQUEST, "missing Host header").into_response(),
                },
                Some(h) if domains.iter().any(|d| host_matches(d, &h)) => h,
                _ => match fallback {
                    Some(name) => name,
                    None => return (axum::http::StatusCode::BAD_REQUEST, "unknown host").into_response(),
                },
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
    pause: Arc<watch::Sender<Accepting>>,
    tasks: Vec<tokio::task::JoinHandle<crate::Result<()>>>,
    /// The listening sockets by role (`main`, `redirect`), for a takeover (`crate::ops::takeover`).
    #[cfg(unix)]
    fds: Vec<(String, std::os::fd::RawFd)>,
}

impl RunningServer {
    /// The listening sockets by role (`main`, `redirect`): valid while the server runs.
    #[cfg(unix)]
    pub fn listener_fds(&self) -> Vec<(String, std::os::fd::RawFd)> {
        self.fds.clone()
    }

    /// The listening addresses by role (`main`, `redirect`).
    pub fn listener_addrs(&self) -> Vec<(String, SocketAddr)> {
        let mut out = vec![("main".to_string(), self.addr)];
        out.extend(self.redirect_addr.map(|a| ("redirect".to_string(), a)));
        out
    }

    /// A handle that stops the server from accepting connections while it keeps serving the open ones (before a
    /// graceful stop).
    pub fn pauser(&self) -> Pauser {
        Pauser(self.pause.clone())
    }

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

    /// Serve until `signal` resolves, then shut down gracefully: stop accepting, and give open requests up to
    /// [`DRAIN_TIMEOUT`] to finish.
    pub async fn run_until(self, signal: impl Future<Output = ()>) -> crate::Result<()> {
        let stop = self.stop.clone();
        let tasks = self.tasks;
        let mut all = futures::future::try_join_all(tasks.into_iter().map(|t| async move { t.await? }));
        tokio::select! {
            r = &mut all => { r?; return Ok(()); }
            _ = signal => {}
        }
        stop.send_replace(true);
        match tokio::time::timeout(DRAIN_TIMEOUT, &mut all).await {
            Ok(r) => {
                r?;
            }
            Err(_) => tracing::warn!("requests still open after {DRAIN_TIMEOUT:?}; stopping anyway"),
        }
        Ok(())
    }
}

/// How long a graceful shutdown waits for open requests.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

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

/// An `axum_server` acceptor that sets `TCP_NODELAY` on each connection, then hands it to `A` (the TLS handshake):
/// see [`crate::server::nodelay`].
#[derive(Clone)]
struct NoDelay<A>(A);

impl<A, S> axum_server::accept::Accept<tokio::net::TcpStream, S> for NoDelay<A>
where
    A: axum_server::accept::Accept<tokio::net::TcpStream, S>,
{
    type Stream = A::Stream;
    type Service = A::Service;
    type Future = A::Future;

    fn accept(&self, stream: tokio::net::TcpStream, service: S) -> Self::Future {
        if let Err(e) = stream.set_nodelay(true) {
            tracing::debug!("could not set TCP_NODELAY: {e}");
        }
        self.0.accept(stream, service)
    }
}

fn std_listener(addr: SocketAddr) -> crate::Result<std::net::TcpListener> {
    let listener = std::net::TcpListener::bind(addr).with_context(|| format!("listening on {addr}"))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Whether a plain HTTP listener accepts connections ([`Pauser`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Accepting {
    Yes,
    /// Not accepting, the socket left open: during a takeover it is open in the new process too, so connections queue
    /// there instead of reaching a server about to stop.
    Paused,
    /// The socket closed (after accepting what had queued): new connections are refused, which a proxy retries
    /// elsewhere. A connection still queued when the socket closes would be reset instead, after the client sent its
    /// request, which nobody can retry safely.
    Closed,
}

/// A listener that stops accepting on [`Accepting::Paused`] or [`Accepting::Closed`].
struct Pausable<L> {
    inner: Option<L>,
    state: watch::Receiver<Accepting>,
}

impl<L: axum::serve::Listener<Addr = SocketAddr>> axum::serve::Listener for Pausable<L> {
    type Io = L::Io;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        use futures::FutureExt;
        loop {
            let state = *self.state.borrow_and_update();
            match (state, self.inner.as_mut()) {
                (Accepting::Yes, Some(inner)) => tokio::select! {
                    accepted = inner.accept() => return accepted,
                    _ = self.state.changed() => {}
                },
                (Accepting::Closed, Some(inner)) => match inner.accept().now_or_never() {
                    Some(accepted) => return accepted,
                    None => self.inner = None,
                },
                _ => {
                    if self.state.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        match &self.inner {
            Some(inner) => inner.local_addr(),
            None => Err(std::io::Error::other("the listener is closed")),
        }
    }
}

/// Stops a running server from accepting connections ([`RunningServer::pauser`]; plain HTTP listeners).
#[derive(Clone)]
pub struct Pauser(Arc<watch::Sender<Accepting>>);

impl Pauser {
    /// Stop accepting, leaving the socket open (a takeover's new process, which has it too, accepts instead).
    pub fn stop_accepting(&self) {
        self.0.send_replace(Accepting::Paused);
    }

    /// Accept what has queued, then close the socket: from then on connections are refused.
    pub fn close(&self) {
        self.0.send_replace(Accepting::Closed);
    }
}

fn spawn_plain(
    listener: tokio::net::TcpListener,
    router: Router,
    stop: Stop,
    state: watch::Receiver<Accepting>,
) -> tokio::task::JoinHandle<crate::Result<()>> {
    tokio::spawn(async move {
        use axum::serve::ListenerExt;
        fn nodelay(stream: &mut tokio::net::TcpStream) {
            if let Err(e) = stream.set_nodelay(true) {
                tracing::debug!("could not set TCP_NODELAY: {e}");
            }
        }
        let listener = Pausable { inner: Some(listener), state }.tap_io(nodelay as fn(&mut tokio::net::TcpStream));
        axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(async move { stopped(&stop).await })
            .await?;
        Ok(())
    })
}

/// Bind every listener of `listen` and start serving `router`.
pub async fn start(router: Router, listen: &Listen) -> crate::Result<RunningServer> {
    start_with(router, listen, Vec::new()).await
}

/// A listening socket taken over from another process (`crate::ops::takeover`), by role (`main`, `redirect`).
pub type Inherited = Vec<(String, std::net::TcpListener)>;

/// The inherited listener for `role` if it listens on `addr`'s port (else it is dropped and a new one is bound).
fn take_inherited(inherited: &mut Inherited, role: &str, addr: SocketAddr) -> Option<std::net::TcpListener> {
    let i = inherited.iter().position(|(r, _)| r == role)?;
    let (_, listener) = inherited.remove(i);
    let bound = listener.local_addr().ok()?;
    let same = crate::ops::takeover::serves(bound, addr);
    if !same {
        tracing::warn!(%bound, wanted = %addr, "not reusing the inherited {role} listener: the address changed");
        return None;
    }
    listener.set_nonblocking(true).ok()?;
    Some(listener)
}

async fn bind_or_take(
    inherited: &mut Inherited,
    role: &str,
    addr: SocketAddr,
) -> crate::Result<tokio::net::TcpListener> {
    match take_inherited(inherited, role, addr) {
        Some(l) => Ok(tokio::net::TcpListener::from_std(l)?),
        None => Ok(tokio::net::TcpListener::bind(addr).await.with_context(|| format!("listening on {addr}"))?),
    }
}

#[cfg(unix)]
fn fd_of(l: &impl std::os::fd::AsRawFd) -> std::os::fd::RawFd {
    l.as_raw_fd()
}

/// Like [`start`], reusing `inherited` listening sockets where they match (a takeover keeps the same sockets, so no
/// connection is refused while one process hands over to the next).
pub async fn start_with(router: Router, listen: &Listen, mut inherited: Inherited) -> crate::Result<RunningServer> {
    let stop = Arc::new(watch::channel(false).0);
    let pause = Arc::new(watch::channel(Accepting::Yes).0);
    match listen {
        Listen::Http(addr) => {
            let listener = bind_or_take(&mut inherited, "main", *addr).await?;
            let addr = listener.local_addr()?;
            #[cfg(unix)]
            let fds = vec![("main".to_string(), fd_of(&listener))];
            let task = spawn_plain(listener, router, stop.clone(), pause.subscribe());
            Ok(RunningServer {
                addr,
                https: false,
                redirect_addr: None,
                stop,
                pause,
                tasks: vec![task],
                #[cfg(unix)]
                fds,
            })
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
            let listener = match take_inherited(&mut inherited, "main", *https) {
                Some(l) => l,
                None => std_listener(*https)?,
            };
            let addr = listener.local_addr()?;
            #[cfg(unix)]
            let fd = fd_of(&listener);
            #[cfg(not(unix))]
            let fd = ();
            let handle = axum_server::Handle::new();
            let serve = axum_server::from_tcp(listener)?
                .acceptor(NoDelay(acceptor))
                .handle(handle.clone())
                .serve(router.into_make_service_with_connect_info::<SocketAddr>());
            start_tls(serve, handle, addr, *redirect, acme.domains.clone(), stop, pause, fd, inherited).await
        }
        Listen::CertFiles { https, redirect, cert, key, reload_every } => {
            let config = server_config_from_pem(cert, key)?;
            let listener = match take_inherited(&mut inherited, "main", *https) {
                Some(l) => l,
                None => std_listener(*https)?,
            };
            let addr = listener.local_addr()?;
            #[cfg(unix)]
            let fd = fd_of(&listener);
            #[cfg(not(unix))]
            let fd = ();
            let rustls = axum_server::tls_rustls::RustlsConfig::from_config(config);
            if let Some(every) = reload_every.filter(|d| !d.is_zero()) {
                spawn_cert_reloader(rustls.clone(), cert.clone(), key.clone(), every, stop.clone());
            }
            let handle = axum_server::Handle::new();
            let serve = axum_server::from_tcp(listener)?
                .acceptor(NoDelay(axum_server::tls_rustls::RustlsAcceptor::new(rustls)))
                .handle(handle.clone())
                .serve(router.into_make_service_with_connect_info::<SocketAddr>());
            start_tls(serve, handle, addr, *redirect, cert_dns_names(cert), stop, pause, fd, inherited).await
        }
    }
}

/// Spawn an HTTPS server's future and its redirect listener; a stop signal shuts it down gracefully.
#[allow(clippy::too_many_arguments)]
async fn start_tls(
    serve: impl Future<Output = std::io::Result<()>> + Send + 'static,
    handle: axum_server::Handle<SocketAddr>,
    addr: SocketAddr,
    redirect: Option<SocketAddr>,
    domains: Vec<String>,
    stop: Stop,
    pause: Arc<watch::Sender<Accepting>>,
    #[cfg(unix)] main_fd: std::os::fd::RawFd,
    #[cfg(not(unix))] _main_fd: (),
    mut inherited: Inherited,
) -> crate::Result<RunningServer> {
    let mut tasks = Vec::new();
    #[cfg(unix)]
    let mut fds = vec![("main".to_string(), main_fd)];
    let redirect_addr = match redirect {
        Some(r) => {
            let listener = bind_or_take(&mut inherited, "redirect", r).await?;
            #[cfg(unix)]
            fds.push(("redirect".to_string(), fd_of(&listener)));
            let bound = listener.local_addr()?;
            tasks.push(spawn_plain(listener, redirect_router(addr.port(), domains), stop.clone(), pause.subscribe()));
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
    Ok(RunningServer {
        addr,
        https: true,
        redirect_addr,
        stop,
        pause,
        tasks,
        #[cfg(unix)]
        fds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pair of connected streams, and the accepted (server) side.
    async fn accepted() -> (tokio::net::TcpStream, tokio::net::TcpStream) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap());
        let (client, accepted) = tokio::join!(client, listener.accept());
        (client.unwrap(), accepted.unwrap().0)
    }

    #[tokio::test]
    async fn plain_listeners_set_nodelay() {
        use axum::serve::Listener;
        let mut listener = crate::server::nodelay(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap());
        let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap());
        let (client, (stream, _)) = tokio::join!(client, listener.accept());
        client.unwrap();
        assert!(stream.nodelay().unwrap(), "TCP_NODELAY on accepted connections");
    }

    #[tokio::test]
    async fn tls_acceptors_set_nodelay() {
        use axum_server::accept::{Accept, DefaultAcceptor};
        let (_client, stream) = accepted().await;
        assert!(!stream.nodelay().unwrap());
        let (stream, ()) = NoDelay(DefaultAcceptor::new()).accept(stream, ()).await.unwrap();
        assert!(stream.nodelay().unwrap(), "TCP_NODELAY before the inner (TLS) acceptor");
    }

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
