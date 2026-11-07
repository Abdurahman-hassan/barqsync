//! Audit 2026-10-30 (A2): synchronization correctness and data integrity, exercised through the binary.
//!
//! Self-contained (std + tempfile). Content is compared byte for byte, never with `barqsync -c`.
//! `RSYNC=/path/to/rsync` (3.x) enables the differential assertions.
//!
//! Tests marked `#[ignore = "AU-COR-NN: ..."]` assert the CORRECT behaviour and fail on 475db4c; run them with
//! `cargo test --locked --test audit_correctness -- --ignored`.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime};

const BIN: &str = env!("CARGO_BIN_EXE_barqsync");

fn run(args: &[&str], cwd: &Path) -> Output {
    Command::new(BIN).args(args).current_dir(cwd).output().expect("run barqsync")
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn rsync_bin() -> Option<PathBuf> {
    std::env::var_os("RSYNC").map(PathBuf::from).filter(|p| p.exists())
}

fn run_rsync(args: &[&str], cwd: &Path) -> Option<Output> {
    rsync_bin().map(|r| Command::new(r).args(args).current_dir(cwd).output().expect("run rsync"))
}

fn data(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn set_mtime(p: &Path, secs: u64) {
    let f = fs::OpenOptions::new().write(true).open(p).unwrap();
    f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
}

fn put_at(p: &Path, content: &[u8], mtime: u64) {
    if let Some(d) = p.parent() {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(p, content).unwrap();
    set_mtime(p, mtime);
}

fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                walk(base, &p, out);
            } else if ft.is_file() {
                out.insert(p.strip_prefix(base).unwrap().to_string_lossy().into_owned(), fs::read(&p).unwrap_or_default());
            }
        }
    }
    let mut m = BTreeMap::new();
    walk(root, root, &mut m);
    m
}

/// Itemized lines of a `-i` run, without directory-only lines.
fn items(o: &Output) -> Vec<String> {
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(".d"))
        .map(String::from)
        .collect()
}

fn case_insensitive(dir: &Path) -> bool {
    fs::write(dir.join("Probe"), b"x").unwrap();
    let ci = dir.join("probe").exists();
    let _ = fs::remove_file(dir.join("Probe"));
    ci
}

// ---------------------------------------------------------------------------------------------
// Regression and differential tests (pass on 475db4c)
// ---------------------------------------------------------------------------------------------

#[test]
fn quick_check_trusts_equal_size_and_mtime_exactly_like_rsync() {
    let t = tempfile::tempdir().unwrap();
    let w = t.path();
    let (new, old) = (data(1, 50_000), data(2, 50_000));
    put_at(&w.join("src/f"), &new, 1_600_000_000);
    put_at(&w.join("dst/f"), &old, 1_600_000_000);
    put_at(&w.join("dst_r/f"), &old, 1_600_000_000);
    let o = run(&["-a", "src/", "dst/"], w);
    assert_eq!(code(&o), 0, "{}", text(&o));
    assert_eq!(fs::read(w.join("dst/f")).unwrap(), old, "equal size and mtime: the blind spot is skipped, as in rsync");
    if let Some(r) = run_rsync(&["-a", "src/", "dst_r/"], w) {
        assert_eq!(code(&r), 0);
        assert_eq!(fs::read(w.join("dst_r/f")).unwrap(), old, "rsync agrees");
    }
    let o = run(&["-ac", "src/", "dst/"], w);
    assert_eq!(code(&o), 0, "{}", text(&o));
    assert_eq!(fs::read(w.join("dst/f")).unwrap(), new, "-c repairs it");
    // --modify-window=2: a destination one second newer counts as equal
    put_at(&w.join("dst/g"), &old, 1_600_000_001);
    put_at(&w.join("src/g"), &new, 1_600_000_000);
    let o = run(&["-a", "--modify-window=2", "src/", "dst/"], w);
    assert_eq!(code(&o), 0, "{}", text(&o));
    assert_eq!(fs::read(w.join("dst/g")).unwrap(), old);
}

#[test]
fn delete_is_withheld_when_a_source_subdirectory_cannot_be_read() {
    let t = tempfile::tempdir().unwrap();
    let w = t.path();
    put_at(&w.join("src/top.txt"), b"top", 1_600_000_000);
    put_at(&w.join("src/b/b1.txt"), b"b1", 1_600_000_000);
    put_at(&w.join("dst/top.txt"), b"top", 1_600_000_000);
    put_at(&w.join("dst/extra_top.txt"), b"X", 1_600_000_000);
    put_at(&w.join("dst/b/b1.txt"), b"b1", 1_600_000_000);
    put_at(&w.join("dst/b/extra_b.txt"), b"X", 1_600_000_000);
    fs::set_permissions(w.join("src/b"), fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(w.join("src/b")).is_ok() {
        fs::set_permissions(w.join("src/b"), fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("permission checks are bypassed (root): skipped");
        return;
    }
    let o = run(&["-a", "--delete", "src/", "dst/"], w);
    // the source directory's mode (0) may have been applied to dst/b: make both searchable before looking inside
    for d in ["src/b", "dst/b"] {
        let _ = fs::set_permissions(w.join(d), fs::Permissions::from_mode(0o755));
    }
    assert_eq!(code(&o), 23, "{}", text(&o));
    for kept in ["dst/extra_top.txt", "dst/b/extra_b.txt", "dst/b/b1.txt", "dst/top.txt"] {
        assert!(w.join(kept).exists(), "{kept} must survive a failed scan: {}", text(&o));
    }
}

#[test]
fn writes_and_deletions_never_follow_destination_symlinks_out_of_the_tree() {
    let sets: [&[&str]; 4] = [&["-a"], &["-a", "--delete"], &["-a", "--inplace"], &["-a", "--no-whole-file"]];
    for flags in sets {
        let t = tempfile::tempdir().unwrap();
        let w = t.path();
        put_at(&w.join("src/keep.txt"), b"k", 1_600_000_000);
        put_at(&w.join("src/sub/new.txt"), b"new", 1_600_000_000);
        put_at(&w.join("src/f"), &data(3, 200_000), 1_700_000_000);
        put_at(&w.join("outside/dirtarget/victim.txt"), b"victim", 1_600_000_000);
        put_at(&w.join("outside/target.txt"), &data(4, 200_000), 1_600_000_000);
        fs::create_dir_all(w.join("dst")).unwrap();
        symlink("../outside/dirtarget", w.join("dst/sub")).unwrap();
        symlink("../outside/target.txt", w.join("dst/f")).unwrap();
        let before = tree(&w.join("outside"));
        let mut args = flags.to_vec();
        args.extend(["src/", "dst/"]);
        let o = run(&args, w);
        assert_eq!(code(&o), 0, "{flags:?}: {}", text(&o));
        assert_eq!(tree(&w.join("outside")), before, "{flags:?}: something outside dst changed");
        assert_eq!(fs::read(w.join("dst/f")).unwrap(), data(3, 200_000), "{flags:?}");
    }
}

#[test]
fn updating_one_name_of_a_hard_linked_destination_leaves_the_other_name_alone() {
    let sets: [&[&str]; 2] = [&[], &["--no-whole-file"]];
    for flags in sets {
        let t = tempfile::tempdir().unwrap();
        let w = t.path();
        let (old, new) = (data(5, 300_000), data(6, 300_000));
        put_at(&w.join("src/a"), &new, 1_600_000_050);
        put_at(&w.join("dst/a"), &old, 1_600_000_000);
        fs::hard_link(w.join("dst/a"), w.join("dst/b")).unwrap();
        let mut args = flags.to_vec();
        args.extend(["-a", "src/", "dst/"]);
        let o = run(&args, w);
        assert_eq!(code(&o), 0, "{}", text(&o));
        assert_eq!(fs::read(w.join("dst/a")).unwrap(), new, "{flags:?}");
        assert_eq!(fs::read(w.join("dst/b")).unwrap(), old, "{flags:?}: the other hard link must keep the old content");
    }
}

#[test]
fn max_delete_stops_at_the_limit_and_reports_25() {
    let t = tempfile::tempdir().unwrap();
    let w = t.path();
    put_at(&w.join("src/keep"), b"k", 1_600_000_000);
    for n in ["keep", "e1", "e2", "e3"] {
        put_at(&w.join("dst").join(n), b"k", 1_600_000_000);
    }
    let o = run(&["-a", "--delete", "--max-delete=1", "src/", "dst/"], w);
    assert_eq!(code(&o), 25, "{}", text(&o));
    let left = ["e1", "e2", "e3"].iter().filter(|n| w.join("dst").join(n).exists()).count();
    assert_eq!(left, 2);
    assert!(w.join("dst/keep").exists());
}

#[test]
fn delta_output_is_exact_around_block_refine_and_whole_send_boundaries() {
    let sizes = [1usize, 4095, 4096, 4097, 65_535, 65_536, 65_537, 131_071, 131_072, 131_073, 262_145];
    let mut pairs: Vec<(String, Vec<u8>, Vec<u8>)> = Vec::new();
    for (i, &n) in sizes.iter().enumerate() {
        let old = data(100 + i as u64, n);
        let mut flip = old.clone();
        flip[n / 2] ^= 0x5A;
        let mut app = old.clone();
        app.extend(data(200 + i as u64, 4097));
        let trunc = old[..n / 2].to_vec();
        let mut ins = data(300 + i as u64, 7);
        ins.extend(&old);
        pairs.push((format!("s{n}_flip"), old.clone(), flip));
        pairs.push((format!("s{n}_append"), old.clone(), app));
        pairs.push((format!("s{n}_trunc"), old.clone(), trunc));
        pairs.push((format!("s{n}_insert"), old, ins));
    }
    let modes: [&[&str]; 3] = [&["--no-whole-file"], &["--no-whole-file", "--inplace"], &["--no-whole-file", "--delta=cdc"]];
    for mode in modes {
        let t = tempfile::tempdir().unwrap();
        let w = t.path();
        for (name, old, new) in &pairs {
            put_at(&w.join("dst").join(name), old, 1_600_000_000);
            put_at(&w.join("src").join(name), new, 1_700_000_000);
        }
        let mut args = vec!["-a"];
        args.extend(mode);
        args.extend(["src/", "dst/"]);
        let o = run(&args, w);
        assert_eq!(code(&o), 0, "{mode:?}: {}", text(&o));
        for (name, _, new) in &pairs {
            assert_eq!(&fs::read(w.join("dst").join(name)).unwrap(), new, "{mode:?}: {name} differs after the transfer");
        }
        let o = run(&["-a", "-i", "src/", "dst/"], w);
        assert_eq!(items(&o), Vec::<String>::new(), "{mode:?}: the rerun must find nothing to do");
    }
}

#[test]
fn kill_9_mid_transfer_leaves_only_complete_or_absent_final_files_and_a_rerun_converges() {
    let t = tempfile::tempdir().unwrap();
    let w = t.path();
    for i in 0..6u64 {
        put_at(&w.join(format!("src/f{i}")), &data(40 + i, 2 * 1024 * 1024 + (i as usize) * 4097), 1_600_000_000 + i);
    }
    for i in 0..60u64 {
        put_at(&w.join(format!("src/sub/s{i:02}")), &data(80 + i, 1000 + i as usize), 1_600_000_000);
    }
    fs::create_dir_all(w.join("dst")).unwrap();
    let mut child = Command::new(BIN)
        .args(["-a", "--bwlimit=3m", "src/", "dst/"])
        .current_dir(w)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    let _ = child.kill(); // SIGKILL
    let _ = child.wait();
    let src = tree(&w.join("src"));
    for (name, content) in tree(&w.join("dst")) {
        if name.split('/').any(|c| c.starts_with('.')) {
            continue; // a temp file of the killed run (".name.XXXXXX")
        }
        assert_eq!(src.get(&name), Some(&content), "{name}: a final-named file is torn after kill -9");
    }
    let o = run(&["-a", "src/", "dst/"], w);
    assert_eq!(code(&o), 0, "{}", text(&o));
    let after: BTreeMap<_, _> = tree(&w.join("dst")).into_iter().filter(|(n, _)| !n.split('/').any(|c| c.starts_with('.'))).collect();
    assert_eq!(after, src, "a rerun must converge");
    let o = run(&["-a", "-i", "src/", "dst/"], w);
    assert_eq!(items(&o), Vec::<String>::new());
}

// ---------------------------------------------------------------------------------------------
// Open defects: these assert the correct behaviour and fail on 475db4c
// ---------------------------------------------------------------------------------------------

const RELAY_PY: &str = r##"import os, subprocess, sys, threading, time
poke, thr = os.environ["POKE_FILE"], int(os.environ.get("POKE_MIN", "4096"))
p = subprocess.Popen(sys.argv[1:], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
def up():
    try:
        while True:
            b = os.read(0, 65536)
            if not b: break
            p.stdin.write(b); p.stdin.flush()
    except Exception: pass
    try: p.stdin.close()
    except Exception: pass
threading.Thread(target=up, daemon=True).start()
done = False
while True:
    b = os.read(p.stdout.fileno(), 65536)
    if not b: break
    if len(b) >= thr and not done:
        done = True
        fd = os.open(poke, os.O_WRONLY); os.pwrite(fd, b"POKEPOKEPOKEPOKE", 0); os.close(fd)
        time.sleep(0.5)
    v = memoryview(b)
    while v:
        n = os.write(1, v); v = v[n:]
p.wait(); sys.exit(p.returncode)
"##;

#[test]
fn au_cor_01_basis_changed_after_signature_must_not_commit_a_wrong_file() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("python3 missing: skipped");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let w = t.path().canonicalize().unwrap();
    fs::write(w.join("relay.py"), RELAY_PY).unwrap();
    let ssh = w.join("relay-ssh");
    fs::write(&ssh, format!("#!/bin/sh\nshift\nexec python3 -u {}/relay.py /bin/sh -c \"$*\"\n", w.display())).unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let old = data(7, 32 << 20);
    let mut new = old.clone();
    *new.last_mut().unwrap() ^= 0xFF;
    put_at(&w.join("dst/f"), &old, 1_600_000_000);
    put_at(&w.join("src/f"), &new, 1_700_000_000);
    let rp = format!("--remote-path={}", BIN);
    let o = Command::new(BIN)
        .args(["-e", ssh.to_str().unwrap(), rp.as_str(), "-a", "src/", "fakehost:dst/"])
        .current_dir(&w)
        .env("POKE_FILE", w.join("dst/f"))
        .env("POKE_MIN", "4096")
        .env("BARQSYNC_CACHE_DIR", w.join("cache"))
        .output()
        .unwrap();
    let got = fs::read(w.join("dst/f")).unwrap();
    assert!(
        code(&o) != 0 || got == new,
        "exit 0 and the committed file matches neither the source nor the old destination (equals old: {})",
        got == old
    );
}

#[test]
fn au_cor_02_inplace_must_not_delete_a_destination_file_that_differs_only_by_case() {
    let t = tempfile::tempdir().unwrap();
    let w = t.path();
    if !case_insensitive(w) {
        eprintln!("case-sensitive filesystem: skipped");
        return;
    }
    put_at(&w.join("src/README.md"), b"new version\n", 1_600_000_000);
    let old = b"old precious content, newer than the source\n";
    put_at(&w.join("dst/readme.md"), old, 1_900_000_000);
    let o = run(&["-au", "--inplace", "src/", "dst/"], w);
    assert!(w.join("dst/readme.md").exists(), "the existing destination file was deleted: {}", text(&o));
    assert_eq!(fs::read(w.join("dst/readme.md")).unwrap(), old.to_vec());
    assert_eq!(code(&o), 0, "{}", text(&o));
}

#[test]
fn au_cor_03_case_only_rename_converges() {
    let t = tempfile::tempdir().unwrap();
    let w = t.path();
    if !case_insensitive(w) {
        eprintln!("case-sensitive filesystem: skipped");
        return;
    }
    put_at(&w.join("src/Docs/a.txt"), b"a\n", 1_600_000_000);
    put_at(&w.join("dst/docs/a.txt"), b"a\n", 1_600_000_000);
    put_at(&w.join("dst/docs/b.txt"), b"b\n", 1_600_000_000);
    put_at(&w.join("src/README.md"), b"new\n", 1_600_000_100);
    put_at(&w.join("dst/readme.md"), b"old\n", 1_600_000_000);
    let o = run(&["-a", "--delete", "src/", "dst/"], w);
    assert_eq!(code(&o), 0, "first run: {}", text(&o));
    let o = run(&["-a", "-i", "--delete", "src/", "dst/"], w);
    assert_eq!(code(&o), 0, "second run: {}", text(&o));
    assert_eq!(items(&o), Vec::<String>::new(), "the second run must find nothing to do");
}

#[test]
#[ignore = "AU-COR-04: files in a read-only (0555) destination directory cannot be updated (EACCES, exit 23); rsync makes the directory writable temporarily"]
fn au_cor_04_update_inside_a_read_only_destination_directory() {
    let t = tempfile::tempdir().unwrap();
    let w = t.path();
    put_at(&w.join("src/a/f"), b"new\n", 1_600_000_100);
    put_at(&w.join("src/a/new.txt"), b"added\n", 1_600_000_000);
    put_at(&w.join("dst/a/f"), b"old\n", 1_600_000_000);
    fs::set_permissions(w.join("dst/a"), fs::Permissions::from_mode(0o555)).unwrap();
    fs::set_permissions(w.join("src/a"), fs::Permissions::from_mode(0o555)).unwrap();
    if fs::write(w.join("dst/a/probe"), b"x").is_ok() {
        eprintln!("permission checks are bypassed (root): skipped");
        return;
    }
    let o = run(&["-a", "src/", "dst/"], w);
    fs::set_permissions(w.join("dst/a"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(w.join("src/a"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(code(&o), 0, "{}", text(&o));
    assert_eq!(fs::read(w.join("dst/a/f")).unwrap(), b"new\n".to_vec());
    assert!(w.join("dst/a/new.txt").exists());
}

#[test]
fn au_cor_05_source_truncated_during_copy_is_reported_or_exact() {
    let t = tempfile::tempdir().unwrap();
    let w = t.path().to_path_buf();
    put_at(&w.join("src/big"), &data(9, 24 << 20), 1_700_000_000);
    fs::create_dir_all(w.join("dst")).unwrap();
    let p = w.join("src/big");
    let h = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        let f = fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_len(5 << 20).unwrap();
    });
    let o = run(&["-a", "--bwlimit=8m", "src/", "dst/"], &w);
    h.join().unwrap();
    let dst = fs::read(w.join("dst/big")).unwrap_or_default();
    let src = fs::read(w.join("src/big")).unwrap();
    assert!(code(&o) != 0 || dst == src, "exit 0 with a torn destination file: {} bytes (source now {} bytes)", dst.len(), src.len());
}
