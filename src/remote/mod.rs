//! Remote transfers over a remote shell (ssh by default), like rsync's: the client
//! runs `barqsync --server` on the other machine and the two talk the protocol in
//! [`crate::proto`] over its stdin/stdout. Either side can be the sender; both sides
//! run the same [`sender`] and [`receiver`] code.

pub mod adapt;
pub mod auto;
pub mod check;
pub mod client;
pub mod receiver;
pub mod sender;
pub mod server;
pub mod quic;
pub mod relay;
pub mod streams;

use crate::fsops;
use crate::proto::{Conn, Frame, FrameSender, Msg, clock};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::time::Duration;

/// How file data travels in a remote transfer (`--transport`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, clap::ValueEnum)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum Transport {
    /// Everything on the ssh pipe.
    Ssh,
    /// File data on `--streams` TLS-over-TCP connections, control on ssh.
    Tcp,
    /// A QUIC connection: control and metadata on one stream, file data on `--streams`
    /// more; ssh only to start it.
    Quic,
    /// QUIC on long links when it connects quickly, the ssh pipe otherwise (see [`auto`]).
    Auto,
}

impl Transport {
    pub const DEFAULT: Transport = Transport::Ssh;
    pub const TCP_STREAMS: usize = 4;
    pub const QUIC_STREAMS: usize = 16;
    pub const MAX_STREAMS: usize = 128;

    /// Whether this transport can end up on QUIC (and takes QUIC's stream count).
    pub fn quic(self) -> bool {
        matches!(self, Transport::Quic | Transport::Auto)
    }
}

/// `--strategy`: adapt routing and compression to a measured link (see [`adapt`]), or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, clap::ValueEnum)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum Strategy {
    Adaptive,
    Fixed,
}

impl Strategy {
    pub const DEFAULT: Strategy = Strategy::Fixed;
}

impl crate::cli::Options {
    /// How this side compresses what it sends.
    pub fn compression(&self) -> adapt::Compression {
        adapt::Compression::new(self.compress, self.strategy == Strategy::Adaptive, self.compress_level)
    }
}

/// `--remove-source-files` in a pull: the sender (the server) removes the last files after
/// the receiver's final `ReceiverDone`, so their output and errors would never reach the
/// client. The sender then ends with `RemovalsDone`, which the client reads before it
/// finishes, and the two swap who closes first. A push removes on the client, which
/// prints them itself.
pub(crate) fn reports_removals(opts: &crate::cli::Options, server_is_sender: bool) -> bool {
    server_is_sender && opts.remove_source_files && !opts.dry_run
}

/// Worker stack size. Tasks block on the connection's backpressure, and rayon lets a
/// waiting thread run other tasks on its stack, so allow for deep nesting.
pub(crate) const WORKER_STACK: usize = 32 << 20;

/// Small-file packs being written at once on the receiver. When this many are queued,
/// the receiver stops reading, which pushes back on the sender through the pipe.
pub(crate) const MAX_PACKS_IN_FLIGHT: usize = 16;

pub(crate) enum Stop {
    Interrupted,
    Io(io::Error),
}

/// The first message this side could not send because it is too large to (see
/// [`crate::proto::MAX_MESSAGE`]). The peer may be waiting for it, so the transfer stops:
/// the peer is told why, and [`recv_unless`] gives up on this side. A closed connection
/// is left to the receive loop, which sees it too.
#[derive(Default)]
pub(crate) struct SendFailure(std::sync::OnceLock<Vec<u8>>);

impl SendFailure {
    /// Send `m`; `what` names it if that fails. Returns whether it was sent.
    pub(crate) fn send(&self, out: &FrameSender, m: Msg, what: impl FnOnce() -> Vec<u8>) -> bool {
        match out.msg(m) {
            Ok(()) => true,
            // The connection is gone, or the run is stopping anyway.
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe || out.stopped() => false,
            Err(e) => {
                let msg = [&b"sending "[..], &what(), format!(" failed: {e}").as_bytes()].concat();
                if self.0.set(msg.clone()).is_ok() {
                    let _ = out.msg(Msg::Fatal { msg: String::from_utf8_lossy(&msg).into_owned() });
                }
                false
            }
        }
    }

    /// Why the transfer stopped, once it has.
    pub(crate) fn reason(&self) -> Option<&[u8]> {
        self.0.get().map(Vec::as_slice)
    }
}

/// Next incoming frame, giving up early on Ctrl-C.
pub(crate) fn recv(conn: &Conn) -> Result<Frame, Stop> {
    recv_until(conn, None)
}

/// [`recv`], also giving up once a message could not be sent (see [`SendFailure`]).
pub(crate) fn recv_unless(conn: &Conn, failed: &SendFailure) -> Result<Frame, Stop> {
    recv_checked(conn, None, Some(failed))
}

/// The other side said it was interrupted (so this side need not tell it).
static PEER_INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// `--timeout` in seconds (0: none), once the options are agreed.
static TIMEOUT: AtomicU64 = AtomicU64::new(0);
/// When the last frame arrived, in milliseconds since [`clock`]'s start.
static LAST_IN: AtomicU64 = AtomicU64::new(0);
/// The transfer ended because nothing arrived for `--timeout` seconds.
static TIMED_OUT: AtomicBool = AtomicBool::new(false);


/// `--timeout`: from now on, a connection that delivers nothing for `secs` seconds ends
/// the transfer (exit 30). Each side sends [`KeepAlive`]s meanwhile, so only a
/// connection that stopped moving data trips it.
pub(crate) fn set_timeout(secs: u64) {
    LAST_IN.store(clock(), SeqCst);
    TIMEOUT.store(secs, SeqCst);
}

/// Whether `--timeout` ended the transfer.
pub fn timed_out() -> bool {
    TIMED_OUT.load(SeqCst)
}

/// The error ending a connection that has been silent for longer than `--timeout`.
fn check_timeout() -> Option<io::Error> {
    let secs = TIMEOUT.load(SeqCst);
    // A frame still arriving is activity too.
    let idle = clock().saturating_sub(LAST_IN.load(SeqCst).max(crate::proto::last_read()));
    if secs == 0 || idle < secs * 1000 {
        return None;
    }
    TIMED_OUT.store(true, SeqCst);
    Some(io::Error::new(io::ErrorKind::TimedOut, format!("io timeout after {} seconds -- exiting", idle / 1000)))
}

/// `--timeout`: sends `KeepAlive` every third of the timeout while it lives, when there
/// is room (a full queue means data is moving), so that a side busy scanning, hashing
/// or deleting never looks dead to the other.
pub(crate) struct KeepAlive(Option<(crossbeam_channel::Sender<()>, std::thread::JoinHandle<()>)>);

impl KeepAlive {
    pub(crate) fn start(out: &FrameSender, secs: u64) -> KeepAlive {
        if secs == 0 {
            return KeepAlive(None);
        }
        let (stop, wait) = crossbeam_channel::bounded::<()>(0);
        let out = out.clone();
        let every = Duration::from_millis((secs * 1000 / 3).max(200));
        let thread = std::thread::Builder::new()
            .name("barqsync-keepalive".into())
            .spawn(move || {
                while let Err(crossbeam_channel::RecvTimeoutError::Timeout) = wait.recv_timeout(every) {
                    out.try_msg(Msg::KeepAlive);
                }
            })
            .expect("spawn keepalive thread");
        KeepAlive(Some((stop, thread)))
    }
}

impl Drop for KeepAlive {
    fn drop(&mut self) {
        if let Some((stop, thread)) = self.0.take() {
            drop(stop);
            let _ = thread.join();
        }
    }
}

/// [`recv`], giving up at `deadline`. Interrupted here, it tells the peer; told by the
/// peer, it stops as if interrupted here.
pub(crate) fn recv_until(conn: &Conn, deadline: Option<std::time::Instant>) -> Result<Frame, Stop> {
    recv_checked(conn, deadline, None)
}

fn recv_checked(conn: &Conn, deadline: Option<std::time::Instant>, failed: Option<&SendFailure>) -> Result<Frame, Stop> {
    loop {
        if fsops::interrupted() {
            if !PEER_INTERRUPTED.load(SeqCst) {
                conn.out.tell_interrupted();
            }
            return Err(Stop::Interrupted);
        }
        if deadline.is_some_and(|d| std::time::Instant::now() > d) {
            return Err(Stop::Io(io::Error::new(io::ErrorKind::TimedOut, "no answer from the peer")));
        }
        if let Some(why) = failed.and_then(SendFailure::reason) {
            return Err(Stop::Io(io::Error::other(String::from_utf8_lossy(why).into_owned())));
        }
        match conn.recv_timeout(Duration::from_millis(200)) {
            Ok(Some(Frame::Msg(Msg::Interrupted))) => {
                PEER_INTERRUPTED.store(true, SeqCst);
                fsops::raise_interrupt();
                eprintln!("barqsync: the remote side was interrupted");
                return Err(Stop::Interrupted);
            }
            Ok(Some(Frame::Msg(Msg::KeepAlive))) => LAST_IN.store(clock(), SeqCst),
            Ok(Some(f)) => {
                LAST_IN.store(clock(), SeqCst);
                return Ok(f);
            }
            Ok(None) => {
                if let Some(e) = check_timeout() {
                    return Err(Stop::Io(e));
                }
            }
            Err(e) => return Err(Stop::Io(e)),
        }
    }
}
