//! Client side of a remote transfer: start `barqsync --server` through the remote
//! shell, send it the role, paths and options, then run the opposite role locally.

use super::{Stop, Transport, auto, receiver, recv, relay, sender, streams};
use crate::cli::Options;
use crate::exit;
use crate::output::{Output, Stats};
use crate::paths::{Location, parse_location};
use crate::proto::{Conn, Frame, MAGIC, Msg, VERSION};
use std::ffi::OsString;
use std::io;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

/// Start the remote shell running `<remote-path> --server` on `host`.
fn spawn_server(shell: &relay::Shell, opts: &Options) -> io::Result<Child> {
    shell.server(opts.net_io_tokio).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn()
}

/// What to tell the user when the server's side ended before the handshake: the remote
/// shell's exit status says whether barqsync was there at all.
fn no_handshake_message(status: Option<i32>, remote: &str) -> String {
    match status {
        Some(127) => format!(
            "the remote shell could not find `{remote}` (exit 127): install barqsync there, or give its \
             full path with --remote-path (a PATH set in ~/.zshrc or ~/.bashrc isn't read for ssh commands; \
             ~/.zshenv is)"
        ),
        _ => "connection unexpectedly closed before the handshake \
              (is barqsync installed on the remote host? see --remote-path)"
            .to_string(),
    }
}

pub fn run(opts: &Options, sources: &[OsString], dest: &OsString) -> i32 {
    let out: &'static Output = Box::leak(Box::new(Output::new(opts)));
    let started = Instant::now();

    // Push (local sources, remote destination) or pull (remote sources on one host,
    // local destination). An empty remote path means the remote home directory.
    let remote_path = |p: OsString| if p.is_empty() { OsString::from(".") } else { p };
    let src_locs: Vec<Location> = sources.iter().map(|s| parse_location(s)).collect();
    let (host, server_is_sender, server_paths) = match parse_location(dest) {
        Location::Remote { host, path } => {
            if src_locs.iter().any(|l| matches!(l, Location::Remote { .. })) {
                out.error("source and destination cannot both be remote");
                return exit::SYNTAX;
            }
            (host, false, vec![remote_path(path)])
        }
        Location::Local(_) => {
            let mut host = None;
            let mut paths = Vec::new();
            for l in src_locs {
                match l {
                    Location::Remote { host: h, path } => {
                        if host.as_ref().is_some_and(|x| *x != h) {
                            out.error("all remote sources must be on the same host");
                            return exit::SYNTAX;
                        }
                        host = Some(h);
                        paths.push(remote_path(path));
                    }
                    Location::Local(_) => {
                        out.error("cannot mix local and remote sources");
                        return exit::SYNTAX;
                    }
                }
            }
            (host.expect("a remote source"), true, paths)
        }
    };

    if host.starts_with('@') {
        out.error(format!("no user name before '@' in {host:?} (an empty variable?)"));
        return exit::SYNTAX;
    }
    let shell = match relay::Shell::new(opts, &host) {
        Ok(s) => s,
        Err(e) => {
            out.error(format!("failed to run the remote shell: {e}"));
            return exit::IPC;
        }
    };
    let mut child = match spawn_server(&shell, opts) {
        Ok(c) => c,
        Err(e) => {
            out.error(format!("failed to run the remote shell: {e}"));
            return exit::IPC;
        }
    };
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        out.error("remote shell has no stdin/stdout");
        return exit::IPC;
    };
    let mut conn = if crate::proto::async_io::enabled() {
        use crate::proto::async_io::FdIo;
        match (FdIo::new(stdout.into()), FdIo::new(stdin.into())) {
            (Ok(r), Ok(w)) => Conn::start_async(r, w, opts.compress),
            (Err(e), _) | (_, Err(e)) => {
                out.error(format!("remote shell pipes: {e}"));
                return exit::IPC;
            }
        }
    } else {
        Conn::start(stdout, stdin, opts.compress)
    };
    // Hello itself always goes out as postcard; everything after it in this format.
    conn.set_wire(opts.wire);
    conn.set_compression(opts.compression());
    // `auto`: a host where QUIC just failed stays on the ssh pipe for a while. The server
    // only needs to know that QUIC may be used.
    let memory = auto::Memory::new();
    let mut transport = opts.transport;
    if transport == Transport::Auto && opts.streams > 0 && memory.unreachable(&host) {
        if out.verbose() > 1 {
            out.info(format!("auto: QUIC to {host} failed recently, using the ssh connection"));
        }
        transport = Transport::Ssh;
    }
    let mut server_opts = opts.clone();
    server_opts.paths = server_paths;
    server_opts.transport = if transport == Transport::Auto { Transport::Quic } else { transport };
    let hello = Msg::Hello { magic: MAGIC, version: VERSION, server_is_sender, opts: Box::new(server_opts) };
    let handshake = conn.msg(hello).map_err(Stop::Io).and_then(|()| recv(&conn));
    match handshake {
        Ok(Frame::Msg(Msg::HelloAck { .. })) => {}
        Ok(Frame::Msg(Msg::Fatal { msg })) => {
            out.error(format!("remote: {msg}"));
            if msg.contains("version mismatch") {
                out.error(format!(
                    "this side is barqsync {} (protocol {VERSION}): install the same version on both machines",
                    env!("CARGO_PKG_VERSION")
                ));
            }
            let _ = child.kill();
            return exit::INCOMPATIBLE;
        }
        Ok(other) => {
            out.error(format!("protocol error: expected a handshake reply, got {other:?}"));
            let _ = child.kill();
            return exit::PROTOCOL;
        }
        Err(Stop::Interrupted) => {
            let _ = child.kill();
            return exit::SIGNAL;
        }
        Err(Stop::Io(_)) => {
            drop(conn);
            let status = child.wait().ok().and_then(|s| s.code());
            out.error(no_handshake_message(status, opts.remote_path.as_deref().unwrap_or("barqsync")));
            return if status == Some(255) { exit::RSH } else { exit::PROTOCOL };
        }
    }

    match streams::setup_client(&mut conn, transport, opts.streams, !server_is_sender, Some(&shell), out) {
        Ok(Some(auto::Outcome::Unreachable)) => memory.remember(&host),
        Ok(Some(auto::Outcome::Quic)) => memory.forget(&host),
        Ok(_) => {}
        Err(e) => {
            out.error(format!("setting up parallel streams failed: {e}"));
            let _ = child.kill();
            return exit::PROTOCOL;
        }
    }

    let reporter = opts.progress.then(|| crate::progress::Reporter::start(out, opts.human));
    let (code, stats) = if server_is_sender {
        let (code, stats) = receiver::run(&conn, opts, dest, out);
        (code, Some(stats))
    } else {
        sender::run(&conn, opts, sources, out)
    };
    if let Some(r) = reporter {
        r.finish(out);
    }
    let _ = conn.finish();
    let _ = child.wait();
    relay::reap(Duration::from_secs(5));
    out.flush();
    if let Some(s) = stats
        && (opts.stats || opts.verbose > 0)
        && code != exit::SIGNAL
    {
        let totals = Stats::default();
        totals.absorb(&s);
        totals.print(out, started.elapsed(), opts.human, opts.stats);
    }
    out.flush();
    code
}
