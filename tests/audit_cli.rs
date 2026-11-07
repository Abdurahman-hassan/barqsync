//! Audit 2026-10-30 (A6): command-line behaviour that scripts depend on: version and
//! help, loud refusal of rsync options barqsync does not implement, rsync's exit codes
//! for usage errors and partial transfers, dry runs that change nothing (locally and
//! through a stand-in remote shell), and the precedence of the hidden BARQSYNC_*
//! environment defaults, plus (second wave, A6b) read-only destination directories, the
//! diagnosis of a noisy or silent remote shell, stdout/stderr routing, `--log-file` modes,
//! `--timeout`, ssh failures and the README's trailing-slash table. Ignored tests record the open
//! findings AU-CLI-01, -02, -07, -08 and -09.

mod common;

use common::{barqsync, fake_ssh, Tree};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Stdio};

/// Everything a dry run must leave alone: type, mode, size, mtime (with nanoseconds),
/// inode, link count and content of every entry, the root included.
fn manifest(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
        entries.sort();
        for p in entries {
            let md = fs::symlink_metadata(&p).unwrap();
            let content = if md.file_type().is_file() {
                blake3::hash(&fs::read(&p).unwrap()).to_hex().to_string()
            } else if md.file_type().is_symlink() {
                fs::read_link(&p).unwrap().display().to_string()
            } else {
                String::new()
            };
            let rel = p.strip_prefix(root).unwrap().display().to_string();
            out.insert(
                rel,
                format!("{:o} {} {}.{} {} {} {}", md.mode(), md.len(), md.mtime(), md.mtime_nsec(), md.ino(), md.nlink(), content),
            );
            if md.is_dir() {
                walk(root, &p, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    let md = fs::symlink_metadata(root).unwrap();
    out.insert(".".into(), format!("{:o} {}.{} {}", md.mode(), md.mtime(), md.mtime_nsec(), md.ino()));
    walk(root, root, &mut out);
    out
}

/// A source and a destination that differ in every way `-a --delete` cares about.
fn differing_pair(root: &Path) {
    let src = Tree::new(root.join("src"));
    let dst = Tree::new(root.join("dst"));
    src.file("same", "same").file("changed", "new contents").file("sub/new", "n").file("newdir/x", "x");
    src.file("perm", "p").file("time", "t").symlink("link", "same");
    dst.file("same", "same").file("changed", "old").file("perm", "p").file("time", "t");
    dst.file("extra", "e").file("gone/deep/f", "f");
    src.chmod("perm", 0o644);
    dst.chmod("perm", 0o600);
    src.mtime("same", 1_600_000_000).mtime("perm", 1_600_000_000).mtime("time", 1_600_000_000);
    dst.mtime("same", 1_600_000_000).mtime("perm", 1_600_000_000).mtime("time", 1_500_000_000);
}

#[test]
fn version_and_help_go_to_stdout_with_exit_0() {
    let tmp = tempfile::tempdir().unwrap();
    let v = barqsync(tmp.path(), &["--version"]);
    assert_eq!(v.code, 0);
    assert_eq!(v.stdout, format!("barqsync {}\n", env!("CARGO_PKG_VERSION")));
    assert!(v.stderr.is_empty());
    let h = barqsync(tmp.path(), &["--help"]);
    assert_eq!(h.code, 0);
    assert!(h.stdout.contains("--human-readable") && h.stdout.contains("--no-OPTION"), "{}", h.stdout);
    // As in rsync, -h is --human-readable, not help: alone it is a usage error.
    let short = barqsync(tmp.path(), &["-h"]);
    assert_eq!(short.code, 1, "{}", short.stderr);
    assert!(short.stdout.is_empty());
}

#[test]
fn unimplemented_rsync_options_are_refused_not_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", "a");
    for flag in [
        "--fuzzy",
        "--copy-dest=x",
        "-y",
        "--daemon",
        "--iconv=utf8",
        "--checksum-choice=md5",
        "--compress-choice=zstd",
        "--preallocate",
        "--old-args",
        "--contimeout=5",
        "--no-implied-dirs",
        "--no-R",
        "--list-only",
        "--write-batch=b",
        "--debug=ALL",
    ] {
        let r = barqsync(tmp.path(), &["-a", flag, "src/", "dst/"]);
        assert_eq!(r.code, 1, "{flag}: {}", r.stderr);
        assert!(r.stdout.is_empty(), "{flag}: {}", r.stdout);
        let name = flag.split('=').next().unwrap();
        assert!(r.stderr.contains(name), "{flag}: {}", r.stderr);
        assert!(!tmp.path().join("dst").exists(), "{flag} created the destination");
    }
}

#[test]
fn daemon_locations_and_listing_mode_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", "a");
    for args in [&["-a", "rsync://localhost/mod/", "dst/"][..], &["-a", "localhost::mod/", "dst/"], &["-a", "src/", "rsync://localhost/mod/"]] {
        let r = barqsync(tmp.path(), args);
        assert_eq!(r.code, 1, "{args:?}: {}", r.stderr);
        assert!(r.stderr.contains("daemon mode is not supported"), "{args:?}: {}", r.stderr);
    }
    let list = barqsync(tmp.path(), &["-a", "src/"]);
    assert_eq!(list.code, 1);
    assert!(list.stderr.contains("listing mode is not supported"), "{}", list.stderr);
}

#[test]
fn usage_and_file_errors_use_rsyncs_exit_codes() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", "a");
    let cases: [(&[&str], i32, &str); 7] = [
        (&["-a", "--max-size=abc", "src/", "dst/"], 1, "--max-size value is invalid"),
        (&["--delete", "src/", "dst/"], 1, "--delete does not work without --recursive"),
        (&["-a", "--delete-before", "--delete-after", "src/", "dst/"], 1, "multiple --delete-WHEN"),
        (&["-a", "--info=bogus", "src/", "dst/"], 1, "Unknown --info item"),
        (&["-a", "--exclude-from=/nonexistent/x", "src/", "dst/"], 11, "failed to open exclude file"),
        (&["-a", "missing/", "dst/"], 23, "No such file or directory"),
        (&["-a", "src/", "no/such/parent/dst/"], 11, "No such file or directory"),
    ];
    for (args, code, msg) in cases {
        let r = barqsync(tmp.path(), args);
        assert_eq!(r.code, code, "{args:?}: {}", r.stderr);
        assert!(r.stderr.contains(msg), "{args:?}: {}", r.stderr);
        assert!(r.stdout.is_empty() || r.code != 1, "{args:?}: {}", r.stdout);
    }
}

#[test]
fn max_delete_stops_with_25() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("keep", "k");
    Tree::new(tmp.path().join("dst")).file("x1", "1").file("x2", "2").file("x3", "3");
    let r = barqsync(tmp.path(), &["-a", "--delete", "--max-delete=1", "src/", "dst/"]);
    assert_eq!(r.code, 25, "{}", r.stderr);
    assert!(r.stderr.contains("Deletions stopped due to --max-delete limit (2 skipped)"), "{}", r.stderr);
    let left = fs::read_dir(tmp.path().join("dst")).unwrap().count();
    assert_eq!(left, 3, "keep plus two of the three extras");
}

#[test]
fn quiet_leaves_stdout_empty_and_errors_on_stderr() {
    if unsafe { libc::geteuid() } == 0 {
        return; // root reads mode-000 files
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = Tree::new(tmp.path().join("src"));
    src.file("ok", "ok").file("noread", "secret");
    fs::set_permissions(src.path("noread"), fs::Permissions::from_mode(0o000)).unwrap();
    let r = barqsync(tmp.path(), &["-avq", "--stats", "src/", "dst/"]);
    fs::set_permissions(src.path("noread"), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(r.code, 23, "{}", r.stderr);
    assert!(r.stdout.is_empty(), "{}", r.stdout);
    assert!(r.stderr.contains("Permission denied"), "{}", r.stderr);
    assert_eq!(fs::read(tmp.path().join("dst/ok")).unwrap(), b"ok");
}

/// `-n --delete -i` changes nothing in either tree, and lists exactly what the real run
/// then does (as a set: barqsync's order follows its parallel transfer).
fn check_dry_run(root: &Path, args: &[&str]) {
    differing_pair(root);
    let before = (manifest(&root.join("src")), manifest(&root.join("dst")));
    let mut dry_args = vec!["-an", "--delete", "-i"];
    dry_args.extend_from_slice(args);
    let dry = barqsync(root, &dry_args);
    assert_eq!(dry.code, 0, "{}", dry.stderr);
    let after = (manifest(&root.join("src")), manifest(&root.join("dst")));
    assert_eq!(before, after, "the dry run changed something");
    let mut real_args = vec!["-a", "--delete", "-i"];
    real_args.extend_from_slice(args);
    let real = barqsync(root, &real_args);
    assert_eq!(real.code, 0, "{}", real.stderr);
    assert!(!dry.items().is_empty());
    assert_eq!(dry.items(), real.items(), "dry run:\n{}\nreal run:\n{}", dry.stdout, real.stdout);
    assert!(real.items().iter().any(|l| l.starts_with("*deleting")), "{}", real.stdout);
}

#[test]
fn dry_run_changes_nothing_and_previews_the_real_run_locally() {
    let tmp = tempfile::tempdir().unwrap();
    check_dry_run(tmp.path(), &["src/", "dst/"]);
}

#[test]
fn dry_run_changes_nothing_and_previews_the_real_run_remotely() {
    for pull in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let shell = fake_ssh(tmp.path());
        let rsh = shell.display().to_string();
        let bin = env!("CARGO_BIN_EXE_barqsync");
        let src = format!("fakehost:{}/src/", tmp.path().display());
        let dst = format!("fakehost:{}/dst/", tmp.path().display());
        let (from, to) = if pull { (src.as_str(), "dst/") } else { ("src/", dst.as_str()) };
        check_dry_run(tmp.path(), &["-e", &rsh, "--remote-path", bin, from, to]);
    }
}

#[test]
fn command_line_overrides_hidden_environment_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", "a");
    let run = |env: &str, extra: &[&str]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_barqsync"));
        c.env("BARQSYNC_TRANSPORT", env).arg("-a").args(extra).arg("src/").arg("dst/");
        common::run(&mut c, tmp.path())
    };
    // The hidden variable is read even for a local copy, and a bad value stops it...
    let bad = run("bogus", &[]);
    assert_eq!(bad.code, 1);
    assert!(bad.stderr.contains("--transport"), "{}", bad.stderr);
    // ...unless the option itself is given, which wins.
    let ok = run("bogus", &["--transport", "ssh"]);
    assert_eq!(ok.code, 0, "{}", ok.stderr);
    assert_eq!(fs::read(tmp.path().join("dst/a")).unwrap(), b"a");
}

/// AU-CLI-02: a client of an older protocol gets "protocol version mismatch" only if its
/// whole `Hello` (options included) still decodes as this version's. Options grew with
/// every protocol, so an older client's `Hello` fails to decode and the server answers
/// "expected a barqsync client handshake" (exit 12), and the client blames a missing
/// barqsync. The magic and version should be read before the options.
#[test]
#[ignore = "AU-CLI-02: open finding; the server cannot name an older client's protocol"]
fn server_names_an_older_clients_protocol() {
    // Frame: u32 LE body length, kind 0 (postcard message), body = Msg::Hello (variant 0),
    // magic, version 10 (0.4.x), server_is_sender = false, then 0.4.x's options (here cut
    // short, as they no longer match this version's layout).
    let body: Vec<u8> = [&[0u8][..], b"BARQSYNC", &[10, 0]].concat();
    let mut frame = (body.len() as u32).to_le_bytes().to_vec();
    frame.push(0);
    frame.extend_from_slice(&body);
    let mut child = Command::new(env!("CARGO_BIN_EXE_barqsync"))
        .arg("--server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&frame).unwrap();
    let out = child.wait_with_output().unwrap();
    let all = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(all.contains("protocol version mismatch"), "{all}");
    assert_eq!(out.status.code(), Some(2), "{all}");
}

// ---------------------------------------------------------------------------------------
// Second wave (A6b): read-only directories, remote-shell diagnosis, output routing, modes.
// ---------------------------------------------------------------------------------------

/// Makes every directory below the path writable again so the temporary directory can go.
struct Unlock(std::path::PathBuf);
impl Drop for Unlock {
    fn drop(&mut self) {
        fn walk(p: &Path) {
            if fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()) {
                let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o755));
                if let Ok(rd) = fs::read_dir(p) {
                    for e in rd.flatten() {
                        walk(&e.path());
                    }
                }
            }
        }
        walk(&self.0);
    }
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// The head of a stand-in remote shell: skip ssh's options and the host name.
const SKIP_OPTIONS: &str = "while [ $# -gt 0 ]; do case \"$1\" in -o|-p|-l|-i|-c|-F|-J|-S) shift 2;; -*) shift;; *) break;; esac; done\nshift\n";

/// AU-CLI-07: rsync makes a read-only directory it has to write into writable for the
/// transfer and restores its mode; barqsync fails every file inside it ("Permission denied",
/// exit 23), locally and through the remote receiver. Fresh copies of such trees work.
#[test]
#[ignore = "AU-CLI-07: open finding; files inside an existing read-only destination directory are not updated"]
fn updates_files_inside_an_existing_read_only_destination_directory() {
    if unsafe { libc::geteuid() } == 0 {
        return; // root writes anywhere
    }
    for remote in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let _unlock = Unlock(tmp.path().to_path_buf());
        let src = Tree::new(tmp.path().join("src"));
        src.file("ro/file", "one");
        fs::set_permissions(src.path("ro"), fs::Permissions::from_mode(0o555)).unwrap();
        let shell = fake_ssh(tmp.path()).display().to_string();
        let bin = env!("CARGO_BIN_EXE_barqsync");
        let dst = format!("fakehost:{}/dst/", tmp.path().display());
        let args: Vec<&str> = if remote { vec!["-a", "-e", &shell, "--remote-path", bin, "src/", &dst] } else { vec!["-a", "src/", "dst/"] };
        let first = barqsync(tmp.path(), &args);
        assert_eq!(first.code, 0, "remote={remote}: {}", first.stderr);
        fs::set_permissions(src.path("ro"), fs::Permissions::from_mode(0o755)).unwrap();
        src.file("ro/file", "changed").file("ro/new", "n");
        fs::set_permissions(src.path("ro"), fs::Permissions::from_mode(0o555)).unwrap();
        let second = barqsync(tmp.path(), &args);
        assert_eq!(second.code, 0, "remote={remote}: {}", second.stderr);
        assert_eq!(fs::read(tmp.path().join("dst/ro/file")).unwrap(), b"changed", "remote={remote}");
        assert_eq!(fs::read(tmp.path().join("dst/ro/new")).unwrap(), b"n", "remote={remote}");
        let mode = fs::metadata(tmp.path().join("dst/ro")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o555, "remote={remote}: the directory's mode is restored");
    }
}

/// AU-CLI-08: a file that could not be copied is still listed by -v, -i and --out-format,
/// where rsync lists only what it started to transfer.
#[test]
#[ignore = "AU-CLI-08: open finding; a file that failed to copy is listed as transferred"]
fn a_file_that_failed_to_copy_is_not_listed() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = Tree::new(tmp.path().join("src"));
    src.file("ok", "ok").file("noread", "secret");
    fs::set_permissions(src.path("noread"), fs::Permissions::from_mode(0o000)).unwrap();
    let r = barqsync(tmp.path(), &["-a", "--out-format=%n", "src/", "dst/"]);
    fs::set_permissions(src.path("noread"), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(r.code, 23, "{}", r.stderr);
    assert!(r.stdout.lines().any(|l| l == "ok"), "{}", r.stdout);
    assert!(!r.stdout.lines().any(|l| l == "noread"), "listed although the copy failed: {}", r.stdout);
}

/// What does pass today: items on stdout, errors on stderr, nothing mixed.
#[test]
fn items_go_to_stdout_and_errors_to_stderr() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = Tree::new(tmp.path().join("src"));
    src.file("ok", "ok").file("noread", "secret");
    fs::set_permissions(src.path("noread"), fs::Permissions::from_mode(0o000)).unwrap();
    let r = barqsync(tmp.path(), &["-avi", "src/", "dst/"]);
    fs::set_permissions(src.path("noread"), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(r.code, 23, "{}", r.stderr);
    assert!(r.stdout.lines().any(|l| l == ">f+++++++++ ok"), "{}", r.stdout);
    assert!(!r.stdout.to_lowercase().contains("denied"), "{}", r.stdout);
    assert!(r.stderr.contains("Permission denied"), "{}", r.stderr);
    assert!(!r.stderr.contains(">f+++++++++"), "{}", r.stderr);
}

/// AU-CLI-09: text a remote login script prints on stdout corrupts the protocol. rsync says
/// "protocol version mismatch -- is your shell clean?" (exit 2); barqsync blames a missing
/// barqsync (exit 12).
#[test]
#[ignore = "AU-CLI-09: open finding; a noisy remote shell is reported as a missing barqsync"]
fn text_printed_by_the_remote_shell_is_reported_as_a_dirty_shell() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", "a");
    let shell = tmp.path().join("banner_ssh");
    write_script(&shell, &format!("#!/bin/sh\n{SKIP_OPTIONS}echo 'Welcome to the host!'\nif [ $# -eq 1 ]; then exec sh -c \"$1\"; fi\nexec \"$@\"\n"));
    let rsh = shell.display().to_string();
    let dst = format!("fakehost:{}/dst/", tmp.path().display());
    let r = barqsync(tmp.path(), &["-a", "-e", &rsh, "--remote-path", env!("CARGO_BIN_EXE_barqsync"), "src/", &dst]);
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(!r.stderr.contains("installed on the remote host"), "{}", r.stderr);
}

/// Text on the remote shell's stderr is harmless, as with rsync.
#[test]
fn text_on_the_remote_shells_stderr_is_harmless() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", "a");
    let shell = tmp.path().join("banner_err_ssh");
    write_script(&shell, &format!("#!/bin/sh\n{SKIP_OPTIONS}echo 'Welcome banner' >&2\nif [ $# -eq 1 ]; then exec sh -c \"$1\"; fi\nexec \"$@\"\n"));
    let rsh = shell.display().to_string();
    let dst = format!("fakehost:{}/dst/", tmp.path().display());
    let r = barqsync(tmp.path(), &["-a", "-e", &rsh, "--remote-path", env!("CARGO_BIN_EXE_barqsync"), "src/", &dst]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(fs::read(tmp.path().join("dst/a")).unwrap(), b"a");
}

/// AU-CLI-01 again, now through the remote receiver: rsync exits 11 (file I/O).
#[test]
#[ignore = "AU-CLI-01: open finding; a missing remote parent directory exits 12 where rsync exits 11"]
fn missing_remote_parent_directory_exits_with_the_file_io_code() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", "a");
    let rsh = fake_ssh(tmp.path()).display().to_string();
    let dst = format!("fakehost:{}/no/such/parent/dst/", tmp.path().display());
    let r = barqsync(tmp.path(), &["-a", "-e", &rsh, "--remote-path", env!("CARGO_BIN_EXE_barqsync"), "src/", &dst]);
    assert_eq!(r.code, 11, "{}", r.stderr);
}

/// A client whose Hello decodes but speaks another protocol version gets the readable
/// message and exit 2 (rsync's "protocol incompatibility"). The Hello is captured from a
/// real client run and its version byte changed on the way to a real server.
#[test]
fn server_names_the_protocol_of_a_decodable_hello_from_another_version() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", "a");
    let capture = tmp.path().join("hello.bin");
    let shell = tmp.path().join("tee_ssh");
    write_script(&shell, &format!("#!/bin/sh\n{SKIP_OPTIONS}if [ $# -eq 1 ]; then tee \"$CAPTURE\" | sh -c \"$1\"; else tee \"$CAPTURE\" | \"$@\"; fi\n"));
    let bin = env!("CARGO_BIN_EXE_barqsync");
    let mut c = Command::new(bin);
    c.env("CAPTURE", &capture).args(["-a", "-e"]).arg(&shell).args(["--remote-path", bin, "src/"]).arg(format!("fakehost:{}/dst/", tmp.path().display()));
    let ok = common::run(&mut c, tmp.path());
    assert_eq!(ok.code, 0, "{}", ok.stderr);
    let captured = fs::read(&capture).unwrap();
    let len = u32::from_le_bytes(captured[..4].try_into().unwrap()) as usize;
    let mut frame = captured[..5 + len].to_vec();
    assert_eq!(&frame[6..14], b"BARQSYNC", "frame layout: u32 length, kind, Msg::Hello variant, magic");
    frame[14] = 99; // the version, a one-byte varint
    let mut child = Command::new(bin).arg("--server").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(&frame).unwrap();
    let out = child.wait_with_output().unwrap();
    let all = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(all.contains("protocol version mismatch: client speaks 99"), "{all}");
    assert_eq!(out.status.code(), Some(2), "{all}");
}

/// Exit codes of remote-shell trouble, as rsync's: a peer that stays silent ends with 30
/// after --timeout, an unreachable ssh port with 255, a peer that exits at once with 12.
#[test]
fn remote_shell_trouble_uses_rsyncs_exit_codes() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", "a");
    let sleepy = tmp.path().join("sleepy_ssh");
    write_script(&sleepy, "#!/bin/sh\nexec sleep 20\n");
    let rsh = sleepy.display().to_string();
    let started = std::time::Instant::now();
    let silent = barqsync(tmp.path(), &["-a", "--timeout=2", "-e", &rsh, "--remote-path", env!("CARGO_BIN_EXE_barqsync"), "src/", "fakehost:dst/"]);
    assert_eq!(silent.code, 30, "{}", silent.stderr);
    assert!(silent.stderr.contains("io timeout after 2 seconds"), "{}", silent.stderr);
    assert!(started.elapsed().as_secs() < 15, "the timeout took {:?}", started.elapsed());
    let refused = barqsync(tmp.path(), &["-a", "-e", "ssh -F /dev/null -p 1 -o BatchMode=yes -o ConnectTimeout=5", "src/", "127.0.0.1:dst/"]);
    assert_eq!(refused.code, 255, "{}", refused.stderr);
    let fake = fake_ssh(tmp.path()).display().to_string();
    let gone = barqsync(tmp.path(), &["-a", "-e", &fake, "--remote-path", "/usr/bin/true", "src/", "fakehost:dst/"]);
    assert_eq!(gone.code, 12, "{}", gone.stderr);
    assert!(gone.stderr.contains("unexpectedly closed"), "{}", gone.stderr);
}

/// --log-file is created with 0644 less the umask, like rsync's, and appended to.
#[test]
fn log_file_is_created_with_0644_less_the_umask_and_appended_to() {
    use std::os::unix::process::CommandExt;
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", "a");
    for (umask, mode) in [(0o022u32, 0o644u32), (0o077, 0o600)] {
        let dir = tmp.path().join(format!("u{umask:o}"));
        fs::create_dir(&dir).unwrap();
        for _ in 0..2 {
            let mut c = Command::new(env!("CARGO_BIN_EXE_barqsync"));
            c.args(["-a", "--log-file=log.txt"]).arg(tmp.path().join("src/")).arg("dst/");
            unsafe {
                c.pre_exec(move || {
                    libc::umask(umask as libc::mode_t);
                    Ok(())
                });
            }
            let r = common::run(&mut c, &dir);
            assert_eq!(r.code, 0, "{}", r.stderr);
        }
        let log = dir.join("log.txt");
        assert_eq!(fs::metadata(&log).unwrap().permissions().mode() & 0o777, mode, "umask {umask:o}");
        let text = fs::read_to_string(&log).unwrap();
        assert_eq!(text.matches("building file list").count(), 2, "appended, not truncated:\n{text}");
    }
}

/// The README's trailing-slash table, and "creates the destination folder but not missing
/// folders above it" (exit 11, as rsync).
#[test]
fn readme_trailing_slash_table_and_missing_parent() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("photos")).file("a.jpg", "a").file("b.jpg", "b");
    let r = barqsync(tmp.path(), &["-a", "photos/", "backup/"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(tmp.path().join("backup/a.jpg").exists() && tmp.path().join("backup/b.jpg").exists());
    assert!(!tmp.path().join("backup/photos").exists());
    let r = barqsync(tmp.path(), &["-a", "photos", "backup2/"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(tmp.path().join("backup2/photos/a.jpg").exists() && !tmp.path().join("backup2/a.jpg").exists());
    let r = barqsync(tmp.path(), &["-av", "photos/", "Nope/Backup/photos/"]);
    assert_eq!(r.code, 11, "{}", r.stderr);
    assert!(!tmp.path().join("Nope").exists());
}

/// -T and --delay-updates refusals carry rsync's wording and exit code.
#[test]
fn temp_dir_and_delay_updates_refusals_match_rsync() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", "a");
    let r = barqsync(tmp.path(), &["-a", "-T", "nodir", "src/", "dst/"]);
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stderr.contains("The temp-dir does not exist: nodir"), "{}", r.stderr);
    let r = barqsync(tmp.path(), &["-a", "--delay-updates", "--inplace", "src/", "dst2/"]);
    assert_eq!(r.code, 1, "{}", r.stderr);
}
