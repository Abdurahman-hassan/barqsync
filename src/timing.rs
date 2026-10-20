//! `BARQSYNC_TIMING=1`: milestones of a remote transfer on stderr, to see where the time
//! goes (setting up a transport, the transfer itself, closing the connection). The client
//! passes the variable on to the server's command line, and the server's stderr reaches
//! the client through ssh. Each line has the wall-clock time, to line both sides up when
//! they share a clock, and the time since the process started:
//!
//! ```text
//! barqsync timing: client 1760948040.123456 +412.3 ms handshake done
//! ```

use std::fmt::Display;
use std::io::Write;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const VAR: &str = "BARQSYNC_TIMING";

static STATE: OnceLock<Option<(Instant, &'static str)>> = OnceLock::new();

/// Start the clock (once, first thing in a run).
pub fn init(server: bool) {
    STATE.get_or_init(|| {
        let on = std::env::var_os(VAR).is_some_and(|v| !v.is_empty() && v != "0");
        on.then(|| (Instant::now(), if server { "server" } else { "client" }))
    });
}

pub fn enabled() -> bool {
    matches!(STATE.get(), Some(Some(_)))
}

/// Print one milestone (nothing unless enabled). Pass `format_args!` for details.
pub fn mark(what: impl Display) {
    let Some(Some((start, side))) = STATE.get() else { return };
    let wall = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let line = format!(
        "barqsync timing: {side} {}.{:06} +{:.1} ms {what}\n",
        wall.as_secs(),
        wall.subsec_micros(),
        start.elapsed().as_secs_f64() * 1e3
    );
    // One write, so lines from both sides sharing a pipe don't interleave.
    let _ = std::io::stderr().write_all(line.as_bytes());
}
