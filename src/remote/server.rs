//! `barqsync --server`: the remote end, started by the client over ssh. Everything it
//! needs (role, paths, options) arrives in the `Hello` frame on stdin; stdout carries
//! only protocol frames, so all output is forwarded to the client as `Log` frames.

use super::{receiver, sender, streams};
use crate::exit;
use crate::output::Output;
use crate::proto::async_io::FdIo;
use crate::proto::{Conn, Frame, MAGIC, Msg, VERSION};
use crate::timing::mark;

pub fn run() -> i32 {
    let mut conn = match async_stdio() {
        Some(Ok((r, w))) => Conn::start_async(r, w, false),
        Some(Err(e)) => {
            eprintln!("barqsync --server: stdin/stdout: {e}");
            return exit::IPC;
        }
        None => Conn::start(std::io::stdin(), std::io::stdout(), false),
    };
    let (server_is_sender, opts) = match conn.recv() {
        Ok(Frame::Msg(Msg::Hello { magic, version, server_is_sender, opts })) => {
            if magic != MAGIC || version != VERSION {
                let msg = format!(
                    "protocol version mismatch: client speaks {version}, server speaks {VERSION} (barqsync {})",
                    env!("CARGO_PKG_VERSION")
                );
                let _ = conn.msg(Msg::Fatal { msg });
                let _ = conn.finish();
                return exit::INCOMPATIBLE;
            }
            // As rsync's server, which can set creation times on macOS only.
            if opts.crtimes && !cfg!(target_os = "macos") {
                let _ = conn.msg(Msg::Fatal { msg: "The server does not support --crtimes (-N)".into() });
                let _ = conn.finish();
                return exit::SYNTAX;
            }
            let mut opts = opts;
            opts.paths = opts.paths.into_iter().map(expand_tilde).collect();
            (server_is_sender, opts)
        }
        _ => {
            eprintln!("barqsync --server: expected a barqsync client handshake on stdin");
            return exit::PROTOCOL;
        }
    };
    conn.set_compression(opts.compression());
    conn.set_wire(opts.wire);
    crate::fsops::cache::set_enabled(opts.drop_cache);
    crate::entry::read_times(opts.atimes > 0, opts.crtimes);
    crate::fsops::set_open_noatime(opts.open_noatime);
    crate::bwlimit::limit_connections(opts.bwlimit);
    mark("handshake done");
    if conn.msg(Msg::HelloAck { version: VERSION }).is_err() {
        return exit::PROTOCOL;
    }
    if let Err(e) = streams::setup_server(&mut conn, opts.transport, opts.streams, server_is_sender) {
        eprintln!("barqsync --server: setting up parallel streams failed: {e}");
        let _ = conn.msg(Msg::Fatal { msg: format!("parallel stream setup failed: {e}") });
        let _ = conn.finish();
        return exit::PROTOCOL;
    }
    mark("setup done");
    super::set_timeout(opts.timeout);
    let keepalive = super::KeepAlive::start(&conn.out, opts.timeout);
    let fwd = conn.out.clone();
    // In a push the items come from this (receiving) side but describe what the
    // client sent, so they are shown as rsync's sender does (`<f...`).
    let out = Output::forwarding(
        &opts,
        !server_is_sender,
        Box::new(move |stderr, escape, text| {
            let _ = fwd.msg(Msg::Log { stderr, escape, text: text.to_vec() });
        }),
    );
    // A sender with the receiver's final counters knows the receiver has everything,
    // unless it reports removals after them: then the client still has to read those.
    let (code, confirmed) = if server_is_sender {
        let removals = super::reports_removals(&opts, server_is_sender);
        let (code, stats) = sender::run(&conn, &opts, &opts.paths, &out, removals);
        (code, stats.is_some() && !removals)
    } else {
        match opts.paths.first() {
            Some(dest) => (receiver::run(&conn, &opts, dest, &out, false).0, false),
            None => {
                let _ = conn.msg(Msg::Fatal { msg: "no destination given to the server".into() });
                (exit::SYNTAX, false)
            }
        }
    };
    drop(keepalive);
    // Interrupted (here, or the client said so): 20, whatever the transfer loop saw.
    let code = if crate::fsops::interrupted() {
        exit::SIGNAL
    } else if super::timed_out() {
        exit::TIMEOUT
    } else {
        code
    };
    mark("transfer done");
    drop(out);
    let _ = if confirmed { conn.finish_confirmed() } else { conn.finish() };
    mark("connection closed");
    code
}

/// `BARQSYNC_NET_IO=tokio`: stdin and stdout (duplicated) as non-blocking tokio I/O.
fn async_stdio() -> Option<std::io::Result<(FdIo, FdIo)>> {
    use std::os::fd::{FromRawFd, OwnedFd};
    if !crate::proto::async_io::enabled() {
        return None;
    }
    let dup = |fd| {
        let d = unsafe { libc::dup(fd) };
        if d < 0 { Err(std::io::Error::last_os_error()) } else { Ok(unsafe { OwnedFd::from_raw_fd(d) }) }
    };
    Some((|| Ok((FdIo::new(dup(0)?)?, FdIo::new(dup(1)?)?)))())
}

/// A leading `~` or `~user` in a path from the client, as the remote shell would have
/// expanded it (rsync's paths go through it; ours arrive in the handshake). Left alone
/// when there's no such user.
fn expand_tilde(p: std::ffi::OsString) -> std::ffi::OsString {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let b = p.as_bytes();
    if b.first() != Some(&b'~') {
        return p;
    }
    let end = b.iter().position(|&c| c == b'/').unwrap_or(b.len());
    let user = &b[1..end];
    let home = if user.is_empty() {
        std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| h.into_vec()).or_else(|| home_of(None))
    } else {
        home_of(Some(user))
    };
    match home {
        Some(mut h) => {
            h.extend_from_slice(&b[end..]);
            std::ffi::OsString::from_vec(h)
        }
        None => p,
    }
}

/// A user's home directory from the password database (`None`: this process's user).
fn home_of(user: Option<&[u8]>) -> Option<Vec<u8>> {
    let pw = match user {
        Some(u) => {
            let name = std::ffi::CString::new(u).ok()?;
            unsafe { libc::getpwnam(name.as_ptr()) }
        }
        None => unsafe { libc::getpwuid(libc::geteuid()) },
    };
    if pw.is_null() {
        return None;
    }
    let dir = unsafe { std::ffi::CStr::from_ptr((*pw).pw_dir) };
    Some(dir.to_bytes().to_vec()).filter(|d| !d.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn tilde_paths_are_expanded_like_a_shell_would() {
        let home = std::env::var("HOME").unwrap();
        let x = |s: &str| expand_tilde(OsString::from(s)).into_string().unwrap();
        assert_eq!(x("~"), home);
        assert_eq!(x("~/dir/"), format!("{home}/dir/"));
        assert_eq!(x("dir/~x"), "dir/~x");
        assert_eq!(x("~no-such-user-here/x"), "~no-such-user-here/x");
        let me = unsafe { std::ffi::CStr::from_ptr((*libc::getpwuid(libc::geteuid())).pw_name) }.to_str().unwrap().to_string();
        let mine = home_of(None).map(|h| String::from_utf8(h).unwrap()).unwrap();
        assert_eq!(x(&format!("~{me}/a")), format!("{mine}/a"));
    }
}
