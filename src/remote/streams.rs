//! Parallel data streams (`--streams N`): extra TLS-over-TCP connections that carry
//! file data next to the ssh control connection, to fill fast or high-latency links
//! that a single ssh channel can't.
//!
//! Setup happens over the already authenticated ssh connection:
//! 1. the server listens on the address the client reached it at (from
//!    `SSH_CONNECTION`), with a freshly generated self-signed certificate, and sends the
//!    address, the certificate's BLAKE3 fingerprint and a random 32-byte token;
//! 2. the client opens up to N TLS connections, accepts only that exact certificate,
//!    and presents the token plus a stream index inside TLS;
//! 3. the server acknowledges each authenticated stream; the client reports how many
//!    it got. Zero means both sides stay on the ssh pipe.
//!
//! When no direct connection opens (a firewall on the server, say), the client carries
//! the streams over extra ssh connections instead (see [`relay`]); the listener also
//! listens on loopback, with the same port, for them.

use super::{Transport, auto, quic, relay};
use crate::proto::async_io;
use crate::output::Output;
use crate::proto::{Conn, Frame, Msg, StreamOffer};
use crate::timing::mark;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, ServerConfig, ServerConnection, SignatureScheme, StreamOwned};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Name in the certificate. Never resolved: the client checks the fingerprint instead.
pub(crate) const CERT_NAME: &str = "barqsync.invalid";
/// Per-connection limit for the TCP connect, TLS handshake and token exchange.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

pub(crate) fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub(crate) fn tls_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

/// Equality without an early exit, so timing doesn't reveal how much of a secret matched.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The address to listen on: the server end of the ssh connection when known (the one
/// address the client has already proven it can reach), else loopback.
pub(crate) fn listen_ip() -> IpAddr {
    std::env::var("SSH_CONNECTION")
        .ok()
        .and_then(|v| v.split_whitespace().nth(2).and_then(|ip| ip.parse().ok()))
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

/// Loopback in the family of `ip`.
pub(crate) fn loopback_for(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
    }
}

/// The listeners: on [`listen_ip`], and on loopback with the same port for relays (left
/// out when that port can't be had there).
fn bind() -> io::Result<Vec<TcpListener>> {
    bind_on(listen_ip())
}

fn bind_on(ip: IpAddr) -> io::Result<Vec<TcpListener>> {
    let lo = loopback_for(ip);
    let mut main = TcpListener::bind((ip, 0))?;
    if ip == lo {
        return Ok(vec![main]);
    }
    for _ in 0..8 {
        match TcpListener::bind((lo, main.local_addr()?.port())) {
            Ok(l) => return Ok(vec![main, l]),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => main = TcpListener::bind((ip, 0))?,
            Err(_) => break,
        }
    }
    Ok(vec![main])
}

/// An authenticated data stream: blocking, or driven by tokio (`BARQSYNC_NET_IO=tokio`).
pub enum DataStream {
    Sync(Box<dyn Stream>),
    Async(Box<dyn AsyncStream>),
}

pub trait AsyncStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> AsyncStream for T {}

type Accepted = Arc<(Mutex<Vec<(u16, DataStream)>>, Condvar)>;

/// Server side: accepts and authenticates streams in the background.
pub struct Acceptor {
    accepted: Accepted,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

pub fn listen(max: usize) -> io::Result<(StreamOffer, Acceptor)> {
    if async_io::enabled() {
        return listen_async(max);
    }
    let listeners = bind()?;
    let addr = listeners[0].local_addr()?;
    let certified = rcgen::generate_simple_self_signed(vec![CERT_NAME.to_string()]).map_err(tls_err)?;
    let cert = certified.cert.der().clone();
    let fingerprint = *blake3::hash(&cert).as_bytes();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()));
    let provider = provider();
    let mut token = [0u8; 32];
    provider.secure_random.fill(&mut token).map_err(|_| io::Error::other("no secure randomness"))?;
    let config = Arc::new(
        ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(tls_err)?
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .map_err(tls_err)?,
    );

    for l in &listeners {
        l.set_nonblocking(true)?;
    }
    let accepted: Accepted = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let (accepted, stop) = (accepted.clone(), stop.clone());
        std::thread::Builder::new().name("barqsync-accept".into()).spawn(move || {
            while !stop.load(Ordering::SeqCst) && accepted.0.lock().unwrap().len() < max {
                let mut idle = true;
                for l in &listeners {
                    if let Ok((sock, _)) = l.accept() {
                        idle = false;
                        if let Ok((idx, s)) = authenticate(sock, config.clone(), &token) {
                            accepted.0.lock().unwrap().push((idx, DataStream::Sync(s)));
                            accepted.1.notify_all();
                        }
                    }
                }
                if idle {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        })?
    };
    // A test hook: advertise a different (e.g. unreachable) address to exercise fallback.
    let advertised = std::env::var("BARQSYNC_TEST_STREAM_ADDR").unwrap_or_else(|_| addr.to_string());
    Ok((StreamOffer { addr: advertised, fingerprint, token }, Acceptor { accepted, stop, thread: Some(thread) }))
}

/// TLS handshake, then the client's token and stream index; acknowledge with one byte.
fn authenticate(sock: TcpStream, config: Arc<ServerConfig>, token: &[u8; 32]) -> io::Result<(u16, Box<dyn Stream>)> {
    sock.set_nonblocking(false)?;
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    sock.set_write_timeout(Some(CONNECT_TIMEOUT))?;
    let conn = ServerConnection::new(config).map_err(tls_err)?;
    let mut tls = StreamOwned::new(conn, sock);
    let mut hello = [0u8; 34];
    tls.read_exact(&mut hello)?;
    if !ct_eq(&hello[..32], token) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "bad stream token"));
    }
    tls.write_all(&[1])?;
    tls.flush()?;
    tls.sock.set_read_timeout(None)?;
    tls.sock.set_write_timeout(None)?;
    Ok((u16::from_le_bytes([hello[32], hello[33]]), Box::new(tls)))
}

impl Acceptor {
    /// Wait for the `n` streams the client established; returned in stream-index order.
    pub fn take(mut self, n: usize, timeout: Duration) -> io::Result<Vec<DataStream>> {
        let deadline = Instant::now() + timeout;
        let (lock, cvar) = &*self.accepted;
        let mut got = lock.lock().unwrap();
        while got.len() < n {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "parallel streams did not arrive"));
            }
            got = cvar.wait_timeout(got, left).unwrap().0;
        }
        let mut streams = std::mem::take(&mut *got);
        drop(got);
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        streams.sort_by_key(|(i, _)| *i);
        Ok(streams.into_iter().map(|(_, s)| s).collect())
    }
}

impl Drop for Acceptor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Accepts exactly one certificate: the one whose fingerprint came over ssh.
#[derive(Debug)]
pub(crate) struct Pinned {
    pub(crate) fingerprint: [u8; 32],
    pub(crate) provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if intermediates.is_empty() && ct_eq(blake3::hash(end_entity).as_bytes(), &self.fingerprint) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("certificate does not match the pinned fingerprint".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// The client's TLS configuration: only the offered certificate is accepted.
pub(crate) fn client_config(offer: &StreamOffer) -> io::Result<Arc<ClientConfig>> {
    let provider = provider();
    Ok(Arc::new(
        ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(tls_err)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pinned { fingerprint: offer.fingerprint, provider }))
            .with_no_client_auth(),
    ))
}

/// Client side: open up to `n` authenticated streams in parallel. Failed ones are
/// dropped; if none opens, the error says why (the caller then tries relays, then uses
/// the ssh pipe).
pub fn connect(offer: &StreamOffer, n: usize) -> io::Result<Vec<DataStream>> {
    if std::env::var_os("BARQSYNC_TEST_DIRECT_BLOCKED").is_some() {
        return Err(io::Error::new(io::ErrorKind::ConnectionReset, "Connection reset by peer (test)"));
    }
    let addr = offer.addr.parse::<SocketAddr>().map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let config = client_config(offer)?;
    let results = if async_io::enabled() {
        connect_async(addr, config, offer.token, n)
    } else {
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let (config, token) = (config.clone(), offer.token);
                std::thread::spawn(move || connect_one(addr, config, &token, i as u16).map(DataStream::Sync))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(io::Error::other("connect thread panicked")))).collect()
    };
    let (streams, errors): (Vec<_>, Vec<_>) = results.into_iter().partition(Result::is_ok);
    match errors.into_iter().next() {
        Some(Err(e)) if streams.is_empty() => Err(e),
        _ => Ok(streams.into_iter().flatten().collect()),
    }
}

// ---------------------------------------------------------------------------
// The same over tokio (`BARQSYNC_NET_IO=tokio`)
// ---------------------------------------------------------------------------

fn listen_async(max: usize) -> io::Result<(StreamOffer, Acceptor)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let _rt = async_io::rt().enter();
    let std_listeners = bind()?;
    let addr = std_listeners[0].local_addr()?;
    let certified = rcgen::generate_simple_self_signed(vec![CERT_NAME.to_string()]).map_err(tls_err)?;
    let cert = certified.cert.der().clone();
    let fingerprint = *blake3::hash(&cert).as_bytes();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()));
    let provider = provider();
    let mut token = [0u8; 32];
    provider.secure_random.fill(&mut token).map_err(|_| io::Error::other("no secure randomness"))?;
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(tls_err)?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(tls_err)?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let accepted: Accepted = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
    let stop = Arc::new(AtomicBool::new(false));
    for l in std_listeners {
        l.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(l)?;
        let (accepted, stop, acceptor) = (accepted.clone(), stop.clone(), acceptor.clone());
        async_io::rt().spawn(async move {
            while !stop.load(Ordering::SeqCst) && accepted.0.lock().unwrap().len() < max {
                let Ok(Ok((sock, _))) = tokio::time::timeout(Duration::from_millis(100), listener.accept()).await else { continue };
                let (acceptor, accepted) = (acceptor.clone(), accepted.clone());
                tokio::spawn(async move {
                    let auth = async {
                        sock.set_nodelay(true)?;
                        let mut tls = acceptor.accept(sock).await?;
                        let mut hello = [0u8; 34];
                        tls.read_exact(&mut hello).await?;
                        if !ct_eq(&hello[..32], &token) {
                            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "bad stream token"));
                        }
                        tls.write_all(&[1]).await?;
                        tls.flush().await?;
                        Ok::<_, io::Error>((u16::from_le_bytes([hello[32], hello[33]]), tls))
                    };
                    if let Ok(Ok((idx, tls))) = tokio::time::timeout(CONNECT_TIMEOUT, auth).await {
                        accepted.0.lock().unwrap().push((idx, DataStream::Async(Box::new(tls))));
                        accepted.1.notify_all();
                    }
                });
            }
        });
    }
    let advertised = std::env::var("BARQSYNC_TEST_STREAM_ADDR").unwrap_or_else(|_| addr.to_string());
    Ok((StreamOffer { addr: advertised, fingerprint, token }, Acceptor { accepted, stop, thread: None }))
}

fn connect_async(addr: SocketAddr, config: Arc<ClientConfig>, token: [u8; 32], n: usize) -> Vec<io::Result<DataStream>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let connector = tokio_rustls::TlsConnector::from(config);
    async_io::rt().block_on(async move {
        let tasks: Vec<_> = (0..n)
            .map(|i| {
                let connector = connector.clone();
                tokio::spawn(tokio::time::timeout(CONNECT_TIMEOUT, async move {
                    let sock = tokio::net::TcpStream::connect(addr).await?;
                    sock.set_nodelay(true)?;
                    let name = ServerName::try_from(CERT_NAME.to_string()).map_err(tls_err)?;
                    let mut tls = connector.connect(name, sock).await?;
                    let mut hello = token.to_vec();
                    hello.extend_from_slice(&(i as u16).to_le_bytes());
                    tls.write_all(&hello).await?;
                    tls.flush().await?;
                    let mut ack = [0u8; 1];
                    tls.read_exact(&mut ack).await?;
                    if ack[0] != 1 {
                        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "stream rejected"));
                    }
                    Ok::<_, io::Error>(tls)
                }))
            })
            .collect();
        let mut out = Vec::new();
        for t in tasks {
            out.push(match t.await {
                Ok(Ok(Ok(tls))) => Ok(DataStream::Async(Box::new(tls) as Box<dyn AsyncStream>)),
                Ok(Ok(Err(e))) => Err(e),
                Ok(Err(_)) => Err(io::Error::new(io::ErrorKind::TimedOut, "connection timed out")),
                Err(_) => Err(io::Error::other("connect task failed")),
            });
        }
        out
    })
}

fn connect_one(addr: SocketAddr, config: Arc<ClientConfig>, token: &[u8; 32], idx: u16) -> io::Result<Box<dyn Stream>> {
    let sock = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    sock.set_write_timeout(Some(CONNECT_TIMEOUT))?;
    let tls = client_handshake(sock, config, token, idx)?;
    tls.sock.set_read_timeout(None)?;
    tls.sock.set_write_timeout(None)?;
    Ok(Box::new(tls))
}

/// TLS over `sock` (a TCP connection, or a relay's pipes), then the token and the stream
/// index; the server acknowledges with one byte.
pub(crate) fn client_handshake<S: Read + Write>(
    sock: S,
    config: Arc<ClientConfig>,
    token: &[u8; 32],
    idx: u16,
) -> io::Result<StreamOwned<ClientConnection, S>> {
    let name = ServerName::try_from(CERT_NAME.to_string()).map_err(tls_err)?;
    let conn = ClientConnection::new(config, name).map_err(tls_err)?;
    let mut tls = StreamOwned::new(conn, sock);
    let mut hello = token.to_vec();
    hello.extend_from_slice(&idx.to_le_bytes());
    tls.write_all(&hello)?;
    tls.flush()?;
    let mut ack = [0u8; 1];
    tls.read_exact(&mut ack)?;
    if ack[0] != 1 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "stream rejected"));
    }
    Ok(tls)
}

/// Wait for one frame during setup, with a deadline (the main connection isn't sealed
/// yet, so a dead peer would otherwise go unnoticed).
fn recv_setup(conn: &Conn, timeout: Duration) -> io::Result<Frame> {
    match conn.rx.recv_timeout(timeout) {
        Ok(r) => r,
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "no reply while setting up parallel streams")),
    }
}

fn attach(conn: &mut Conn, streams: Vec<DataStream>, sending: bool) {
    let (mut sync, mut tasks) = (Vec::new(), Vec::new());
    for s in streams {
        match s {
            DataStream::Sync(s) => sync.push(s),
            DataStream::Async(s) => tasks.push(s),
        }
    }
    if sending {
        conn.attach_data_out(sync.into_iter().map(|s| s as Box<dyn Write + Send>).collect());
        if !tasks.is_empty() {
            conn.attach_data_out_async(tasks.into_iter().map(|s| s as Box<dyn tokio::io::AsyncWrite + Unpin + Send>).collect());
        }
    } else {
        conn.attach_data_in(sync.into_iter().map(|s| s as Box<dyn Read + Send>).collect());
        conn.attach_data_in_async(tasks.into_iter().map(|s| s as Box<dyn tokio::io::AsyncRead + Unpin + Send>).collect());
    }
}

/// Move the connection onto an established QUIC session: control on its first
/// stream, file data on `n` more, sent by the side that sends files.
fn quic_switch(conn: &mut Conn, mut session: quic::Session, n: usize, sending: bool) -> io::Result<()> {
    let (reader, writer) = session.control();
    let data_out = if sending { Some(session.open_data(n)?) } else { None };
    let data_in = if sending { None } else { Some(session.accept_data(n)) };
    conn.seal();
    conn.switch(reader, writer, Box::new(session));
    if let Some(d) = data_out {
        conn.attach_data_out(d);
    }
    if let Some(d) = data_in {
        conn.attach_data_in(d);
    }
    Ok(())
}

/// `--transport auto`, client side: ping over ssh while QUIC connects; on a long round
/// trip, give QUIC a short while and use it if it connects and measures a long round trip
/// too, else stay on the ssh pipe (see [`auto`]). The server answers the ping and waits
/// for `StreamsReady`.
fn auto_client(conn: &mut Conn, offer: &StreamOffer, want: usize, sending: bool, out: &Output) -> io::Result<auto::Outcome> {
    let started = Instant::now();
    conn.msg(Msg::Ping)?;
    // Connecting alongside the ping hides the handshake behind it on a long link; on a
    // short one the attempt is dropped once the pong is back.
    let attempt = quic::start(offer, Some(quic::INITIAL_RTT));
    match recv_setup(conn, Duration::from_secs(60))? {
        Frame::Msg(Msg::Pong) => {}
        other => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("expected a pong, got {other:?}"))),
    }
    let rtt = started.elapsed();
    mark(format_args!("auto: pong after {:.1} ms", rtt.as_secs_f64() * 1e3));
    let threshold = auto::threshold();
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    let stay = |conn: &Conn, why: String| -> io::Result<()> {
        conn.msg(Msg::StreamsReady { count: 0 })?;
        if out.verbose() > 1 {
            out.info(format!("auto: {why}, using the ssh connection"));
        }
        Ok(())
    };
    if !auto::wants_quic(rtt, threshold) {
        drop(attempt);
        stay(conn, format!("{:.1} ms round trip", ms(rtt)))?;
        return Ok(auto::Outcome::Ssh);
    }
    let limit = auto::deadline(rtt);
    let attempt = attempt.and_then(|a| a.wait(started + limit));
    mark(format_args!("auto: QUIC attempt ended ({})", if attempt.is_ok() { "connected" } else { "failed" }));
    match attempt {
        // A ping that looked long because ssh resent it: QUIC's own measurement decides.
        Ok(session) if !auto::wants_quic(session.min_rtt(), threshold) => {
            stay(conn, format!("{:.1} ms round trip over QUIC ({:.1} ms over ssh)", ms(session.min_rtt()), ms(rtt)))?;
            Ok(auto::Outcome::Ssh)
        }
        Ok(session) => {
            conn.msg(Msg::StreamsReady { count: want as u16 })?;
            if out.verbose() > 1 {
                out.info(format!(
                    "auto: {:.1} ms round trip, QUIC connected in {:.0} ms, using QUIC to {} ({want} data streams)",
                    ms(rtt),
                    ms(started.elapsed()),
                    offer.addr
                ));
            }
            quic_switch(conn, session, want, sending)?;
            Ok(auto::Outcome::Quic)
        }
        Err(e) => {
            stay(conn, format!("QUIC to {} did not connect within {:.0} ms ({e})", offer.addr, ms(limit)))?;
            Ok(auto::Outcome::Unreachable)
        }
    }
}

/// Client side of the setup (right after the handshake). `sending`: this side sends
/// file data. Always seals the connection. With `--transport auto`, says what it chose.
pub fn setup_client(
    conn: &mut Conn,
    transport: Transport,
    want: usize,
    sending: bool,
    shell: Option<&relay::Shell>,
    out: &Output,
) -> io::Result<Option<auto::Outcome>> {
    let r = (|| {
        if want == 0 || transport == Transport::Ssh {
            return Ok(None);
        }
        match recv_setup(conn, Duration::from_secs(60))? {
            Frame::Msg(Msg::QuicOffer(offer)) if transport == Transport::Auto => {
                auto_client(conn, &offer, want, sending, out).map(Some)
            }
            Frame::Msg(Msg::QuicOffer(offer)) => {
                mark("QUIC: connecting");
                let session = quic::connect(&offer);
                mark("QUIC: connect ended");
                match session {
                    Ok(session) => {
                        conn.msg(Msg::StreamsReady { count: want as u16 })?;
                        if out.verbose() > 1 {
                            out.info(format!("using QUIC to {} ({want} data streams)", offer.addr));
                        }
                        quic_switch(conn, session, want, sending)?;
                    }
                    Err(e) => {
                        conn.msg(Msg::StreamsReady { count: 0 })?;
                        out.info(format!("QUIC: could not connect to {} ({e}), using the ssh connection", offer.addr));
                    }
                }
                Ok(None)
            }
            Frame::Msg(Msg::StreamOffer(offer)) => {
                let why = |r: io::Result<Vec<DataStream>>| r.err().map_or_else(|| "no stream opened".to_string(), |e| e.to_string());
                match connect(&offer, want) {
                    Ok(streams) if !streams.is_empty() => {
                        let n = streams.len();
                        conn.msg(Msg::StreamsReady { count: n as u16 })?;
                        if out.verbose() > 1 {
                            out.info(format!("using {n} parallel TLS data streams to {}", offer.addr));
                        }
                        attach(conn, streams, sending);
                    }
                    direct => {
                        // Blocked, most likely by a firewall: the same streams through ssh.
                        let direct = why(direct);
                        match shell.map(|s| relay::connect(s, &offer, want)) {
                            Some(Ok(streams)) if !streams.is_empty() => {
                                let n = streams.len();
                                conn.msg(Msg::StreamsReady { count: n as u16 })?;
                                if out.verbose() > 0 {
                                    out.info(format!(
                                        "parallel streams: {} refused a direct connection ({direct}), \
                                         using {n} streams through ssh",
                                        offer.addr
                                    ));
                                }
                                attach(conn, streams, sending);
                            }
                            relayed => {
                                conn.msg(Msg::StreamsReady { count: 0 })?;
                                let relayed = relayed.map_or(String::new(), |r| format!("; through ssh: {}", why(r)));
                                out.info(format!(
                                    "parallel streams: could not connect to {} ({direct}{relayed}), using the ssh connection",
                                    offer.addr
                                ));
                            }
                        }
                    }
                }
                Ok(None)
            }
            Frame::Msg(Msg::StreamUnavailable { reason }) if transport == Transport::Auto => {
                if out.verbose() > 1 {
                    out.info(format!("auto: no QUIC on the server ({reason}), using the ssh connection"));
                }
                Ok(Some(auto::Outcome::Ssh))
            }
            Frame::Msg(Msg::StreamUnavailable { reason }) => {
                out.info(format!("parallel streams unavailable ({reason}), using the ssh connection"));
                Ok(None)
            }
            other => Err(io::Error::new(io::ErrorKind::InvalidData, format!("expected a stream offer, got {other:?}"))),
        }
    })();
    conn.seal();
    r
}

/// Server side of the setup. Always seals the connection.
pub fn setup_server(conn: &mut Conn, transport: Transport, want: usize, sending: bool) -> io::Result<()> {
    let r = (|| {
        if want == 0 || transport == Transport::Ssh {
            return Ok(());
        }
        if transport.quic() {
            let (offer, listener) = match quic::listen() {
                Ok(x) => x,
                Err(e) => return conn.msg(Msg::StreamUnavailable { reason: e.to_string() }),
            };
            let pending = listener.accept();
            conn.msg(Msg::QuicOffer(offer))?;
            loop {
                match recv_setup(conn, Duration::from_secs(60))? {
                    // `auto` measures the round trip first.
                    Frame::Msg(Msg::Ping) => conn.msg(Msg::Pong)?,
                    Frame::Msg(Msg::StreamsReady { count: 0 }) => return Ok(()),
                    Frame::Msg(Msg::StreamsReady { count }) => {
                        mark("QUIC: client ready");
                        let session = pending.wait(Duration::from_secs(10))?;
                        mark("QUIC: session accepted");
                        return quic_switch(conn, session, count as usize, sending);
                    }
                    other => {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("expected stream count, got {other:?}")));
                    }
                }
            }
        }
        let (offer, acceptor) = match listen(want) {
            Ok(x) => x,
            Err(e) => return conn.msg(Msg::StreamUnavailable { reason: e.to_string() }),
        };
        conn.msg(Msg::StreamOffer(offer))?;
        match recv_setup(conn, Duration::from_secs(60))? {
            Frame::Msg(Msg::StreamsReady { count: 0 }) => Ok(()),
            Frame::Msg(Msg::StreamsReady { count }) => {
                let streams = acceptor.take(count as usize, Duration::from_secs(10))?;
                attach(conn, streams, sending);
                Ok(())
            }
            other => Err(io::Error::new(io::ErrorKind::InvalidData, format!("expected stream count, got {other:?}"))),
        }
    })();
    conn.seal();
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync(v: Vec<DataStream>) -> Vec<Box<dyn Stream>> {
        v.into_iter()
            .map(|s| match s {
                DataStream::Sync(s) => s,
                DataStream::Async(_) => panic!("thread layer expected"),
            })
            .collect()
    }

    fn roundtrip(mut a: Box<dyn Stream>, mut b: Box<dyn Stream>) {
        a.write_all(b"ping").unwrap();
        a.flush().unwrap();
        let mut buf = [0u8; 4];
        b.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");
    }

    #[test]
    fn streams_connect_authenticate_and_carry_data() {
        let (offer, acceptor) = listen(3).unwrap();
        let client = sync(connect(&offer, 3).unwrap());
        assert_eq!(client.len(), 3);
        let server = sync(acceptor.take(3, Duration::from_secs(5)).unwrap());
        for (c, s) in client.into_iter().zip(server) {
            roundtrip(c, s);
        }
    }

    #[test]
    fn wrong_fingerprint_is_refused() {
        let (mut offer, _acceptor) = listen(1).unwrap();
        offer.fingerprint[0] ^= 1;
        assert!(connect(&offer, 1).is_err());
    }

    #[test]
    fn wrong_token_is_refused() {
        let (mut offer, acceptor) = listen(1).unwrap();
        offer.token[31] ^= 1;
        assert!(connect(&offer, 1).is_err());
        assert!(acceptor.take(1, Duration::from_millis(300)).is_err());
    }

    #[test]
    fn the_listener_also_listens_on_loopback_with_the_same_port() {
        let one = bind_on(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        assert_eq!(one.len(), 1);
        // Any other local address will do; Linux has all of 127/8 (macOS only 127.0.0.1).
        let Ok(two) = bind_on("127.0.0.2".parse().unwrap()) else { return };
        assert_eq!(two.len(), 2);
        let (a, b) = (two[0].local_addr().unwrap(), two[1].local_addr().unwrap());
        assert_eq!((a.ip().to_string().as_str(), b.ip()), ("127.0.0.2", IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert_eq!(a.port(), b.port());
    }

    #[test]
    fn unreachable_address_yields_no_streams() {
        let (mut offer, _acceptor) = listen(1).unwrap();
        // Port 1 on loopback: refused immediately.
        offer.addr = "127.0.0.1:1".into();
        let e = connect(&offer, 2).err().expect("no streams");
        assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused);
    }
}
