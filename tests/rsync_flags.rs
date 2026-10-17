//! The less common rsync flags, each run through the reference rsync and barqsync
//! (locally, and over the fake ssh for the ones that involve the protocol): the printed
//! items, exit codes and resulting trees must match.

mod common;

use common::*;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

type Snap = BTreeMap<Vec<u8>, Node>;

/// Same exit code and the same item lines (with the per-tool names normalized).
fn same_output(r: &RunOut, f: &RunOut, names: &[(&str, &str)], what: &str) {
    assert_eq!(
        r.code, f.code,
        "{what}: exit codes differ\n--- rsync stderr:\n{}\n--- barqsync stderr:\n{}\n--- barqsync stdout:\n{}",
        r.stderr, f.stderr, f.stdout
    );
    let norm = |out: &RunOut, rsync_side: bool| {
        const STATS: [&str; 3] = ["Literal data", "Matched data", "File list"];
        let mut items: Vec<String> = out
            .items()
            .into_iter()
            .filter(|l| !STATS.iter().any(|s| l.starts_with(s)))
            .map(|l| if l.starts_with("created directory ") { "created directory".into() } else { l })
            .collect();
        for (i, n) in names.iter().enumerate() {
            items = normalize(items, if rsync_side { n.0 } else { n.1 }, &format!("<{i}>"));
        }
        items
    };
    assert_eq!(norm(r, true), norm(f, false), "{what}: items differ\nbarqsync stderr: {}", f.stderr);
}

fn snap(p: &Path, with_root: bool) -> Snap {
    snapshot(p, with_root)
}

/// Snapshot without directory mtimes (for trees whose directories were changed "now").
fn snap_no_dir_times(p: &Path) -> Snap {
    let mut m = snapshot(p, false);
    for n in m.values_mut() {
        if n.kind == 'd' {
            n.mtime = None;
        }
    }
    m
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

/// Replace a file's content but keep its mtime (a change only `-c` can see).
fn rewrite_keeping_mtime(p: &Path, content: &[u8]) {
    let m = fs::metadata(p).unwrap().mtime() as u64;
    fs::write(p, content).unwrap();
    set_mtime(p, m);
}

/// Copy `src/` to each destination with barqsync -a (identical starting points).
fn seed(cwd: &Path, src: &str, dsts: &[&str]) {
    for d in dsts {
        let r = barqsync(cwd, &["-a", src, d]);
        assert_eq!(r.code, 0, "{}", r.stderr);
    }
}

// ---------------------------------------------------------------------------
// -c / --checksum
// ---------------------------------------------------------------------------

fn checksum_fixture(root: &Path) -> Tree {
    let t = Tree::new(root.join("src"));
    t.file("same", "same-content")
        .file("swapped", "AAAA")
        .file("touched", "touched")
        .file("grown", "short")
        .file("sub/deep", vec![3u8; 300_000])
        .settle_mtimes();
    t
}

fn checksum_mutate(t: &Tree) {
    rewrite_keeping_mtime(&t.path("swapped"), b"BBBB");
    let mut big = vec![3u8; 300_000];
    big[150_000] = 9;
    rewrite_keeping_mtime(&t.path("sub/deep"), &big);
    t.mtime("touched", BASE_TIME + 50_000);
    let m = fs::metadata(t.path("grown")).unwrap().mtime() as u64;
    t.file("grown", "now longer").mtime("grown", m);
}

#[test]
fn checksum_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = checksum_fixture(tmp.path());
    seed(tmp.path(), "src/", &["dr", "df"]);
    checksum_mutate(&t);
    // Without -c the quick check misses the same-size, same-mtime changes.
    let (r, f) = both(tmp.path(), &["-ai", "src/", "{dst}"], "dr", "df").unwrap();
    same_output(&r, &f, &[], "-ai");
    let (r, f) = both(tmp.path(), &["-aci", "src/", "{dst}"], "dr", "df").unwrap();
    same_output(&r, &f, &[], "-aci");
    assert!(f.stdout.contains(">fc........ swapped"), "{}", f.stdout);
    assert_same_tree(&snap(&tmp.path().join("dr"), true), &snap(&tmp.path().join("df"), true));
    // A second run finds nothing to do.
    let (r, f) = both(tmp.path(), &["-aci", "src/", "{dst}"], "dr", "df").unwrap();
    same_output(&r, &f, &[], "-aci again");
    assert!(f.items().is_empty(), "{}", f.stdout);
}

#[test]
fn checksum_over_the_wire_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = checksum_fixture(tmp.path());
    seed(tmp.path(), "src/", &["pr", "pf", "lr", "lf"]);
    checksum_mutate(&t);
    let (r, f) = both_remote(tmp.path(), &["-aci", "src/", "{host}{dst}"], "pr", "pf").unwrap();
    same_output(&r, &f, &[], "push -aci");
    assert_same_tree(&snap(&tmp.path().join("pr"), true), &snap(&tmp.path().join("pf"), true));
    let (r, f) = both_remote(tmp.path(), &["-aci", "{host}src/", "{dst}"], "lr", "lf").unwrap();
    same_output(&r, &f, &[], "pull -aci");
    assert_same_tree(&snap(&tmp.path().join("lr"), true), &snap(&tmp.path().join("lf"), true));
    assert_same_tree(&snap(&tmp.path().join("src"), true), &snap(&tmp.path().join("lf"), true));
}

// ---------------------------------------------------------------------------
// -S / --sparse
// ---------------------------------------------------------------------------

/// Does this filesystem turn unwritten ranges into holes? (APFS volumes with data
/// protection do not.)
fn makes_holes(dir: &Path) -> bool {
    let p = dir.join("hole-probe");
    let f = fs::File::create(&p).unwrap();
    std::os::unix::fs::FileExt::write_all_at(&f, b"x", 8 << 20).unwrap();
    drop(f);
    let holes = fs::metadata(&p).unwrap().blocks() * 512 < 1 << 20;
    fs::remove_file(p).unwrap();
    holes
}

#[test]
fn sparse_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let mut mixed = noise(100_000, 1);
    mixed.extend(vec![0u8; 4 << 20]);
    mixed.extend(noise(1000, 2));
    let mut zeros = vec![0u8; 8 << 20];
    zeros.push(b'x');
    Tree::new(tmp.path().join("src"))
        .file("zeros", &zeros)
        .file("mixed", &mixed)
        .file("small", [0u8; 100])
        .file("tail", [vec![1u8; 10], vec![0u8; 70_000]].concat())
        .settle_mtimes();
    let (r, f) = both(tmp.path(), &["-aiS", "src/", "{dst}"], "dr", "df").unwrap();
    same_output(&r, &f, &[], "-aS");
    assert_same_tree(&snap(&tmp.path().join("dr"), true), &snap(&tmp.path().join("df"), true));
    let (r, f) = both_remote(tmp.path(), &["-aiS", "src/", "{host}{dst}"], "pr", "pf").unwrap();
    same_output(&r, &f, &[], "push -aS");
    assert_same_tree(&snap(&tmp.path().join("pr"), true), &snap(&tmp.path().join("pf"), true));
    if makes_holes(tmp.path()) {
        for d in ["df", "pf"] {
            let blocks = fs::metadata(tmp.path().join(d).join("zeros")).unwrap().blocks();
            assert!(blocks * 512 < 1 << 20, "{d}/zeros is not sparse: {blocks} blocks");
        }
    }
}

// ---------------------------------------------------------------------------
// --partial / --partial-dir
// ---------------------------------------------------------------------------

fn partial_dir_fixture(root: &Path) {
    let t = Tree::new(root.join("src"));
    t.file("big", noise(300_000, 7)).file("sub/big2", noise(200_000, 8)).file("s", "small").settle_mtimes();
    for d in ["dr", "df", "pr", "pf"] {
        let big = fs::read(t.path("big")).unwrap();
        let big2 = fs::read(t.path("sub/big2")).unwrap();
        Tree::new(root.join(d))
            .file(".rsync-partial/big", &big[..100_000])
            .file(".rsync-partial/other", "unrelated")
            .file("sub/.rsync-partial/big2", &big2[..1000]);
    }
}

/// `.rsync-partial` itself was modified "now" by both tools; ignore its mtime.
fn snap_partial(p: &Path) -> Snap {
    let mut m = snapshot(p, true);
    if let Some(n) = m.get_mut(b".rsync-partial".as_slice()) {
        n.mtime = None;
    }
    m
}

#[test]
fn partial_dir_files_are_used_and_removed_like_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    partial_dir_fixture(tmp.path());
    let args = ["-ai", "--partial-dir=.rsync-partial", "--delete", "src/", "{dst}"];
    let (r, f) = both(tmp.path(), &args, "dr", "df").unwrap();
    same_output(&r, &f, &[], "local --partial-dir");
    assert_same_tree(&snap_partial(&tmp.path().join("dr")), &snap_partial(&tmp.path().join("df")));
    assert!(!tmp.path().join("df/sub/.rsync-partial").exists());
    assert!(tmp.path().join("df/.rsync-partial/other").exists(), "--delete must spare the partial dir");

    let args = ["-ai", "--partial-dir=.rsync-partial", "--delete", "--stats", "src/", "{host}{dst}"];
    // The delta basis is the point here; keep adaptive routing from sending it whole.
    let (r, f) = both_remote_env(tmp.path(), &args, "pr", "pf", &[("BARQSYNC_STRATEGY", "fixed")]).unwrap();
    same_output(&r, &f, &[], "push --partial-dir");
    assert_same_tree(&snap_partial(&tmp.path().join("pr")), &snap_partial(&tmp.path().join("pf")));
    let matched = f.stdout.lines().find(|l| l.starts_with("Matched data")).unwrap_or_default().to_string();
    assert!(!matched.contains(": 0 bytes"), "the partial file should be the delta basis: {matched}");
}

fn is_strict_prefix(part: &Path, full: &[u8]) -> bool {
    let p = fs::read(part).unwrap();
    !p.is_empty() && p.len() < full.len() && full.starts_with(&p)
}

#[test]
fn partial_keeps_received_data_when_the_connection_drops() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let big = noise(20 << 20, 9);
    Tree::new(tmp.path().join("src")).file("a", "small").file("big", &big).settle_mtimes();
    let cut = cut_ssh(tmp.path());

    let (r, f) = both_remote_via(tmp.path(), &cut, &["-a", "--partial", "{host}src/", "{dst}"], "kr", "kf").unwrap();
    assert!(r.code != 0 && f.code != 0, "the cut connection must fail both: {} / {}", r.code, f.code);
    for d in ["kr", "kf"] {
        assert!(is_strict_prefix(&tmp.path().join(d).join("big"), &big), "{d}/big is not a partial copy");
    }

    let (_, f) = both_remote_via(tmp.path(), &cut, &["-a", "--partial-dir=.part", "{host}src/", "{dst}"], "qr", "qf").unwrap();
    assert!(f.code != 0);
    for d in ["qr", "qf"] {
        let dir = tmp.path().join(d);
        assert!(is_strict_prefix(&dir.join(".part/big"), &big), "{d}/.part/big is not a partial copy");
        assert!(!dir.join("big").exists());
        assert_eq!(fs::metadata(dir.join(".part")).unwrap().permissions().mode() & 0o777, 0o700);
    }

    let (_, f) = both_remote_via(tmp.path(), &cut, &["-a", "{host}src/", "{dst}"], "nr", "nf").unwrap();
    assert!(f.code != 0);
    let names = |d: &str| -> Vec<_> { fs::read_dir(tmp.path().join(d)).unwrap().map(|e| e.unwrap().file_name()).collect() };
    assert_eq!(names("nr").len(), names("nf").len(), "without --partial nothing is kept: {:?}", names("nf"));

    // Resuming completes the file and clears the partial dir.
    let (r, f) = both_remote(tmp.path(), &["-a", "--partial-dir=.part", "{host}src/", "{dst}"], "qr", "qf").unwrap();
    assert_eq!((r.code, f.code), (0, 0), "{}", f.stderr);
    assert_same_tree(&snap(&tmp.path().join("qr"), true), &snap(&tmp.path().join("qf"), true));
    assert!(!tmp.path().join("qf/.part").exists());
}

// ---------------------------------------------------------------------------
// -R / --relative
// ---------------------------------------------------------------------------

fn relative_fixture(root: &Path) {
    Tree::new(root.join("src"))
        .file("x/y/f", "1")
        .file("x/g", "2")
        .file("z/h", "3")
        .file("w/i", "4")
        .file("top", "5")
        .chmod("x", 0o750)
        .settle_mtimes();
}

#[test]
fn relative_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    relative_fixture(tmp.path());
    let cases: &[&[&str]] = &[
        &["-aiR", "src/x/y/f", "src/z"],
        &["-aiR", "src/x/"],
        &["-aiR", "src/./x/y", "src/./top"],
        &["-aiR", "./src/w"],
        &["-aiRn", "src/x/y/f"],
    ];
    for (i, args) in cases.iter().enumerate() {
        let (dr, df) = (format!("r{i}"), format!("f{i}"));
        let mut a = args.to_vec();
        a.push("{dst}");
        let (r, f) = both(tmp.path(), &a, &dr, &df).unwrap();
        same_output(&r, &f, &[(&dr, &df)], &format!("{args:?}"));
        if tmp.path().join(&dr).exists() {
            assert_same_tree(&snap(&tmp.path().join(&dr), false), &snap(&tmp.path().join(&df), false));
        }
    }
}

#[test]
fn relative_delete_spares_implied_dirs_like_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    relative_fixture(tmp.path());
    for d in ["dr", "df", "pr", "pf"] {
        Tree::new(tmp.path().join(d)).file("src/x/extra/e", "e").file("src/x/y/e2", "e2").file("src/stray", "s");
    }
    let (r, f) = both(tmp.path(), &["-aiR", "--delete", "src/x/y", "{dst}"], "dr", "df").unwrap();
    same_output(&r, &f, &[], "local -R --delete");
    assert_same_tree(&snap_no_dir_times(&tmp.path().join("dr")), &snap_no_dir_times(&tmp.path().join("df")));
    let (r, f) = both_remote(tmp.path(), &["-aiR", "--delete", "src/x/y", "src/z", "{host}{dst}"], "pr", "pf").unwrap();
    same_output(&r, &f, &[], "push -R --delete");
    assert_same_tree(&snap_no_dir_times(&tmp.path().join("pr")), &snap_no_dir_times(&tmp.path().join("pf")));
}

#[test]
fn relative_over_the_wire_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    relative_fixture(tmp.path());
    let (r, f) = both_remote(tmp.path(), &["-aiR", "src/x/y/f", "src/z", "{host}{dst}"], "pr", "pf").unwrap();
    same_output(&r, &f, &[], "push -R");
    assert_same_tree(&snap(&tmp.path().join("pr"), false), &snap(&tmp.path().join("pf"), false));
    let (r, f) = both_remote(tmp.path(), &["-aiR", "{host}src/x/y/f", "{host}src/./z", "{dst}"], "lr", "lf").unwrap();
    same_output(&r, &f, &[], "pull -R");
    assert_same_tree(&snap(&tmp.path().join("lr"), false), &snap(&tmp.path().join("lf"), false));
}

// ---------------------------------------------------------------------------
// --files-from / --from0 / -d
// ---------------------------------------------------------------------------

fn files_from_fixture(root: &Path) {
    relative_fixture(root);
    Tree::new(root.join("src")).file("w/deep/d", "6").settle_mtimes();
    fs::write(root.join("list"), "# comment\nx/y/f\n\n/z\n; also a comment\ntop\nw/\n").unwrap();
    fs::write(root.join("dotlist"), "./x/g\nw/deep/../i\n").unwrap();
    fs::write(root.join("list0"), "x/y/f\0z\0top\0").unwrap();
    // rsync forwards a list to a remote sender without dropping comment lines.
    fs::write(root.join("plainlist"), "x/y/f\n/z\ntop\nw/\n").unwrap();
    // ... and a remote sender rejects names it did not expect (`deep/../i`).
    fs::write(root.join("dotlist-remote"), "./x/g\nw/i\n").unwrap();
    fs::write(root.join("missing"), "top\nnope\n").unwrap();
}

#[test]
fn files_from_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    files_from_fixture(tmp.path());
    let cases: &[&[&str]] = &[
        &["-ai", "--files-from=list"],
        &["-air", "--files-from=list"],
        &["-ai", "--files-from=dotlist"],
        &["-ai", "--from0", "--files-from=list0"],
        &["-ai", "--files-from=missing"],
    ];
    for (i, args) in cases.iter().enumerate() {
        let (dr, df) = (format!("r{i}"), format!("f{i}"));
        if args[1].ends_with("missing") {
            // rsync omits "created directory" after a file-list error; use an existing DEST.
            Tree::new(tmp.path().join(&dr));
            Tree::new(tmp.path().join(&df));
        }
        let mut a = args.to_vec();
        a.extend(["src", "{dst}"]);
        let (r, f) = both(tmp.path(), &a, &dr, &df).unwrap();
        same_output(&r, &f, &[(&dr, &df)], &format!("{args:?}"));
        // DEST itself only gets attributes when the list names `.` (or `./x`).
        let with_root = args[1].contains("dotlist");
        assert_same_tree(&snap(&tmp.path().join(&dr), with_root), &snap(&tmp.path().join(&df), with_root));
    }
    // Exactly one source is allowed.
    let f = barqsync(tmp.path(), &["-a", "--files-from=list", "src", "src", "x"]);
    assert_eq!(f.code, 1);
}

#[test]
fn files_from_over_the_wire_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    files_from_fixture(tmp.path());
    for (i, args) in [
        ["-ai", "--files-from=list", "src", "{host}{dst}"],
        ["-air", "--files-from=plainlist", "{host}src", "{dst}"],
        ["-ai", "--files-from=dotlist-remote", "{host}src", "{dst}"],
    ]
    .iter()
    .enumerate()
    {
        let (dr, df) = (format!("r{i}"), format!("f{i}"));
        let (r, f) = both_remote(tmp.path(), args, &dr, &df).unwrap();
        same_output(&r, &f, &[(&dr, &df)], &format!("{args:?}"));
        // DEST itself only gets attributes when the list names `.`.
        let with_root = args[1].contains("dotlist");
        assert_same_tree(&snap(&tmp.path().join(&dr), with_root), &snap(&tmp.path().join(&df), with_root));
    }
}

#[test]
fn dirs_without_recursion_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    relative_fixture(tmp.path());
    for (i, args) in [["-dit", "src/"], ["-dit", "src"]].iter().enumerate() {
        let (dr, df) = (format!("r{i}"), format!("f{i}"));
        let mut a = args.to_vec();
        a.push("{dst}");
        let (r, f) = both(tmp.path(), &a, &dr, &df).unwrap();
        same_output(&r, &f, &[(&dr, &df)], &format!("{args:?}"));
        assert_same_tree(&snap(&tmp.path().join(&dr), false), &snap(&tmp.path().join(&df), false));
    }
}

// ---------------------------------------------------------------------------
// --existing / --max-size / --min-size
// ---------------------------------------------------------------------------

#[test]
fn existing_and_size_limits_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = Tree::new(tmp.path().join("src"));
    t.file("a", "hello").file("same", "same").file("sub/b", "world").settle_mtimes();
    let cases: &[&[&str]] = &[&["--existing"], &["--max-size=4K"], &["--min-size=10"], &["--max-size=1.5K", "--min-size=6"]];
    let dsts: Vec<String> = (0..cases.len()).flat_map(|i| [format!("r{i}"), format!("f{i}"), format!("pr{i}"), format!("pf{i}")]).collect();
    seed(tmp.path(), "src/", &dsts.iter().map(String::as_str).collect::<Vec<_>>());
    t.file("a", "changed!")
        .file("big", vec![1u8; 5000])
        .file("mid", vec![1u8; 1200])
        .file("newfile", "n")
        .file("newdir/x", "x")
        .file("sub/b", "a longer world")
        .symlink("newlink", "a")
        .settle_mtimes();
    for (i, flags) in cases.iter().enumerate() {
        let mut a = vec!["-ai"];
        a.extend_from_slice(flags);
        a.extend(["src/", "{dst}"]);
        let (r, f) = both(tmp.path(), &a, &format!("r{i}"), &format!("f{i}")).unwrap();
        same_output(&r, &f, &[], &format!("{flags:?}"));
        assert_same_tree(&snap(&tmp.path().join(format!("r{i}")), true), &snap(&tmp.path().join(format!("f{i}")), true));
        let mut a = vec!["-ai"];
        a.extend_from_slice(flags);
        a.extend(["src/", "{host}{dst}"]);
        let (r, f) = both_remote(tmp.path(), &a, &format!("pr{i}"), &format!("pf{i}")).unwrap();
        same_output(&r, &f, &[], &format!("push {flags:?}"));
        assert_same_tree(&snap(&tmp.path().join(format!("pr{i}")), true), &snap(&tmp.path().join(format!("pf{i}")), true));
    }
}

// ---------------------------------------------------------------------------
// --backup / --suffix / --backup-dir
// ---------------------------------------------------------------------------

#[test]
fn backups_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = Tree::new(tmp.path().join("src"));
    t.file("a", "hello").file("sub/b", "world").file("k", "keep").file("ip", "inplace").settle_mtimes();
    let cases: &[&[&str]] = &[
        &["-b", "--delete"],
        &["--backup-dir=../{dst}-bk", "--delete"],
        &["-b", "--suffix=.old"],
        &["-b", "--inplace"],
        &["--backup-dir=bk", "--suffix=.prev", "--delete"],
    ];
    let dsts: Vec<String> = (0..cases.len()).flat_map(|i| [format!("r{i}"), format!("f{i}"), format!("pr{i}"), format!("pf{i}")]).collect();
    seed(tmp.path(), "src/", &dsts.iter().map(String::as_str).collect::<Vec<_>>());
    for d in &dsts {
        Tree::new(tmp.path().join(d)).file("sub/extra", "extra").file("gone", "gone").file("old~", "older backup");
        // The same times in every tool's copy (backups keep them).
        for (rel, t) in [("sub/extra", 21), ("gone", 22), ("old~", 23)] {
            set_mtime(&tmp.path().join(d).join(rel), BASE_TIME + 900_000 + t);
        }
    }
    t.file("a", "HELLO, changed").file("sub/b", "WORLD").file("ip", "INPLACE").settle_mtimes();
    for (i, flags) in cases.iter().enumerate() {
        for (remote, dr, df) in [(false, format!("r{i}"), format!("f{i}")), (true, format!("pr{i}"), format!("pf{i}"))] {
            let mut a = vec!["-ai"];
            a.extend_from_slice(flags);
            a.extend(["src/", if remote { "{host}{dst}" } else { "{dst}" }]);
            let (r, f) = if remote { both_remote(tmp.path(), &a, &dr, &df) } else { both(tmp.path(), &a, &dr, &df) }.unwrap();
            same_output(&r, &f, &[], &format!("remote={remote} {flags:?}"));
            assert_same_tree(&snap_no_dir_times(&tmp.path().join(&dr)), &snap_no_dir_times(&tmp.path().join(&df)));
            let (bkr, bkf) = (tmp.path().join(format!("{dr}-bk")), tmp.path().join(format!("{df}-bk")));
            assert_eq!(bkr.exists(), bkf.exists());
            if bkr.exists() {
                assert_same_tree(&snap_no_dir_times(&bkr), &snap_no_dir_times(&bkf));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// --link-dest / --compare-dest
// ---------------------------------------------------------------------------

fn alt_dest_fixture(root: &Path) -> Tree {
    let t = Tree::new(root.join("src"));
    t.file("same", "same").file("changed", "changed").file("perms", "perms").file("sub/new", "new").symlink("lnk", "same");
    t.settle_mtimes();
    seed(root, "src/", &["prev"]);
    t.file("changed", "CHANGED").mtime("changed", BASE_TIME + 90_000);
    fs::set_permissions(root.join("prev/perms"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(root.join("prev/stale"), "stale").unwrap();
    t
}

fn ino(p: &Path) -> u64 {
    fs::symlink_metadata(p).unwrap().ino()
}

#[test]
fn link_dest_and_compare_dest_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    alt_dest_fixture(tmp.path());
    let cases: &[&[&str]] = &[
        &["-ai", "--link-dest=../prev"],
        &["-aii", "--link-dest=../prev"],
        &["-ai", "--compare-dest=../prev"],
        &["-aii", "--compare-dest=../prev"],
        &["-aic", "--link-dest=../prev"],
        &["-ai", "--link-dest=nope", "--link-dest=../prev"],
    ];
    for (i, flags) in cases.iter().enumerate() {
        let (dr, df) = (format!("r{i}"), format!("f{i}"));
        let mut a = flags.to_vec();
        a.extend(["src/", "{dst}"]);
        let (r, f) = both(tmp.path(), &a, &dr, &df).unwrap();
        same_output(&r, &f, &[(&dr, &df)], &format!("{flags:?}"));
        assert_same_tree(&snap(&tmp.path().join(&dr), true), &snap(&tmp.path().join(&df), true));
        if flags[1].starts_with("--link-dest") && !flags[0].contains('c') {
            for name in ["same", "sub/new", "lnk"] {
                let prev = ino(&tmp.path().join("prev").join(name));
                assert_eq!(ino(&tmp.path().join(&dr).join(name)) == prev, ino(&tmp.path().join(&df).join(name)) == prev, "{name} linked differently");
            }
            assert_eq!(ino(&tmp.path().join(&df).join("same")), ino(&tmp.path().join("prev/same")));
        }
    }
    // An existing, different destination file is replaced by the link too.
    for d in ["xr", "xf"] {
        Tree::new(tmp.path().join(d)).file("same", "other content");
    }
    let (r, f) = both(tmp.path(), &["-ai", "--link-dest=../prev", "src/", "{dst}"], "xr", "xf").unwrap();
    same_output(&r, &f, &[], "existing dest");
    assert_same_tree(&snap(&tmp.path().join("xr"), true), &snap(&tmp.path().join("xf"), true));
}

#[test]
fn link_dest_over_the_wire_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    alt_dest_fixture(tmp.path());
    let (r, f) = both_remote(tmp.path(), &["-aii", "--link-dest=../prev", "src/", "{host}{dst}"], "pr", "pf").unwrap();
    same_output(&r, &f, &[], "push --link-dest");
    assert_same_tree(&snap(&tmp.path().join("pr"), true), &snap(&tmp.path().join("pf"), true));
    assert_eq!(ino(&tmp.path().join("pf/same")), ino(&tmp.path().join("prev/same")));
    let (r, f) = both_remote(tmp.path(), &["-ai", "--compare-dest=../prev", "{host}src/", "{dst}"], "lr", "lf").unwrap();
    same_output(&r, &f, &[], "pull --compare-dest");
    assert_same_tree(&snap(&tmp.path().join("lr"), true), &snap(&tmp.path().join("lf"), true));
}

// ---------------------------------------------------------------------------
// -L / -k / -K
// ---------------------------------------------------------------------------

fn links_fixture(root: &Path) {
    Tree::new(root.join("src"))
        .file("real/f", "f")
        .file("target", "t")
        .symlink("flink", "target")
        .symlink("dlink", "real")
        .symlink("dangling", "nowhere")
        .symlink("real/up", "../src/other")
        .settle_mtimes();
}

#[test]
fn copy_links_and_dirlinks_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    links_fixture(tmp.path());
    for (i, flag) in ["-L", "-k", "--copy-links", "--copy-dirlinks"].iter().enumerate() {
        let (dr, df) = (format!("r{i}"), format!("f{i}"));
        let (r, f) = both(tmp.path(), &["-ai", flag, "src/", "{dst}"], &dr, &df).unwrap();
        same_output(&r, &f, &[], flag);
        assert_same_tree(&snap(&tmp.path().join(&dr), true), &snap(&tmp.path().join(&df), true));
    }
    let (r, f) = both_remote(tmp.path(), &["-aiL", "{host}src/", "{dst}"], "lr", "lf").unwrap();
    same_output(&r, &f, &[], "pull -L");
    assert_same_tree(&snap(&tmp.path().join("lr"), true), &snap(&tmp.path().join("lf"), true));
    let (r, f) = both_remote(tmp.path(), &["-aik", "src/", "{host}{dst}"], "pr", "pf").unwrap();
    same_output(&r, &f, &[], "push -k");
    assert_same_tree(&snap(&tmp.path().join("pr"), true), &snap(&tmp.path().join("pf"), true));
}

#[test]
fn keep_dirlinks_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    links_fixture(tmp.path());
    for d in ["dr", "df", "pr", "pf", "nr", "nf"] {
        Tree::new(tmp.path().join(d)).file("realdir/old", "old").symlink("real", "realdir").file("tgt", "x").symlink("target", "tgt");
    }
    // rsync sets the time on the kept symlink, so the real directory keeps "now".
    let (r, f) = both(tmp.path(), &["-aiK", "src/", "{dst}"], "dr", "df").unwrap();
    same_output(&r, &f, &[], "-K");
    assert_same_tree(&snap_no_dir_times(&tmp.path().join("dr")), &snap_no_dir_times(&tmp.path().join("df")));
    assert!(fs::symlink_metadata(tmp.path().join("df/real")).unwrap().file_type().is_symlink());
    assert!(tmp.path().join("df/realdir/f").exists());
    let (r, f) = both_remote(tmp.path(), &["-aiK", "src/", "{host}{dst}"], "pr", "pf").unwrap();
    same_output(&r, &f, &[], "push -K");
    assert_same_tree(&snap_no_dir_times(&tmp.path().join("pr")), &snap_no_dir_times(&tmp.path().join("pf")));
    // Without -K the symlink is replaced by a real directory.
    let (r, f) = both(tmp.path(), &["-ai", "src/", "{dst}"], "nr", "nf").unwrap();
    same_output(&r, &f, &[], "no -K");
    assert_same_tree(&snap_no_dir_times(&tmp.path().join("nr")), &snap_no_dir_times(&tmp.path().join("nf")));
    assert_eq!(snap(&tmp.path().join("nr/real"), true), snap(&tmp.path().join("nf/real"), true));
}

// ---------------------------------------------------------------------------
// --numeric-ids
// ---------------------------------------------------------------------------

fn gids(root: &Path) -> BTreeMap<Vec<u8>, (u32, u32)> {
    snapshot(root, true)
        .keys()
        .map(|k| {
            let p = if k == b"." { root.to_path_buf() } else { root.join(std::ffi::OsStr::new(std::str::from_utf8(k).unwrap())) };
            let m = fs::symlink_metadata(p).unwrap();
            (k.clone(), (m.uid(), m.gid()))
        })
        .collect()
}

#[test]
fn numeric_ids_and_name_mapping_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    rich_tree(&tmp.path().join("src"));
    for (flags, dr, df) in [(&["-ai", "--numeric-ids"][..], "nr", "nf"), (&["-ai"][..], "mr", "mf")] {
        let mut a = flags.to_vec();
        a.extend(["src/", "{host}{dst}"]);
        let (r, f) = both_remote(tmp.path(), &a, dr, df).unwrap();
        same_output(&r, &f, &[], &format!("{flags:?}"));
        assert_same_tree(&snap(&tmp.path().join(dr), true), &snap(&tmp.path().join(df), true));
        assert_eq!(gids(&tmp.path().join(dr)), gids(&tmp.path().join(df)));
    }
}

// ---------------------------------------------------------------------------
// --chmod
// ---------------------------------------------------------------------------

#[test]
fn chmod_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", "a").file("sub/b", "b").file("sub/x", "x").symlink("lnk", "a").chmod("a", 0o640).chmod("sub/x", 0o755).chmod("sub", 0o751).settle_mtimes();
    let cases: &[&[&str]] = &[
        &["-ai", "--chmod=Du+rwx,Fgo-w"],
        &["-ai", "--chmod=F664,D755"],
        &["-ai", "--chmod=ugo=rwX"],
        &["-ai", "--chmod=a+x"],
        &["-ai", "--chmod=go-rwx,u+X"],
        &["-ai", "--chmod=Fu+s", "--chmod=Fo-r"],
        &["-rti", "--chmod=F600,D700"],
    ];
    for (i, flags) in cases.iter().enumerate() {
        let (dr, df) = (format!("r{i}"), format!("f{i}"));
        let mut a = flags.to_vec();
        a.extend(["src/", "{dst}"]);
        let (r, f) = both(tmp.path(), &a, &dr, &df).unwrap();
        same_output(&r, &f, &[], &format!("{flags:?}"));
        assert_same_tree(&snap(&tmp.path().join(&dr), true), &snap(&tmp.path().join(&df), true));
    }
    let (r, f) = both_remote(tmp.path(), &["-ai", "--chmod=D750,Fg+w,o-r", "src/", "{host}{dst}"], "pr", "pf").unwrap();
    same_output(&r, &f, &[], "push --chmod");
    assert_same_tree(&snap(&tmp.path().join("pr"), true), &snap(&tmp.path().join("pf"), true));
    let (r, f) = both(tmp.path(), &["-a", "--chmod=Zq", "src/", "{dst}"], "br", "bf").unwrap();
    assert_eq!((r.code, f.code), (1, 1));
}

// ---------------------------------------------------------------------------
// --remove-source-files
// ---------------------------------------------------------------------------

fn removal_source(root: &Path, name: &str) {
    Tree::new(root.join(name))
        .file("f1", "1")
        .file("sub/f2", "2")
        .file("same", "same")
        .symlink("lnk", "f1")
        .fifo("fifo")
        .settle_mtimes();
}

#[test]
fn remove_source_files_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // Each tool needs its own source; `{dst}-src` expands per tool.
    for d in ["dr", "df", "pr", "pf", "lr", "lf"] {
        removal_source(tmp.path(), &format!("{d}-src"));
        seed(tmp.path(), &format!("{d}-src/"), &[d]);
        fs::write(tmp.path().join(format!("{d}-src/f3")), "3").unwrap();
        fs::write(tmp.path().join(format!("{d}-src/sub/f4")), "4").unwrap();
        // The same times in every tool's copy, however long creating them took.
        for (rel, t) in [("f3", 11), ("sub/f4", 12), ("sub", 13), ("", 14)] {
            set_mtime(&tmp.path().join(format!("{d}-src")).join(rel), BASE_TIME + 900_000 + t);
        }
    }
    let (r, f) = both(tmp.path(), &["-ai", "--remove-source-files", "{dst}-src/", "{dst}"], "dr", "df").unwrap();
    same_output(&r, &f, &[], "local");
    let (r, f) = both_remote(tmp.path(), &["-ai", "--remove-source-files", "{dst}-src/", "{host}{dst}"], "pr", "pf").unwrap();
    same_output(&r, &f, &[], "push");
    let (r, f) = both_remote(tmp.path(), &["-ai", "--remove-source-files", "{host}{dst}-src/", "{dst}"], "lr", "lf").unwrap();
    same_output(&r, &f, &[], "pull");
    for (a, b) in [("dr", "df"), ("pr", "pf"), ("lr", "lf")] {
        assert_same_tree(&snap(&tmp.path().join(a), true), &snap(&tmp.path().join(b), true));
        let (sa, sb) = (tmp.path().join(format!("{a}-src")), tmp.path().join(format!("{b}-src")));
        assert_same_tree(&snap_no_dir_times(&sa), &snap_no_dir_times(&sb));
        assert!(!sb.join("f1").exists() && sb.join("sub").is_dir());
    }
}
