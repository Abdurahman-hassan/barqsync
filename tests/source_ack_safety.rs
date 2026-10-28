//! `--remove-source-files` over the protocol: the receiver's `Synced` names entries, but
//! only entries the sender listed in this run (and hasn't removed yet) may be removed,
//! only when removal was asked for and it isn't a dry run, and only if unchanged by the
//! sender's own record. The peer here is a receiver that misbehaves.

use barqsync::cli::Options;
use barqsync::fsops;
use barqsync::output::{Output, Side, StatsSnapshot};
use barqsync::proto::{Conn, Frame, Msg, NeedReq, Wire};
use barqsync::remote::sender;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

fn pair(wire: Wire) -> (Conn, Conn) {
    let (a, b) = UnixStream::pair().unwrap();
    let mut ours = Conn::start(a.try_clone().unwrap(), a, false);
    let mut peer = Conn::start(b.try_clone().unwrap(), b, false);
    ours.set_wire(wire);
    peer.set_wire(wire);
    ours.seal();
    peer.seal();
    (ours, peer)
}

fn next(peer: &Conn) -> Frame {
    peer.rx.recv_timeout(Duration::from_secs(10)).expect("the sender went quiet").unwrap()
}

/// What the receiver claims, and what is on disk meanwhile.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Case {
    /// No --remove-source-files: a file outside the transfer, by absolute path.
    NotRequested,
    /// The same in a dry run.
    NotRequestedDry,
    /// --remove-source-files in a dry run: no `Synced` is expected at all.
    Dry,
    Absolute,
    Traversal,
    UnknownRoot,
    /// A file created in the source after it was listed.
    NotListed,
    /// The listed file, twice: removed once, then an error.
    Twice,
    /// Changed within the same second, at the same size.
    Changed,
    /// Replaced by another file with the same size and time.
    Replaced,
    /// The listed file, named before the receiver even answered its listing, then
    /// `ReceiverDone` before the sender is done.
    Premature,
    Valid,
}

fn run_case(wire: Wire, case: Case) {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    let file = src.join("f");
    fs::write(&file, b"GOOD").unwrap();
    fsops::set_mtime(&file, 1_600_000_000, 100).unwrap();
    let outside = tmp.path().join("outside");
    fs::write(&outside, b"SAVE").unwrap();

    let mut opts = Options::parse_from(["barqsync", "-a", "--threads=2", "s/", "d/"]).unwrap();
    opts.remove_source_files = !matches!(case, Case::NotRequested | Case::NotRequestedDry);
    opts.dry_run = matches!(case, Case::NotRequestedDry | Case::Dry);
    let sources = vec![format!("{}/", src.display()).into()];
    let (ours, peer) = pair(wire);
    let (done_tx, done_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let out = Output::new(&opts, Side::Push);
        let code = sender::run(&ours, &opts, &sources, &out, false).0;
        let _ = ours.finish();
        done_tx.send(code).unwrap();
    });

    let (dir, root_entry) = loop {
        if let Frame::Msg(Msg::DirBatch { dir, entries, .. }) = next(&peer) {
            break (dir, entries[0].name.clone());
        }
    };
    assert_eq!(root_entry, "f");
    if case != Case::Premature {
        // In a dry run the receiver asks for no data.
        let dry = matches!(case, Case::NotRequestedDry | Case::Dry);
        let files = if dry { vec![] } else { vec![NeedReq { idx: 0, sig: None }] };
        peer.msg(Msg::Need { dir, files, xattrs: vec![] }).unwrap();
        while !matches!(next(&peer), Frame::Msg(Msg::SenderDone { .. })) {}
    }

    let _held;
    match case {
        Case::NotListed => fs::write(src.join("late"), b"LATE").unwrap(),
        Case::Changed => {
            fs::write(&file, b"EDIT").unwrap();
            fsops::set_mtime(&file, 1_600_000_000, 900_000_000).unwrap();
        }
        Case::Replaced => {
            // Kept open, the old inode can't be reused for the new file.
            _held = fs::File::open(&file).unwrap();
            fs::remove_file(&file).unwrap();
            fs::write(&file, b"EDIT").unwrap();
            fsops::set_mtime(&file, 1_600_000_000, 100).unwrap();
        }
        _ => {}
    }
    let abs = outside.as_os_str().as_bytes().to_vec();
    let (root, rel) = match case {
        Case::NotRequested | Case::NotRequestedDry | Case::Absolute => (0, abs),
        Case::Traversal => (0, b"../outside".to_vec()),
        Case::UnknownRoot => (42, b"f".to_vec()),
        Case::NotListed => (0, b"late".to_vec()),
        _ => (0, b"f".to_vec()),
    };
    let _ = peer.msg(Msg::Synced { root, files: vec![rel.clone()] });
    if case == Case::Twice {
        let _ = peer.msg(Msg::Synced { root, files: vec![rel] });
    }
    let _ = peer.msg(Msg::ReceiverDone { stats: StatsSnapshot::default() });
    let code = done_rx.recv_timeout(Duration::from_secs(10)).expect("the sender didn't finish");
    handle.join().unwrap();
    let _ = peer.finish();

    let what = format!("{wire:?} {case:?}");
    assert_eq!(fs::read(&outside).unwrap(), b"SAVE", "{what}");
    if case == Case::NotListed {
        assert!(src.join("late").exists(), "{what}");
    }
    match case {
        Case::Valid => {
            assert_eq!(code, 0, "{what}");
            assert!(!file.exists(), "{what}: the listed file wasn't removed");
        }
        // Like rsync, a listed and unchanged entry the receiver names is removed whenever
        // it is named (the receiver is trusted to have stored it); a second time, or
        // finishing before the sender, is an error.
        Case::Twice | Case::Premature => {
            assert_eq!(code, barqsync::exit::PARTIAL, "{what}");
            assert!(!file.exists(), "{what}");
        }
        // Without removal asked for (or in a dry run) a `Synced` is a protocol violation.
        Case::NotRequested | Case::NotRequestedDry | Case::Dry => {
            assert_eq!(code, barqsync::exit::PROTOCOL, "{what}");
            assert!(file.exists(), "{what}");
        }
        _ => {
            assert_eq!(code, barqsync::exit::PARTIAL, "{what}");
            assert!(file.exists(), "{what}: the source was removed");
        }
    }
}

#[test]
fn a_receiver_cannot_remove_what_it_is_not_entitled_to() {
    use Case::*;
    for wire in [Wire::Postcard, Wire::Rkyv] {
        for case in [NotRequested, NotRequestedDry, Dry, Absolute, Traversal, UnknownRoot, NotListed, Twice, Changed, Replaced, Premature, Valid] {
            run_case(wire, case);
        }
    }
}

/// The removal helper refuses on its own too, whatever its caller checked.
#[test]
fn the_removal_helper_needs_the_flag_and_no_dry_run() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("f");
    fs::write(&p, b"x").unwrap();
    let listed = fsops::lstat(&p).unwrap();
    for (flag, dry) in [(false, false), (false, true), (true, true)] {
        let mut opts = Options::parse_from(["barqsync", "-a", "s/", "d/"]).unwrap();
        opts.remove_source_files = flag;
        opts.dry_run = dry;
        assert!(barqsync::local::flags::remove_source(&opts, &p, &listed, None).is_err());
        assert!(Path::new(&p).exists());
    }
}
