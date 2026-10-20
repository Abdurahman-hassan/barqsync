//! `--delete` deletes only what the complete source lacks: nothing after a source read
//! error (rsync's "IO error encountered -- skipping file deletion").

use std::fs;
use std::io::{Read, Seek};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_barqsync");

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Run barqsync in `dir`, killing its process group if it takes over 30 seconds.
fn sync(dir: &Path, args: &[&str]) -> Out {
    let (mut so, mut se) = (tempfile::tempfile().unwrap(), tempfile::tempfile().unwrap());
    let mut child = Command::new(BIN)
        .current_dir(dir)
        .env("LC_ALL", "C.UTF-8")
        .args(args)
        .stdin(Stdio::null())
        .stdout(so.try_clone().unwrap())
        .stderr(se.try_clone().unwrap())
        .process_group(0)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            panic!("barqsync {args:?} took over 30 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let read = |f: &mut fs::File| {
        f.rewind().unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        s
    };
    Out { code: status.code().unwrap_or(-1), stdout: read(&mut so), stderr: read(&mut se) }
}

fn fake_ssh(dir: &Path) {
    let p = dir.join("fake-ssh");
    fs::write(&p, "#!/bin/sh\nshift\nexec /bin/sh -c \"$*\"\n").unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Each way of running a transfer: local with both scanners, push and pull.
const MODES: [&str; 4] = ["local", "local-jwalk", "push", "pull"];

/// `args` with the sources and destination of `mode` (`fake:` marks the remote side).
fn mode_args<'a>(mode: &str, args: &[&'a str], srcs: &[&'a str], dst: &'a str) -> Vec<String> {
    let mut v: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    if mode == "local-jwalk" {
        v.push("--scanner=jwalk".into());
    }
    if matches!(mode, "push" | "pull") {
        v.extend(["-e".into(), "./fake-ssh".into(), "--remote-path".into(), BIN.into()]);
    }
    for s in srcs {
        v.push(if mode == "pull" { format!("fake:{s}") } else { s.to_string() });
    }
    v.push(if mode == "push" { format!("fake:{dst}") } else { dst.to_string() });
    v
}

fn run_mode(dir: &Path, mode: &str, args: &[&str], srcs: &[&str], dst: &str) -> Out {
    let v = mode_args(mode, args, srcs, dst);
    sync(dir, &v.iter().map(String::as_str).collect::<Vec<_>>())
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    v.sort();
    v
}

const SKIPPING: &str = "IO error encountered -- skipping file deletion";

#[test]
fn a_dangling_symlink_under_copy_links_stops_deletion() {
    for mode in MODES {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        fake_ssh(t);
        fs::create_dir_all(t.join("src")).unwrap();
        fs::create_dir_all(t.join("dst")).unwrap();
        fs::write(t.join("src/alpha"), "a").unwrap();
        std::os::unix::fs::symlink("missing-target", t.join("src/dangle")).unwrap();
        fs::write(t.join("dst/keep"), "precious").unwrap();
        fs::write(t.join("dst/dangle"), "precious too").unwrap();
        let r = run_mode(t, mode, &["-aL", "--delete"], &["src/"], "dst/");
        assert_eq!(names(&t.join("dst")), ["alpha", "dangle", "keep"], "{mode}: {}{}", r.stdout, r.stderr);
        assert_eq!(fs::read(t.join("dst/keep")).unwrap(), b"precious");
        assert_eq!(r.code, 23, "{mode}: {}{}", r.stdout, r.stderr);
        assert!(r.stderr.contains("symlink has no referent"), "{mode}: {}", r.stderr);
        assert!(r.stdout.contains(SKIPPING), "{mode}: {}", r.stdout);
    }
}

#[test]
fn entries_that_cannot_be_read_stop_deletion() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("SKIP: root reads every directory");
        return;
    }
    for mode in MODES {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        fake_ssh(t);
        for side in ["src", "dst"] {
            for f in ["one", "two"] {
                fs::create_dir_all(t.join(side).join("d")).unwrap();
                fs::write(t.join(side).join("d").join(f), f).unwrap();
            }
        }
        fs::write(t.join("dst/d/only-in-dst"), "x").unwrap();
        // Names can be listed, but not looked up: every entry fails to stat.
        fs::set_permissions(t.join("src/d"), fs::Permissions::from_mode(0o644)).unwrap();
        let r = run_mode(t, mode, &["-a", "--delete"], &["src/"], "dst/");
        fs::set_permissions(t.join("src/d"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(names(&t.join("dst/d")), ["one", "only-in-dst", "two"], "{mode}: {}{}", r.stdout, r.stderr);
        assert_eq!(r.code, 23, "{mode}: {}{}", r.stdout, r.stderr);
    }
}
