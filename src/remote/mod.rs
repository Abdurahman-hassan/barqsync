//! Remote transfers over a remote shell (ssh by default), like rsync's: the client
//! runs `fastrsync --server` on the other machine and the two talk the protocol in
//! [`crate::proto`] over its stdin/stdout. Either side can be the sender; both sides
//! run the same [`sender`] and [`receiver`] code.

pub mod adapt;
pub mod client;
pub mod receiver;
pub mod sender;
pub mod server;
pub mod quic;
pub mod streams;

use crate::fsops;
use crate::proto::{Conn, Frame};
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
}

impl Transport {
    pub const DEFAULT: Transport = Transport::Ssh;
    pub const TCP_STREAMS: usize = 4;
    pub const QUIC_STREAMS: usize = 16;
    pub const MAX_STREAMS: usize = 128;
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

/// Next incoming frame, giving up early on Ctrl-C.
pub(crate) fn recv(conn: &Conn) -> Result<Frame, Stop> {
    recv_until(conn, None)
}

/// [`recv`], giving up at `deadline`.
pub(crate) fn recv_until(conn: &Conn, deadline: Option<std::time::Instant>) -> Result<Frame, Stop> {
    loop {
        if fsops::interrupted() {
            return Err(Stop::Interrupted);
        }
        if deadline.is_some_and(|d| std::time::Instant::now() > d) {
            return Err(Stop::Io(io::Error::new(io::ErrorKind::TimedOut, "no answer from the peer")));
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
