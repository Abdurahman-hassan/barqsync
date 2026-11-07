//! Security audit of 2026-10-30 (agent A3): adversarial checks at barqsync's trust
//! boundaries. See docs/audit/2026-10-30/03-security.md for the findings (AU-SEC-NN).
//!
//! - Tests without `#[ignore]` guard properties that held at 475db4c and must keep
//!   holding.
//! - `#[ignore]`d tests reproduce findings that were open at 475db4c. Each asserts the
//!   secure behavior, so it FAILS until its finding is fixed; then drop the `#[ignore]`.
//!   Run them with `cargo test --locked --test audit_security -- --ignored`.
//!
//! The peer is played by hand over a socket pair against the real receiver or sender
//! (like tests/peer_input.rs), or the real binary runs on a scratch tree. Nothing here
//! touches anything outside its own temporary directories, and sockets bind loopback.

mod common;

use barqsync::cli::Options;
use barqsync::entry::{DirEnt, Kind, Meta};
use barqsync::fsops;
use barqsync::output::{Output, Side, StatsSnapshot};
use barqsync::proto::{self, Conn, Frame, Msg, NeedReq, RootInfo, Wire, file_id};
use barqsync::remote::{receiver, sender, streams};
use barqsync::select::Sel;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Helpers: a hand-played peer (as in tests/peer_input.rs)
// ---------------------------------------------------------------------------

/// One in-process transfer at a time: temp files are registered process-wide.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

fn conn(s: UnixStream) -> Conn {
    let mut c = Conn::start(s.try_clone().unwrap(), s, false);
    c.set_wire(Wire::Postcard);
    c.seal();
    c
}

fn pair() -> (Conn, Conn) {
    let (a, b) = UnixStream::pair().unwrap();
    (conn(a), conn(b))
}

fn next(c: &Conn) -> Option<Frame> {
    match c.rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(f)) => Some(f),
        Ok(Err(_)) => None,
        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => None,
        Err(e) => panic!("no frame from the peer: {e}"),
    }
}

/// The receiver's `Need` for listing `dir`.
fn need(c: &Conn, dir: u32) -> Vec<NeedReq> {
    loop {
        match next(c) {
            Some(Frame::Msg(Msg::Need { dir: d, files, .. })) if d == dir => return files,
            Some(Frame::Msg(Msg::Fatal { msg })) => panic!("receiver gave up: {msg}"),
            Some(_) => {}
            None => panic!("receiver stopped before answering listing {dir}"),
        }
    }
}

fn finish(c: &Conn, dirs: u32) {
    c.msg(Msg::ScanDone { dirs }).unwrap();
    c.msg(Msg::SenderDone { errors: 0, vanished: 0 }).unwrap();
    while let Some(f) = next(c) {
        if matches!(f, Frame::Msg(Msg::ReceiverDone { .. })) {
            return;
        }
    }
    panic!("no ReceiverDone");
}

/// Run the real receiver into `dest` against the sender `script` plays (as the pulling
/// client's receiver would run against a server). Returns the exit code and its messages.
fn receive(dest: &Path, args: &[&str], script: impl FnOnce(&Conn)) -> (i32, String) {
    let mut argv = vec!["barqsync", "-a", "--threads=2"];
    argv.extend_from_slice(args);
    argv.extend_from_slice(&["s/", "d/"]);
    let opts = Options::parse_from(argv).unwrap();
    let _one = one_at_a_time();
    let (actual, peer) = pair();
    let dest = dest.as_os_str().to_owned();
    let log = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let sink = log.clone();
    let handle = std::thread::spawn(move || {
        let out = Output::forwarding(&opts, Box::new(move |m| {
            if let Msg::Log { text, .. } = m {
                let mut log = sink.lock().unwrap();
                log.push_str(&String::from_utf8_lossy(&text));
                log.push('\n');
            }
        }));
        receiver::run(&actual, &opts, &dest, &out, false).0
    });
    script(&peer);
    let code = handle.join().unwrap();
    drop(peer);
    let log = log.lock().unwrap().clone();
    (code, log)
}

fn meta_of(p: &Path) -> Meta {
    fsops::lstat(p).unwrap()
}

fn ent(name: impl Into<OsString>, meta: Meta) -> DirEnt {
    DirEnt::new(name.into(), meta, None)
}

fn symlink_ent(name: &str, target: &Path, like: Meta) -> DirEnt {
    let mut e = DirEnt::new(name.into(), like, Some(target.as_os_str().as_bytes().to_vec()));
    e.meta.kind = Kind::Symlink;
    e.meta.size = target.as_os_str().len() as u64;
    e
}

fn root(dir: &Path) -> RootInfo {
    RootInfo { contents: true, meta: Some(fsops::stat(dir).unwrap()), items: vec![], sel: None, extra: None }
}

/// `dest` (the destination), `outside` (a sibling holding `victim`, which no transfer
/// into `dest` may touch), and metadata for a 4-byte file and a directory.
struct Fixture {
    _tmp: tempfile::TempDir,
    dest: PathBuf,
    outside: PathBuf,
    file: Meta,
    dir: Meta,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("dest");
    let outside = tmp.path().join("outside");
    fs::create_dir(&dest).unwrap();
    fs::create_dir(&outside).unwrap();
    let seed = tmp.path().join("seed");
    fs::write(&seed, b"GOOD").unwrap();
    fs::write(outside.join("victim"), b"PRECIOUS").unwrap();
    fs::set_permissions(outside.join("victim"), fs::Permissions::from_mode(0o600)).unwrap();
    fsops::set_mtime(&outside.join("victim"), 1_200_000_000, 0).unwrap();
    let (file, dir) = (meta_of(&seed), meta_of(&outside));
    Fixture { _tmp: tmp, dest, outside, file, dir }
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

/// What changed outside the destination (`None`: nothing).
fn outside_change(f: &Fixture) -> Option<String> {
    let victim = f.outside.join("victim");
    let names = names_in(&f.outside);
    if names != ["victim"] {
        return Some(format!("the directory outside holds {names:?} instead of [\"victim\"]"));
    }
    let data = fs::read(&victim).unwrap();
    if data != b"PRECIOUS" {
        return Some(format!("victim now holds {:?}", String::from_utf8_lossy(&data)));
    }
    let m = meta_of(&victim);
    if (m.mode & 0o7777, m.mtime) != (0o600, 1_200_000_000) {
        return Some(format!("victim's mode/mtime became {:o}/{}", m.mode & 0o7777, m.mtime));
    }
    None
}

/// `outside` still holds exactly `victim`, unchanged in content, mode and mtime.
fn assert_outside_untouched(f: &Fixture, what: &str) {
    let victim = f.outside.join("victim");
    assert_eq!(names_in(&f.outside), ["victim"], "{what}: something appeared in or vanished from the directory outside the destination");
    assert_eq!(fs::read(&victim).unwrap(), b"PRECIOUS", "{what}: the file outside the destination was rewritten");
    let m = meta_of(&victim);
    assert_eq!((m.mode & 0o7777, m.mtime), (0o600, 1_200_000_000), "{what}: attributes changed outside the destination");
}

/// The binary under test: the cargo-built one, or `AUDIT_BARQSYNC_BIN` (a release or dist build).
fn bin() -> String {
    std::env::var("AUDIT_BARQSYNC_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_barqsync").to_string())
}

// ---------------------------------------------------------------------------
// Guards: properties that hold at 475db4c
// ---------------------------------------------------------------------------

/// A remote location never becomes an option or a shell word on the remote shell's
/// command line (CLI-01's validation): it is refused before the remote shell runs.
#[test]
fn remote_locations_cannot_inject_remote_shell_options() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("rsh-ran");
    let rsh = tmp.path().join("rsh");
    fs::write(&rsh, format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display())).unwrap();
    fs::set_permissions(&rsh, fs::Permissions::from_mode(0o755)).unwrap();
    let bad = [
        "-oProxyCommand=touch PWNED:src/",
        "-oProxyCommand=sh@host:src/",
        "user@-oProxyCommand=x:src/",
        "host;touch PWNED:src/",
        "host$(touch PWNED):src/",
        "ho st:src/",
        "@host:src/",
        "[::1;touch PWNED]:src/",
        "-l:src/",
    ];
    for loc in bad {
        let o = Command::new(bin())
            .args(["-a", "-e"])
            .arg(&rsh)
            .args(["--", loc, "dst"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert_eq!(o.status.code(), Some(barqsync::exit::SYNTAX), "{loc}: {}", String::from_utf8_lossy(&o.stderr));
        assert!(!marker.exists(), "{loc}: the remote shell was started");
        assert!(!tmp.path().join("PWNED").exists(), "{loc}");
    }
}

/// A receiver asking for an entry that isn't a regular file (a symlink to a file outside
/// the source, a directory) never gets data read through it: the sender opens listed files
/// without following symlinks and checks their identity.
#[test]
fn a_receiver_cannot_make_the_sender_read_through_a_listed_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let secret = tmp.path().join("secret");
    fs::create_dir_all(src.join("d")).unwrap();
    fs::write(&secret, b"TOPSECRET-TOPSECRET").unwrap();
    fs::write(src.join("plain"), b"ok").unwrap();
    std::os::unix::fs::symlink(&secret, src.join("link")).unwrap();
    let opts = Options::parse_from(["barqsync", "-a", "--threads=2", "s/", "d/"]).unwrap();
    let _one = one_at_a_time();
    let (actual, peer) = pair();
    let sources = vec![OsString::from(format!("{}/", src.display()))];
    let handle = std::thread::spawn(move || {
        let out = Output::new(&opts, Side::Push);
        sender::run(&actual, &opts, &sources, &out, false).0
    });
    let mut received = Vec::new();
    let mut forged = false;
    loop {
        match next(&peer).expect("sender stopped") {
            Frame::Msg(Msg::DirBatch { dir, entries, .. }) => {
                let mut files = Vec::new();
                if !forged {
                    forged = true;
                    for (i, e) in entries.iter().enumerate() {
                        if e.name == "link" || e.name == "d" {
                            files.push(NeedReq { idx: i as u32, sig: None });
                        }
                    }
                    assert_eq!(files.len(), 2, "the root listing has `link` and `d`");
                }
                peer.msg(Msg::Need { dir, files, xattrs: vec![] }).unwrap();
            }
            Frame::Msg(Msg::SmallPack { blob, .. }) => received.extend_from_slice(&blob),
            Frame::Data { bytes, .. } => received.extend_from_slice(&bytes),
            Frame::Msg(Msg::SenderDone { .. }) => break,
            _ => {}
        }
    }
    peer.msg(Msg::ReceiverDone { stats: StatsSnapshot::default() }).unwrap();
    let code = handle.join().unwrap();
    assert!(!received.windows(9).any(|w| w == b"TOPSECRET"), "the symlink's target was sent");
    assert_ne!(code, 0, "the forged requests are reported as errors");
}

/// Frame limits: a header claiming more than `MAX_FRAME` is refused before its body is
/// read, and so is a compressed frame claiming to decompress to more. What a compressed
/// frame may expand to is bounded by `MAX_FRAME` (64 MiB), whatever its compressed size:
/// the ratio printed here is evidence for the memory finding in the report.
#[test]
fn frame_limits_hold_and_decompression_is_bounded_per_frame() {
    let too_big = ((proto::MAX_FRAME + 1) as u32).to_le_bytes();
    let mut hdr = too_big.to_vec();
    hdr.push(0);
    let e = proto::read_frame(&mut std::io::Cursor::new(hdr)).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
    // KIND_DATA | FLAG_ZSTD, claiming one byte more than a frame may hold.
    let mut z = 8u32.to_le_bytes().to_vec();
    z.push(0x81);
    z.extend_from_slice(&((proto::MAX_FRAME + 1) as u32).to_le_bytes());
    z.extend_from_slice(&[0x28, 0xb5, 0x2f, 0xfd]);
    let e = proto::read_frame(&mut std::io::Cursor::new(z)).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
    // One frame of zeros at the limit: accepted, from a few KiB on the wire.
    let frame = Frame::Data { id: 1, off: 0, bytes: vec![0u8; proto::MAX_FRAME - 16] };
    let wire = proto::encode(&frame, Some(19), Wire::Postcard).unwrap();
    let back = proto::read_frame(&mut std::io::Cursor::new(&wire)).unwrap().unwrap();
    let Frame::Data { bytes, .. } = back else { panic!("a data frame") };
    assert_eq!(bytes.len(), proto::MAX_FRAME - 16);
    eprintln!("evidence: {} bytes on the wire decode to {} bytes ({}:1)", wire.len(), proto::MAX_FRAME, proto::MAX_FRAME / wire.len());
}

// ---------------------------------------------------------------------------
// AU-SEC-01: a relative --partial-dir / --delay-updates directory supplied by the source
// as a symlink is followed: files outside the destination are replaced and deleted
// ---------------------------------------------------------------------------

/// The source tree has `<partial dir> -> outside` and a file named like a file there.
/// rsync excludes the partial dir from the transfer and replaces a non-directory one
/// before using it; nothing outside the destination may change.
fn partial_dir_case(flag: &str, link_name: &str, how: &str) -> (Fixture, i32, String) {
    let f = fixture();
    let tmp = f.dest.parent().unwrap().to_path_buf();
    let src = tmp.join("src");
    fs::create_dir(&src).unwrap();
    std::os::unix::fs::symlink(&f.outside, src.join(link_name)).unwrap();
    fs::write(src.join("victim"), b"FROM-THE-SOURCE").unwrap();
    let mut cmd = Command::new(bin());
    cmd.args(["-a", flag]);
    match how {
        "local" => {
            cmd.args(["src/", "dest/"]);
        }
        // A real client and server through a fake ssh: a pull of a tree on the server.
        "pull" => {
            let shim = common::fake_ssh(&tmp);
            cmd.arg("-e").arg(shim).arg(format!("--remote-path={}", bin())).args(["fakehost:src/", "dest/"]);
        }
        _ => unreachable!(),
    }
    cmd.env_remove("SSH_CONNECTION");
    let r = common::run(&mut cmd, &tmp);
    (f, r.code, format!("{}{}", r.stdout, r.stderr))
}

#[test]
fn au_sec_01_a_partial_dir_symlink_from_the_source_cannot_touch_files_outside() {
    let mut failures = Vec::new();
    for (flag, link) in [("--delay-updates", ".~tmp~"), ("--partial-dir=.p", ".p")] {
        for how in ["local", "pull"] {
            let (f, code, out) = partial_dir_case(flag, link, how);
            let change = outside_change(&f);
            eprintln!("{flag} {how}: exit {code}, outside: {}", change.as_deref().unwrap_or("untouched"));
            if let Some(c) = change {
                failures.push(format!("{flag} {how} (exit {code}): {c}; output: {}", out.trim()));
            }
        }
    }
    assert!(failures.is_empty(), "files outside the destination changed:\n{}", failures.join("\n"));
}

/// The same through the protocol: a malicious server lists the symlink and the file; the
/// pulling client's receiver must not write or delete through it.
#[test]
fn au_sec_01_a_server_cannot_redirect_the_clients_partial_dir() {
    let mut failures = Vec::new();
    for (flag, link) in [("--delay-updates", ".~tmp~"), ("--partial-dir=.p", ".p")] {
        let f = fixture();
        let entries = vec![symlink_ent(link, &f.outside, f.file), ent("victim", f.file)];
        let (code, log) = receive(&f.dest, &[flag], |p| {
            p.msg(Msg::Roots(vec![root(&f.dest)])).unwrap();
            p.msg(Msg::DirBatch { root: 0, dir: 0, rel: vec![], entries, sums: vec![], mounts: vec![] }).unwrap();
            let files = need(p, 0);
            for r in files {
                let id = file_id(0, r.idx);
                p.msg(Msg::FileBegin { id, size: 4, xattrs: None }).unwrap();
                p.send(Frame::Data { id, off: 0, bytes: b"PWND".to_vec() }).unwrap();
                p.msg(Msg::FileEnd { id, ok: true }).unwrap();
            }
            finish(p, 1);
        });
        let change = outside_change(&f);
        eprintln!("{flag} over the protocol: exit {code}, outside: {}", change.as_deref().unwrap_or("untouched"));
        if let Some(c) = change {
            failures.push(format!("{flag} (exit {code}): {c}; log: {}", log.trim()));
        }
    }
    assert!(failures.is_empty(), "files outside the destination changed:\n{}", failures.join("\n"));
}

// ---------------------------------------------------------------------------
// AU-SEC-02 (known open, prior SEC-01 part 1B): a destination directory swapped for a
// symlink while the transfer runs redirects writes and attribute changes
// ---------------------------------------------------------------------------

/// A local process that can write in the destination swaps `dest/sub` for a symlink to a
/// directory outside after the receiver created `sub` and asked for its file. The file
/// must not land outside the destination.
#[test]
#[ignore = "AU-SEC-02 (known open: descriptor confinement, prior SEC-01 1B): fails until fixed"]
fn au_sec_02_a_directory_swapped_mid_transfer_cannot_redirect_a_file() {
    let f = fixture();
    let (code, log) = receive(&f.dest, &[], |p| {
        p.msg(Msg::Roots(vec![root(&f.dest)])).unwrap();
        p.msg(Msg::DirBatch { root: 0, dir: 0, rel: vec![], entries: vec![ent("sub", f.dir)], sums: vec![], mounts: vec![] }).unwrap();
        need(p, 0);
        p.msg(Msg::DirBatch { root: 0, dir: 1, rel: b"sub".to_vec(), entries: vec![ent("victim", f.file)], sums: vec![], mounts: vec![] }).unwrap();
        let files = need(p, 1);
        assert_eq!(files.len(), 1);
        // The local attacker's move.
        fs::rename(f.dest.join("sub"), f.dest.join("sub.moved")).unwrap();
        std::os::unix::fs::symlink(&f.outside, f.dest.join("sub")).unwrap();
        p.msg(Msg::SmallPack { files: vec![(file_id(1, files[0].idx), 4)], blob: b"PWND".to_vec(), xattrs: vec![] }).unwrap();
        finish(p, 2);
    });
    assert_outside_untouched(&f, &format!("exit {code}: {log}"));
}

/// The same swap after `sub`'s listing: the directory's mode and mtime, applied at the end
/// of the run, must not land on what the symlink points to.
#[test]
#[ignore = "AU-SEC-02 (known open: descriptor confinement, prior SEC-01 1B): fails until fixed"]
fn au_sec_02_directory_attributes_are_not_applied_through_a_swapped_symlink() {
    let f = fixture();
    let sub = Meta { mode: 0o751, mtime: 1_000_000_000, mtime_ns: 0, ..f.dir };
    let (code, log) = receive(&f.dest, &[], |p| {
        p.msg(Msg::Roots(vec![root(&f.dest)])).unwrap();
        p.msg(Msg::DirBatch { root: 0, dir: 0, rel: vec![], entries: vec![ent("sub", sub)], sums: vec![], mounts: vec![] }).unwrap();
        need(p, 0);
        p.msg(Msg::DirBatch { root: 0, dir: 1, rel: b"sub".to_vec(), entries: vec![], sums: vec![], mounts: vec![] }).unwrap();
        need(p, 1);
        fs::rename(f.dest.join("sub"), f.dest.join("sub.moved")).unwrap();
        std::os::unix::fs::symlink(f.outside.join("victim"), f.dest.join("sub")).unwrap();
        finish(p, 2);
    });
    assert_outside_untouched(&f, &format!("exit {code}: {log}"));
}

// ---------------------------------------------------------------------------
// AU-SEC-03: a deeply nested selection tree (`Sel`) in a peer's message overflows the
// decoding thread's stack: the process dies by a signal instead of a protocol error
// ---------------------------------------------------------------------------

/// `Roots` with a `Sel` chain `depth` deep, encoded in `wire`. Built, encoded and dropped
/// on a thread with a large stack: every step recurses.
fn deep_roots(depth: usize, wire: Wire) -> Vec<u8> {
    std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(move || {
            let mut s = Sel::default();
            for _ in 0..depth {
                let mut parent = Sel::default();
                parent.children.insert(Vec::new(), s);
                s = parent;
            }
            let root = RootInfo { contents: true, meta: None, items: vec![], sel: Some(s), extra: None };
            proto::encode(&Frame::Msg(Msg::Roots(vec![root])), None, wire).unwrap()
        })
        .unwrap()
        .join()
        .unwrap()
}

#[test]
#[ignore = "AU-SEC-03 (open at 475db4c): fails until fixed"]
fn au_sec_03_a_deeply_nested_message_is_refused_without_crashing() {
    let mut crashed = Vec::new();
    for wire in [Wire::Postcard, Wire::Rkyv] {
        let bytes = deep_roots(200_000, wire);
        // Any frame is decoded by the reader thread as soon as it arrives, before the
        // handshake is looked at; a pulling client's reader decodes `Roots` the same way.
        let mut child = Command::new(bin())
            .arg("--server")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let _ = stdin.write_all(&bytes);
        drop(stdin);
        let o = child.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&o.stderr);
        eprintln!("{wire:?}: {} bytes -> {:?}: {}", bytes.len(), o.status, err.trim());
        if o.status.code().is_none() {
            crashed.push(format!("{wire:?}: killed by a signal ({:?}): {}", o.status, err.trim()));
        }
    }
    assert!(crashed.is_empty(), "{}", crashed.join("\n"));
}

// ---------------------------------------------------------------------------
// AU-SEC-04 (known open, prior NET-04): with the default thread I/O layer, one stalled
// pre-authentication connection blocks every legitimate data stream
// ---------------------------------------------------------------------------

#[test]
#[ignore = "AU-SEC-04 (known open, prior NET-04): fails until fixed"]
fn au_sec_04_a_stalled_connection_does_not_block_stream_admission() {
    // No SSH_CONNECTION here: the listener is on loopback only.
    let (offer, acceptor) = streams::listen(1).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let staller = {
        let (addr, stop) = (offer.addr.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut s = std::net::TcpStream::connect(addr).unwrap();
            // A TLS handshake record announcing 16 KiB, then one byte a second: every read
            // of the server's 5 s read timeout succeeds.
            s.write_all(&[0x16, 0x03, 0x01, 0x40, 0x00]).unwrap();
            let start = Instant::now();
            while !stop.load(std::sync::atomic::Ordering::SeqCst) && start.elapsed() < Duration::from_secs(30) {
                if s.write_all(&[0]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1000));
            }
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    let got = streams::connect(&offer, 1);
    let took = started.elapsed();
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = staller.join();
    drop(acceptor);
    eprintln!("legitimate stream: {:?} after {took:?}", got.as_ref().map(Vec::len));
    assert!(got.is_ok_and(|s| s.len() == 1), "a stalled unauthenticated connection kept the legitimate stream out");
}

// ---------------------------------------------------------------------------
// AU-SEC-05: an `--append` request from the receiver for more bytes than the file has
// makes the sender subtract past zero (debug builds panic; release wraps)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "AU-SEC-05 (open at 475db4c, debug builds): fails until fixed"]
fn au_sec_05_an_oversized_append_request_is_an_error_not_a_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("f"), vec![7u8; 200 << 10]).unwrap();
    let opts = Options::parse_from(["barqsync", "-a", "--threads=2", "s/", "d/"]).unwrap();
    let _one = one_at_a_time();
    let (actual, peer) = pair();
    let sources = vec![OsString::from(format!("{}/", src.display()))];
    let handle = std::thread::spawn(move || {
        let out = Output::new(&opts, Side::Push);
        sender::run(&actual, &opts, &sources, &out, false).0
    });
    let mut done = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let f = match peer.rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(f)) => Some(f),
            Ok(Err(_)) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => None,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
        };
        match f {
            Some(Frame::Msg(Msg::DirBatch { dir, entries, .. })) => {
                let files = entries
                    .iter()
                    .position(|e| e.name == "f")
                    .map(|i| NeedReq { idx: i as u32, sig: Some(proto::Sig::Append { len: u64::MAX / 2, sum: None }) })
                    .into_iter()
                    .collect();
                peer.msg(Msg::Need { dir, files, xattrs: vec![] }).unwrap();
            }
            Some(Frame::Msg(Msg::SenderDone { .. })) => {
                peer.msg(Msg::ReceiverDone { stats: StatsSnapshot::default() }).unwrap();
                done = true;
                break;
            }
            Some(_) => {}
            None => break,
        }
    }
    // Without SenderDone the sender is stuck (its worker died). Shut our end down (dropping
    // `peer` would not: its reader thread keeps a clone of the socket open) so that the
    // sender's receive loop ends, then give it 10 s: it must return, not hang.
    let _ = peer.finish();
    let end = Instant::now() + Duration::from_secs(10);
    while !handle.is_finished() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    let returned = handle.is_finished();
    eprintln!("SenderDone: {done}; sender returned within 10 s of the connection closing: {returned}");
    assert!(returned, "the sender never returned after its worker panicked on a peer's request");
    let r = handle.join();
    eprintln!("sender: {:?}", r.as_ref().map_err(|_| "panicked"));
    assert!(done && r.is_ok(), "the sender panicked on a peer's request");
}

// ---------------------------------------------------------------------------
// AU-SEC-06 (Linux): a root receiver with -X stores `system.*` names a sender sends,
// though the module says that namespace is never copied
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[test]
#[ignore = "AU-SEC-06 (open at 475db4c): fails until fixed"]
fn au_sec_06_a_root_receiver_drops_system_namespace_names_from_the_wire() {
    let r = barqsync::xattrs::Rules { level: 1, am_root: true };
    for name in [&b"system.posix_acl_access"[..], b"system.posix_acl_default", b"system.nfs4_acl"] {
        assert_eq!(r.from_wire(name), None, "{}", String::from_utf8_lossy(name));
    }
}

// ---------------------------------------------------------------------------
// AU-SEC-07 (fixed): the BARQSYNC_TEST_* hooks are not in release builds
// ---------------------------------------------------------------------------

/// A release binary (this test built with `--release`, or `AUDIT_BARQSYNC_BIN`) ignores
/// `BARQSYNC_TEST_VANISH` and `BARQSYNC_TEST_SHRINK`, which in a debug build delete and
/// cut source files: the source and the copy stay whole, and the exit is 0.
#[test]
fn au_sec_07_a_release_build_ignores_the_destructive_test_hooks() {
    if barqsync::test_hooks::ENABLED && std::env::var_os("AUDIT_BARQSYNC_BIN").is_none() {
        return eprintln!("SKIP: a debug build has the test hooks; run with --release or AUDIT_BARQSYNC_BIN");
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("f1"), b"0123456789").unwrap();
    fs::write(src.join("f2"), b"0123456789").unwrap();
    let r = common::run(
        Command::new(bin()).args(["-a", "src/", "dest/"]).env("BARQSYNC_TEST_VANISH", "f1").env("BARQSYNC_TEST_SHRINK", "f2"),
        tmp.path(),
    );
    assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
    for d in ["src", "dest"] {
        for f in ["f1", "f2"] {
            assert_eq!(fs::read(tmp.path().join(d).join(f)).unwrap(), b"0123456789", "{d}/{f}");
        }
    }
}
