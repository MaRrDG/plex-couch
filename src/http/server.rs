//! Listeners and connection handling (spec 1.3).
//!
//! - At most [`MAX_CONNECTIONS`] connections per listener; one more is closed right away
//!   without a response.
//! - [`CONNECTION_TIMEOUT`] (15 s) for the TLS handshake, for the request headers (the
//!   keep-alive idle time included), for a request body and per blocked write (Python had
//!   one per-socket-operation timeout; see [`TimeoutIo`]).
//! - The TLS handshake runs in the connection task, never in the accept loop.
//! - A handler that panics drops its connection with no response; the panic is logged at
//!   ERROR (`request error from <ip>`). Connection errors are not logged (Python: DEBUG).
//! - HTTP/1.1 keep-alive, title-case header names, `Server: PlexCouch`, a `Date` header.

use crate::app::AppState;
use crate::http::routes;
use crate::{log_debug, log_error};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::Sleep;
use tokio_rustls::rustls;
use tokio_rustls::TlsAcceptor;

pub const MAX_CONNECTIONS: usize = 64;
pub const CONNECTION_TIMEOUT: Duration = Duration::from_secs(15);

/// What a listener serves.
#[derive(Clone, Debug)]
pub enum Role {
    /// The app (`Handler`; `SecureHandler` when `secure`).
    App { secure: bool },
    /// Plain HTTP next to HTTPS: install page, `/ca.crt`, 301 to HTTPS (`InstallHandler`).
    Install { https_port: u16, ca_cert: PathBuf },
    /// No usable certificate and no `--allow-http`: a 503 page only (`NoCertHandler`).
    NoCert,
}

impl Role {
    /// `Handler.secure`: HSTS and `Secure` cookies.
    pub fn secure(&self) -> bool {
        matches!(self, Role::App { secure: true })
    }
}

pub struct ListenerCfg {
    pub role: Role,
    pub tls: Option<TlsAcceptor>,
    pub max_connections: usize,
    pub timeout: Duration,
}

impl ListenerCfg {
    pub fn new(role: Role, tls: Option<TlsAcceptor>) -> ListenerCfg {
        ListenerCfg { role, tls, max_connections: MAX_CONNECTIONS, timeout: CONNECTION_TIMEOUT }
    }
}

/// Per-connection facts the handlers need.
pub struct ConnCtx {
    pub peer_ip: String,
    /// The local address of the connection (`getsockname()[0]`), for the HTTPS redirect.
    pub local_ip: String,
    pub cfg: Arc<ListenerCfg>,
    pub app: Arc<AppState>,
}

// ---------------------------------------------------------------------------
// Binding and TLS
// ---------------------------------------------------------------------------

/// Bind `host:port`. Names resolve with IPv4 preferred (the Python server was AF_INET).
pub fn bind(host: &str, port: u16) -> io::Result<std::net::TcpListener> {
    let mut addrs: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
    addrs.sort_by_key(|a| !a.is_ipv4());
    let mut last = io::Error::new(io::ErrorKind::AddrNotAvailable, "no address");
    for a in addrs {
        match std::net::TcpListener::bind(a) {
            Ok(l) => {
                l.set_nonblocking(true)?;
                return Ok(l);
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn exc_name(e: &io::Error) -> &'static str {
    crate::state::io_error_name(e)
}

/// `load_tls`: `Ok(acceptor)`, or `Err("missing")` / `Err("unreadable (<Type>)")`.
pub fn load_tls(cert: &str, key: &str) -> Result<TlsAcceptor, String> {
    if cert.is_empty() || key.is_empty() || !Path::new(cert).is_file() || !Path::new(key).is_file() {
        return Err("missing".into());
    }
    make_tls(Path::new(cert), Path::new(key))
}

fn make_tls(cert: &Path, key: &Path) -> Result<TlsAcceptor, String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let unreadable = |t: &str| format!("unreadable ({t})");
    let cert_pem = std::fs::read(cert).map_err(|e| unreadable(exc_name(&e)))?;
    let key_pem = std::fs::read(key).map_err(|e| unreadable(exc_name(&e)))?;
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<_, _>>()
        .map_err(|_| unreadable("SSLError"))?;
    if certs.is_empty() {
        return Err(unreadable("SSLError"));
    }
    let key = PrivateKeyDer::from_pem_slice(&key_pem).map_err(|_| unreadable("SSLError"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
        .map_err(|_| unreadable("SSLError"))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|_| unreadable("SSLError"))?;
    // No ALPN: HTTP/1.1 only, like the Python server.
    Ok(TlsAcceptor::from(Arc::new(cfg)))
}

// ---------------------------------------------------------------------------
// Per-operation timeout around a stream
// ---------------------------------------------------------------------------

/// Fails a write that stays pending longer than `timeout` (a client that stops reading).
///
/// Reads are not timed here: hyper keeps a read pending while a handler runs (to notice a
/// closed connection), and a slow handler (`power/cancel` waits for shutdown.exe) must not
/// lose its connection. Waiting for request headers (keep-alive idle included) is limited by
/// hyper's `header_read_timeout`, a request body by `routes::read_body`, the TLS handshake
/// by `serve_conn`; all use the same 15 s.
pub struct TimeoutIo<T> {
    inner: T,
    timeout: Duration,
    write_timer: Option<Pin<Box<Sleep>>>,
}

impl<T> TimeoutIo<T> {
    pub fn new(inner: T, timeout: Duration) -> Self {
        TimeoutIo { inner, timeout, write_timer: None }
    }
}

fn poll_timer(slot: &mut Option<Pin<Box<Sleep>>>, timeout: Duration, cx: &mut Context<'_>) -> bool {
    let t = slot.get_or_insert_with(|| Box::pin(tokio::time::sleep(timeout)));
    if t.as_mut().poll(cx).is_ready() {
        *slot = None;
        true
    } else {
        false
    }
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "socket timeout")
}

impl<T: AsyncRead + Unpin> AsyncRead for TimeoutIo<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for TimeoutIo<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        let me = &mut *self;
        match Pin::new(&mut me.inner).poll_write(cx, data) {
            Poll::Ready(r) => {
                me.write_timer = None;
                Poll::Ready(r)
            }
            Poll::Pending => {
                if poll_timer(&mut me.write_timer, me.timeout, cx) {
                    Poll::Ready(Err(timed_out()))
                } else {
                    Poll::Pending
                }
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let me = &mut *self;
        match Pin::new(&mut me.inner).poll_flush(cx) {
            Poll::Ready(r) => {
                me.write_timer = None;
                Poll::Ready(r)
            }
            Poll::Pending => {
                if poll_timer(&mut me.write_timer, me.timeout, cx) {
                    Poll::Ready(Err(timed_out()))
                } else {
                    Poll::Pending
                }
            }
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------
// Serving
// ---------------------------------------------------------------------------

/// Accept connections forever.
pub async fn serve(listener: TcpListener, cfg: Arc<ListenerCfg>, app: Arc<AppState>) {
    let slots = Arc::new(Semaphore::new(cfg.max_connections));
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => {
                // e.g. too many open files: back off a little instead of spinning
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            log_debug!("connection limit reached, closing one from {}", peer.ip());
            drop(stream);
            continue;
        };
        let (cfg, app) = (cfg.clone(), app.clone());
        tokio::spawn(async move {
            serve_conn(stream, peer, cfg, app).await;
            drop(permit);
        });
    }
}

async fn serve_conn(stream: TcpStream, peer: SocketAddr, cfg: Arc<ListenerCfg>, app: Arc<AppState>) {
    let local_ip = stream.local_addr().map(|a| ip_string(a.ip())).unwrap_or_else(|_| "127.0.0.1".into());
    let _ = stream.set_nodelay(true);
    let ctx = Arc::new(ConnCtx { peer_ip: ip_string(peer.ip()), local_ip, cfg: cfg.clone(), app });
    let io = TimeoutIo::new(stream, cfg.timeout);
    match cfg.tls.clone() {
        Some(acceptor) => match tokio::time::timeout(cfg.timeout, acceptor.accept(io)).await {
            Ok(Ok(tls)) => serve_http(tls, ctx).await,
            _ => {
                log_debug!("tls handshake failed from {}", ctx.peer_ip);
            }
        },
        None => serve_http(io, ctx).await,
    }
}

fn ip_string(ip: IpAddr) -> String {
    ip.to_string()
}

#[derive(Debug)]
struct Dropped;

impl std::fmt::Display for Dropped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("connection dropped")
    }
}

impl std::error::Error for Dropped {}

async fn serve_http<I>(io: I, ctx: Arc<ConnCtx>)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let timeout = ctx.cfg.timeout;
    let svc = service_fn(move |req| {
        let ctx = ctx.clone();
        async move {
            let ip = ctx.peer_ip.clone();
            // Own task: a panic is caught here (the connection is dropped, nothing sent),
            // and a client that disconnects never cancels a half-done login.
            match tokio::spawn(routes::handle(req, ctx)).await {
                Ok(Some(resp)) => Ok(resp),
                Ok(None) => Err(Dropped),
                Err(e) => {
                    if e.is_panic() {
                        log_error!("request error from {ip}");
                    }
                    Err(Dropped)
                }
            }
        }
    });
    let mut b = http1::Builder::new();
    b.timer(TokioTimer::new())
        .header_read_timeout(timeout)
        .keep_alive(true)
        .title_case_headers(true);
    let _ = b.serve_connection(TokioIo::new(io), svc).await;
}

/// Log panics (Python: `log.exception`), in place of the default stderr hook (pythonw and a
/// GUI-subsystem exe have no stderr).
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic".into());
        let loc = info.location().map(|l| format!(" at {}:{}", l.file(), l.line())).unwrap_or_default();
        log_error!("panic: {msg}{loc}");
    }));
}
