//! `--delete` deletes only what the complete source lacks: nothing after a source read
//! error (rsync's "IO error encountered -- skipping file deletion"), and nothing after
//! a file could not be written to the destination.

mod common;

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
    sync_with(dir, args, |_| {})
}

/// [`sync`], with `setup` applied to the command before it starts.
fn sync_with(dir: &Path, args: &[&str], setup: impl FnOnce(&mut Command)) -> Out {
    let (mut so, mut se) = (tempfile::tempfile().unwrap(), tempfile::tempfile().unwrap());
    let mut cmd = Command::new(BIN);
    cmd.current_dir(dir)
        .env("LC_ALL", "C.UTF-8")
        .args(args)
        .stdin(Stdio::null())
        .stdout(so.try_clone().unwrap())
        .stderr(se.try_clone().unwrap())
        .process_group(0);
    setup(&mut cmd);
    let mut child = cmd.spawn().unwrap();
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
fn a_late_scan_error_preserves_earlier_deletion_candidates() {
    for mode in MODES {
        for backend in ["threads", "tokio"] {
            let tmp = tempfile::tempdir().unwrap();
            let t = tmp.path();
            fake_ssh(t);
            files(t, &[("src/good", "copied"), ("dst/keep", "precious root entry"),
                ("dst/deep/broken", "precious nested entry")]);
            fs::create_dir(t.join("src/deep")).unwrap();
            std::os::unix::fs::symlink("missing-target", t.join("src/deep/broken")).unwrap();
            let net = format!("--net-io={backend}");
            let args = ["-aL", "--delete", "--threads=1", "--transport=ssh", &net];
            let r = run_mode(t, mode, &args, &["src/"], "dst/");
            assert_eq!(r.code, 23, "{mode} {backend}: {}{}", r.stdout, r.stderr);
            assert!(t.join("dst/keep").exists(), "{mode} {backend}: deleted before the later scan error");
            assert_eq!(fs::read(t.join("dst/keep")).unwrap(), b"precious root entry");
            assert_eq!(fs::read(t.join("dst/deep/broken")).unwrap(), b"precious nested entry");
            assert_eq!(fs::read(t.join("dst/good")).unwrap(), b"copied");
            assert!(r.stdout.contains(SKIPPING), "{mode} {backend}: {}", r.stdout);
        }
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

/// Every entry below `dir`: `name/` for a directory, `name=content` for a file.
fn tree(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            if p.is_dir() {
                out.push(format!("{rel}/"));
                walk(root, &p, out);
            } else {
                out.push(format!("{rel}={}", fs::read_to_string(&p).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn files(root: &Path, list: &[(&str, &str)]) {
    for (rel, content) in list {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }
}

#[test]
fn several_sources_into_one_destination_merge_like_rsync() {
    for mode in MODES {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        fake_ssh(t);
        files(t, &[
            ("a/dup", "from-a"),
            ("a/fd", "file in a"),
            ("a/mm/p", "p"),
            ("a/mm/n/deep", "deep"),
            ("a/df/q", "q"),
            ("a/only-a", "A"),
            ("b/dup", "from-b, longer"),
            ("b/fd/inner", "dir in b"),
            ("b/mm/q2", "q2"),
            ("b/mm/n/other", "other"),
            ("b/df", "file in b"),
            ("b/only-b", "B"),
            ("b/skip-b", "excluded"),
            ("dst/dup", "old"),
            ("dst/stale", "gone"),
            ("dst/mm/old", "gone"),
            ("dst/mm/n/gone", "gone"),
            ("dst/skip-x", "excluded, kept"),
        ]);
        fs::set_permissions(t.join("a/mm"), fs::Permissions::from_mode(0o700)).unwrap();
        let args = ["-a", "--delete", "--exclude=skip-*"];
        let r = run_mode(t, mode, &args, &["a/", "b/"], "dst/");
        assert_eq!(r.code, 0, "{mode}: {}{}", r.stdout, r.stderr);
        // First source wins, a directory wins over a file, directories merge, and only
        // what neither source has is deleted.
        let want = [
            "df/", "df/q=q", "dup=from-a", "fd/", "fd/inner=dir in b", "mm/", "mm/n/", "mm/n/deep=deep",
            "mm/n/other=other", "mm/p=p", "mm/q2=q2", "only-a=A", "only-b=B", "skip-x=excluded, kept",
        ];
        assert_eq!(tree(&t.join("dst")), want, "{mode}: {}{}", r.stdout, r.stderr);
        // A merged directory has the first source's attributes.
        assert_eq!(fs::metadata(t.join("dst/mm")).unwrap().permissions().mode() & 0o777, 0o700, "{mode}");
        // Nothing left to do: the sources no longer undo each other.
        let mut again = args.to_vec();
        again.push("-i");
        let r = run_mode(t, mode, &again, &["a/", "b/"], "dst/");
        assert_eq!((r.code, r.stdout.as_str()), (0, ""), "{mode}: {}", r.stderr);
        assert_eq!(tree(&t.join("dst")), want, "{mode}");
    }
}

#[test]
fn named_items_merge_with_contents_and_each_other() {
    for mode in MODES {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        fake_ssh(t);
        files(t, &[
            ("a/x", "x"),
            ("c/e/z", "z"),
            ("dst/c/e/zz", "gone"),
            ("dst/stale", "gone"),
            ("p/y/one", "1"),
            ("q/y/two", "2"),
            ("dst2/y/three", "gone"),
            ("dst2/keep", "named items leave the destination's own entries alone"),
        ]);
        // `src/` with a named directory: the named one is merged into the contents.
        let r = run_mode(t, mode, &["-a", "--delete"], &["a/", "c"], "dst/");
        assert_eq!(r.code, 0, "{mode}: {}{}", r.stdout, r.stderr);
        assert_eq!(tree(&t.join("dst")), ["c/", "c/e/", "c/e/z=z", "x=x"], "{mode}");
        // The same into a destination that doesn't exist yet.
        let r = run_mode(t, mode, &["-a", "--delete"], &["a/", "c"], "new/");
        assert_eq!(r.code, 0, "{mode}: {}{}", r.stdout, r.stderr);
        assert_eq!(tree(&t.join("new")), ["c/", "c/e/", "c/e/z=z", "x=x"], "{mode}");
        // Two named directories of one name merge.
        let r = run_mode(t, mode, &["-a", "--delete"], &["p/y", "q/y"], "dst2/");
        assert_eq!(r.code, 0, "{mode}: {}{}", r.stdout, r.stderr);
        let want = ["keep=named items leave the destination's own entries alone", "y/", "y/one=1", "y/two=2"];
        assert_eq!(tree(&t.join("dst2")), want, "{mode}");
    }
}

#[test]
fn relative_paths_from_several_bases_merge() {
    for mode in MODES {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        fake_ssh(t);
        files(t, &[
            ("a/m/one", "1"),
            ("a/m/n/three", "3"),
            ("b/m/two", "2"),
            ("b/m/n/four", "4"),
            ("dst/m/gone", "x"),
            ("dst/m/n/gone", "x"),
            ("dst/keep", "outside the named paths"),
        ]);
        let r = run_mode(t, mode, &["-aR", "--delete"], &["a/./m/", "b/./m/"], "dst/");
        assert_eq!(r.code, 0, "{mode}: {}{}", r.stdout, r.stderr);
        let want = ["keep=outside the named paths", "m/", "m/n/", "m/n/four=4", "m/n/three=3", "m/one=1", "m/two=2"];
        assert_eq!(tree(&t.join("dst")), want, "{mode}");
    }
}

#[test]
fn a_missing_source_among_several_stops_deletion() {
    for mode in MODES {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        fake_ssh(t);
        files(t, &[("a/x", "x"), ("dst/stale", "kept: the other source is missing"), ("dst/sub/f", "kept")]);
        let r = run_mode(t, mode, &["-a", "--delete"], &["a/", "missing/"], "dst/");
        assert_eq!(tree(&t.join("dst")), ["stale=kept: the other source is missing", "sub/", "sub/f=kept", "x=x"], "{mode}: {}{}", r.stdout, r.stderr);
        assert_eq!(r.code, 23, "{mode}: {}{}", r.stdout, r.stderr);
        assert!(r.stdout.contains(SKIPPING), "{mode}: {}", r.stdout);
    }
}

/// Writes that would make a file longer than `limit` bytes fail with EFBIG ("File too
/// large"), in barqsync and in the server it starts: a destination write error without
/// a full disk. With SIGXFSZ ignored, such a write fails instead of killing the writer.
fn limit_file_size(cmd: &mut Command, limit: u64) {
    unsafe {
        cmd.pre_exec(move || {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            let lim = libc::rlimit { rlim_cur: limit as libc::rlim_t, rlim_max: limit as libc::rlim_t };
            match libc::setrlimit(libc::RLIMIT_FSIZE, &lim) {
                0 => Ok(()),
                _ => Err(std::io::Error::last_os_error()),
            }
        });
    }
}

/// The source has `new.bin`, the destination still has `old.bin`, which the source no
/// longer has, as after a rename. If `new.bin` can't be written, deleting `old.bin` leaves
/// the destination with neither. rsync's `--delete-after` keeps it, since a write error
/// ends rsync's run before it deletes; barqsync carries on with the other files, but
/// deletes nothing.
#[test]
fn a_failed_write_stops_deletion() {
    for mode in MODES {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        fake_ssh(t);
        files(t, &[("src/ok.txt", "copied"), ("dst/old.bin", "the only copy left")]);
        fs::write(t.join("src/new.bin"), vec![b'n'; 2 << 20]).unwrap();
        // --no-clone: a clone would make the copy without writing it.
        let args = mode_args(mode, &["-a", "--delete", "--no-clone"], &["src/"], "dst/");
        let r = sync_with(t, &args.iter().map(String::as_str).collect::<Vec<_>>(), |cmd| limit_file_size(cmd, 1 << 20));
        assert_eq!(r.code, 23, "{mode}: {}{}", r.stdout, r.stderr);
        assert!(r.stderr.contains("File too large"), "{mode}: {}", r.stderr);
        assert!(r.stdout.contains(SKIPPING), "{mode}: {}", r.stdout);
        // The other file is still copied, and nothing is left of the one that failed.
        assert_eq!(tree(&t.join("dst")), ["ok.txt=copied", "old.bin=the only copy left"], "{mode}: {}{}", r.stdout, r.stderr);
    }
}

/// [`a_failed_write_stops_deletion`] on a disk that is really full: a 32 MiB volume that
/// holds the 20 MiB `old.bin` has no room for the 20 MiB `new.bin`.
#[cfg(target_os = "macos")]
#[test]
fn a_full_disk_stops_deletion() {
    let Some(vol) = common::Volume::new(32) else {
        eprintln!("SKIP: no disk image could be attached");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    fake_ssh(t);
    fs::create_dir(t.join("src")).unwrap();
    fs::write(t.join("src/new.bin"), vec![b'n'; 20 << 20]).unwrap();
    for mode in MODES {
        let dst = vol.mount.join(mode);
        fs::create_dir(&dst).unwrap();
        fs::write(dst.join("old.bin"), vec![b'o'; 20 << 20]).unwrap();
        let dst_arg = format!("{}/", dst.display());
        let args = mode_args(mode, &["-a", "--delete"], &["src/"], &dst_arg);
        let r = sync(t, &args.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(r.code, 23, "{mode}: {}{}", r.stdout, r.stderr);
        assert!(r.stderr.contains("No space left on device"), "{mode}: {}", r.stderr);
        assert!(r.stdout.contains(SKIPPING), "{mode}: {}", r.stdout);
        // `old.bin` is untouched, and no temp file is left behind.
        assert_eq!(names(&dst), ["old.bin"], "{mode}: {}{}", r.stdout, r.stderr);
        assert_eq!(fs::read(dst.join("old.bin")).unwrap(), vec![b'o'; 20 << 20], "{mode}");
        fs::remove_dir_all(&dst).unwrap();
    }
}
