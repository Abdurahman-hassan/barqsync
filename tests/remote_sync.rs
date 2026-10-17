//! Remote transfers (push and pull) through a fake ssh that runs the server locally.
//! Where possible the same transfer also runs through rsync over the same shim, and
//! printed items and resulting trees must match.

mod common;

use common::*;
use std::fs;

#[test]
fn push_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    rich_tree(&tmp.path().join("src"));
    let (r, f) = both_remote(tmp.path(), &["-av", "src/", "{host}{dst}"], "dr", "df").unwrap();
    assert_eq!(f.code, 0, "{}\n{}", f.stdout, f.stderr);
    assert_eq!(normalize(r.items(), "dr", "D"), normalize(f.items(), "df", "D"));
    assert_same_tree(&snapshot(&tmp.path().join("dr"), true), &snapshot(&tmp.path().join("df"), true));
}

#[test]
fn pull_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    rich_tree(&tmp.path().join("src"));
    let (r, f) = both_remote(tmp.path(), &["-av", "{host}src", "{dst}"], "dr", "df").unwrap();
    assert_eq!(f.code, 0, "{}\n{}", f.stdout, f.stderr);
    assert_eq!(normalize(r.items(), "dr", "D"), normalize(f.items(), "df", "D"));
    assert_same_tree(&snapshot(&tmp.path().join("dr"), false), &snapshot(&tmp.path().join("df"), false));
}

#[test]
fn incremental_push_with_delete_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = rich_tree(&tmp.path().join("src"));
    for d in ["dr", "df"] {
        assert_eq!(fastrsync(tmp.path(), &["-a", "src/", d]).code, 0);
    }
    t.file("a.txt", "ALPHA\n")
        .file("sub/b.txt", "bravo-appended")
        .file("new/nested/file", "new")
        .remove("sub/deep")
        .remove("emptydir")
        .file("emptydir", "dir became file")
        .symlink("link", "sub/b.txt")
        .chmod("big.bin", 0o640)
        .settle_mtimes();
    for d in ["dr", "df"] {
        Tree::new(tmp.path().join(d)).file("extra.txt", "x").file("olddir/deeper/f", "y");
    }
    let (r, f) = both_remote(tmp.path(), &["-av", "--delete", "src/", "{host}{dst}"], "dr", "df").unwrap();
    assert_eq!(f.code, 0, "{}\n{}", f.stdout, f.stderr);
    assert_eq!(r.items(), f.items());
    assert_same_tree(&snapshot(&tmp.path().join("dr"), true), &snapshot(&tmp.path().join("df"), true));
}

#[test]
fn excludes_and_dry_run_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src"))
        .file("keep.log", "k")
        .file("drop.log", "d")
        .file("build/out.o", "o")
        .file("src/main.c", "m")
        .settle_mtimes();
    for d in ["dr", "df"] {
        Tree::new(tmp.path().join(d)).file("drop.log", "old").file("stale", "s");
    }
    let before = snapshot(&tmp.path().join("df"), true);
    let args = ["-avn", "--delete", "--include=keep.log", "--exclude=*.log", "--exclude=/build", "src/", "{host}{dst}"];
    let (r, f) = both_remote(tmp.path(), &args, "dr", "df").unwrap();
    assert_eq!(f.code, 0, "{}", f.stderr);
    assert_eq!(r.items(), f.items());
    assert_eq!(before, snapshot(&tmp.path().join("df"), true), "dry run changed the destination");

    let args = ["-av", "--delete", "--include=keep.log", "--exclude=*.log", "--exclude=/build", "src/", "{host}{dst}"];
    let (r, f) = both_remote(tmp.path(), &args, "dr", "df").unwrap();
    assert_eq!(r.items(), f.items());
    assert_same_tree(&snapshot(&tmp.path().join("dr"), false), &snapshot(&tmp.path().join("df"), false));
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Old and new versions of a few large files (same size and changed size), big
/// enough (8 MiB+) to get page-cache hints.
fn delta_fixture(root: &std::path::Path) {
    let base = noise((8 << 20) + 12_345, 1);
    let mut edited = base.clone();
    edited[2_000_000..2_000_050].copy_from_slice(&noise(50, 2));
    let mut grown = noise(333, 3);
    grown.extend_from_slice(&base);
    grown.extend(noise(10_000, 4));
    let old = Tree::new(root.join("dst"));
    old.file("edited.img", &base).file("grown.log", &base).file("small.txt", "old").settle_mtimes();
    let new = Tree::new(root.join("src"));
    new.file("edited.img", &edited).file("grown.log", &grown).file("small.txt", "new!").settle_mtimes();
    new.mtime("edited.img", BASE_TIME + 900_001).mtime("grown.log", BASE_TIME + 900_002);
}

fn stat_bytes(stdout: &str, label: &str) -> u64 {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix(label))
        .and_then(|r| r.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no {label:?} in:\n{stdout}"))
}

#[test]
fn delta_over_the_wire() {
    let modes: [&[&str]; 7] =
        [&[], &["-z"], &["--inplace"], &["--no-clone"], &["--delta=cdc"], &["--delta=cdc", "--inplace"], &["--delta=cdc", "-z"]];
    for extra in modes {
        for pull in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            delta_fixture(tmp.path());
            // Delta mechanics: keep the adaptive strategy from sending these whole.
            let mut args = vec!["-a", "--stats", "--strategy", "fixed"];
            args.extend_from_slice(extra);
            if pull {
                args.extend_from_slice(&["{host}src/", "dst"]);
            } else {
                args.extend_from_slice(&["src/", "{host}dst"]);
            }
            let f = fastrsync_remote(tmp.path(), &args);
            assert_eq!(f.code, 0, "{extra:?} pull={pull}\n{}\n{}", f.stdout, f.stderr);
            let mut want = snapshot(&tmp.path().join("src"), true);
            let got = snapshot(&tmp.path().join("dst"), true);
            want.get_mut(b".".as_slice()).unwrap().mtime = got[b".".as_slice()].mtime;
            assert_same_tree(&want, &got);
            let literal = stat_bytes(&f.stdout, "Literal data: ");
            let matched = stat_bytes(&f.stdout, "Matched data: ");
            assert!(matched > 9 << 20, "{extra:?} pull={pull}: matched {matched}, literal {literal}");
            assert!(literal < 1 << 20, "{extra:?} pull={pull}: literal {literal}");
        }
    }
}

#[test]
fn whole_file_flag_disables_remote_delta() {
    let tmp = tempfile::tempdir().unwrap();
    delta_fixture(tmp.path());
    let f = fastrsync_remote(tmp.path(), &["-a", "-W", "--stats", "src/", "{host}dst"]);
    assert_eq!(f.code, 0, "{}", f.stderr);
    assert_eq!(stat_bytes(&f.stdout, "Matched data: "), 0);
    let mut want = snapshot(&tmp.path().join("src"), true);
    let got = snapshot(&tmp.path().join("dst"), true);
    want.get_mut(b".".as_slice()).unwrap().mtime = got[b".".as_slice()].mtime;
    assert_same_tree(&want, &got);
}

#[test]
fn many_small_files_are_packed() {
    let tmp = tempfile::tempdir().unwrap();
    let t = Tree::new(tmp.path().join("src"));
    for d in 0..20 {
        for i in 0..150 {
            t.file(&format!("d{d:02}/f{i:03}"), noise(100 + i * 37, (d * 1000 + i) as u64));
        }
    }
    t.settle_mtimes();
    for compress in [false, true] {
        let dst = if compress { "dz" } else { "d" };
        let mut args = vec!["-a"];
        if compress {
            args.push("-z");
        }
        let target = format!("{{host}}{dst}");
        args.extend_from_slice(&["src/", &target]);
        let f = fastrsync_remote(tmp.path(), &args);
        assert_eq!(f.code, 0, "{}", f.stderr);
        assert_same_tree(&snapshot(&tmp.path().join("src"), true), &snapshot(&tmp.path().join(dst), true));
    }
}

#[test]
fn single_file_to_new_remote_name() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path()).file("one.txt", "1").dir("into");
    let r = fastrsync_remote(tmp.path(), &["-a", "one.txt", "{host}renamed.txt"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(fs::read_to_string(tmp.path().join("renamed.txt")).unwrap(), "1");
    let r = fastrsync_remote(tmp.path(), &["-a", "{host}one.txt", "into/"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(fs::read_to_string(tmp.path().join("into/one.txt")).unwrap(), "1");
}

#[test]
fn missing_source_is_a_partial_transfer() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("ok", "1");
    let r = fastrsync_remote(tmp.path(), &["-a", "src", "nope", "{host}dst"]);
    assert_eq!(r.code, 23, "{}", r.stderr);
    assert!(r.stderr.contains("nope"), "{}", r.stderr);
    assert!(tmp.path().join("dst/src/ok").exists());
    // Pull: the error happens on the server and is forwarded to the client.
    let r = fastrsync_remote(tmp.path(), &["-a", "{host}src", "{host}nope", "dst2"]);
    assert_eq!(r.code, 23, "{}", r.stderr);
    assert!(r.stderr.contains("nope"), "{}", r.stderr);
    assert!(tmp.path().join("dst2/src/ok").exists());
}

#[test]
fn bad_remote_path_fails_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", "1");
    let shim = fake_ssh(tmp.path());
    let r = fastrsync(
        tmp.path(),
        &["-a", "-e", shim.to_str().unwrap(), "--remote-path=/nonexistent/fastrsync", "src/", "fakehost:dst"],
    );
    assert_ne!(r.code, 0);
    assert!(r.stderr.contains("connection unexpectedly closed"), "{}", r.stderr);
}

/// fastrsync through the fake ssh with extra environment (inherited by the server).
fn remote_with_env(cwd: &std::path::Path, args: &[&str], env: &[(&str, &str)]) -> RunOut {
    let shim = fake_ssh(cwd);
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_fastrsync"));
    cmd.arg("-e")
        .arg(&shim)
        .arg(format!("--remote-path={}", env!("CARGO_BIN_EXE_fastrsync")))
        .args(args.iter().map(|a| a.replace("{host}", "fakehost:")))
        .env_remove("SSH_CONNECTION");
    for (k, v) in env {
        cmd.env(k, v);
    }
    run(&mut cmd, cwd)
}

#[test]
fn parallel_tls_streams_push_and_pull() {
    for extra in [&[][..], &["-z"]] {
        for pull in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            delta_fixture(tmp.path());
            let t = Tree::new(tmp.path().join("src"));
            for i in 0..300 {
                t.file(&format!("many/f{i:03}"), noise(1000 + i * 13, i as u64));
            }
            t.settle_mtimes();
            let mut args = vec!["-avv", "--stats", "--strategy", "fixed", "--transport", "tcp", "--streams", "4"];
            args.extend_from_slice(extra);
            if pull {
                args.extend_from_slice(&["{host}src/", "dst"]);
            } else {
                args.extend_from_slice(&["src/", "{host}dst"]);
            }
            let f = remote_with_env(tmp.path(), &args, &[]);
            assert_eq!(f.code, 0, "{extra:?} pull={pull}\n{}\n{}", f.stdout, f.stderr);
            assert!(f.stdout.contains("using 4 parallel TLS data streams"), "{}", f.stdout);
            let mut want = snapshot(&tmp.path().join("src"), true);
            let got = snapshot(&tmp.path().join("dst"), true);
            want.get_mut(b".".as_slice()).unwrap().mtime = got[b".".as_slice()].mtime;
            assert_same_tree(&want, &got);
            assert!(stat_bytes(&f.stdout, "Matched data: ") > 9 << 20, "delta over streams: {}", f.stdout);
        }
    }
}

#[test]
fn parallel_streams_fall_back_to_ssh() {
    let tmp = tempfile::tempdir().unwrap();
    rich_tree(&tmp.path().join("src"));
    let f = remote_with_env(
        tmp.path(),
        &["-av", "--transport", "tcp", "--streams", "3", "src/", "{host}dst"],
        &[("FASTRSYNC_TEST_STREAM_ADDR", "127.0.0.1:1")],
    );
    assert_eq!(f.code, 0, "{}\n{}", f.stdout, f.stderr);
    assert!(f.stdout.contains("using the ssh connection"), "{}", f.stdout);
    assert_same_tree(&snapshot(&tmp.path().join("src"), true), &snapshot(&tmp.path().join("dst"), true));
}

#[test]
fn quic_push_and_pull_in_both_wire_formats() {
    for wire in ["postcard", "rkyv"] {
        for pull in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            delta_fixture(tmp.path());
            let t = Tree::new(tmp.path().join("src"));
            for i in 0..300 {
                t.file(&format!("many/f{i:03}"), noise(1000 + i * 13, i as u64));
            }
            t.settle_mtimes();
            let mut args = vec!["-avv", "--stats", "--strategy", "fixed", "--transport", "quic", "--streams", "4", "--wire", wire];
            if pull {
                args.extend_from_slice(&["{host}src/", "dst"]);
            } else {
                args.extend_from_slice(&["src/", "{host}dst"]);
            }
            let f = remote_with_env(tmp.path(), &args, &[]);
            assert_eq!(f.code, 0, "{wire} pull={pull}\n{}\n{}", f.stdout, f.stderr);
            assert!(f.stdout.contains("using QUIC to"), "{}", f.stdout);
            let mut want = snapshot(&tmp.path().join("src"), true);
            let got = snapshot(&tmp.path().join("dst"), true);
            want.get_mut(b".".as_slice()).unwrap().mtime = got[b".".as_slice()].mtime;
            assert_same_tree(&want, &got);
            assert!(stat_bytes(&f.stdout, "Matched data: ") > 9 << 20, "delta over QUIC: {}", f.stdout);
        }
    }
}

#[test]
fn quic_falls_back_to_ssh() {
    let tmp = tempfile::tempdir().unwrap();
    rich_tree(&tmp.path().join("src"));
    let started = std::time::Instant::now();
    let f = remote_with_env(
        tmp.path(),
        &["-av", "--transport", "quic", "src/", "{host}dst"],
        &[("FASTRSYNC_TEST_STREAM_ADDR", "127.0.0.1:1")],
    );
    assert_eq!(f.code, 0, "{}\n{}", f.stdout, f.stderr);
    assert!(f.stdout.contains("QUIC: could not connect") && f.stdout.contains("using the ssh connection"), "{}", f.stdout);
    assert!(started.elapsed() < std::time::Duration::from_secs(15), "fallback took {:?}", started.elapsed());
    assert_same_tree(&snapshot(&tmp.path().join("src"), true), &snapshot(&tmp.path().join("dst"), true));
}

#[test]
fn adaptive_strategy_routes_by_link_speed() {
    // delta_fixture: two files of ~8 MiB with old copies on the receiver.
    for (bps, whole) in [("10000000000", true), ("20000000", false)] {
        for pull in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            delta_fixture(tmp.path());
            let mut args = vec!["-a", "--stats", "--strategy", "adaptive"];
            if pull {
                args.extend_from_slice(&["{host}src/", "dst"]);
            } else {
                args.extend_from_slice(&["src/", "{host}dst"]);
            }
            let f = remote_with_env(tmp.path(), &args, &[("FASTRSYNC_TEST_LINK_BPS", bps)]);
            assert_eq!(f.code, 0, "{bps} pull={pull}\n{}\n{}", f.stdout, f.stderr);
            let matched = stat_bytes(&f.stdout, "Matched data: ");
            // 10 Gbit/s: files under 32 MiB go whole; 20 Mbit/s: as deltas.
            assert_eq!(matched == 0, whole, "{bps} pull={pull}: matched {matched}");
            let mut want = snapshot(&tmp.path().join("src"), true);
            let got = snapshot(&tmp.path().join("dst"), true);
            want.get_mut(b".".as_slice()).unwrap().mtime = got[b".".as_slice()].mtime;
            assert_same_tree(&want, &got);
        }
    }
}

#[test]
fn adaptive_strategy_probes_the_link() {
    for transport in ["ssh", "tcp", "quic"] {
        let tmp = tempfile::tempdir().unwrap();
        delta_fixture(tmp.path());
        let f = remote_with_env(
            tmp.path(),
            &["-avv", "--stats", "--strategy", "adaptive", "--transport", transport, "src/", "{host}dst"],
            &[],
        );
        assert_eq!(f.code, 0, "{transport}\n{}\n{}", f.stdout, f.stderr);
        assert!(f.stdout.lines().any(|l| l.starts_with("link: ") && l.contains("Mbit/s")), "{transport}: {}", f.stdout);
        let mut want = snapshot(&tmp.path().join("src"), true);
        let got = snapshot(&tmp.path().join("dst"), true);
        want.get_mut(b".".as_slice()).unwrap().mtime = got[b".".as_slice()].mtime;
        assert_same_tree(&want, &got);
    }
}

#[test]
fn whole_file_flags_override_the_adaptive_strategy() {
    for (flag, delta) in [("-W", false), ("--no-whole-file", true)] {
        let tmp = tempfile::tempdir().unwrap();
        delta_fixture(tmp.path());
        let f = remote_with_env(
            tmp.path(),
            &["-avv", "--stats", "--strategy", "adaptive", flag, "src/", "{host}dst"],
            &[("FASTRSYNC_TEST_LINK_BPS", "10000000000")],
        );
        assert_eq!(f.code, 0, "{flag}\n{}\n{}", f.stdout, f.stderr);
        assert!(!f.stdout.lines().any(|l| l.starts_with("link: ")), "{flag}: no probe expected: {}", f.stdout);
        assert_eq!(stat_bytes(&f.stdout, "Matched data: ") > 0, delta, "{flag}: {}", f.stdout);
    }
}

#[test]
fn io_uring_pack_writes_match_rsync() {
    // Linux writes received packs in io_uring batches; elsewhere the flag is a no-op.
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = Tree::new(tmp.path().join("src"));
    for d in 0..12 {
        for i in 0..120 {
            t.file(&format!("d{d:02}/f{i:03}"), noise(50 + i * 41, (d * 1000 + i) as u64));
        }
    }
    t.file("empty", "").chmod("d00/f000", 0o600).chmod("d01/f001", 0o4755).settle_mtimes();
    let (r, f) = both_remote_env(tmp.path(), &["-ai", "src/", "{host}{dst}"], "dr", "df", &[("FASTRSYNC_IO_URING", "1")]).unwrap();
    assert_eq!(f.code, 0, "{}", f.stderr);
    assert_eq!(normalize(r.items(), "dr", "D"), normalize(f.items(), "df", "D"));
    assert_same_tree(&snapshot(&tmp.path().join("dr"), true), &snapshot(&tmp.path().join("df"), true));
    // Again into an existing tree: every file replaced through the batch path.
    t.file("d03/f010", "changed").settle_mtimes();
    for d in ["dr", "df"] {
        Tree::new(tmp.path().join(d)).file("d05/f005", "stale");
    }
    let (r, f) = both_remote_env(tmp.path(), &["-ai", "src/", "{host}{dst}"], "dr", "df", &[("FASTRSYNC_IO_URING", "1")]).unwrap();
    assert_eq!(f.code, 0, "{}", f.stderr);
    assert_eq!(normalize(r.items(), "dr", "D"), normalize(f.items(), "df", "D"));
    assert_same_tree(&snapshot(&tmp.path().join("dr"), true), &snapshot(&tmp.path().join("df"), true));
}
