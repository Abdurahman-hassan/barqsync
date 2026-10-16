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

use crate::output::Output;
use crate::proto::{Conn, Frame, Msg, StreamOffer};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, ServerConfig, ServerConnection, SignatureScheme, StreamOwned};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Name in the certificate. Never resolved: the client checks the fingerprint instead.
const CERT_NAME: &str = "fastrsync.invalid";
/// Per-connection limit for the TCP connect, TLS handshake and token exchange.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn tls_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

/// Equality without an early exit, so timing doesn't reveal how much of a secret matched.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The address to listen on: the server end of the ssh connection when known (the one
/// address the client has already proven it can reach), else loopback.
fn listen_ip() -> IpAddr {
    std::env::var("SSH_CONNECTION")
        .ok()
        .and_then(|v| v.split_whitespace().nth(2).and_then(|ip| ip.parse().ok()))
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

type Accepted = Arc<(Mutex<Vec<(u16, Box<dyn Stream>)>>, Condvar)>;

/// Server side: accepts and authenticates streams in the background.
pub struct Acceptor {
    accepted: Accepted,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

pub fn listen(max: usize) -> io::Result<(StreamOffer, Acceptor)> {
    let listener = TcpListener::bind((listen_ip(), 0))?;
    let addr = listener.local_addr()?;
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

    listener.set_nonblocking(true)?;
    let accepted: Accepted = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let (accepted, stop) = (accepted.clone(), stop.clone());
        std::thread::Builder::new().name("fastrsync-accept".into()).spawn(move || {
            while !stop.load(Ordering::SeqCst) && accepted.0.lock().unwrap().len() < max {
                match listener.accept() {
                    Ok((sock, _)) => {
                        if let Ok((idx, s)) = authenticate(sock, config.clone(), &token) {
                            accepted.0.lock().unwrap().push((idx, s));
                            accepted.1.notify_all();
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        })?
    };
    // A test hook: advertise a different (e.g. unreachable) address to exercise fallback.
    let advertised = std::env::var("FASTRSYNC_TEST_STREAM_ADDR").unwrap_or_else(|_| addr.to_string());
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
    pub fn take(mut self, n: usize, timeout: Duration) -> io::Result<Vec<Box<dyn Stream>>> {
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
struct Pinned {
    fingerprint: [u8; 32],
    provider: Arc<CryptoProvider>,
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

/// Client side: open up to `n` authenticated streams in parallel. Failed ones are
/// dropped; an empty result means "use the ssh pipe".
pub fn connect(offer: &StreamOffer, n: usize) -> Vec<Box<dyn Stream>> {
    let Ok(addr) = offer.addr.parse::<SocketAddr>() else { return Vec::new() };
    let provider = provider();
    let config = match ClientConfig::builder_with_provider(provider.clone()).with_safe_default_protocol_versions() {
        Ok(b) => Arc::new(
            b.dangerous()
                .with_custom_certificate_verifier(Arc::new(Pinned { fingerprint: offer.fingerprint, provider }))
                .with_no_client_auth(),
        ),
        Err(_) => return Vec::new(),
    };
    let handles: Vec<_> = (0..n)
        .map(|i| {
            let (config, token) = (config.clone(), offer.token);
            std::thread::spawn(move || connect_one(addr, config, &token, i as u16))
        })
        .collect();
    handles.into_iter().filter_map(|h| h.join().ok().and_then(Result::ok)).collect()
}

fn connect_one(addr: SocketAddr, config: Arc<ClientConfig>, token: &[u8; 32], idx: u16) -> io::Result<Box<dyn Stream>> {
    let sock = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    sock.set_write_timeout(Some(CONNECT_TIMEOUT))?;
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
    tls.sock.set_read_timeout(None)?;
    tls.sock.set_write_timeout(None)?;
    Ok(Box::new(tls))
}

/// Wait for one frame during setup, with a deadline (the main connection isn't sealed
/// yet, so a dead peer would otherwise go unnoticed).
fn recv_setup(conn: &Conn, timeout: Duration) -> io::Result<Frame> {
    match conn.rx.recv_timeout(timeout) {
        Ok(r) => r,
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "no reply while setting up parallel streams")),
    }
}

fn attach(conn: &mut Conn, streams: Vec<Box<dyn Stream>>, sending: bool) {
    if sending {
        conn.attach_data_out(streams.into_iter().map(|s| s as Box<dyn Write + Send>).collect());
    } else {
        conn.attach_data_in(streams.into_iter().map(|s| s as Box<dyn Read + Send>).collect());
    }
}

/// Client side of the setup (right after the handshake). `sending`: this side sends
/// file data. Always seals the connection.
pub fn setup_client(conn: &mut Conn, want: usize, sending: bool, out: &Output) -> io::Result<()> {
    let r = (|| {
        if want == 0 {
            return Ok(());
        }
        match recv_setup(conn, Duration::from_secs(60))? {
            Frame::Msg(Msg::StreamOffer(offer)) => {
                let streams = connect(&offer, want);
                let n = streams.len();
                conn.msg(Msg::StreamsReady { count: n as u16 })?;
                if n == 0 {
                    out.info(format!(
                        "parallel streams: could not connect to {}, using the ssh connection",
                        offer.addr
                    ));
                } else {
                    if out.verbose() > 1 {
                        out.info(format!("using {n} parallel TLS data streams to {}", offer.addr));
                    }
                    attach(conn, streams, sending);
                }
                Ok(())
            }
            Frame::Msg(Msg::StreamUnavailable { reason }) => {
                out.info(format!("parallel streams unavailable ({reason}), using the ssh connection"));
                Ok(())
            }
            other => Err(io::Error::new(io::ErrorKind::InvalidData, format!("expected a stream offer, got {other:?}"))),
        }
    })();
    conn.seal();
    r
}

/// Server side of the setup. Always seals the connection.
pub fn setup_server(conn: &mut Conn, want: usize, sending: bool) -> io::Result<()> {
    let r = (|| {
        if want == 0 {
            return Ok(());
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
        let client = connect(&offer, 3);
        assert_eq!(client.len(), 3);
        let server = acceptor.take(3, Duration::from_secs(5)).unwrap();
        for (c, s) in client.into_iter().zip(server) {
            roundtrip(c, s);
        }
    }

    #[test]
    fn wrong_fingerprint_is_refused() {
        let (mut offer, _acceptor) = listen(1).unwrap();
        offer.fingerprint[0] ^= 1;
        assert!(connect(&offer, 1).is_empty());
    }

    #[test]
    fn wrong_token_is_refused() {
        let (mut offer, acceptor) = listen(1).unwrap();
        offer.token[31] ^= 1;
        assert!(connect(&offer, 1).is_empty());
        assert!(acceptor.take(1, Duration::from_millis(300)).is_err());
    }

    #[test]
    fn unreachable_address_yields_no_streams() {
        let (mut offer, _acceptor) = listen(1).unwrap();
        // Port 1 on loopback: refused immediately.
        offer.addr = "127.0.0.1:1".into();
        assert!(connect(&offer, 2).is_empty());
    }
}
