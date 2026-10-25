//! What the other side of a remote transfer sends is checked before it is acted on: a
//! sender playing the protocol by hand (in process, over a socket pair) cannot make the
//! receiver write outside its destination or commit a file whose data is missing, and
//! messages too large for one frame still arrive. Every case runs with both wire formats
//! and both connection layers (reader threads, tokio tasks).

mod common;

use barqsync::cli::Options;
use barqsync::delta::Op;
use barqsync::entry::{DirEnt, Kind, Meta};
use barqsync::fsops;
use barqsync::output::{Output, StatsSnapshot};
use barqsync::proto::{Conn, Frame, Msg, NeedReq, RootInfo, Wire, file_id};
use barqsync::remote::{receiver, sender};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

const MODES: [(Wire, bool); 4] = [(Wire::Postcard, false), (Wire::Rkyv, false), (Wire::Postcard, true), (Wire::Rkyv, true)];

/// One in-process transfer at a time: temp files are registered process-wide, and a
/// receiver that fails removes every registered one, other tests' too.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

fn conn(s: UnixStream, wire: Wire, tokio: bool) -> Conn {
    let mut c = if tokio {
        use barqsync::proto::async_io::FdIo;
        let r = FdIo::new(s.try_clone().unwrap().into()).unwrap();
        Conn::start_async(r, FdIo::new(s.into()).unwrap(), false)
    } else {
        Conn::start(s.try_clone().unwrap(), s, false)
    };
    c.set_wire(wire);
    c.seal();
    c
}

fn pair(wire: Wire, tokio: bool) -> (Conn, Conn) {
    let (a, b) = UnixStream::pair().unwrap();
    (conn(a, wire, tokio), conn(b, wire, tokio))
}

/// The next frame from the receiver (`None`: it has stopped).
fn next(c: &Conn) -> Option<Frame> {
    match c.rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(f)) => Some(f),
        Ok(Err(_)) => None,
        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => None,
        Err(e) => panic!("no frame from the receiver: {e}"),
    }
}

/// Wait for the `Need` answering listing `dir`: the receiver has dealt with it.
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

/// Finish as a sender does, and wait for the receiver to say it is done.
fn finish(c: &Conn, dirs: u32) {
    finish_with_errors(c, dirs, 0);
}

fn finish_with_errors(c: &Conn, dirs: u32, errors: u64) {
    c.msg(Msg::ScanDone { dirs }).unwrap();
    c.msg(Msg::SenderDone { errors, vanished: 0 }).unwrap();
    while let Some(f) = next(c) {
        if matches!(f, Frame::Msg(Msg::ReceiverDone { .. })) {
            return;
        }
    }
    panic!("no ReceiverDone");
}

/// The receiver refused with a protocol error, and said so to its peer.
fn refused(c: &Conn) -> String {
    while let Some(f) = next(c) {
        if let Frame::Msg(Msg::Fatal { msg }) = f {
            return msg;
        }
    }
    panic!("the receiver stopped without a Fatal");
}

/// Run a receiver into `dest` against the sender `script` plays; returns its exit code.
fn receive(dest: &Path, args: &[&str], (wire, tokio): (Wire, bool), script: impl FnOnce(&Conn)) -> i32 {
    let mut argv = vec!["barqsync", "-a", "--threads=2"];
    argv.extend_from_slice(args);
    argv.extend_from_slice(&["s/", "d/"]);
    let opts = Options::parse_from(argv).unwrap();
    let _one = one_at_a_time();
    let (actual, peer) = pair(wire, tokio);
    let dest = dest.as_os_str().to_owned();
    let handle = std::thread::spawn(move || {
        let out = Output::new(&opts);
        receiver::run(&actual, &opts, &dest, &out, false).0
    });
    script(&peer);
    let code = handle.join().unwrap();
    drop(peer);
    code
}

fn meta_of(p: &Path) -> Meta {
    fsops::lstat(p).unwrap()
}

fn ent(name: impl Into<OsString>, meta: Meta) -> DirEnt {
    DirEnt::new(name.into(), meta, None)
}

fn root(dir: &Path) -> RootInfo {
    RootInfo { contents: true, meta: Some(fsops::stat(dir).unwrap()), items: vec![], sel: None, extra: None }
}

/// A scratch area: `dest` (the destination), `outside` (a sibling that must stay as it
/// is), and a 4-byte file whose metadata the listings use.
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
    let (file, dir) = (meta_of(&seed), meta_of(&outside));
    Fixture { _tmp: tmp, dest, outside, file, dir }
}

fn files_in(dir: &Path) -> Vec<OsString> {
    let mut v: Vec<OsString> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name()).collect();
    v.sort();
    v
}

// ---------------------------------------------------------------------------
// SEC-01: names and listings
// ---------------------------------------------------------------------------

/// The audit's probe: a listing naming `../escaped` wrote `escaped` next to the destination.
#[test]
fn listing_names_cannot_leave_the_destination() {
    let bad: [&[u8]; 8] = [b"../escaped", b"..", b".", b"", b"a/b", b"/abs", b"sub/../../escaped", b"x\0y"];
    for mode in MODES {
        for name in bad {
            let f = fixture();
            let entries = vec![ent(OsString::from_vec(name.to_vec()), f.file)];
            let code = receive(&f.dest, &[], mode, |p| {
                p.msg(Msg::Roots(vec![root(&f.dest)])).unwrap();
                p.msg(Msg::DirBatch { root: 0, dir: 0, rel: vec![], entries, sums: vec![] }).unwrap();
                let msg = refused(p);
                assert!(msg.contains("protocol error"), "{msg}");
            });
            assert_eq!(code, barqsync::exit::PROTOCOL, "{mode:?} {name:?}");
            assert!(files_in(&f.dest).is_empty(), "{mode:?} {name:?}: {:?}", files_in(&f.dest));
            let sibling: Vec<OsString> = files_in(f.dest.parent().unwrap());
            assert_eq!(sibling, ["dest", "outside", "seed"].map(OsString::from), "{mode:?} {name:?}");
        }
    }
}

#[test]
fn malformed_roots_and_listings_are_refused_before_anything_is_written() {
    let f = fixture();
    let item = |name: &str| RootInfo { contents: false, meta: None, items: vec![ent(name, f.file)], sel: None, extra: None };
    type Script = Box<dyn Fn(&Conn)>;
    let (r, d, file) = (root(&f.dest), f.dir, f.file);
    let cases: Vec<(&str, Script)> = vec![
        ("an item named ..", Box::new({
            let i = item("..");
            move |p| p.msg(Msg::Roots(vec![i.clone()])).unwrap()
        })),
        ("a src/ root with items", Box::new({
            let mut r = r.clone();
            r.items = vec![ent("x", file)];
            move |p| p.msg(Msg::Roots(vec![r.clone()])).unwrap()
        })),
        ("a listing for an unknown root", Box::new({
            let r = r.clone();
            move |p| {
                p.msg(Msg::Roots(vec![r.clone()])).unwrap();
                p.msg(Msg::DirBatch { root: 1, dir: 0, rel: vec![], entries: vec![], sums: vec![] }).unwrap();
            }
        })),
        ("a listing no directory announced", Box::new({
            let r = r.clone();
            move |p| {
                p.msg(Msg::Roots(vec![r.clone()])).unwrap();
                p.msg(Msg::DirBatch { root: 0, dir: 0, rel: vec![], entries: vec![ent("sub", d)], sums: vec![] }).unwrap();
                need(p, 0);
                p.msg(Msg::DirBatch { root: 0, dir: 1, rel: b"other".to_vec(), entries: vec![], sums: vec![] }).unwrap();
            }
        })),
        ("a directory path with ..", Box::new({
            let r = r.clone();
            move |p| {
                p.msg(Msg::Roots(vec![r.clone()])).unwrap();
                p.msg(Msg::DirBatch { root: 0, dir: 0, rel: vec![], entries: vec![ent("sub", d)], sums: vec![] }).unwrap();
                need(p, 0);
                p.msg(Msg::DirBatch { root: 0, dir: 1, rel: b"sub/..".to_vec(), entries: vec![], sums: vec![] }).unwrap();
            }
        })),
        ("a listing sent twice", Box::new({
            let r = r.clone();
            move |p| {
                p.msg(Msg::Roots(vec![r.clone()])).unwrap();
                p.msg(Msg::DirBatch { root: 0, dir: 0, rel: vec![], entries: vec![ent("sub", d)], sums: vec![] }).unwrap();
                need(p, 0);
                p.msg(Msg::DirBatch { root: 0, dir: 1, rel: b"sub".to_vec(), entries: vec![], sums: vec![] }).unwrap();
                need(p, 1);
                p.msg(Msg::DirBatch { root: 0, dir: 2, rel: b"sub".to_vec(), entries: vec![ent("late", file)], sums: vec![] }).unwrap();
            }
        })),
        ("a reused listing id", Box::new({
            let r = r.clone();
            move |p| {
                p.msg(Msg::Roots(vec![r.clone()])).unwrap();
                p.msg(Msg::DirBatch { root: 0, dir: 0, rel: vec![], entries: vec![ent("sub", d)], sums: vec![] }).unwrap();
                need(p, 0);
                p.msg(Msg::DirBatch { root: 0, dir: 0, rel: b"sub".to_vec(), entries: vec![ent("late", file)], sums: vec![] }).unwrap();
            }
        })),
        ("a name listed twice", Box::new({
            let r = r.clone();
            move |p| {
                p.msg(Msg::Roots(vec![r.clone()])).unwrap();
                let entries = vec![ent("x", d), ent("x", file)];
                p.msg(Msg::DirBatch { root: 0, dir: 0, rel: vec![], entries, sums: vec![] }).unwrap();
            }
        })),
        ("named items that differ from the roots", Box::new({
            let i = item("x");
            move |p| {
                p.msg(Msg::Roots(vec![i.clone()])).unwrap();
                p.msg(Msg::DirBatch { root: 0, dir: 0, rel: vec![], entries: vec![ent("y", file)], sums: vec![] }).unwrap();
            }
        })),
    ];
    for mode in MODES {
        for (what, script) in &cases {
            let dest = f.dest.join(format!("{what} {mode:?}"));
            let code = receive(&dest, &[], mode, |p| {
                script(p);
                refused(p);
            });
            assert_eq!(code, barqsync::exit::PROTOCOL, "{what} {mode:?}");
            // Refused roots leave no trace; a refused listing adds nothing to what came before.
            let made = fs::read_dir(&dest).map(|d| d.map(|e| e.unwrap().file_name()).collect::<Vec<_>>()).unwrap_or_default();
            assert!(made.iter().all(|n| n == "sub"), "{what} {mode:?}: {made:?}");
        }
    }
    assert_eq!(files_in(&f.outside), Vec::<OsString>::new());
}

/// Names that are valid Unix names, not UTF-8 included (where the filesystem takes
/// them: APFS doesn't), are still accepted.
#[test]
fn unusual_but_valid_names_are_written() {
    for mode in MODES {
        let f = fixture();
        let names: &[&[u8]] = if cfg!(target_os = "linux") {
            &[b"\xff\xfe", b"...", b"a\\..\\b", b".hidden", b"new\nline", b"-"]
        } else {
            &[b"...", b"a\\..\\b", b".hidden", b"new\nline", b"-"]
        };
        let code = receive(&f.dest, &[], mode, |p| {
            p.msg(Msg::Roots(vec![root(&f.dest)])).unwrap();
            let entries = names.iter().map(|n| ent(OsString::from_vec(n.to_vec()), f.file)).collect();
            p.msg(Msg::DirBatch { root: 0, dir: 0, rel: vec![], entries, sums: vec![] }).unwrap();
            let files = need(p, 0);
            assert_eq!(files.len(), names.len());
            let pack = files.iter().map(|r| (file_id(0, r.idx), 4)).collect();
            p.msg(Msg::SmallPack { files: pack, blob: b"GOOD".repeat(names.len()), xattrs: vec![] }).unwrap();
            finish(p, 1);
        });
        assert_eq!(code, 0, "{mode:?}");
        for n in names {
            assert_eq!(fs::read(f.dest.join(OsStr::from_bytes(n))).unwrap(), b"GOOD", "{mode:?} {n:?}");
        }
    }
}

/// Two roots write into one destination: one makes a directory and lists its contents
/// later, the other puts a symlink where that directory is in between. The contents
/// must still land in the destination, not where the symlink points.
#[test]
fn another_root_cannot_swap_a_directory_for_a_symlink() {
    for mode in MODES {
        let f = fixture();
        let mut l = DirEnt::new("a".into(), f.file, Some(f.outside.as_os_str().as_bytes().to_vec()));
        l.meta.kind = Kind::Symlink;
        let code = receive(&f.dest, &[], mode, |p| {
            p.msg(Msg::Roots(vec![root(&f.dest), root(&f.dest)])).unwrap();
            p.msg(Msg::DirBatch { root: 1, dir: 0, rel: vec![], entries: vec![ent("a", f.dir)], sums: vec![] }).unwrap();
            need(p, 0);
            p.msg(Msg::DirBatch { root: 0, dir: 1, rel: vec![], entries: vec![l.clone()], sums: vec![] }).unwrap();
            need(p, 1);
            p.msg(Msg::DirBatch { root: 1, dir: 2, rel: b"a".to_vec(), entries: vec![ent("pwn", f.file)], sums: vec![] }).unwrap();
            let files = need(p, 2);
            assert_eq!(files.len(), 1);
            p.msg(Msg::SmallPack { files: vec![(file_id(2, 0), 4)], blob: b"PWND".to_vec(), xattrs: vec![] }).unwrap();
            finish(p, 3);
        });
        assert!(files_in(&f.outside).is_empty(), "{mode:?}: wrote outside: {:?}", files_in(&f.outside));
        assert_eq!(fs::read(f.dest.join("a/pwn")).unwrap(), b"PWND", "{mode:?}");
        assert_eq!(code, barqsync::exit::PARTIAL, "{mode:?}: the symlink is refused");
    }
}

/// With `-K`, a symlink to a directory stands in for it, but not one the transfer made.
#[test]
fn keep_dirlinks_does_not_follow_a_symlink_another_root_made() {
    for mode in MODES {
        let f = fixture();
        let mut l = DirEnt::new("a".into(), f.file, Some(f.outside.as_os_str().as_bytes().to_vec()));
        l.meta.kind = Kind::Symlink;
        let code = receive(&f.dest, &["-K"], mode, |p| {
            p.msg(Msg::Roots(vec![root(&f.dest), root(&f.dest)])).unwrap();
            p.msg(Msg::DirBatch { root: 0, dir: 0, rel: vec![], entries: vec![l.clone()], sums: vec![] }).unwrap();
            need(p, 0);
            p.msg(Msg::DirBatch { root: 1, dir: 1, rel: vec![], entries: vec![ent("a", f.dir)], sums: vec![] }).unwrap();
            need(p, 1);
            p.msg(Msg::DirBatch { root: 1, dir: 2, rel: b"a".to_vec(), entries: vec![ent("pwn", f.file)], sums: vec![] }).unwrap();
            need(p, 2);
            p.msg(Msg::SmallPack { files: vec![(file_id(2, 0), 4)], blob: b"PWND".to_vec(), xattrs: vec![] }).unwrap();
            finish(p, 3);
        });
        assert!(files_in(&f.outside).is_empty(), "{mode:?}: wrote outside: {:?}", files_in(&f.outside));
        assert_eq!(fs::read(f.dest.join("a/pwn")).unwrap(), b"PWND", "{mode:?}");
        assert_eq!(code, 0, "{mode:?}");
    }
}

// ---------------------------------------------------------------------------
// DATA-03: deletion waits for a complete, successful source scan
// ---------------------------------------------------------------------------

#[test]
fn deletion_waits_for_successful_scan_completion() {
    for mode in MODES {
        for outcome in ["success", "io-error", "sender-error", "fatal"] {
            let f = fixture();
            fs::write(f.dest.join("keep"), b"PRECIOUS").unwrap();
            let code = receive(&f.dest, &["--delete"], mode, |p| {
                p.msg(Msg::Roots(vec![root(&f.dest)])).unwrap();
                p.msg(Msg::DirBatch { root: 0, dir: 0, rel: vec![], entries: vec![], sums: vec![] }).unwrap();
                need(p, 0);
                assert_eq!(fs::read(f.dest.join("keep")).unwrap(), b"PRECIOUS", "deleted while still scanning");
                if outcome == "fatal" {
                    p.msg(Msg::Fatal { msg: "source scan failed".into() }).unwrap();
                    return;
                }
                if outcome == "io-error" {
                    p.msg(Msg::IoError).unwrap();
                }
                finish_with_errors(p, 1, u64::from(outcome != "success"));
            });
            let expected = match outcome {
                "success" => barqsync::exit::OK,
                "fatal" => barqsync::exit::PROTOCOL,
                _ => barqsync::exit::PARTIAL,
            };
            assert_eq!(code, expected, "{mode:?} {outcome}");
            assert_eq!(f.dest.join("keep").exists(), outcome != "success", "{mode:?} {outcome}");
        }
    }
}

#[test]
fn invalid_scan_completion_cannot_authorize_deletion() {
    for mode in MODES {
        for case in ["missing", "count", "repeated", "late-listing"] {
            let f = fixture();
            fs::write(f.dest.join("keep"), b"PRECIOUS").unwrap();
            let code = receive(&f.dest, &["--delete"], mode, |p| {
                p.msg(Msg::Roots(vec![root(&f.dest)])).unwrap();
                p.msg(Msg::DirBatch { root: 0, dir: 0, rel: vec![], entries: vec![], sums: vec![] }).unwrap();
                need(p, 0);
                match case {
                    "missing" => p.msg(Msg::SenderDone { errors: 0, vanished: 0 }).unwrap(),
                    "count" => p.msg(Msg::ScanDone { dirs: 2 }).unwrap(),
                    "repeated" => {
                        p.msg(Msg::ScanDone { dirs: 1 }).unwrap();
                        p.msg(Msg::ScanDone { dirs: 1 }).unwrap();
                    }
                    _ => {
                        p.msg(Msg::ScanDone { dirs: 1 }).unwrap();
                        p.msg(Msg::DirBatch { root: 0, dir: 1, rel: vec![], entries: vec![], sums: vec![] }).unwrap();
                    }
                }
                assert!(refused(p).contains("protocol error"));
            });
            assert_eq!(code, barqsync::exit::PROTOCOL, "{mode:?} {case}");
            assert_eq!(fs::read(f.dest.join("keep")).unwrap(), b"PRECIOUS");
        }
    }
}

#[test]
fn missing_selected_directory_listing_preserves_deletion_candidates() {
    for mode in MODES {
        let f = fixture();
        fs::write(f.dest.join("keep"), b"PRECIOUS").unwrap();
        let code = receive(&f.dest, &["--delete"], mode, |p| {
            p.msg(Msg::Roots(vec![root(&f.dest)])).unwrap();
            p.msg(Msg::DirBatch { root: 0, dir: 0, rel: vec![], entries: vec![ent("sub", f.dir)], sums: vec![] }).unwrap();
            need(p, 0);
            // The count matches, but the selected subdirectory was never listed.
            finish(p, 1);
        });
        assert_eq!(code, barqsync::exit::PARTIAL, "{mode:?}");
        assert_eq!(fs::read(f.dest.join("keep")).unwrap(), b"PRECIOUS");
    }
}

#[test]
fn a_deletion_candidate_selected_by_another_root_survives() {
    for mode in MODES {
        for selected_first in [false, true] {
            let f = fixture();
            fs::write(f.dest.join("keep"), b"PRECIOUS").unwrap();
            let meta = meta_of(&f.dest.join("keep"));
            let code = receive(&f.dest, &["--delete"], mode, |p| {
                p.msg(Msg::Roots(vec![root(&f.dest), root(&f.dest)])).unwrap();
                for dir in 0..2 {
                    let selected = (dir == 0) == selected_first;
                    let entries = if selected { vec![ent("keep", meta)] } else { vec![] };
                    p.msg(Msg::DirBatch { root: dir as u16, dir, rel: vec![], entries, sums: vec![] }).unwrap();
                    assert!(need(p, dir).is_empty());
                }
                finish(p, 2);
            });
            assert_eq!(code, 0, "{mode:?}, selected_first={selected_first}");
            assert_eq!(fs::read(f.dest.join("keep")).unwrap(), b"PRECIOUS");
        }
    }
}

#[test]
fn a_replaced_deletion_candidate_is_kept() {
    for mode in MODES {
        let f = fixture();
        fs::write(f.dest.join("keep"), b"OLD").unwrap();
        let code = receive(&f.dest, &["--delete"], mode, |p| {
            p.msg(Msg::Roots(vec![root(&f.dest)])).unwrap();
            p.msg(Msg::DirBatch { root: 0, dir: 0, rel: vec![], entries: vec![], sums: vec![] }).unwrap();
            need(p, 0);
            let replacement = f.outside.join("replacement");
            fs::write(&replacement, b"NEW").unwrap();
            fs::rename(replacement, f.dest.join("keep")).unwrap();
            finish(p, 1);
        });
        assert_eq!(code, barqsync::exit::PARTIAL, "{mode:?}");
        assert_eq!(fs::read(f.dest.join("keep")).unwrap(), b"NEW");
    }
}

// ---------------------------------------------------------------------------
// DATA-05: file data
// ---------------------------------------------------------------------------

/// One needed file `f` (the destination has an older `f` holding "OLD!"): `send`
/// supplies its data. Returns the exit code and what `f` holds afterwards.
fn one_file(mode: (Wire, bool), send: impl FnOnce(&Conn, u64)) -> (i32, Vec<u8>) {
    let f = fixture();
    fs::write(f.dest.join("f"), b"OLD!").unwrap();
    fsops::set_mtime(&f.dest.join("f"), 1_000_000_000, 0).unwrap();
    let code = receive(&f.dest, &[], mode, |p| {
        p.msg(Msg::Roots(vec![root(&f.dest)])).unwrap();
        p.msg(Msg::DirBatch { root: 0, dir: 0, rel: vec![], entries: vec![ent("f", f.file)], sums: vec![] }).unwrap();
        assert_eq!(need(p, 0).len(), 1);
        send(p, file_id(0, 0));
        finish(p, 1);
    });
    let names = files_in(&f.dest);
    assert_eq!(names, [OsString::from("f")], "no temp file is left");
    (code, fs::read(f.dest.join("f")).unwrap())
}

fn data(p: &Conn, id: u64, off: u64, bytes: &[u8]) {
    p.send(Frame::Data { id, off, bytes: bytes.to_vec() }).unwrap();
}

#[test]
fn incomplete_file_data_is_never_committed() {
    type Case = (&'static str, fn(&Conn, u64));
    let cases: [Case; 8] = [
        // The audit's probes: no data at all, and a pack shorter than it says.
        ("no data", |p, id| {
            p.msg(Msg::FileBegin { id, size: 4, xattrs: None }).unwrap();
            p.msg(Msg::FileEnd { id, ok: true }).unwrap();
        }),
        ("short pack", |p, id| p.msg(Msg::SmallPack { files: vec![(id, 4)], blob: b"X".to_vec(), xattrs: vec![] }).unwrap()),
        ("long pack", |p, id| p.msg(Msg::SmallPack { files: vec![(id, 4)], blob: b"NEW!!".to_vec(), xattrs: vec![] }).unwrap()),
        ("pack with stray attributes", |p, id| {
            let xattrs = vec![(id + 1, vec![])];
            p.msg(Msg::SmallPack { files: vec![(id, 4)], blob: b"NEW!".to_vec(), xattrs }).unwrap()
        }),
        ("part of the data", |p, id| {
            p.msg(Msg::FileBegin { id, size: 4, xattrs: None }).unwrap();
            data(p, id, 0, b"NE");
            p.msg(Msg::FileEnd { id, ok: true }).unwrap();
        }),
        ("data past the end", |p, id| {
            p.msg(Msg::FileBegin { id, size: 4, xattrs: None }).unwrap();
            data(p, id, 0, b"NEW!!!!!");
            p.msg(Msg::FileEnd { id, ok: true }).unwrap();
        }),
        ("data out of order", |p, id| {
            p.msg(Msg::FileBegin { id, size: 4, xattrs: None }).unwrap();
            data(p, id, 2, b"W!");
            data(p, id, 0, b"NE");
            p.msg(Msg::FileEnd { id, ok: true }).unwrap();
        }),
        ("an end without a beginning", |p, id| p.msg(Msg::FileEnd { id, ok: true }).unwrap()),
    ];
    for mode in MODES {
        for (what, send) in cases {
            let (code, got) = one_file(mode, send);
            assert_eq!(got, b"OLD!", "{what} {mode:?}: the old file stays");
            assert_eq!(code, barqsync::exit::PARTIAL, "{what} {mode:?}");
        }
        // The sender's own failure: nothing committed, its error is counted on its side.
        let (code, got) = one_file(mode, |p, id| {
            p.msg(Msg::FileBegin { id, size: 4, xattrs: None }).unwrap();
            data(p, id, 0, b"NEW!");
            p.msg(Msg::FileEnd { id, ok: false }).unwrap();
        });
        assert_eq!((code, &got[..]), (0, &b"OLD!"[..]), "{mode:?}");
        // And complete data still goes through, in one piece or several.
        let (code, got) = one_file(mode, |p, id| {
            p.msg(Msg::FileBegin { id, size: 4, xattrs: None }).unwrap();
            data(p, id, 0, b"NE");
            data(p, id, 2, b"W!");
            p.msg(Msg::FileEnd { id, ok: true }).unwrap();
        });
        assert_eq!((code, &got[..]), (0, &b"NEW!"[..]), "{mode:?}");
    }
}

/// Deltas: copies must come from inside the basis and land inside the new file, and the
/// literal data must fill exactly the rest.
#[test]
fn delta_ranges_are_checked_against_the_basis_and_the_file() {
    const SIZE: usize = 256 << 10;
    let old: Vec<u8> = (0..SIZE).map(|i| (i * 7 % 251) as u8).collect();
    type Case = (&'static str, Vec<Op>, Vec<(u64, Vec<u8>)>, bool);
    let half = SIZE as u64 / 2;
    let copy = |off, basis_off, len| Op::Copy { off, basis_off, len };
    let lit = vec![0xAA; SIZE / 2];
    let cases: Vec<Case> = vec![
        ("a valid delta", vec![copy(0, 0, half)], vec![(half, lit.clone())], true),
        ("a copy past the basis", vec![copy(0, half + 1, half)], vec![(half, lit.clone())], false),
        ("a copy past the file", vec![copy(half, 0, half + 1)], vec![(0, lit.clone())], false),
        ("overlapping copies", vec![copy(0, 0, half), copy(half - 1, 0, 10)], vec![(half + 9, lit[9..].to_vec())], false),
        ("an overflowing copy", vec![copy(u64::MAX, 0, 2)], vec![], false),
        ("a literal among the copies", vec![Op::Literal { off: 0, len: half }], vec![], false),
        ("missing literal data", vec![copy(0, 0, half)], vec![(half, lit[1..].to_vec())], false),
        ("literal data over a copy", vec![copy(0, 0, half)], vec![(0, lit.clone()), (half, lit.clone())], false),
    ];
    for mode in MODES {
        for (what, copies, literals, ok) in &cases {
            let f = fixture();
            let path = f.dest.join("big");
            fs::write(&path, &old).unwrap();
            fsops::set_mtime(&path, 1_000_000_000, 0).unwrap();
            let src_meta = Meta { size: SIZE as u64, mtime: 1_100_000_000, ..meta_of(&path) };
            let code = receive(&f.dest, &[], mode, |p| {
                p.msg(Msg::Roots(vec![root(&f.dest)])).unwrap();
                p.msg(Msg::DirBatch { root: 0, dir: 0, rel: vec![], entries: vec![ent("big", src_meta)], sums: vec![] }).unwrap();
                let reqs = need(p, 0);
                assert!(reqs[0].sig.is_some(), "a basis signature is asked for");
                let id = file_id(0, 0);
                p.msg(Msg::DeltaBegin { id, size: SIZE as u64, copies: copies.clone(), xattrs: None }).unwrap();
                for (off, bytes) in literals {
                    data(p, id, *off, bytes);
                }
                p.msg(Msg::FileEnd { id, ok: true }).unwrap();
                finish(p, 1);
            });
            let got = fs::read(&path).unwrap();
            if *ok {
                assert_eq!(code, 0, "{what} {mode:?}");
                assert_eq!(&got[..SIZE / 2], &old[..SIZE / 2], "{what} {mode:?}");
                assert_eq!(&got[SIZE / 2..], &lit[..], "{what} {mode:?}");
            } else {
                assert_eq!(code, barqsync::exit::PARTIAL, "{what} {mode:?}");
                assert!(got == old, "{what} {mode:?}: the old file stays");
            }
            assert_eq!(files_in(&f.dest), [OsString::from("big")], "{what} {mode:?}: no temp file is left");
        }
    }
}

// ---------------------------------------------------------------------------
// PROTO-01: large messages
// ---------------------------------------------------------------------------

/// A listing far larger than one frame (64 MiB) arrives whole and is answered.
#[test]
fn a_listing_larger_than_a_frame_arrives() {
    for mode in [MODES[0], MODES[3]] {
        let f = fixture();
        let entries: Vec<DirEnt> = (0..450_000u32).map(|i| ent(format!("{i:0>120}"), f.file)).collect();
        let n = entries.len();
        let code = receive(&f.dest, &["--exclude=0*"], mode, |p| {
            p.msg(Msg::Roots(vec![root(&f.dest)])).unwrap();
            p.msg(Msg::DirBatch { root: 0, dir: 0, rel: vec![], entries, sums: vec![] }).unwrap();
            assert!(need(p, 0).is_empty(), "all {n} are excluded");
            finish(p, 1);
        });
        assert_eq!(code, 0, "{mode:?}");
    }
}

/// A file listed as small that has grown by the time it is sent goes as a large file,
/// not as one oversized entry of a small-file pack.
#[test]
fn a_small_file_that_grew_is_sent_whole() {
    for mode in MODES {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("f"), b"small").unwrap();
        let opts = Options::parse_from(["barqsync", "-a", "--threads=2", "s/", "d/"]).unwrap();
        let _one = one_at_a_time();
        let (actual, peer) = pair(mode.0, mode.1);
        let sources = vec![OsString::from(format!("{}/", src.display()))];
        let handle = std::thread::spawn(move || {
            let out = Output::new(&opts);
            sender::run(&actual, &opts, &sources, &out, false).0
        });
        let grown = vec![7u8; 300 << 10];
        let (mut begin, mut got, mut packs) = (None, 0, 0);
        loop {
            match next(&peer).expect("sender stopped") {
                Frame::Msg(Msg::DirBatch { dir, entries, .. }) => {
                    let idx = entries.iter().position(|e| e.name == "f").unwrap() as u32;
                    fs::write(src.join("f"), &grown).unwrap();
                    peer.msg(Msg::Need { dir, files: vec![NeedReq { idx, sig: None }], xattrs: vec![] }).unwrap();
                }
                Frame::Msg(Msg::SmallPack { .. }) => packs += 1,
                Frame::Msg(Msg::FileBegin { size, .. }) => begin = Some(size),
                Frame::Data { bytes, .. } => got += bytes.len(),
                Frame::Msg(Msg::SenderDone { .. }) => break,
                _ => {}
            }
        }
        peer.msg(Msg::ReceiverDone { stats: StatsSnapshot::default() }).unwrap();
        assert_eq!(handle.join().unwrap(), 0, "{mode:?}");
        assert_eq!((packs, begin, got), (0, Some(grown.len() as u64), grown.len()), "{mode:?}");
    }
}

/// Through the real client and server, on every transport, both ways, with each
/// connection layer and wire format: messages cut into many frames (here at 4 KiB, see
/// `BARQSYNC_TEST_FRAME_PIECE`; the real cut is at 64 MiB) arrive whole.
#[test]
fn messages_cut_into_frames_cross_every_transport() {
    let tmp = tempfile::tempdir().unwrap();
    let t = common::Tree::new(tmp.path().join("src"));
    for i in 0..200 {
        t.file(&format!("many/f{i:03}"), format!("file {i} ").repeat(i + 1));
    }
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    t.file("big", &big);
    let shim = common::fake_ssh(tmp.path());
    let runs: [(&str, &str, &str, bool); 6] = [
        ("ssh", "threads", "postcard", true),
        ("ssh", "tokio", "rkyv", false),
        ("tcp", "threads", "rkyv", true),
        ("tcp", "tokio", "postcard", false),
        ("quic", "threads", "postcard", false),
        ("quic", "threads", "rkyv", true),
    ];
    for (i, (transport, io, wire, pull)) in runs.into_iter().enumerate() {
        let dst = format!("dst{i}");
        let (src, dst_arg) = if pull { ("fakehost:src/".to_string(), dst.clone()) } else { ("src/".to_string(), format!("fakehost:{dst}")) };
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_barqsync"));
        cmd.args(["-a", "--strategy=fixed", "--transport", transport, "--streams=2", "--wire", wire, "-e"])
            .arg(&shim)
            .arg(format!("--remote-path={}", env!("CARGO_BIN_EXE_barqsync")))
            .args(if i % 2 == 0 { &["-z"][..] } else { &[] })
            .args([&src, &dst_arg])
            .env("BARQSYNC_NET_IO", io)
            .env("BARQSYNC_TEST_FRAME_PIECE", "4096")
            .env_remove("SSH_CONNECTION");
        let r = common::run(&mut cmd, tmp.path());
        let what = format!("{transport} {io} {wire} pull={pull}");
        assert_eq!(r.code, 0, "{what}: {}\n{}", r.stdout, r.stderr);
        let mut want = common::snapshot(&tmp.path().join("src"), true);
        let got = common::snapshot(&tmp.path().join(&dst), true);
        want.get_mut(b".".as_slice()).unwrap().mtime = got[b".".as_slice()].mtime;
        common::assert_same_tree(&want, &got);
    }
}
