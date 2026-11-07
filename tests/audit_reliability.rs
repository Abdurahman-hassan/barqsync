//! Audit 2026-10-30 (A4): reliability under interruption, faults and concurrency, and
//! robustness of the frame decoder against truncated or garbled input.
//!
//! Every transfer runs in a disposable temp dir, through a fake ssh (`sh -c`), with a
//! deadline that kills the whole process group on a hang. The remote side runs through a
//! per-test symlink to the binary, so its processes can be found (and must be gone) by
//! that unique path.
//!
//! Tests marked `#[ignore = "AU-TST-..."]` reproduce audit findings: they assert the
//! behaviour the audit expects and fail on 475db4c. Run them with
//! `cargo test --locked --test audit_reliability -- --ignored`.
//!
//! `AUDIT_STRESS_ITERS` (default 2) sets the repetitions of the concurrency tests.

mod common;

use proptest::prelude::*;
use std::collections::BTreeMap;
use std::fs;
use std::io::Cursor;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_barqsync");

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    /// Unique path of the remote-side binary (a symlink to it).
    server: PathBuf,
    ssh: PathBuf,
    cache: PathBuf,
}

static SEQ: AtomicU32 = AtomicU32::new(0);

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let ssh = root.join("fake-ssh");
    fs::write(&ssh, "#!/bin/sh\nshift\nexec /bin/sh -c \"$*\"\n").unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let server = root.join(format!("audit-srv-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::SeqCst)));
    std::os::unix::fs::symlink(BIN, &server).unwrap();
    let cache = root.join("cache");
    fs::create_dir(&cache).unwrap();
    Env { _tmp: tmp, root, server, ssh, cache }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Local,
    Push,
    Pull,
}

const MODES: [Mode; 3] = [Mode::Local, Mode::Push, Mode::Pull];

impl Env {
    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// Arguments for `opts SRC DST` in `mode` (`src`/`dst` relative to the root).
    fn args(&self, mode: Mode, opts: &[&str], src: &str, dst: &str) -> Vec<String> {
        let mut a: Vec<String> = Vec::new();
        if mode != Mode::Local {
            a.push("-e".into());
            a.push(self.ssh.display().to_string());
            a.push(format!("--remote-path={}", self.server.display()));
        }
        a.extend(opts.iter().map(|s| s.to_string()));
        let remote = |rel: &str| format!("fakehost:{}/{rel}", self.root.display());
        match mode {
            Mode::Local => a.extend([src.to_string(), dst.to_string()]),
            Mode::Push => a.extend([src.to_string(), remote(dst)]),
            Mode::Pull => a.extend([remote(src), dst.to_string()]),
        }
        a
    }

    fn command(&self, args: &[String]) -> Command {
        let mut c = Command::new(BIN);
        c.current_dir(&self.root)
            .env("LC_ALL", "C.UTF-8")
            .env("BARQSYNC_CACHE_DIR", &self.cache)
            .args(args)
            .stdin(Stdio::null())
            .process_group(0);
        c
    }

    /// Start a transfer with its output in files under the root.
    fn spawn(&self, args: &[String], tag: &str) -> Child {
        let out = fs::File::create(self.p(&format!("{tag}.out"))).unwrap();
        let err = fs::File::create(self.p(&format!("{tag}.err"))).unwrap();
        self.command(args).stdout(out).stderr(err).spawn().unwrap()
    }

    fn stderr(&self, tag: &str) -> String {
        fs::read_to_string(self.p(&format!("{tag}.err"))).unwrap_or_default()
    }

    /// Run to completion within `limit`; returns (exit code, stderr).
    fn run(&self, args: &[String], limit: Duration) -> (i32, String) {
        let tag = format!("run{}", SEQ.fetch_add(1, Ordering::SeqCst));
        let mut child = self.spawn(args, &tag);
        let code = wait_for(&mut child, limit).unwrap_or_else(|| panic!("hung for {limit:?}: {args:?}\n{}", self.stderr(&tag)));
        (code, self.stderr(&tag))
    }

    /// Remote-side processes of this test still running (not the client, whose
    /// `--remote-path=` names the same path).
    fn servers(&self) -> Vec<i32> {
        pids(&self.server.display().to_string(), "--server", "--remote-path=")
    }

    /// Wait until no remote-side process of this test is left.
    fn servers_gone(&self, limit: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < limit {
            if self.servers().is_empty() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Never leave a server of this test behind, even when it failed.
        for pid in self.servers() {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let _ = Command::new("chmod").args(["-R", "u+rwx"]).arg(&self.root).status();
    }
}

/// Exit code, or `None` (after killing its process group) when it outlives `limit`.
/// A death by signal is reported as 128 + the signal.
fn wait_for(child: &mut Child, limit: Duration) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    let start = Instant::now();
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return Some(s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0)));
        }
        if start.elapsed() > limit {
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Processes whose command line contains `pat` and `also` but not `not`.
fn pids(pat: &str, also: &str, not: &str) -> Vec<i32> {
    let mut out = Vec::new();
    let ps = Command::new("ps").args(["-axww", "-o", "pid=,command="]).output().unwrap();
    for line in String::from_utf8_lossy(&ps.stdout).lines() {
        let line = line.trim_start();
        if let Some((pid, cmd)) = line.split_once(' ')
            && cmd.contains(pat)
            && cmd.contains(also)
            && !cmd.contains(not)
            && !cmd.starts_with("ps ")
        {
            out.push(pid.parse().unwrap());
        }
    }
    out.retain(|&p| p != std::process::id() as i32);
    out
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// About 13 MiB: 60 small and medium files (20 of them in `sub/`) and three of 3 MiB.
/// Under `--bwlimit=4000` a transfer of it takes ~3 s.
fn source_tree(dir: &Path) {
    fs::create_dir_all(dir.join("sub")).unwrap();
    for i in 0..60u64 {
        let len = [100, 5000, 200_000][(i % 3) as usize];
        let rel = if i % 3 == 0 { format!("sub/f{i:03}") } else { format!("f{i:03}") };
        fs::write(dir.join(rel), noise(len, i + 1)).unwrap();
    }
    for i in 0..3u64 {
        fs::write(dir.join(format!("big{i}")), noise(3 << 20, 100 + i)).unwrap();
    }
}

/// `.name.XXXXXX`, as `fsops::temp_name` makes them.
fn is_temp(name: &[u8]) -> bool {
    name.len() > 8 && name[0] == b'.' && name[name.len() - 7] == b'.' && name[name.len() - 6..].iter().all(u8::is_ascii_alphanumeric)
}

/// Regular files below `dir`, by relative path.
fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            // A temp file may vanish while the interrupted run is still cleaning up.
            let Ok(md) = fs::symlink_metadata(&p) else { continue };
            if md.is_dir() {
                walk(root, &p, out);
            } else if md.is_file() {
                out.insert(p.strip_prefix(root).unwrap().to_path_buf(), fs::read(&p).unwrap_or_default());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// Every final-named file in `dst` equals the source's file of that name; returns the
/// temp files found.
fn check_complete_or_absent(src: &Path, dst: &Path) -> Vec<PathBuf> {
    let want = files(src);
    let mut temps = Vec::new();
    for (rel, data) in files(dst) {
        if is_temp(rel.file_name().unwrap().as_bytes()) {
            temps.push(rel);
            continue;
        }
        let Some(w) = want.get(&rel) else { panic!("unexpected file in destination: {}", rel.display()) };
        assert!(*w == data, "{}: a final-named destination file differs from the source ({} vs {} bytes)", rel.display(), data.len(), w.len());
    }
    temps
}

fn assert_same_files(src: &Path, dst: &Path, what: &str) {
    let (a, b) = (files(src), files(dst));
    let names = |m: &BTreeMap<PathBuf, Vec<u8>>| m.keys().cloned().collect::<Vec<_>>();
    assert_eq!(names(&a), names(&b), "{what}: file names differ");
    for (k, v) in &a {
        assert!(b[k] == *v, "{what}: {} differs", k.display());
    }
}

/// Wait until the transfer has written something into `dst` (a file or a temp file).
fn wait_started(child: &mut Child, dst: &Path) {
    let start = Instant::now();
    while files(dst).is_empty() {
        if let Some(s) = child.try_wait().unwrap() {
            panic!("transfer ended before writing anything: {s}");
        }
        assert!(start.elapsed() < Duration::from_secs(30), "transfer never started");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn stress_iters() -> usize {
    std::env::var("AUDIT_STRESS_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(2)
}

fn am_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

const LIMIT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Interruption: SIGINT/SIGTERM, a second signal, SIGKILL of either side
// ---------------------------------------------------------------------------

/// An interrupt mid-transfer ends with 20, leaves only complete files under their final
/// names and no temp file, and a rerun converges.
#[test]
fn an_interrupt_mid_transfer_leaves_complete_files_only_and_a_rerun_converges() {
    for mode in MODES {
        for sig in [libc::SIGINT, libc::SIGTERM] {
            let e = env();
            source_tree(&e.p("src"));
            let mut child = e.spawn(&e.args(mode, &["-a", "--bwlimit=4000"], "src/", "dst/"), "t");
            wait_started(&mut child, &e.p("dst"));
            std::thread::sleep(Duration::from_millis(300));
            unsafe { libc::kill(child.id() as i32, sig) };
            let code = wait_for(&mut child, Duration::from_secs(20)).unwrap_or_else(|| panic!("{mode:?} sig {sig}: hung after the signal"));
            assert_eq!(code, 20, "{mode:?} sig {sig}: {}", e.stderr("t"));
            if mode != Mode::Local {
                assert!(e.servers_gone(Duration::from_secs(10)), "{mode:?} sig {sig}: the remote side is still running");
            }
            let temps = check_complete_or_absent(&e.p("src"), &e.p("dst"));
            assert!(temps.is_empty(), "{mode:?} sig {sig}: temp files left: {temps:?}");
            let (code, err) = e.run(&e.args(mode, &["-a", "--delete"], "src/", "dst/"), LIMIT);
            assert_eq!(code, 0, "{mode:?} sig {sig}: rerun: {err}");
            assert_same_files(&e.p("src"), &e.p("dst"), &format!("{mode:?} sig {sig} rerun"));
        }
    }
}

/// A second signal forces an immediate exit (20); it too removes registered temp files.
#[test]
fn a_second_signal_exits_at_once_without_corrupt_files() {
    for mode in MODES {
        let e = env();
        source_tree(&e.p("src"));
        let mut child = e.spawn(&e.args(mode, &["-a", "--bwlimit=4000"], "src/", "dst/"), "t");
        wait_started(&mut child, &e.p("dst"));
        std::thread::sleep(Duration::from_millis(300));
        let pid = child.id() as i32;
        unsafe { libc::kill(pid, libc::SIGINT) };
        std::thread::sleep(Duration::from_millis(5));
        unsafe { libc::kill(pid, libc::SIGINT) };
        let code = wait_for(&mut child, Duration::from_secs(20)).unwrap_or_else(|| panic!("{mode:?}: hung after two signals"));
        assert_eq!(code, 20, "{mode:?}: {}", e.stderr("t"));
        if mode != Mode::Local {
            assert!(e.servers_gone(Duration::from_secs(10)), "{mode:?}: the remote side is still running");
        }
        // The client's own temp files are swept; a push's are the server's to remove.
        let temps = check_complete_or_absent(&e.p("src"), &e.p("dst"));
        assert!(temps.is_empty(), "{mode:?}: temp files left: {temps:?}");
    }
}

/// SIGKILL of the client: no cleanup is possible on its own side, but no final-named file
/// is ever incomplete, the remote side ends, and a rerun with --delete converges and
/// removes the leftovers.
#[test]
fn a_killed_client_never_leaves_a_corrupt_file_and_a_rerun_converges() {
    for mode in MODES {
        let e = env();
        source_tree(&e.p("src"));
        let mut child = e.spawn(&e.args(mode, &["-a", "--bwlimit=4000"], "src/", "dst/"), "t");
        wait_started(&mut child, &e.p("dst"));
        std::thread::sleep(Duration::from_millis(300));
        unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
        let _ = child.wait();
        if mode != Mode::Local {
            assert!(e.servers_gone(Duration::from_secs(15)), "{mode:?}: the remote side outlived its client");
        }
        let temps = check_complete_or_absent(&e.p("src"), &e.p("dst"));
        if mode == Mode::Push {
            // The server noticed its client's end and removed its own temp files.
            assert!(temps.is_empty(), "push: the server left temp files: {temps:?}");
        }
        let (code, err) = e.run(&e.args(mode, &["-a", "--delete"], "src/", "dst/"), LIMIT);
        assert_eq!(code, 0, "{mode:?}: rerun: {err}");
        assert_same_files(&e.p("src"), &e.p("dst"), &format!("{mode:?} rerun after SIGKILL"));
    }
}

/// SIGKILL of the remote side: the client ends (not 0) within seconds, nothing
/// final-named is incomplete, and a rerun converges.
#[test]
fn a_killed_server_ends_the_client_and_a_rerun_converges() {
    for mode in [Mode::Push, Mode::Pull] {
        let e = env();
        source_tree(&e.p("src"));
        let mut child = e.spawn(&e.args(mode, &["-a", "--bwlimit=4000"], "src/", "dst/"), "t");
        wait_started(&mut child, &e.p("dst"));
        std::thread::sleep(Duration::from_millis(300));
        let servers = e.servers();
        assert!(!servers.is_empty(), "{mode:?}: no server found");
        for pid in servers {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let code = wait_for(&mut child, Duration::from_secs(20)).unwrap_or_else(|| panic!("{mode:?}: the client hung after its server died"));
        assert_ne!(code, 0, "{mode:?}: a transfer whose server died reported success");
        assert!(!e.stderr("t").contains("panicked"), "{mode:?}: {}", e.stderr("t"));
        check_complete_or_absent(&e.p("src"), &e.p("dst"));
        let (code, err) = e.run(&e.args(mode, &["-a", "--delete"], "src/", "dst/"), LIMIT);
        assert_eq!(code, 0, "{mode:?}: rerun: {err}");
        assert_same_files(&e.p("src"), &e.p("dst"), &format!("{mode:?} rerun after the server died"));
    }
}

// ---------------------------------------------------------------------------
// Permission failures
// ---------------------------------------------------------------------------

/// The two-run scenario of a read-only directory: the first run mirrors `ro/` (0555), the
/// second must update a file inside it. Returns (exit code, stderr, content of ro/f,
/// whether dst/stale survived --delete).
fn read_only_dir_second_run(e: &Env, mode: Mode) -> (i32, String, Vec<u8>, bool) {
    fs::create_dir_all(e.p("src/ro")).unwrap();
    fs::write(e.p("src/ro/f"), "v1").unwrap();
    fs::write(e.p("src/top"), "top").unwrap();
    fs::set_permissions(e.p("src/ro"), fs::Permissions::from_mode(0o555)).unwrap();
    let (code, err) = e.run(&e.args(mode, &["-a"], "src/", "dst/"), LIMIT);
    assert_eq!(code, 0, "{mode:?}: first run: {err}");
    assert_eq!(fs::metadata(e.p("dst/ro")).unwrap().permissions().mode() & 0o777, 0o555);
    // A file inside the read-only source directory is edited in place (no directory
    // write needed), and the destination has an entry the source no longer has.
    fs::write(e.p("src/ro/f"), "v2, longer").unwrap();
    common::set_mtime(&e.p("src/ro/f"), common::BASE_TIME);
    fs::write(e.p("dst/stale"), "stale").unwrap();
    let (code, err) = e.run(&e.args(mode, &["-a", "--delete"], "src/", "dst/"), LIMIT);
    let content = fs::read(e.p("dst/ro/f")).unwrap();
    (code, err, content, e.p("dst/stale").exists())
}

/// What barqsync guarantees today when it cannot write into a read-only destination
/// directory: exit 23, the old file intact, no temp file, and no deletion.
#[test]
fn a_read_only_destination_directory_fails_safely() {
    if am_root() {
        eprintln!("SKIP: root writes into read-only directories");
        return;
    }
    for mode in MODES {
        let e = env();
        let (code, err, content, stale_kept) = read_only_dir_second_run(&e, mode);
        if code == 0 {
            // The behaviour AU-TST-01 asks for: then the update must be complete.
            assert_eq!(content, b"v2, longer", "{mode:?}");
            continue;
        }
        assert_eq!(code, 23, "{mode:?}: {err}");
        assert!(err.contains("Permission denied"), "{mode:?}: {err}");
        assert_eq!(content, b"v1", "{mode:?}: the old file must stay intact");
        assert!(stale_kept, "{mode:?}: a failed write must stop deletion");
        let temps: Vec<_> = files(&e.p("dst")).into_keys().filter(|p| is_temp(p.file_name().unwrap().as_bytes())).collect();
        assert!(temps.is_empty(), "{mode:?}: {temps:?}");
    }
}

/// AU-TST-01: rsync updates a file inside a read-only (0555) destination directory by
/// making the directory writable for the update and restoring its mode; barqsync fails
/// with exit 23 on every run, which also stops every --delete in that tree.
#[test]
#[ignore = "AU-TST-01: updates inside a read-only destination directory fail (exit 23); rsync succeeds"]
fn a_file_inside_a_read_only_directory_is_updated_like_rsync() {
    if am_root() {
        eprintln!("SKIP: root writes into read-only directories");
        return;
    }
    for mode in MODES {
        let e = env();
        let (code, err, content, stale_kept) = read_only_dir_second_run(&e, mode);
        assert_eq!(code, 0, "{mode:?}: {err}");
        assert_eq!(content, b"v2, longer", "{mode:?}");
        assert!(!stale_kept, "{mode:?}: --delete did not run");
        assert_eq!(fs::metadata(e.p("dst/ro")).unwrap().permissions().mode() & 0o777, 0o555, "{mode:?}: mode restored");
    }
}

/// An unreadable source file: exit 23, the other files arrive, nothing is deleted.
#[test]
fn an_unreadable_source_file_is_a_partial_transfer_without_deletion() {
    if am_root() {
        eprintln!("SKIP: root reads every file");
        return;
    }
    for mode in MODES {
        let e = env();
        fs::create_dir_all(e.p("src")).unwrap();
        fs::create_dir_all(e.p("dst")).unwrap();
        fs::write(e.p("src/ok"), "ok").unwrap();
        fs::write(e.p("src/secret"), noise(300_000, 1)).unwrap();
        fs::set_permissions(e.p("src/secret"), fs::Permissions::from_mode(0o000)).unwrap();
        fs::write(e.p("dst/stale"), "stale").unwrap();
        let (code, err) = e.run(&e.args(mode, &["-a", "--delete"], "src/", "dst/"), LIMIT);
        assert_eq!(code, 23, "{mode:?}: {err}");
        assert_eq!(fs::read(e.p("dst/ok")).unwrap(), b"ok", "{mode:?}");
        assert!(!e.p("dst/secret").exists(), "{mode:?}: an unreadable file must not arrive (empty or partial)");
        assert!(e.p("dst/stale").exists(), "{mode:?}: deletion after a read error");
    }
}

// ---------------------------------------------------------------------------
// The destination disappears mid-run
// ---------------------------------------------------------------------------

/// Rename the destination while the transfer runs. Returns (exit code, stderr, temp
/// files left in the moved directory).
fn destination_moved_mid_run(e: &Env, mode: Mode) -> (i32, String, Vec<PathBuf>) {
    source_tree(&e.p("src"));
    let mut child = e.spawn(&e.args(mode, &["-a", "--bwlimit=4000"], "src/", "dst/"), "t");
    wait_started(&mut child, &e.p("dst"));
    std::thread::sleep(Duration::from_millis(300));
    fs::rename(e.p("dst"), e.p("moved")).unwrap();
    let code = wait_for(&mut child, LIMIT).unwrap_or_else(|| panic!("{mode:?}: hung after the destination moved"));
    if mode != Mode::Local {
        assert!(e.servers_gone(Duration::from_secs(10)), "{mode:?}: the remote side is still running");
    }
    let temps = check_complete_or_absent(&e.p("src"), &e.p("moved"));
    (code, e.stderr("t"), temps)
}

/// What is guaranteed today: the run ends (no hang, no panic), with an error, and every
/// final-named file in the moved directory is complete.
#[test]
fn a_destination_moved_mid_run_ends_the_run_without_corrupt_files() {
    for mode in [Mode::Local, Mode::Push] {
        let e = env();
        let (code, err, _temps) = destination_moved_mid_run(&e, mode);
        assert!(!err.contains("panicked"), "{mode:?}: {err}");
        assert!(code == 0 || code == 23 || code == 11, "{mode:?}: exit {code}: {err}");
    }
}

/// AU-TST-02: rsync works relative to the destination directory it opened (chdir) and
/// completes into the moved directory with exit 0; barqsync addresses files by path, so
/// it fails (23) and leaves the temp files of its in-flight files in the moved directory.
#[test]
#[ignore = "AU-TST-02: a destination moved mid-run leaves temp files behind (path-based cleanup)"]
fn a_destination_moved_mid_run_leaves_no_temp_files() {
    for mode in [Mode::Local, Mode::Push] {
        let e = env();
        let (code, err, temps) = destination_moved_mid_run(&e, mode);
        assert!(temps.is_empty(), "{mode:?}: exit {code}, temp files left in the moved directory: {temps:?}\n{err}");
    }
}

// ---------------------------------------------------------------------------
// Storage exhaustion (macOS: a disposable APFS disk image)
// ---------------------------------------------------------------------------

/// A delta update (clone of the old file, then patching) that runs out of space keeps the
/// old file intact and leaves no temp file; exit 23. Locally (--no-whole-file) and
/// remotely (delta is the remote default).
#[cfg(target_os = "macos")]
#[test]
fn a_full_disk_during_a_delta_update_keeps_the_old_file() {
    let Some(vol) = common::Volume::new(48) else {
        eprintln!("SKIP: no disk image could be attached");
        return;
    };
    let e = env();
    let old = noise(12 << 20, 1);
    let mut new = old.clone();
    // 8 MiB rewritten in the middle: the patch needs 8 MiB more than the clone.
    new[2 << 20..10 << 20].copy_from_slice(&noise(8 << 20, 2));
    fs::create_dir_all(e.p("src")).unwrap();
    fs::write(e.p("src/db.bin"), &new).unwrap();
    for (i, mode) in [Mode::Local, Mode::Push].into_iter().enumerate() {
        let dst = vol.mount.join(format!("d{i}"));
        fs::create_dir_all(&dst).unwrap();
        fs::write(dst.join("db.bin"), &old).unwrap();
        common::set_mtime(&dst.join("db.bin"), common::BASE_TIME);
        // Fill the volume so that less than the 8 MiB patch is free.
        let mut n = 0u32;
        while fs::write(vol.mount.join(format!("fill{i}-{n}")), vec![0x5au8; 1 << 20]).is_ok() {
            n += 1;
        }
        let _ = fs::remove_file(vol.mount.join(format!("fill{i}-{}", n.saturating_sub(1))));
        let _ = fs::remove_file(vol.mount.join(format!("fill{i}-{}", n.saturating_sub(2))));
        let dst_arg = format!("{}/", dst.display());
        let mut args = e.args(mode, &["-a", "--no-whole-file"], "src/", "DST");
        let last = args.len() - 1;
        args[last] = if mode == Mode::Local { dst_arg } else { format!("fakehost:{dst_arg}") };
        let (code, err) = e.run(&args, LIMIT);
        let now = fs::read(dst.join("db.bin")).unwrap();
        assert!(now == old || now == new, "{mode:?}: the destination file is neither old nor new (exit {code}): {err}");
        if now == new {
            assert_eq!(code, 0, "{mode:?}: {err}");
        } else {
            assert_eq!(code, 23, "{mode:?}: {err}");
            assert!(err.contains("No space left on device"), "{mode:?}: {err}");
        }
        let names: Vec<_> = fs::read_dir(&dst).unwrap().flatten().map(|d| d.file_name()).collect();
        assert_eq!(names.len(), 1, "{mode:?}: temp files left: {names:?}");
        for n in 0..=n {
            let _ = fs::remove_file(vol.mount.join(format!("fill{i}-{n}")));
        }
    }
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

/// Four identical transfers into one existing destination at once (with --delete): all
/// succeed, the destination equals the source, and no temp file is left.
#[test]
fn concurrent_transfers_into_one_destination_converge() {
    for _ in 0..stress_iters() {
        for (mode, extra) in [(Mode::Local, "--threads=1"), (Mode::Local, "--threads=8"), (Mode::Push, "--transport=ssh"), (Mode::Push, "--transport=tcp")] {
            let e = env();
            source_tree(&e.p("src"));
            fs::create_dir(e.p("dst")).unwrap();
            let args = e.args(mode, &["-a", "--delete", extra], "src/", "dst/");
            let mut kids: Vec<Child> = (0..4).map(|i| e.spawn(&args, &format!("c{i}"))).collect();
            let codes: Vec<Option<i32>> = kids.iter_mut().map(|k| wait_for(k, LIMIT)).collect();
            let errs: Vec<String> = (0..4).map(|i| e.stderr(&format!("c{i}"))).collect();
            assert!(codes.iter().all(|c| *c == Some(0)), "{mode:?} {extra}: exit codes {codes:?}\n{errs:?}");
            assert_same_files(&e.p("src"), &e.p("dst"), &format!("{mode:?} {extra} concurrent"));
        }
    }
}

/// Remote transfers with one or many workers, over the ssh pipe and parallel streams, with
/// both I/O layers: none may hang (prior NET-02: a deadlock with `--threads=1`) and each
/// must reproduce the source exactly, also when the destination already holds older data
/// (deltas) and with --delete.
#[test]
fn remote_transfers_with_any_worker_count_never_hang() {
    let configs: &[&[&str]] = &[
        &["--threads=1"],
        &["--threads=2"],
        &["--threads=1", "--transport=tcp", "--streams=1"],
        &["--threads=2", "--transport=tcp", "--streams=4"],
        &["--threads=1", "--net-io=tokio"],
        &["--threads=16", "--transport=tcp", "--streams=8", "--net-io=tokio"],
    ];
    for iter in 0..stress_iters() {
        for cfg in configs {
            for mode in [Mode::Push, Mode::Pull] {
                let e = env();
                source_tree(&e.p("src"));
                // An older destination: half the files present with other content.
                fs::create_dir_all(e.p("dst/sub")).unwrap();
                for (rel, data) in files(&e.p("src")).into_iter().step_by(2) {
                    let mut d = data.clone();
                    let mid = d.len() / 2;
                    if let Some(b) = d.get_mut(mid) {
                        *b ^= 1;
                    }
                    d.extend_from_slice(b"tail");
                    fs::write(e.p("dst").join(&rel), d).unwrap();
                    common::set_mtime(&e.p("dst").join(&rel), common::BASE_TIME);
                }
                fs::write(e.p("dst/extra"), "extra").unwrap();
                let mut opts = vec!["-a", "--delete"];
                opts.extend_from_slice(cfg);
                let (code, err) = e.run(&e.args(mode, &opts, "src/", "dst/"), Duration::from_secs(45));
                assert_eq!(code, 0, "iter {iter} {mode:?} {cfg:?}: {err}");
                assert_same_files(&e.p("src"), &e.p("dst"), &format!("iter {iter} {mode:?} {cfg:?}"));
                assert!(e.servers_gone(Duration::from_secs(10)), "iter {iter} {mode:?} {cfg:?}: server left running");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// A source file that shrinks while `--delta=cdc` has it mapped (prior PLAT-01)
// ---------------------------------------------------------------------------

/// Prior PLAT-01 (known, still open): `--delta=cdc` maps the source file; truncating it
/// while it is hashed kills barqsync with SIGBUS (exit 138 here). Timing-dependent: the
/// file is large enough that a debug build is still hashing it 150 ms after the start.
#[test]
#[ignore = "PLAT-01 (known open): a source shrinking under --delta=cdc kills barqsync with SIGBUS"]
fn a_source_shrinking_under_cdc_does_not_crash() {
    let e = env();
    fs::create_dir_all(e.p("src")).unwrap();
    fs::create_dir_all(e.p("dst")).unwrap();
    let data = noise(64 << 20, 9);
    let mut old = data.clone();
    old[1000] ^= 1;
    let mut crashed = 0;
    for round in 0..3 {
        fs::write(e.p("src/big"), &data).unwrap();
        fs::write(e.p("dst/big"), &old).unwrap();
        common::set_mtime(&e.p("dst/big"), common::BASE_TIME);
        let mut child = e.spawn(&e.args(Mode::Local, &["-a", "--no-whole-file", "--delta=cdc"], "src/", "dst/"), "t");
        std::thread::sleep(Duration::from_millis(150));
        fs::OpenOptions::new().write(true).open(e.p("src/big")).unwrap().set_len(4096).unwrap();
        let code = wait_for(&mut child, LIMIT).expect("hung");
        let now = fs::read(e.p("dst/big")).unwrap();
        assert!(now == old || now == data || now == data[..4096], "round {round}: destination is neither old nor new");
        if code == 128 + libc::SIGBUS {
            crashed += 1;
        }
    }
    assert_eq!(crashed, 0, "SIGBUS in {crashed} of 3 rounds");
}

// ---------------------------------------------------------------------------
// The frame decoder against truncated and garbled input
// ---------------------------------------------------------------------------

use barqsync::proto::wire::Wire;
use barqsync::proto::{self, Frame, Msg};

/// Valid encodings of a few messages and data frames, in both wire formats, compressed
/// or not, and cut into several frames (`KIND_PART`) when bigger than the piece size.
fn samples() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for wire in [Wire::Postcard, Wire::Rkyv] {
        for level in [None, Some(3)] {
            let msgs = [
                Msg::KeepAlive,
                Msg::Fatal { msg: "x".repeat(600) },
                Msg::Deleted { rel: b"some/dir/name".to_vec(), dir: true },
                Msg::Log { stderr: true, escape: false, text: noise(5000, 3) },
                Msg::SenderDone { errors: 3, vanished: 1 },
            ];
            for m in msgs {
                out.push(proto::encode(&Frame::Msg(m), level, wire).unwrap());
            }
            out.push(proto::encode(&Frame::Data { id: 7, off: 1 << 40, bytes: vec![0u8; 3000] }, level, wire).unwrap());
            out.push(proto::encode(&Frame::Data { id: 1, off: 0, bytes: Vec::new() }, level, wire).unwrap());
        }
    }
    out
}

/// Read frames until the input ends or an error: never a panic, and it terminates.
fn read_all(bytes: &[u8]) -> (usize, Option<std::io::Error>) {
    let mut cur = Cursor::new(bytes);
    let mut n = 0;
    loop {
        match proto::read_frame(&mut cur) {
            Ok(Some(_)) => n += 1,
            Ok(None) => return (n, None),
            Err(e) => return (n, Some(e)),
        }
        assert!(n < 10_000, "runaway decoding");
    }
}

/// Every proper prefix of a valid frame is an error (or, for the empty prefix, a clean end):
/// a cut connection is never mistaken for a complete message.
#[test]
fn every_truncation_of_a_frame_is_an_error() {
    for s in samples() {
        assert_eq!(read_all(&s).0, 1);
        assert!(read_all(&s).1.is_none());
        for cut in 1..s.len() {
            let (n, err) = read_all(&s[..cut]);
            assert_eq!(n, 0, "a prefix of {cut}/{} bytes decoded as a frame", s.len());
            assert!(err.is_some(), "a prefix of {cut}/{} bytes ended cleanly", s.len());
        }
        assert!(read_all(&[]).1.is_none());
    }
}

/// A message cut into several frames (`KIND_PART`) whose last piece never arrives is an
/// error, not a clean end of stream.
#[test]
fn a_message_missing_its_last_piece_is_an_error() {
    // Two pieces: body of 300 bytes with BARQSYNC_TEST_FRAME_PIECE unset is one frame, so
    // build the pieces by hand: KIND_PART (3) then nothing.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&10u32.to_le_bytes());
    bytes.push(3);
    bytes.extend_from_slice(&[0u8; 10]);
    let (n, err) = read_all(&bytes);
    assert_eq!(n, 0);
    assert_eq!(err.expect("an error").kind(), std::io::ErrorKind::UnexpectedEof);
}

/// A compressed frame that claims more (or less) than it holds, an oversized length and
/// unknown kinds are errors.
#[test]
fn lying_frame_headers_are_errors() {
    let cases: Vec<Vec<u8>> = vec![
        // Body length above MAX_FRAME.
        [((64u32 << 20) + 1).to_le_bytes().as_slice(), &[0]].concat(),
        // Unknown kind.
        [4u32.to_le_bytes().as_slice(), &[9, 0, 0, 0, 0]].concat(),
        // Compressed postcard message, raw length above MAX_FRAME.
        [8u32.to_le_bytes().as_slice(), &[0x80], &((64u32 << 20) + 1).to_le_bytes(), &[0, 0, 0, 0]].concat(),
        // Compressed body shorter than its raw-length field.
        [2u32.to_le_bytes().as_slice(), &[0x80, 1, 2]].concat(),
        // Compressed body that is not zstd.
        [12u32.to_le_bytes().as_slice(), &[0x81], &100u32.to_le_bytes(), &[1, 2, 3, 4, 5, 6, 7, 8]].concat(),
        // Data frame shorter than its id and offset.
        [8u32.to_le_bytes().as_slice(), &[1], &[0u8; 8]].concat(),
        // rkyv message of garbage.
        [16u32.to_le_bytes().as_slice(), &[2], &[0xffu8; 16]].concat(),
    ];
    for (i, c) in cases.iter().enumerate() {
        let (n, err) = read_all(c);
        assert_eq!(n, 0, "case {i}");
        assert!(err.is_some(), "case {i} decoded");
    }
    // A zstd frame that decompresses to more than it claims.
    let raw = noise(100, 1).repeat(50);
    let z = zstd_like(&raw);
    let mut f = Vec::new();
    f.extend_from_slice(&((4 + z.len()) as u32).to_le_bytes());
    f.push(0x81);
    f.extend_from_slice(&10u32.to_le_bytes());
    f.extend_from_slice(&z);
    assert!(read_all(&f).1.is_some(), "a frame decompressing past its claimed size was accepted");
}

/// A real zstd stream of `raw`, taken from an encoded data frame (the codec compresses
/// bodies of 256 bytes or more when that helps).
fn zstd_like(raw: &[u8]) -> Vec<u8> {
    let enc = proto::encode(&Frame::Data { id: 0, off: 0, bytes: raw.to_vec() }, Some(3), Wire::Postcard).unwrap();
    assert_eq!(enc[4], 0x81, "expected a compressed data frame");
    enc[9..].to_vec()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// Arbitrary bytes never panic the decoder.
    #[test]
    fn arbitrary_bytes_never_panic_the_decoder(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        let _ = read_all(&bytes);
    }

    /// Valid frames with bytes flipped: an error or some frame, never a panic.
    #[test]
    fn corrupted_frames_never_panic_the_decoder(which in any::<prop::sample::Index>(), flips in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..8)) {
        let samples = samples();
        let mut s = samples[which.index(samples.len())].clone();
        for (at, x) in flips {
            let i = at.index(s.len());
            // Keep the length field plausible so the body is actually parsed.
            if i >= 4 {
                s[i] ^= x | 1;
            }
        }
        let _ = read_all(&s);
    }
}

// ---------------------------------------------------------------------------
// Fault injection without privileges (A4b wave): resource limits on the receiving side,
// a stopped or killed peer or transport, a transport that delivers tiny pieces.
// Only test processes are touched: no mount, no root, no change to the machine.
// ---------------------------------------------------------------------------

/// A `/bin/sh` script in the root that runs `prelude` and then `exec`s `prog`.
fn wrapper(e: &Env, name: &str, prelude: &str, prog: &Path) -> PathBuf {
    let path = e.p(name);
    fs::write(&path, format!("#!/bin/sh\n{prelude}\nexec '{}' \"$@\"\n", prog.display())).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// `args` with `--remote-path=` pointing at `prog`.
fn with_remote_path(mut args: Vec<String>, prog: &Path) -> Vec<String> {
    for a in &mut args {
        if a.starts_with("--remote-path=") {
            *a = format!("--remote-path={}", prog.display());
        }
    }
    args
}

/// `args` with the remote shell (`-e`) replaced by `rsh`.
fn with_transport(mut args: Vec<String>, rsh: &Path) -> Vec<String> {
    let i = args.iter().position(|a| a == "-e").expect("a remote mode");
    args[i + 1] = rsh.display().to_string();
    args
}

/// Files below `dir` that are not temp files.
fn final_files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut m = files(dir);
    m.retain(|k, _| !is_temp(k.file_name().unwrap().as_bytes()));
    m
}

fn python_available() -> bool {
    Command::new("python3").arg("--version").output().is_ok_and(|o| o.status.success())
}

/// A stand-in for ssh that can be stopped or killed on its own (the server runs as its
/// child, as the remote program does behind a real ssh) and that passes the bytes on in
/// pieces of at most `RELAY_CHUNK` (default 64 KiB), so every read and write is short.
const RELAY_PY: &str = r#"#!/usr/bin/env python3
import os, subprocess, sys, threading
chunk = int(os.environ.get("RELAY_CHUNK", "65536"))
p = subprocess.Popen(["/bin/sh", "-c", " ".join(sys.argv[2:])], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
def pump(r, w, close):
    try:
        while True:
            b = os.read(r, chunk)
            if not b:
                break
            v = memoryview(b)
            while v:
                v = v[os.write(w, v):]
    except OSError:
        pass
    finally:
        if close:
            try:
                os.close(w)
            except OSError:
                pass
t = threading.Thread(target=pump, args=(0, p.stdin.fileno(), True))
t.start()
pump(p.stdout.fileno(), 1, False)
p.wait()
sys.exit(p.returncode)
"#;

impl Env {
    /// Run `prog` (the binary itself or a wrapper script around it) with `envs` added.
    fn run_prog(&self, prog: &Path, args: &[String], envs: &[(&str, &str)], limit: Duration) -> (i32, String) {
        let tag = format!("run{}", SEQ.fetch_add(1, Ordering::SeqCst));
        let out = fs::File::create(self.p(&format!("{tag}.out"))).unwrap();
        let err = fs::File::create(self.p(&format!("{tag}.err"))).unwrap();
        let mut c = Command::new(prog);
        c.current_dir(&self.root)
            .env("LC_ALL", "C.UTF-8")
            .env("BARQSYNC_CACHE_DIR", &self.cache)
            .envs(envs.iter().copied())
            .args(args)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .process_group(0);
        let mut child = c.spawn().unwrap();
        let code = wait_for(&mut child, limit).unwrap_or_else(|| panic!("hung for {limit:?}: {args:?}\n{}", self.stderr(&tag)));
        (code, self.stderr(&tag))
    }

    fn relay(&self) -> PathBuf {
        let p = self.p("relay.py");
        fs::write(&p, RELAY_PY).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// The relay processes of this test (not the client, whose `-e` names the same path).
    fn relays(&self, relay: &Path) -> Vec<i32> {
        pids(&relay.display().to_string(), "--server", "--remote-path=")
    }
}

/// `opts SRC DST` in `mode`, with `prelude` (shell commands such as `ulimit -f 2048`)
/// applied to the side that receives: the client (local, pull) or the server (push).
fn run_limited(e: &Env, mode: Mode, prelude: &str, opts: &[&str]) -> (i32, String) {
    let args = e.args(mode, opts, "src/", "dst/");
    if mode == Mode::Push {
        let w = wrapper(e, "limited-server", prelude, &e.server);
        e.run_prog(Path::new(BIN), &with_remote_path(args, &w), &[], LIMIT)
    } else {
        let w = wrapper(e, "limited-client", prelude, Path::new(BIN));
        e.run_prog(&w, &args, &[], LIMIT)
    }
}

/// The source of `source_tree` (three 3 MiB files) and a destination holding an old
/// `big0` of the same size with other bytes.
fn old_big0_setup(e: &Env) -> Vec<u8> {
    source_tree(&e.p("src"));
    let old = vec![b'O'; 3 << 20];
    fs::create_dir_all(e.p("dst")).unwrap();
    fs::write(e.p("dst/big0"), &old).unwrap();
    common::set_mtime(&e.p("dst/big0"), common::BASE_TIME);
    old
}

/// After a failed run `big0` is still the old file and every other final-named file is the
/// source's; returns the temp files found.
fn check_old_kept(e: &Env, old: &[u8], what: &str) -> Vec<PathBuf> {
    let want = files(&e.p("src"));
    let mut temps = Vec::new();
    for (rel, data) in files(&e.p("dst")) {
        if is_temp(rel.file_name().unwrap().as_bytes()) {
            temps.push(rel);
        } else if rel == Path::new("big0") {
            assert!(data == old, "{what}: the old big0 was changed");
        } else {
            assert!(want.get(&rel).is_some_and(|w| *w == data), "{what}: {} is torn or unexpected", rel.display());
        }
    }
    temps
}

/// RLIMIT_FSIZE of 1 MiB on the receiving side: the write of a 3 MiB file fails halfway.
fn write_limit_case(ignore_sigxfsz: bool) {
    for mode in MODES {
        let e = env();
        let old = old_big0_setup(&e);
        // `ulimit -f` counts 512-byte blocks in a POSIX sh: 2048 are 1 MiB.
        let prelude = if ignore_sigxfsz { "ulimit -f 2048; trap '' 25" } else { "ulimit -f 2048" };
        let (code, err) = run_limited(&e, mode, prelude, &["-a"]);
        let what = format!("{mode:?} (SIGXFSZ ignored: {ignore_sigxfsz})");
        if code == 0 {
            // A copy by cloning (APFS) writes nothing, so the limit cannot bite there.
            assert!(cfg!(target_os = "macos") && mode == Mode::Local, "{what}: expected a failure: {err}");
            continue;
        }
        let temps = check_old_kept(&e, &old, &what);
        assert_eq!(code, 23, "{what}: {err}");
        assert!(err.contains("File too large"), "{what}: {err}");
        assert!(temps.is_empty(), "{what}: temp files left: {temps:?}");
        let (code, err) = e.run(&e.args(mode, &["-a"], "src/", "dst/"), LIMIT);
        assert_eq!(code, 0, "{what}: rerun: {err}");
        assert_same_files(&e.p("src"), &e.p("dst"), &what);
    }
}

/// With SIGXFSZ ignored, as rsync does, the failing write is an ordinary EFBIG: exit 23,
/// the old file kept, no temp file, and a rerun without the limit converges.
#[test]
fn a_write_failing_with_efbig_keeps_the_old_file_and_leaves_no_temp_files() {
    write_limit_case(true);
}

/// The same under the default disposition of SIGXFSZ (what `ulimit -f` in a shell gives):
/// barqsync is killed by the signal and leaves temp files; rsync 3.2.7 ignores the signal
/// and exits 11 with no temp file.
#[test]
fn a_file_size_limit_ends_the_run_cleanly() {
    write_limit_case(false);
}

/// A descriptor limit far below the usual: no hang, no panic, no signal, no torn file and
/// no temp file; either everything arrives or the run says "Too many open files" (exit 23).
#[test]
fn a_tiny_descriptor_limit_gives_errors_not_a_hang_a_crash_or_torn_files() {
    for mode in MODES {
        for n in [16, 24, 40] {
            let e = env();
            for d in 0..20 {
                fs::create_dir_all(e.p(&format!("src/d{d:02}"))).unwrap();
            }
            for i in 0..300u64 {
                fs::write(e.p(&format!("src/d{:02}/f{i:03}", i % 20)), noise(100 + (i as usize % 50), i + 1)).unwrap();
            }
            let what = format!("{mode:?} nofile={n}");
            let (code, err) = run_limited(&e, mode, &format!("ulimit -n {n}"), &["-a"]);
            assert!(code == 0 || code == 23, "{what}: exit {code}: {err}");
            assert!(!err.contains("panicked"), "{what}: {err}");
            if code == 23 {
                assert!(err.contains("Too many open files"), "{what}: {err}");
            }
            let temps = check_complete_or_absent(&e.p("src"), &e.p("dst"));
            assert!(temps.is_empty(), "{what}: temp files left: {temps:?}");
            let (code, err) = e.run(&e.args(mode, &["-a"], "src/", "dst/"), LIMIT);
            assert_eq!(code, 0, "{what}: rerun: {err}");
            assert_same_files(&e.p("src"), &e.p("dst"), &what);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Fault {
    /// SIGSTOP of the remote-side barqsync (a hung peer).
    StopServer,
    /// SIGSTOP of the transport process (a silent network).
    StopTransport,
    /// SIGKILL of the transport process (a dying ssh).
    KillTransport,
}

/// A transfer slowed to ~13 s by `--bwlimit`, with `--timeout=5`: once it has started,
/// `fault` hits the peer. Stops must end with exit 30 in bounded time, a kill with an
/// error; nothing is torn, and a rerun converges.
///
/// `transport`: the one asked for, or `None` for the default (`BARQSYNC_TRANSPORT`).
fn peer_fault(mode: Mode, fault: Fault, transport: Option<&str>) {
    // `--timeout=5` can expire during the start-up of a machine under heavy load (seen on a
    // macOS host at load average 13: exit 30 before any file existed). That is the machine,
    // not the product: start again, up to three times.
    for attempt in 1..=3 {
        if peer_fault_once(mode, fault, transport, attempt == 3) {
            return;
        }
        eprintln!("{mode:?} {fault:?}: the start-up outlasted the timeout (attempt {attempt}); trying again");
    }
}

fn peer_fault_once(mode: Mode, fault: Fault, transport: Option<&str>, last: bool) -> bool {
    let e = env();
    source_tree(&e.p("src"));
    let relay = e.relay();
    // A killed transport needs no timeout: the closed pipe ends the client by itself.
    let mut opts: Vec<String> = ["-a", "--bwlimit=1000"].map(String::from).to_vec();
    if fault != Fault::KillTransport {
        opts.push("--timeout=5".into());
    }
    opts.extend(transport.map(|t| format!("--transport={t}")));
    let opts: Vec<&str> = opts.iter().map(String::as_str).collect();
    let mut args = e.args(mode, &opts, "src/", "dst/");
    if fault != Fault::StopServer {
        args = with_transport(args, &relay);
    }
    let what = format!("{mode:?} {fault:?} {transport:?}");
    let mut child = e.spawn(&args, "fault");
    let start = Instant::now();
    while files(&e.p("dst")).is_empty() {
        if let Some(s) = child.try_wait().unwrap() {
            assert!(!last, "{what}: the transfer ended before writing anything: {s}: {}", e.stderr("fault"));
            return false;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "{what}: the transfer never started");
        std::thread::sleep(Duration::from_millis(5));
    }
    let victims = if fault == Fault::StopServer { e.servers() } else { e.relays(&relay) };
    assert!(!victims.is_empty(), "{what}: no process to hit");
    let sig = if fault == Fault::KillTransport { libc::SIGKILL } else { libc::SIGSTOP };
    for &p in &victims {
        unsafe { libc::kill(p, sig) };
    }
    let hit = Instant::now();
    let code = wait_for(&mut child, Duration::from_secs(40));
    let took = hit.elapsed();
    let err = e.stderr("fault");
    if fault == Fault::KillTransport {
        // The server sees the end of its input and finishes by itself, cleaning up.
        assert!(e.servers_gone(Duration::from_secs(15)), "{what}: the server outlived its transport");
    }
    for p in e.servers().into_iter().chain(e.relays(&relay)) {
        unsafe { libc::kill(p, libc::SIGKILL) };
    }
    let code = code.unwrap_or_else(|| panic!("{what}: no exit within 40 s of the fault: {err}"));
    if fault == Fault::KillTransport {
        assert!(code != 0 && code < 128, "{what}: exit {code}: {err}");
    } else {
        assert_eq!(code, 30, "{what}: exit {code} after {took:?}: {err}");
    }
    assert!(took < Duration::from_secs(25), "{what}: took {took:?}");
    let temps = check_complete_or_absent(&e.p("src"), &e.p("dst"));
    // A stopped receiver could not clean its temp files; every other case leaves none.
    if fault == Fault::KillTransport || mode == Mode::Pull && fault == Fault::StopServer {
        assert!(temps.is_empty(), "{what}: temp files left: {temps:?}");
    }
    let (code, err) = e.run(&e.args(mode, &["-a"], "src/", "dst/"), LIMIT);
    assert_eq!(code, 0, "{what}: rerun: {err}");
    assert_eq!(final_files(&e.p("src")), final_files(&e.p("dst")), "{what}: the rerun did not converge");
    true
}

#[test]
fn a_stopped_server_ends_the_client_with_the_timeout_exit() {
    for mode in [Mode::Push, Mode::Pull] {
        peer_fault(mode, Fault::StopServer, None);
    }
}

#[test]
fn a_stopped_transport_ends_the_client_with_the_timeout_exit() {
    if !python_available() {
        eprintln!("SKIP: no python3");
        return;
    }
    // The stopped process is the remote shell's pipe, which is the transport with ssh.
    for mode in [Mode::Push, Mode::Pull] {
        peer_fault(mode, Fault::StopTransport, Some("ssh"));
    }
}

/// The same with QUIC carrying the data: the remote shell's pipe stopped, `--timeout=5`
/// must still end the client with exit 30. Found on 2026-11-07, when these tests first
/// ran in CI's QUIC pass; 475db4c fails it the same way.
#[test]
#[ignore = "open: with --transport=quic, a stopped remote shell outlasts --timeout=5 (no exit within 40 s); with ssh the client exits 30"]
fn a_stopped_remote_shell_ends_a_quic_client_with_the_timeout_exit() {
    if !python_available() {
        eprintln!("SKIP: no python3");
        return;
    }
    for mode in [Mode::Push, Mode::Pull] {
        peer_fault(mode, Fault::StopTransport, Some("quic"));
    }
}

#[test]
fn a_killed_transport_ends_both_sides_without_torn_files() {
    if !python_available() {
        eprintln!("SKIP: no python3");
        return;
    }
    for mode in [Mode::Push, Mode::Pull] {
        peer_fault(mode, Fault::KillTransport, None);
    }
}

fn tiny_tree(dir: &Path) {
    fs::create_dir_all(dir.join("sub")).unwrap();
    for (i, len) in [0usize, 1, 100, 3000, 700].into_iter().enumerate() {
        fs::write(dir.join(format!("f{i}")), noise(len, i as u64 + 1)).unwrap();
    }
    fs::write(dir.join("sub/big"), noise(40_000, 9)).unwrap();
    fs::write(dir.join("sub/small"), b"hello").unwrap();
}

/// A full sync, then a changed tree (a rewritten middle of a 40 KB file, a new file, a
/// removed file) with `--delete`, both over a transport whose reads and writes are short.
fn tiny_pieces_case(e: &Env, mode: Mode, what: &str, transport: Option<&Path>, envs: &[(&str, &str)]) {
    let _ = fs::remove_dir_all(e.p("src"));
    let _ = fs::remove_dir_all(e.p("dst"));
    tiny_tree(&e.p("src"));
    let mut args = e.args(mode, &["-a", "--delete"], "src/", "dst/");
    if let Some(t) = transport {
        args = with_transport(args, t);
    }
    let (code, err) = e.run_prog(Path::new(BIN), &args, envs, LIMIT);
    assert_eq!(code, 0, "{what}: first run: {err}");
    assert_same_files(&e.p("src"), &e.p("dst"), what);
    let mut big = noise(40_000, 9);
    big[15_000..16_000].copy_from_slice(&noise(1000, 77));
    fs::write(e.p("src/sub/big"), &big).unwrap();
    common::set_mtime(&e.p("src/sub/big"), common::BASE_TIME + 100);
    fs::write(e.p("src/new"), b"new file").unwrap();
    fs::remove_file(e.p("src/f3")).unwrap();
    let (code, err) = e.run_prog(Path::new(BIN), &args, envs, LIMIT);
    assert_eq!(code, 0, "{what}: second run: {err}");
    assert_same_files(&e.p("src"), &e.p("dst"), what);
}

/// The short-read/short-write seam: the protocol's own `BARQSYNC_TEST_FRAME_PIECE` (frames
/// of 1 and 3 bytes) and a relay that passes the pipes on 1, 7 and 509 bytes at a time.
#[test]
fn a_transport_delivering_tiny_pieces_neither_corrupts_nor_hangs() {
    let e = env();
    for mode in [Mode::Push, Mode::Pull] {
        for piece in ["1", "3"] {
            tiny_pieces_case(&e, mode, &format!("{mode:?} frame piece {piece}"), None, &[("BARQSYNC_TEST_FRAME_PIECE", piece)]);
        }
    }
    if !python_available() {
        eprintln!("SKIP (relay part): no python3");
        return;
    }
    let relay = e.relay();
    for mode in [Mode::Push, Mode::Pull] {
        for chunk in ["1", "7", "509"] {
            tiny_pieces_case(&e, mode, &format!("{mode:?} relay chunk {chunk}"), Some(&relay), &[("RELAY_CHUNK", chunk)]);
        }
    }
}
