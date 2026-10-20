//! Disposable CLI probes with a deadline that also terminates owned descendants.
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn run(dir: &Path, args: &[&str], backend: &str) -> (i32, String) {
    let log = tempfile::tempfile().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_barqsync"));
    cmd.current_dir(dir)
        .env("BARQSYNC_NET_IO", backend)
        .arg("--transport=ssh")
        .args(args)
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log.try_clone().unwrap()))
        .process_group(0);
    if !args.iter().any(|a| a.starts_with("--threads=")) {
        cmd.arg("--threads=2");
    }
    let mut child = cmd.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            // Also stop the owned fake-SSH/server descendants on a regression.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            panic!("sync exceeded 20 seconds: {args:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    use std::io::{Seek, SeekFrom};
    let mut log = log;
    log.seek(SeekFrom::Start(0)).unwrap();
    let mut text = String::new();
    log.read_to_string(&mut text).unwrap();
    (status.code().unwrap_or(-1), text)
}

pub fn fake_ssh(dir: &Path) {
    let path = dir.join("fake-ssh");
    fs::write(&path, "#!/bin/sh\nshift\nexec /bin/sh -c \"$*\"\n").unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
