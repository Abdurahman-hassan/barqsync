//! QUIC transport (`--transport quic`, the original plan's Phase 3).
//!
//! Setup runs over the already authenticated ssh connection, like `--streams`:
//! 1. the server opens a UDP endpoint on the address the client reached it at, with a
//!    fresh self-signed certificate (ALPN `fastrsync`, TLS 1.3), and sends the address,
//!    the certificate's BLAKE3 fingerprint and a random token;
//! 2. the client connects within 3 s, accepting only that certificate, opens the
//!    bidirectional control stream and writes the token on it;
//! 3. the server accepts the connection whose control stream carries the token and
//!    acknowledges with one byte. If anything fails, both sides stay on the ssh pipe.
//!
//! Then control messages, metadata batches included, move to the control stream, and
//! file data travels on unidirectional streams (one per `--streams`), routed by file id
//! as over TCP. The ssh connection stays open, idle, until the end. QUIC runs on a small
//! tokio runtime; the protocol's reader and writer threads talk to it through blocking
//! bridges.

use super::streams::{ct_eq, listen_ip, provider, tls_err, Pinned, CERT_NAME};
use crate::proto::StreamOffer;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, RecvStream, SendStream, TransportConfig};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::io::SyncIoBridge;

const ALPN: &[u8] = b"fastrsync";
/// The client gives up on QUIC (and stays on ssh) after this long.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// How long closing waits for the peer to take the last data.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

fn rt() -> &'static tokio::runtime::Runtime {
    crate::proto::async_io::rt()
}

fn transport() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.max_concurrent_uni_streams(256u32.into());
    // Enough in flight for 1 Gbit/s at 100 ms (12.5 MB), not much more: data buffered
    // beyond what the link holds only delays the control stream behind it.
    t.stream_receive_window((8u32 << 20).into());
    t.receive_window((24u32 << 20).into());
    t.send_window(24 << 20);
    // A peer that vanished is noticed within 15 s (ssh would see EOF at once).
    t.max_idle_timeout(Some(Duration::from_secs(15).try_into().expect("idle timeout")));
    t.keep_alive_interval(Some(Duration::from_secs(3)));
    Arc::new(t)
}

/// An established, authenticated QUIC connection with its control stream.
pub struct Session {
    /// Kept for the session's lifetime: the endpoint drives the connection.
    _endpoint: Endpoint,
    conn: Connection,
    control: Option<(SendStream, RecvStream)>,
}

/// Server side: the endpoint, waiting for the client.
pub struct Listener {
    endpoint: Endpoint,
    token: [u8; 32],
}

pub fn listen() -> io::Result<(StreamOffer, Listener)> {
    let certified = rcgen::generate_simple_self_signed(vec![CERT_NAME.to_string()]).map_err(tls_err)?;
    let cert = certified.cert.der().clone();
    let fingerprint = *blake3::hash(&cert).as_bytes();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()));
    let provider = provider();
    let mut token = [0u8; 32];
    provider.secure_random.fill(&mut token).map_err(|_| io::Error::other("no secure randomness"))?;
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(tls_err)?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(tls_err)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls).map_err(tls_err)?));
    config.transport_config(transport());
    let endpoint = {
        let _rt = rt().enter();
        Endpoint::server(config, SocketAddr::new(listen_ip(), 0))?
    };
    let addr = endpoint.local_addr()?;
    // The same test hook as `--streams`: advertise an unreachable address.
    let advertised = std::env::var("FASTRSYNC_TEST_STREAM_ADDR").unwrap_or_else(|_| addr.to_string());
    Ok((StreamOffer { addr: advertised, fingerprint, token }, Listener { endpoint, token }))
}

impl Listener {
    /// Start accepting the client in the background: the first connection whose
    /// control stream starts with the token (others are closed). The client only reports
    /// success over ssh after this side has acknowledged it, so it must not wait for ssh.
    pub fn accept(self) -> Pending {
        let Listener { endpoint, token } = self;
        let ep = endpoint.clone();
        let task = rt().spawn(async move {
            while let Some(incoming) = ep.accept().await {
                let Ok(conn) = incoming.await else { continue };
                let Ok((mut send, mut recv)) = conn.accept_bi().await else { continue };
                let mut hello = [0u8; 32];
                if recv.read_exact(&mut hello).await.is_ok() && ct_eq(&hello, &token) {
                    send.write_all(&[1]).await.map_err(io::Error::other)?;
                    return Ok((conn, send, recv));
                }
                conn.close(1u32.into(), b"bad token");
            }
            Err(io::Error::other("QUIC endpoint closed"))
        });
        Pending { endpoint: Some(endpoint), task: Some(task) }
    }
}

type Accepted = io::Result<(Connection, SendStream, RecvStream)>;

/// A background accept; dropping it gives up.
pub struct Pending {
    endpoint: Option<Endpoint>,
    task: Option<tokio::task::JoinHandle<Accepted>>,
}

impl Pending {
    /// The accepted session, once the client said it connected.
    pub fn wait(mut self, timeout: Duration) -> io::Result<Session> {
        let task = self.task.take().expect("accept task");
        let (conn, send, recv) = rt().block_on(async move {
            match tokio::time::timeout(timeout, task).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => Err(io::Error::other(e)),
                Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "the QUIC client did not arrive")),
            }
        })?;
        let endpoint = self.endpoint.take().expect("endpoint");
        Ok(Session { _endpoint: endpoint, conn, control: Some((send, recv)) })
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
        if let Some(ep) = self.endpoint.take() {
            ep.close(0u32.into(), b"not used");
        }
    }
}

/// Client side: connect, authenticate, and open the control stream. Fails within
/// [`CONNECT_TIMEOUT`] when UDP doesn't get through.
pub fn connect(offer: &StreamOffer) -> io::Result<Session> {
    let addr: SocketAddr = offer.addr.parse().map_err(|_| io::Error::other("bad QUIC address"))?;
    let provider = provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(tls_err)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned { fingerprint: offer.fingerprint, provider }))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls).map_err(tls_err)?));
    config.transport_config(transport());
    let local = if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let mut endpoint = {
        let _rt = rt().enter();
        Endpoint::client(local.parse().expect("bind address"))?
    };
    endpoint.set_default_client_config(config);
    let token = offer.token;
    let ep = endpoint.clone();
    let (conn, send, recv) = rt().block_on(async move {
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            let conn = ep.connect(addr, CERT_NAME).map_err(io::Error::other)?.await.map_err(io::Error::other)?;
            let (mut send, mut recv) = conn.open_bi().await.map_err(io::Error::other)?;
            send.write_all(&token).await.map_err(io::Error::other)?;
            let mut ack = [0u8; 1];
            recv.read_exact(&mut ack).await.map_err(io::Error::other)?;
            if ack[0] != 1 {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "QUIC session rejected"));
            }
            Ok((conn, send, recv))
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "QUIC did not connect")))
    })?;
    Ok(Session { _endpoint: endpoint, conn, control: Some((send, recv)) })
}

impl Session {
    /// The control stream as blocking halves.
    pub fn control(&mut self) -> (Box<dyn Read + Send>, Box<dyn Write + Send>) {
        let (send, recv) = self.control.take().expect("control stream taken once");
        // Listings and requests go ahead of queued file data: every round trip of the
        // protocol waits on them.
        let _ = send.set_priority(1);
        (Box::new(SyncIoBridge::new_with_handle(recv, rt().handle().clone())), Box::new(QuicWrite::new(send)))
    }

    /// `n` streams to send file data on.
    pub fn open_data(&self, n: usize) -> io::Result<Vec<Box<dyn Write + Send>>> {
        (0..n)
            .map(|_| {
                let s = rt().block_on(self.conn.open_uni()).map_err(io::Error::other)?;
                Ok(Box::new(QuicWrite::new(s)) as Box<dyn Write + Send>)
            })
            .collect()
    }

    /// `n` streams to receive file data on (each is accepted when its first data arrives).
    pub fn accept_data(&self, n: usize) -> Vec<Box<dyn Read + Send>> {
        (0..n).map(|_| Box::new(AcceptRead { conn: self.conn.clone(), stream: None }) as Box<dyn Read + Send>).collect()
    }
}

impl Drop for Session {
    /// Every stream has been finished and taken by the peer by now (the writers wait for
    /// that), so close without waiting out QUIC's closing period (three probe timeouts,
    /// ~100 ms on a LAN): nothing is left to deliver.
    fn drop(&mut self) {
        if std::env::var_os("FASTRSYNC_QUIC_STATS").is_some() {
            let s = self.conn.stats();
            eprintln!(
                "quic: rtt {:?}, cwnd {}, sent {} pkts / {} B, lost {} pkts, congestion events {}, mtu {}",
                s.path.rtt,
                s.path.cwnd,
                s.path.sent_packets,
                s.udp_tx.bytes,
                s.path.lost_packets,
                s.path.congestion_events,
                s.path.current_mtu
            );
        }
        self.conn.close(0u32.into(), b"done");
    }
}

/// A send stream as `Write`. Dropping it ends the stream and waits (bounded) until the
/// peer has read everything, so nothing is lost when the process exits.
struct QuicWrite {
    inner: Option<SyncIoBridge<SendStream>>,
}

impl QuicWrite {
    fn new(s: SendStream) -> QuicWrite {
        QuicWrite { inner: Some(SyncIoBridge::new_with_handle(s, rt().handle().clone())) }
    }
}

impl Write for QuicWrite {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.as_mut().expect("open stream").write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.as_mut().expect("open stream").flush()
    }
}

impl Drop for QuicWrite {
    fn drop(&mut self) {
        let Some(bridge) = self.inner.take() else { return };
        let mut send = bridge.into_inner();
        if send.finish().is_ok() {
            rt().block_on(async move {
                let _ = tokio::time::timeout(CLOSE_TIMEOUT, send.stopped()).await;
            });
        }
    }
}

/// A receive stream accepted on first read.
struct AcceptRead {
    conn: Connection,
    stream: Option<SyncIoBridge<RecvStream>>,
}

impl Read for AcceptRead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.stream.is_none() {
            let s = rt().block_on(self.conn.accept_uni()).map_err(io::Error::other)?;
            self.stream = Some(SyncIoBridge::new_with_handle(s, rt().handle().clone()));
        }
        self.stream.as_mut().expect("accepted").read(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Session, Session) {
        let (offer, listener) = listen().unwrap();
        let pending = listener.accept();
        let client = connect(&offer).unwrap();
        (client, pending.wait(Duration::from_secs(5)).unwrap())
    }

    #[test]
    fn session_carries_control_and_data() {
        let (mut client, mut server) = pair();
        let (mut cr, mut cw) = client.control();
        let (mut sr, mut sw) = server.control();
        cw.write_all(b"ping").unwrap();
        cw.flush().unwrap();
        let mut buf = [0u8; 4];
        sr.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");
        sw.write_all(b"pong").unwrap();
        sw.flush().unwrap();
        cr.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"pong");
        let outs = client.open_data(3).unwrap();
        let ins = server.accept_data(3);
        let writers: Vec<_> = outs
            .into_iter()
            .enumerate()
            .map(|(i, mut w)| std::thread::spawn(move || w.write_all(&vec![i as u8; 100_000]).unwrap()))
            .collect();
        let mut got: Vec<Vec<u8>> = ins
            .into_iter()
            .map(|mut r| {
                let mut v = vec![0u8; 100_000];
                r.read_exact(&mut v).unwrap();
                v
            })
            .collect();
        writers.into_iter().for_each(|t| t.join().unwrap());
        got.sort();
        assert_eq!(got, (0..3u8).map(|i| vec![i; 100_000]).collect::<Vec<_>>());
    }

    #[test]
    fn wrong_fingerprint_is_refused() {
        let (mut offer, listener) = listen().unwrap();
        let _pending = listener.accept();
        offer.fingerprint[0] ^= 1;
        assert!(connect(&offer).is_err());
    }

    #[test]
    fn wrong_token_is_refused() {
        let (mut offer, listener) = listen().unwrap();
        let pending = listener.accept();
        offer.token[31] ^= 1;
        assert!(connect(&offer).is_err());
        assert!(pending.wait(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn unreachable_address_fails_within_the_timeout() {
        let (mut offer, _listener) = listen().unwrap();
        offer.addr = "127.0.0.1:1".into();
        let t = std::time::Instant::now();
        assert!(connect(&offer).is_err());
        assert!(t.elapsed() < CONNECT_TIMEOUT + Duration::from_secs(1));
    }
}
