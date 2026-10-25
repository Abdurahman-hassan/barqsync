//! SIGTERM and SIGHUP end a local transfer the way Ctrl-C (SIGINT) does: the in-flight
//! file is removed, nothing half-written is left, and the exit code is 20, as rsync does.
//! One ignored when barqsync starts (`nohup`) stays ignored. (ssh and remote shutdown
//! signals are covered in `remote_shutdown.rs`.)

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_barqsync");

/// Whether `dir` holds a leftover temp file (`.name.XXXXXX`).
fn temp_files(dir: &Path) -> Vec<std::ffi::OsString> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name())
        .filter(|n| n.as_encoded_bytes().starts_with(b"."))
        .collect()
}

/// Start copying a large (sparse) file into `dst`, with SIGTERM and SIGHUP at their
/// default action in the child, or SIGHUP ignored (`ignore_hup`, as under `nohup`) --
/// whatever the test runner inherited -- and return once the copy is under way.
fn start_copy(dir: &Path, dst: &Path, ignore_hup: bool) -> Child {
    let src = dir.join("src");
    fs::create_dir(&src).unwrap();
    fs::create_dir(dst).unwrap();
    // Sparse and far too big to copy before it is signalled, but it takes no disk space
    // and the read loop checks for the interrupt every chunk. --no-clone forces that loop
    // instead of an instant reflink.
    fs::File::create(src.join("big")).unwrap().set_len(16 << 30).unwrap();

    let mut cmd = Command::new(BIN);
    cmd.current_dir(dir)
        .args(["-a", "--no-clone", "src/", "dst/"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0); // our signal reaches only this child, not the test runner
    unsafe {
        cmd.pre_exec(move || {
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            libc::signal(libc::SIGHUP, if ignore_hup { libc::SIG_IGN } else { libc::SIG_DFL });
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();

    // Wait for the destination temp file to appear: the copy is running.
    let start = Instant::now();
    while temp_files(dst).is_empty() {
        if let Some(s) = child.try_wait().unwrap() {
            panic!("exited before copying started: {s}");
        }
        assert!(start.elapsed() < Duration::from_secs(30), "copy never started");
        std::thread::sleep(Duration::from_millis(5));
    }
    child
}

/// Send `sig` once the copy is under way, and check the client ends with code 20 and
/// leaves no half-written temp file behind.
fn interrupted_by(sig: i32) {
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("dst");
    let mut child = start_copy(tmp.path(), &dst, false);
    let pid = child.id() as i32;

    unsafe { libc::kill(pid, sig) };

    let signalled = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if signalled.elapsed() > Duration::from_secs(10) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            let _ = child.wait();
            panic!("sig {sig}: still running 10 s after the signal");
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    assert_eq!(status.code(), Some(20), "sig {sig}: exit code");
    assert!(temp_files(&dst).is_empty(), "sig {sig}: left a temp file: {:?}", temp_files(&dst));
    assert!(!dst.join("big").exists(), "sig {sig}: left a partial destination file");
}

#[test]
fn sigterm_ends_the_transfer_cleanly() {
    interrupted_by(libc::SIGTERM);
}

#[test]
fn sighup_ends_the_transfer_cleanly() {
    interrupted_by(libc::SIGHUP);
}

#[test]
fn an_ignored_sighup_stays_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("dst");
    let mut child = start_copy(tmp.path(), &dst, true);
    let pid = child.id() as i32;

    // Like a terminal closing under `nohup barqsync ...`: the copy carries on.
    unsafe { libc::kill(pid, libc::SIGHUP) };
    std::thread::sleep(Duration::from_millis(500));
    let still = child.try_wait().unwrap();
    if still.is_none() {
        unsafe { libc::kill(pid, libc::SIGINT) };
    }
    let status = child.wait().unwrap();
    assert!(still.is_none(), "an ignored SIGHUP ended the transfer: {status}");
    // Ctrl-C still works, and still cleans up.
    assert_eq!(status.code(), Some(20), "exit code after SIGINT");
    assert!(temp_files(&dst).is_empty(), "left a temp file: {:?}", temp_files(&dst));
}
