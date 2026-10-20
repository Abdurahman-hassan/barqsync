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
use crate::proto::{Conn, Frame, FrameSender, Msg};
use crossbeam_channel::RecvTimeoutError;
use std::io;
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
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => false,
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

/// [`recv`], giving up at `deadline`.
pub(crate) fn recv_until(conn: &Conn, deadline: Option<std::time::Instant>) -> Result<Frame, Stop> {
    recv_checked(conn, deadline, None)
}

fn recv_checked(conn: &Conn, deadline: Option<std::time::Instant>, failed: Option<&SendFailure>) -> Result<Frame, Stop> {
    loop {
        if fsops::interrupted() {
            return Err(Stop::Interrupted);
        }
        if deadline.is_some_and(|d| std::time::Instant::now() > d) {
            return Err(Stop::Io(io::Error::new(io::ErrorKind::TimedOut, "no answer from the peer")));
        }
        if let Some(why) = failed.and_then(SendFailure::reason) {
            return Err(Stop::Io(io::Error::other(String::from_utf8_lossy(why).into_owned())));
        }
        match conn.rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(f)) => return Ok(f),
            Ok(Err(e)) => return Err(Stop::Io(e)),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                return Err(Stop::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed by peer")));
            }
        }
    }
}
