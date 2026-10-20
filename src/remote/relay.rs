//! Parallel streams through ssh. When the direct TCP connections to the server's stream
//! listener fail (a firewall on the remote host, a NAT or a VPN that only lets ssh in),
//! each stream travels over a separate ssh connection instead: the remote shell runs
//! `barqsync --stream-relay ADDR[,ADDR]`, which passes bytes between its stdin/stdout
//! and the listener, on that host. TLS with the pinned certificate and the token still
//! runs end to end between client and server; the relay only ever sees ciphertext.
//!
//! Every stream is its own ssh (so TCP) connection, so a link is still filled by several
//! connections, and nothing on the remote host needs to accept a new port. With ssh as
//! the remote shell, relays never prompt (`BatchMode`) and never share a multiplexed
//! master connection (`ControlPath=none`): without key or agent authentication they
//! fail, and the transfer stays on the main ssh pipe.

use super::streams::{self, DataStream};
use crate::cli::Options;
use crate::proto::StreamOffer;
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::os::fd::FromRawFd;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// Longest wait for all relays (ssh connection, authentication, TLS through it).
const RELAY_TIMEOUT: Duration = Duration::from_secs(20);
/// What the relay allows itself to reach the listener.
const RELAY_CONNECT: Duration = Duration::from_secs(5);

/// The remote shell and the remote barqsync, as the client starts them.
pub struct Shell {
    program: String,
    args: Vec<String>,
    host: String,
    /// `--remote-path` (or `barqsync`), ready for the remote shell's command line.
    remote: String,
}

impl Shell {
    pub fn new(opts: &Options, host: &str) -> io::Result<Shell> {
        let rsh = opts
            .rsh
            .clone()
            .or_else(|| std::env::var("BARQSYNC_RSH").ok())
            .unwrap_or_else(|| "ssh".to_string());
        let mut args = shlex::split(&rsh)
            .filter(|a| !a.is_empty())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("cannot parse remote shell {rsh:?}")))?;
        let program = args.remove(0);
        let remote = remote_command(opts.remote_path.as_deref().unwrap_or("barqsync"))?;
        Ok(Shell { program, args, host: host.to_string(), remote })
    }

    fn is_ssh(&self) -> bool {
        Path::new(&self.program).file_name().is_some_and(|n| n == "ssh")
    }

    /// The remote shell running `<env><remote barqsync> <args>` on the host.
    fn command(&self, extra: &[&str], env: &str, args: &str) -> Command {
        let mut c = Command::new(&self.program);
        c.args(extra).args(&self.args).arg(&self.host).arg(format!("{env}{} {args}", self.remote));
        c
    }

    /// `barqsync --server` on the host, passed `BARQSYNC_TIMING` (see [`crate::timing`]).
    pub fn server(&self, net_io_tokio: bool) -> Command {
        let env = if crate::timing::enabled() { format!("env {}=1 ", crate::timing::VAR) } else { String::new() };
        self.command(&[], &env, if net_io_tokio { "--server --net-io=tokio" } else { "--server" })
    }

    /// A relay to `targets` on the host. ssh options given first win over later ones
    /// and over its configuration files.
    fn relay(&self, targets: &str) -> io::Result<Command> {
        let quoted = shlex::try_quote(targets).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let ssh: &[&str] = &["-o", "BatchMode=yes", "-o", "ControlMaster=no", "-o", "ControlPath=none", "-o", "ConnectTimeout=10"];
        Ok(self.command(if self.is_ssh() { ssh } else { &[] }, "", &format!("--stream-relay {quoted}")))
    }
}

/// A remote path for the remote shell's command line: quoted, except for a leading `~`
/// or `~user`, which the remote shell expands (as rsync leaves it).
pub fn remote_command(path: &str) -> io::Result<String> {
    let quote = |s: &str| {
        shlex::try_quote(s)
            .map(|q| q.into_owned())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("bad --remote-path: {e}")))
    };
    if let Some(rest) = path.strip_prefix('~') {
        let (user, tail) = rest.split_once('/').map_or((rest, None), |(u, t)| (u, Some(t)));
        if user.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) {
            return Ok(match tail {
                Some("") => format!("~{user}/"),
                Some(t) => format!("~{user}/{}", quote(t)?),
                None => format!("~{user}"),
            });
        }
    }
    quote(path)
}

/// Where the relay looks for the listener: loopback on the listener's port first (a
/// firewall never filters it), then the address the server advertised.
fn targets(addr: &str) -> io::Result<String> {
    let a: SocketAddr = addr.parse().map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let lo = SocketAddr::new(streams::loopback_for(a.ip()), a.port());
    Ok(if lo == a { a.to_string() } else { format!("{lo},{a}") })
}

/// Relay processes started by this client, waited for by [`reap`].
static RELAYS: Mutex<Vec<Child>> = Mutex::new(Vec::new());

/// A relay's pipes as one byte stream.
struct Pipe {
    from: ChildStdout,
    to: ChildStdin,
}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.from.read(buf)
    }
}

impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.to.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.to.flush()
    }
}

/// Client side: open up to `n` authenticated streams through relays. Like
/// [`streams::connect`], failed ones are dropped and an error says why none opened.
pub fn connect(shell: &Shell, offer: &StreamOffer, n: usize) -> io::Result<Vec<DataStream>> {
    let targets = targets(&offer.addr)?;
    let config = streams::client_config(offer)?;
    let deadline = Instant::now() + RELAY_TIMEOUT;
    let pids = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel();
    for i in 0..n {
        let mut cmd = shell.relay(&targets)?;
        let (tx, config, token, pids) = (tx.clone(), config.clone(), offer.token, pids.clone());
        std::thread::spawn(move || {
            let r = open(&mut cmd, &pids, |pipe| streams::client_handshake(pipe, config, &token, i as u16));
            let _ = tx.send(r);
        });
    }
    drop(tx);
    let mut results = Vec::with_capacity(n);
    while results.len() < n {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(r) => results.push(r),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Stuck relays: stop them, which ends their handshakes with an error.
                for &pid in pids.lock().unwrap().iter() {
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                }
                results.extend(rx.iter());
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let (streams, errors): (Vec<_>, Vec<_>) = results.into_iter().partition(Result::is_ok);
    match errors.into_iter().next() {
        Some(Err(e)) if streams.is_empty() => Err(e),
        _ => Ok(streams.into_iter().flatten().collect()),
    }
}

/// Start one relay and authenticate a stream through it; what ssh printed explains a
/// failure.
fn open<S: streams::Stream + 'static>(
    cmd: &mut Command,
    pids: &Mutex<Vec<u32>>,
    handshake: impl FnOnce(Pipe) -> io::Result<S>,
) -> io::Result<DataStream> {
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let pipe = Pipe { from: child.stdout.take().expect("piped"), to: child.stdin.take().expect("piped") };
    let mut stderr = child.stderr.take().expect("piped");
    pids.lock().unwrap().push(child.id());
    RELAYS.lock().unwrap().push(child);
    let said = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut stderr).take(4096).read_to_end(&mut buf);
        // Keep draining so a chatty ssh never blocks.
        let _ = io::copy(&mut stderr, &mut io::sink());
        buf
    });
    match handshake(pipe) {
        Ok(s) => Ok(DataStream::Sync(Box::new(s))),
        Err(e) => {
            let text = said.join().unwrap_or_default();
            let line = String::from_utf8_lossy(&text);
            let line = line.lines().map(str::trim).find(|l| !l.is_empty());
            Err(match line {
                Some(l) => io::Error::new(e.kind(), l.to_string()),
                None if e.kind() == io::ErrorKind::UnexpectedEof => {
                    io::Error::new(e.kind(), "the relay closed the connection")
                }
                None => e,
            })
        }
    }
}

/// Wait for the relays to exit (they do once both ends have closed their stream), at
/// most `timeout`; stop the rest.
pub fn reap(timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut relays = std::mem::take(&mut *RELAYS.lock().unwrap());
    while !relays.is_empty() && Instant::now() < deadline {
        relays.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
        std::thread::sleep(Duration::from_millis(10));
    }
    for mut c in relays {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// `barqsync --stream-relay ADDR[,ADDR]` (remote side): connect to the first address
/// that answers and pass bytes both ways until the listener's side closes.
pub fn run(targets: &str) -> i32 {
    let sock = targets.split(',').find_map(|t| {
        let addr: SocketAddr = t.trim().parse().ok()?;
        TcpStream::connect_timeout(&addr, RELAY_CONNECT).ok()
    });
    let Some(mut sock) = sock else {
        eprintln!("barqsync --stream-relay: could not connect to {targets}");
        return crate::exit::IPC;
    };
    let _ = sock.set_nodelay(true);
    let Ok(mut up) = sock.try_clone() else { return crate::exit::IPC };
    // Unbuffered stdin/stdout: a relay must not hold bytes back.
    let mut stdin = unsafe { File::from_raw_fd(0) };
    let mut stdout = unsafe { File::from_raw_fd(1) };
    std::thread::spawn(move || {
        let _ = pump(&mut stdin, &mut up);
        let _ = up.shutdown(Shutdown::Write);
    });
    // Done when the listener's side is: the client's half may still be open, and the
    // thread copying it would otherwise wait for ssh to close it.
    match pump(&mut sock, &mut stdout) {
        Ok(()) => crate::exit::OK,
        Err(_) => crate::exit::IPC,
    }
}

/// Pass everything from `from` to `to` as it arrives. Plain reads and writes, not
/// `io::copy`: on Linux that splices between the pipe and the socket, and through sshd's
/// pipes the TLS handshake then stalled after the server's first flight
/// (`bench/net/firewall.sh`).
fn pump(from: &mut impl Read, to: &mut impl Write) -> io::Result<()> {
    let mut buf = vec![0u8; 256 << 10];
    loop {
        match from.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => to.write_all(&buf[..n])?,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_paths_keep_a_leading_tilde_for_the_shell() {
        let r = |p| remote_command(p).unwrap();
        assert_eq!(r("barqsync"), "barqsync");
        assert_eq!(r("~/.cargo/bin/barqsync"), "~/.cargo/bin/barqsync");
        assert_eq!(r("~/my bin/barqsync"), "~/'my bin/barqsync'");
        assert_eq!(r("~alice/bin/barqsync"), "~alice/bin/barqsync");
        assert_eq!(r("~"), "~");
        assert_eq!(r("/opt/a b/barqsync"), "'/opt/a b/barqsync'");
        // Not a user name: quoted whole, no expansion.
        assert_eq!(r("~a b/x"), "'~a b/x'");
    }

    #[test]
    fn relay_targets_try_loopback_first() {
        assert_eq!(targets("192.168.1.4:5000").unwrap(), "127.0.0.1:5000,192.168.1.4:5000");
        assert_eq!(targets("[fd7a::1]:5000").unwrap(), "[::1]:5000,[fd7a::1]:5000");
        assert_eq!(targets("127.0.0.1:5000").unwrap(), "127.0.0.1:5000");
    }
}
