//! Remote transfers over a remote shell (ssh by default), like rsync's: the client
//! runs `fastrsync --server` on the other machine and the two talk the protocol in
//! [`crate::proto`] over its stdin/stdout. Either side can be the sender; both sides
//! run the same [`sender`] and [`receiver`] code.

pub mod client;
pub mod receiver;
pub mod sender;
pub mod server;

use crate::fsops;
use crate::proto::{Conn, Frame};
use crossbeam_channel::RecvTimeoutError;
use std::io;
use std::time::Duration;

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
    loop {
        if fsops::interrupted() {
            return Err(Stop::Interrupted);
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
