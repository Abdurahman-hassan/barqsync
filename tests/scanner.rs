//! `--scanner=jwalk` (the original plan's scan + index + queues) end to end: the same
//! commands must leave the same trees and print the same items as rsync. The whole
//! suite also runs under this scanner with `FASTRSYNC_SCANNER=jwalk cargo test`.

mod common;

use common::*;
use std::fs;

const J: &str = "--scanner=jwalk";

/// [`both`] with `--scanner=jwalk` for fastrsync only.
fn both_j(cwd: &std::path::Path, args: &[&str]) -> (RunOut, RunOut) {
    let sub = |d: &str| -> Vec<String> { args.iter().map(|a| a.replace("{dst}", d)).collect() };
    let ra = sub("dr");
    let r = rsync(cwd, &ra.iter().map(String::as_str).collect::<Vec<_>>()).unwrap();
    let mut fa = vec![J.to_string()];
    fa.extend(sub("df"));
    (r, fastrsync(cwd, &fa.iter().map(String::as_str).collect::<Vec<_>>()))
}

/// [`both_remote`] with `--scanner=jwalk` for fastrsync only (it reaches the receiver
/// through the options the client sends).
fn both_remote_j(cwd: &std::path::Path, args: &[&str]) -> (RunOut, RunOut) {
    let shim = fake_ssh(cwd);
    let rsync_bin = reference_rsync().unwrap();
    let mut ra = vec!["-e".to_string(), shim.to_str().unwrap().to_string(), format!("--rsync-path={}", rsync_bin.display())];
    ra.extend(args.iter().map(|a| a.replace("{dst}", "dr").replace("{host}", "fakehost:")));
    let r = rsync(cwd, &ra.iter().map(String::as_str).collect::<Vec<_>>()).unwrap();
    let mut fa = vec![J.to_string()];
    fa.extend(args.iter().map(|a| a.replace("{dst}", "df")));
    (r, fastrsync_remote(cwd, &fa.iter().map(String::as_str).collect::<Vec<_>>()))
}

/// Mutate a synced copy of [`rich_tree`] in every way the engine must notice.
fn mutate(t: &Tree) {
    t.file("a.txt", "ALPHA\n")
        .file("sub/b.txt", "bravo-appended")
        .file("new/nested/file", "new")
        .file("new/nested/big.bin", vec![9u8; 300_000])
        .remove("sub/deep")
        .remove("emptydir")
        .file("emptydir", "dir became file")
        .remove("with space.txt")
        .dir("with space.txt")
        .file("with space.txt/inside", "file became dir")
        .symlink("link", "sub/b.txt")
        .chmod("big.bin", 0o640)
        .chmod("sub", 0o700)
        .settle_mtimes();
}

fn check(tmp: &std::path::Path, r: &RunOut, f: &RunOut, with_root: bool) {
    assert_eq!(f.code, 0, "stderr: {}", f.stderr);
    assert_eq!(normalize(r.items(), "dr", "D"), normalize(f.items(), "df", "D"));
    assert_same_tree(&snapshot(&tmp.join("dr"), with_root), &snapshot(&tmp.join("df"), with_root));
}

#[test]
fn fresh_copy_and_incremental_sync_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = rich_tree(&tmp.path().join("src"));
    let (r, f) = both_j(tmp.path(), &["-ai", "src/", "{dst}"]);
    check(tmp.path(), &r, &f, true);
    mutate(&t);
    for d in ["dr", "df"] {
        fs::write(tmp.path().join(d).join("extra.txt"), "x").unwrap();
        fs::create_dir_all(tmp.path().join(d).join("olddir/deeper")).unwrap();
        fs::write(tmp.path().join(d).join("olddir/deeper/f"), "y").unwrap();
    }
    let (r, f) = both_j(tmp.path(), &["-ai", "--delete", "src/", "{dst}"]);
    check(tmp.path(), &r, &f, true);
    // Nothing left to do.
    let f = fastrsync(tmp.path(), &["-ai", "--delete", J, "src/", "df"]);
    assert_eq!((f.code, f.items()), (0, Vec::<String>::new()), "{}", f.stderr);
}

#[test]
fn directory_items_and_excludes_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = rich_tree(&tmp.path().join("src"));
    t.file("sub/skip.o", "obj").file("cache/x", "c").settle_mtimes();
    let args = ["-ai", "--exclude=*.o", "--exclude=cache/", "src", "{dst}"];
    let (r, f) = both_j(tmp.path(), &args);
    check(tmp.path(), &r, &f, false);
    mutate(&t);
    let (r, f) = both_j(tmp.path(), &args);
    check(tmp.path(), &r, &f, false);
}

#[test]
fn hard_links_and_symlinked_dirs_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = rich_tree(&tmp.path().join("src"));
    fs::hard_link(t.path("a.txt"), t.path("sub/a-link")).unwrap();
    // -L turns the directory symlink into a directory the scan never walked (and, like
    // rsync, fails on the dangling symlink).
    let (r, f) = both_j(tmp.path(), &["-aiL", "src/", "{dst}"]);
    assert_eq!(r.code, f.code, "stderr: {}", f.stderr);
    assert_eq!(normalize(r.items(), "dr", "D"), normalize(f.items(), "df", "D"));
    assert_same_tree(&snapshot(&tmp.path().join("dr"), true), &snapshot(&tmp.path().join("df"), true));
    // -H: the same link groups (the leader named in `=>` lines can differ, see m6_features).
    let (r, f) = both_j(tmp.path(), &["-aH", "src/", "{dst}-h"]);
    assert_eq!((r.code, f.code), (0, 0), "stderr: {}", f.stderr);
    assert_same_tree(&snapshot(&tmp.path().join("dr-h"), true), &snapshot(&tmp.path().join("df-h"), true));
    // -K keeps a destination symlink to a directory: its entries can't come from the index.
    let t2 = Tree::new(tmp.path().join("src2"));
    t2.file("real/f", "one").settle_mtimes();
    for d in ["dr2", "df2"] {
        Tree::new(tmp.path().join(d)).file("realdir/old", "old").symlink("real", "realdir");
    }
    let (r, f) = both_j(tmp.path(), &["-aiK", "src2/", "{dst}2"]);
    assert_eq!(f.code, 0, "{}", f.stderr);
    assert_eq!(r.items(), f.items());
    assert!(fs::symlink_metadata(tmp.path().join("df2/real")).unwrap().file_type().is_symlink());
    assert_eq!(fs::read(tmp.path().join("df2/realdir/f")).unwrap(), b"one");
}

#[test]
fn remote_receiver_with_destination_index_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = rich_tree(&tmp.path().join("src"));
    let (r, f) = both_remote_j(tmp.path(), &["-ai", "src/", "{host}{dst}"]);
    check(tmp.path(), &r, &f, true);
    mutate(&t);
    let (r, f) = both_remote_j(tmp.path(), &["-ai", "--delete", "src/", "{host}{dst}"]);
    check(tmp.path(), &r, &f, true);
}

#[test]
fn many_directories_and_files() {
    let tmp = tempfile::tempdir().unwrap();
    let t = Tree::new(tmp.path().join("src"));
    for d in 0..40 {
        for f in 0..25 {
            t.file(&format!("d{d}/e{}/f{f}", d % 3), format!("{d}-{f}"));
        }
    }
    t.settle_mtimes();
    for scanner in ["--scanner=jwalk", "--scanner=dirtask"] {
        let dst = format!("dst-{scanner}");
        let f = fastrsync(tmp.path(), &["-a", scanner, "src/", &dst]);
        assert_eq!(f.code, 0, "{}", f.stderr);
        assert_same_tree(&snapshot(&tmp.path().join("src"), false), &snapshot(&tmp.path().join(&dst), false));
    }
}
