//! Ctrl-C and a lost peer end every process of a remote transfer within seconds, with
//! rsync's exit codes (20 interrupted, 12 connection lost), and leave no server or relay
//! running. Each transfer is stopped while data is flowing: the server is frozen
//! (SIGSTOP) as soon as data arrives, so a fast machine can't finish it first.

use std::fs;
use std::io::{Read, Seek};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_barqsync");
/// Longest a client may take to exit once stopped (close, then the remote shell's end).
const BOUND: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq)]
enum Act {
    /// The server dies.
    KillServer,
    /// Ctrl-C in a terminal: SIGINT to the client's whole process group.
    CtrlC,
    /// SIGINT to the client alone.
    IntClient,
    /// SIGINT after the connection closes, while the remote shell is being reaped.
    IntClosing,
    /// SIGINT to the server alone.
    IntServer,
    /// SIGINT to the server alone, its notice held back on the client's control stream
    /// (`BARQSYNC_TEST_NOTICE_LAG_MS`) until the data streams have ended: the interrupt,
    /// not their end, decides how the client ends.
    LateNotice,
    /// The server stops reading (SIGSTOP), then Ctrl-C to the client alone.
    StuckServer,
    /// The pipe the file data travels on is cut (into the server for a push, out of it
    /// for a pull); both processes stay alive.
    CutPipe,
}

/// Processes whose command line contains `pat` (with `start`: begins with it, so not a
/// shell running it, as dash's `sh -c` stays around).
fn pids_where(pat: &str, start: bool) -> Vec<i32> {
    let found = |cmd: &str| if start { cmd.starts_with(pat) } else { cmd.contains(pat) };
    let mut out = Vec::new();
    if cfg!(target_os = "linux") {
        for e in fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
            let Ok(cmd) = fs::read(e.path().join("cmdline")) else { continue };
            let cmd: Vec<u8> = cmd.iter().map(|&b| if b == 0 { b' ' } else { b }).collect();
            if found(&String::from_utf8_lossy(&cmd)) {
                out.push(pid);
            }
        }
    } else {
        let ps = Command::new("ps").args(["-axww", "-o", "pid=,command="]).output().unwrap();
        for line in String::from_utf8_lossy(&ps.stdout).lines() {
            let line = line.trim_start();
            if let Some((pid, cmd)) = line.split_once(' ')
                && found(cmd.trim_start())
            {
                out.push(pid.parse().unwrap());
            }
        }
    }
    out.retain(|&p| p != std::process::id() as i32);
    out
}

fn pids(pat: &str) -> Vec<i32> {
    pids_where(pat, false)
}

fn signal(pid: i32, sig: i32) {
    unsafe { libc::kill(pid, sig) };
}

/// Whether `dir` has an entry yet (the receiver's temp file: file data is arriving).
fn receiving(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|mut d| d.next().is_some())
}

fn read_back(f: &mut fs::File) -> String {
    f.rewind().unwrap();
    let mut s = String::new();
    f.read_to_string(&mut s).unwrap();
    s
}

/// Start a transfer of a big file, stop it with `act` while data flows, and check how
/// the client ends and that nothing is left running.
fn stop_midway(act: Act, transport: &str, pull: bool, net_io: &str) {
    let what = format!("{act:?} {transport} {} {net_io}", if pull { "pull" } else { "push" });
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    fs::create_dir(t.join("src")).unwrap();
    fs::create_dir(t.join("dst")).unwrap();
    // Sparse, and written sparse (`-S`): quick to make and read, far too big to send
    // before it is stopped, yet nothing lands on disk however much is sent.
    fs::File::create(t.join("src/big")).unwrap().set_len(64 << 30).unwrap();
    // A unique name for the remote barqsync, to find the server and relays by.
    let remote = t.join("barq-remote");
    std::os::unix::fs::symlink(BIN, &remote).unwrap();
    let remote = remote.to_str().unwrap().to_string();
    let cutter = t.join("cutter");
    let shell = if act == Act::IntClosing {
        // Close the protocol pipes after the server exits but keep the remote shell
        // alive, so the client is still cleaning up when it receives SIGINT.
        "#!/bin/sh\nshift\n/bin/sh -c \"exec $*\"\nexec 0<&- 1>&-\nexec sleep 3\n".to_string()
    } else if act == Act::CutPipe {
        // The data reaches the other side through a copier the test can kill.
        fs::write(&cutter, "#!/usr/bin/perl\nwhile (($n = sysread(STDIN, $b, 65536)) > 0) { syswrite(STDOUT, $b, $n) }\n").unwrap();
        match pull {
            true => format!("#!/bin/sh\nshift\n/bin/sh -c \"exec $*\" | {}\n", cutter.display()),
            false => format!("#!/bin/sh\nshift\n{} | /bin/sh -c \"exec $*\"\n", cutter.display()),
        }
    } else {
        "#!/bin/sh\nshift\nexec /bin/sh -c \"$*\"\n".to_string()
    };
    fs::write(t.join("fake-ssh"), shell).unwrap();
    for f in ["fake-ssh", "cutter"].map(|f| t.join(f)).iter().filter(|p| p.exists()) {
        fs::set_permissions(f, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut args = vec!["-aS", "--strategy=fixed", "-e", "./fake-ssh", "--remote-path", &remote];
    match transport {
        "ssh" => args.push("--transport=ssh"),
        "quic" => args.extend(["--transport=quic", "--streams=2"]),
        _ => args.extend(["--transport=tcp", "--streams=2"]),
    }
    args.extend(if pull { ["fake:src/", "dst/"] } else { ["src/", "fake:dst/"] });
    let (mut so, mut se) = (tempfile::tempfile().unwrap(), tempfile::tempfile().unwrap());
    let mut cmd = Command::new(BIN);
    cmd.current_dir(t)
        .args(&args)
        .env("BARQSYNC_NET_IO", net_io)
        .env("BARQSYNC_CACHE_DIR", t)
        .stdin(Stdio::null())
        .stdout(so.try_clone().unwrap())
        .stderr(se.try_clone().unwrap())
        .process_group(0);
    if transport == "relay" {
        cmd.env("BARQSYNC_TEST_DIRECT_BLOCKED", "1");
    }
    if act == Act::IntClosing {
        cmd.env("BARQSYNC_TIMING", "1");
    }
    if act == Act::LateNotice {
        cmd.env("BARQSYNC_TEST_NOTICE_LAG_MS", "500");
    }
    let mut client: Child = cmd.spawn().unwrap();
    let pid = client.id() as i32;
    let cleanup = |client: &mut Child| {
        signal(-pid, libc::SIGKILL);
        let _ = client.wait();
        for p in pids(&remote) {
            signal(p, libc::SIGCONT);
            signal(p, libc::SIGKILL);
        }
    };

    // Wait for data to arrive, then freeze the server.
    let started = Instant::now();
    let mut server = None;
    while server.is_none() || !receiving(&t.join("dst")) {
        server = server.or_else(|| pids_where(&format!("{remote} --server"), true).first().copied());
        if let Some(status) = client.try_wait().unwrap() {
            panic!("{what}: ended before any data arrived: {status}\n{}", read_back(&mut se));
        }
        if started.elapsed() > Duration::from_secs(30) {
            cleanup(&mut client);
            panic!("{what}: no data after 30 s\n{}", read_back(&mut se));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let server = server.unwrap();
    signal(server, libc::SIGSTOP);
    match act {
        Act::KillServer | Act::IntClosing => signal(server, libc::SIGKILL),
        Act::CtrlC => signal(-pid, libc::SIGINT),
        Act::IntClient => signal(pid, libc::SIGINT),
        Act::IntServer | Act::LateNotice => signal(server, libc::SIGINT),
        Act::StuckServer => {
            // Long enough for the client to fill the pipe and block on it.
            std::thread::sleep(Duration::from_millis(300));
            signal(pid, libc::SIGINT);
        }
        Act::CutPipe => {
            let copier = *pids(&cutter.display().to_string()).first().unwrap_or_else(|| panic!("{what}: no copier"));
            signal(copier, libc::SIGKILL);
        }
    }
    if act != Act::StuckServer {
        signal(server, libc::SIGCONT);
    }
    let stopped = Instant::now();
    if act == Act::IntClosing {
        loop {
            let log = read_back(&mut se);
            if log.lines().any(|line| line.starts_with("barqsync timing: client ") && line.ends_with("connection closed")) {
                signal(pid, libc::SIGINT);
                break;
            }
            if client.try_wait().unwrap().is_some() || stopped.elapsed() > BOUND {
                cleanup(&mut client);
                panic!("{what}: did not reach connection cleanup\n{log}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let status = loop {
        if let Some(s) = client.try_wait().unwrap() {
            break s;
        }
        if stopped.elapsed() > Duration::from_secs(30) {
            cleanup(&mut client);
            panic!("{what}: the client was still running 30 s later\n{}", read_back(&mut se));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let took = stopped.elapsed();
    if act == Act::StuckServer {
        // A stopped process can't be ended from the other side of ssh (only the remote
        // shell, which may be a `sh -c` above it, is). Once it runs again it must find
        // its connection gone and exit.
        signal(server, libc::SIGCONT);
    }
    // No server and no relay outlives the client for long.
    let mut left = pids(&remote);
    while !left.is_empty() && stopped.elapsed() < took + Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
        left = pids(&remote);
    }
    cleanup(&mut client);
    let out = format!("{}{}", read_back(&mut so), read_back(&mut se));
    assert!(left.is_empty(), "{what}: still running: {left:?}\n{out}");
    let want = match act {
        Act::KillServer | Act::CutPipe => 12,
        _ => 20,
    };
    assert_eq!(status.code(), Some(want), "{what}\n{out}");
    assert!(took < BOUND, "{what}: took {took:?} to exit\n{out}");
}

fn each_transport(act: Act, net_io: &str) {
    // Over QUIC a dead server is noticed only by QUIC's idle timeout (15 s), and closing a
    // stopped run waits up to 5 s for a peer that no longer answers: both take too long yet.
    let quic = !matches!(act, Act::KillServer | Act::StuckServer);
    for transport in ["ssh", "tcp", "relay", "quic"].into_iter().filter(|&t| quic || t != "quic") {
        for pull in [false, true] {
            stop_midway(act, transport, pull, net_io);
        }
    }
}

#[test]
fn a_dead_server_ends_the_client() {
    each_transport(Act::KillServer, "threads");
    each_transport(Act::KillServer, "tokio");
}

#[test]
fn ctrl_c_ends_both_sides() {
    each_transport(Act::CtrlC, "threads");
}

#[test]
fn an_interrupted_client_exits_20_and_ends_the_server() {
    each_transport(Act::IntClient, "threads");
}

#[test]
fn an_interrupt_while_reaping_the_remote_shell_exits_20() {
    for net_io in ["threads", "tokio"] {
        for pull in [false, true] {
            stop_midway(Act::IntClosing, "ssh", pull, net_io);
        }
    }
}

#[test]
fn an_interrupted_server_ends_the_client_with_20() {
    each_transport(Act::IntServer, "threads");
}

#[test]
fn a_server_interrupt_noticed_after_the_data_streams_end_exits_20() {
    each_transport(Act::LateNotice, "threads");
    each_transport(Act::LateNotice, "tokio");
}

#[test]
fn ctrl_c_ends_a_client_whose_server_stopped_reading() {
    each_transport(Act::StuckServer, "threads");
    each_transport(Act::StuckServer, "tokio");
}

#[test]
fn a_cut_pipe_ends_both_sides() {
    for pull in [false, true] {
        stop_midway(Act::CutPipe, "ssh", pull, "threads");
    }
}
