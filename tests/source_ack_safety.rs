//! A hostile receiver must not turn acknowledgements into filesystem authority.

use barqsync::{
    cli::Options, fsops, output::{Output, StatsSnapshot},
    proto::{Conn, Frame, Msg, NeedReq, Wire}, remote::sender,
};
use std::fs;
use std::os::unix::{ffi::OsStrExt, net::UnixStream};
use std::sync::mpsc;
use std::time::Duration;

fn pair(wire: Wire) -> (Conn, Conn) {
    let (a, b) = UnixStream::pair().unwrap();
    let mut actual = Conn::start(a.try_clone().unwrap(), a, false);
    let mut peer = Conn::start(b.try_clone().unwrap(), b, false);
    actual.set_wire(wire);
    peer.set_wire(wire);
    actual.seal();
    peer.seal();
    (actual, peer)
}

fn next(peer: &Conn) -> Frame {
    peer.rx.recv_timeout(Duration::from_secs(10)).expect("sender timed out").unwrap()
}

#[test]
fn hostile_acknowledgements_preserve_sources_and_outside_sentinels() {
    for wire in [Wire::Postcard, Wire::Rkyv] {
        for attack in ["disabled", "dry", "absolute", "traversal", "unknown_root", "duplicate", "changed", "replaced", "premature", "valid"] {
            let tmp = tempfile::tempdir().unwrap();
            let src = tmp.path().join("src");
            fs::create_dir(&src).unwrap();
            let file = src.join("f");
            let outside = tmp.path().join("outside");
            fs::write(&file, b"GOOD").unwrap();
            fsops::set_mtime(&file, 1_600_000_000, 100).unwrap();
            fs::write(&outside, b"SAVE").unwrap();
            let mut opts = Options::parse_from(["barqsync", "-a", "--threads=2", "s/", "d/"]).unwrap();
            opts.remove_source_files = attack != "disabled";
            opts.dry_run = attack == "dry";
            let sources = vec![format!("{}/", src.display()).into()];
            let (actual, peer) = pair(wire);
            let (result_tx, result_rx) = mpsc::channel();
            let handle = std::thread::spawn(move || {
                let out = Output::new(&opts);
                let code = sender::run(&actual, &opts, &sources, &out).0;
                let _ = actual.finish();
                result_tx.send(code).unwrap();
            });
            let (dir, meta) = loop {
                if let Frame::Msg(Msg::DirBatch { dir, entries, .. }) = next(&peer) {
                    break (dir, entries[0].meta);
                }
            };
            if attack == "premature" {
                peer.msg(Msg::Synced { root: 0, files: vec![(b"f".to_vec(), meta.size, meta.mtime)] }).unwrap();
                peer.msg(Msg::ReceiverDone { stats: StatsSnapshot::default() }).unwrap();
            } else {
                let files = if attack == "dry" { vec![] } else { vec![NeedReq { idx: 0, sig: None }] };
                peer.msg(Msg::Need { dir, files, xattrs: vec![] }).unwrap();
                while !matches!(next(&peer), Frame::Msg(Msg::SenderDone { .. })) {}
                let rel = match attack {
                    "disabled" | "dry" | "absolute" => outside.as_os_str().as_bytes().to_vec(),
                    "traversal" => b"../outside".to_vec(),
                    _ => b"f".to_vec(),
                };
                if attack == "changed" {
                    fs::write(&file, b"EDIT").unwrap();
                    fsops::set_mtime(&file, meta.mtime, 900_000_000).unwrap();
                }
                if attack == "replaced" {
                    fs::rename(&file, src.join("old")).unwrap();
                    fs::write(&file, b"EDIT").unwrap();
                    fsops::set_mtime(&file, meta.mtime, meta.mtime_ns).unwrap();
                }
                let root = if attack == "unknown_root" { 42 } else { 0 };
                // Even these deliberately wrong peer facts cannot override the
                // sender's own identity snapshot (or block a legitimate ack).
                peer.msg(Msg::Synced { root, files: vec![(rel.clone(), 999, -1)] }).unwrap();
                if attack == "duplicate" {
                    peer.msg(Msg::Synced { root, files: vec![(rel, 999, -1)] }).unwrap();
                }
                peer.msg(Msg::ReceiverDone { stats: StatsSnapshot::default() }).unwrap();
            }
            let code = result_rx.recv_timeout(Duration::from_secs(10)).expect("sender did not terminate");
            handle.join().unwrap();
            peer.finish().unwrap();
            assert_eq!(fs::read(&outside).unwrap(), b"SAVE", "{wire:?} {attack}");
            if attack == "valid" {
                assert_eq!(code, 0, "{wire:?} {attack}");
                assert!(!file.exists());
            } else {
                assert_ne!(code, 0, "{wire:?} {attack}");
                assert!(file.exists(), "{wire:?} {attack}: source disappeared");
            }
        }
    }
}
