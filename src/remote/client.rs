//! Client side of a remote transfer: start `barqsync --server` through the remote
//! shell, send it the role, paths and options, then run the opposite role locally.

use super::{Stop, Transport, auto, quic, receiver, recv, sender, streams};
use crate::cli::Options;
use crate::exit;
use crate::output::{Output, Stats};
use crate::paths::{Location, parse_location};
use crate::proto::{Conn, Frame, MAGIC, Msg, VERSION};
use crate::timing::mark;
use std::ffi::OsString;
use std::io;
use std::process::{Child, Command, Stdio};
use std::time::Instant;

/// Start the remote shell running `<remote-path> --server` on `host`.
fn spawn_server(opts: &Options, host: &str) -> io::Result<Child> {
    let rsh = opts
        .rsh
        .clone()
        .or_else(|| std::env::var("BARQSYNC_RSH").ok())
        .unwrap_or_else(|| "ssh".to_string());
    let mut argv = shlex::split(&rsh)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("cannot parse remote shell {rsh:?}")))?;
    let program = argv.remove(0);
    let remote = opts.remote_path.clone().unwrap_or_else(|| "barqsync".to_string());
    let quoted = shlex::try_quote(&remote)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("bad --remote-path: {e}")))?;
    let mut command = if opts.net_io_tokio { format!("{quoted} --server --net-io=tokio") } else { format!("{quoted} --server") };
    if crate::timing::enabled() {
        command = format!("env {}=1 {command}", crate::timing::VAR);
    }
    Command::new(program)
        .args(argv)
        .arg(host)
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
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

    let mut child = match spawn_server(opts, &host) {
        Ok(c) => c,
        Err(e) => {
            out.error(format!("failed to run the remote shell: {e}"));
            return exit::IPC;
        }
    };
    mark("remote shell started");
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
    // `auto`: a host where QUIC just failed, or just found a clean link, stays on the ssh
    // pipe for a while. The server only needs to know that QUIC may be used.
    let memory = auto::Memory::new();
    let mut transport = opts.transport;
    if transport == Transport::Auto && opts.streams > 0 {
        let why = if memory.has(auto::Mark::Unreachable, &host) {
            Some("failed recently")
        } else if memory.has(auto::Mark::Clean, &host) {
            Some("lost no packets lately")
        } else {
            None
        };
        if let Some(why) = why {
            if out.verbose() > 1 {
                out.info(format!("auto: QUIC to {host} {why}, using the ssh connection"));
            }
            transport = Transport::Ssh;
        }
    }
    let mut server_opts = opts.clone();
    server_opts.paths = server_paths;
    server_opts.transport = if transport == Transport::Auto { Transport::Quic } else { transport };
    let hello = Msg::Hello { magic: MAGIC, version: VERSION, server_is_sender, opts: Box::new(server_opts) };
    let handshake = conn.msg(hello).map_err(Stop::Io).and_then(|()| recv(&conn));
    match handshake {
        Ok(Frame::Msg(Msg::HelloAck { .. })) => mark("handshake done"),
        Ok(Frame::Msg(Msg::Fatal { msg })) => {
            out.error(format!("remote: {msg}"));
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
            out.error(
                "connection unexpectedly closed before the handshake \
                 (is barqsync installed on the remote host? see --remote-path)",
            );
            return if status == Some(255) { exit::RSH } else { exit::PROTOCOL };
        }
    }

    let chosen = match streams::setup_client(&mut conn, transport, opts.streams, !server_is_sender, out) {
        Ok(outcome) => outcome,
        Err(e) => {
            out.error(format!("setting up parallel streams failed: {e}"));
            let _ = child.kill();
            return exit::PROTOCOL;
        }
    };
    match chosen {
        Some(auto::Outcome::Unreachable) => memory.remember(auto::Mark::Unreachable, &host),
        Some(auto::Outcome::Quic) => memory.forget(auto::Mark::Unreachable, &host),
        _ => {}
    }

    mark("setup done");
    let reporter = opts.progress.then(|| crate::progress::Reporter::start(out, opts.human));
    // `confirmed`: this side has the peer's last frame, so the peer has everything this
    // side sent: the sender (of a push) once it has `ReceiverDone`, the receiver of a pull
    // that reports removals once it has `RemovalsDone`.
    let (code, stats, confirmed) = if server_is_sender {
        let removals = super::reports_removals(opts, server_is_sender);
        let (code, stats, confirmed) = receiver::run(&conn, opts, dest, out, removals);
        (code, Some(stats), confirmed)
    } else {
        let (code, stats) = sender::run(&conn, opts, sources, out, false);
        let confirmed = stats.is_some();
        (code, stats, confirmed)
    };
    if let Some(r) = reporter {
        r.finish(out);
    }
    mark("transfer done");
    if chosen == Some(auto::Outcome::Quic)
        && code != exit::SIGNAL
        && let Some(session) = conn.session::<quic::Session>()
    {
        learn(&memory, &host, &session.seen(), out);
    }
    let _ = if confirmed { conn.finish_confirmed() } else { conn.finish() };
    mark("connection closed");
    let _ = child.wait();
    mark("remote shell exited");
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

/// `auto`, after a transfer over QUIC: remember a clean link (the ssh pipe is then used for
/// a while), forget it after a lossy one.
fn learn(memory: &auto::Memory, host: &str, seen: &auto::Seen, out: &Output) {
    let verdict = auto::verdict(seen);
    match verdict {
        Some(auto::Verdict::Clean) => memory.remember(auto::Mark::Clean, host),
        Some(auto::Verdict::Lossy | auto::Verdict::Fast) => memory.forget(auto::Mark::Clean, host),
        None => {}
    }
    if out.verbose() > 1 {
        let next = match verdict {
            Some(auto::Verdict::Clean) => "a clean link: the ssh connection next time",
            Some(auto::Verdict::Lossy) => "QUIC next time too",
            Some(auto::Verdict::Fast) => "faster than the ssh connection's window allows: QUIC next time too",
            None => "too few to tell",
        };
        out.info(format!("auto: QUIC lost {} of {} packets, {next}", seen.lost_packets, seen.sent_packets));
    }
}
