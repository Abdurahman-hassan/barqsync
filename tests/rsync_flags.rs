//! The less common rsync flags, each run through the reference rsync and barqsync
//! (locally, and over the fake ssh for the ones that involve the protocol): the printed
//! items, exit codes and resulting trees must match.

mod common;

use common::*;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Snapshot without the mtimes from `since` on: without -t a written file gets the time
/// of the copy, which two runs a moment apart needn't share. Older times must still match.
fn snap_old_times(p: &Path, since: i64) -> Snap {
    let mut m = snapshot(p, true);
    for n in m.values_mut() {
        if n.mtime.is_some_and(|t| t >= since) {
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
        Tree::new(tmp.path().join(d))
            .file("src/x/extra/e", "e")
            .file("src/x/y/e2", "e2")
            .file("src/stray", "s")
            .same_mtimes(BASE_TIME + 800_000);
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
        Tree::new(tmp.path().join(d))
            .file("realdir/old", "old")
            .symlink("real", "realdir")
            .file("tgt", "x")
            .symlink("target", "tgt")
            .same_mtimes(BASE_TIME + 800_000);
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

/// In a pull the server removes the last files after the client's final message: their
/// `-vv` lines and errors must still reach the client, as rsync's do.
#[test]
fn remove_source_files_pull_reports_removals_like_rsync() {
    if skip_without_rsync() {
        return;
    }
    let removed = |o: &RunOut| -> Vec<String> {
        let mut v: Vec<String> = o.stdout.lines().filter(|l| l.starts_with("sender removed ")).map(str::to_string).collect();
        v.sort();
        v
    };
    let tmp = tempfile::tempdir().unwrap();
    let pull = ["-a", "-vv", "--remove-source-files", "{host}{dst}-src/", "{dst}"];
    for d in ["lr", "lf"] {
        removal_source(tmp.path(), &format!("{d}-src"));
    }
    let (r, f) = both_remote(tmp.path(), &pull, "lr", "lf").unwrap();
    assert_eq!((r.code, f.code), (0, 0), "rsync stderr: {}\nbarqsync stderr: {}", r.stderr, f.stderr);
    assert_eq!(removed(&r), removed(&f), "barqsync stdout:\n{}", f.stdout);
    assert!(removed(&f).contains(&"sender removed sub/f2".to_string()), "{}", f.stdout);
    assert_same_tree(&snap(&tmp.path().join("lr"), true), &snap(&tmp.path().join("lf"), true));

    // A removal that fails: reported, and the exit code says so (23).
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    for d in ["lr2", "lf2"] {
        removal_source(tmp.path(), &format!("{d}-src"));
        fs::set_permissions(tmp.path().join(format!("{d}-src/sub")), fs::Permissions::from_mode(0o555)).unwrap();
    }
    let (r, f) = both_remote(tmp.path(), &pull, "lr2", "lf2").unwrap();
    for d in ["lr2", "lf2"] {
        fs::set_permissions(tmp.path().join(format!("{d}-src/sub")), fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert_eq!((r.code, f.code), (23, 23), "rsync stderr: {}\nbarqsync stderr: {}", r.stderr, f.stderr);
    assert_eq!(removed(&r), removed(&f), "barqsync stdout:\n{}", f.stdout);
    assert!(f.stderr.contains("sub/f2"), "{}", f.stderr);
    assert!(tmp.path().join("lf2-src/sub/f2").exists());
}

// ---------------------------------------------------------------------------
// --no-OPTION
// ---------------------------------------------------------------------------

#[test]
fn no_options_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = Tree::new(tmp.path().join("src"));
    t.file("a", "a").file("sub/b", "b").symlink("lnk", "a").fifo("fifo").chmod("a", 0o640).chmod("sub", 0o750).settle_mtimes();
    let cases: &[&[&str]] = &[
        &["-ai", "--no-perms"],
        &["-ai", "--no-p", "--no-t"],
        &["-ai", "--no-o", "--no-g"],
        &["-ai", "--no-D"],
        &["-ai", "--no-l"],
        &["-ai", "--no-specials"],
        &["-ai", "--no-r"],
        &["-ai", "--no-recursive", "-d"],
        // What comes last wins: -a turns -t back on, -t after --no-times too.
        &["--no-t", "-ai"],
        &["-ai", "--no-times", "-t"],
        &["-a", "-ii", "--no-i", "-i", "-a"],
    ];
    for (i, flags) in cases.iter().enumerate() {
        let (dr, df) = (format!("r{i}"), format!("f{i}"));
        let mut a = flags.to_vec();
        a.extend(["src/", "{dst}"]);
        let (r, f) = both(tmp.path(), &a, &dr, &df).unwrap();
        same_output(&r, &f, &[(&dr, &df)], &format!("{flags:?}"));
        let (pr, pf) = (tmp.path().join(&dr), tmp.path().join(&df));
        assert_eq!(pr.exists(), pf.exists(), "{flags:?}");
        if pr.exists() {
            // Without -t, files get the time they were written.
            assert_same_tree(&without_mtimes(snap(&pr, false)), &without_mtimes(snap(&pf, false)));
        }
    }
    // An update: changed modes and times are left alone without -p and -t. sub/b's time
    // differs, so both rewrite it, and it and sub get the time of the copy; those times
    // are left out (from a second before the runs: file times can trail this clock).
    seed(tmp.path(), "src/", &["ur", "uf", "pr", "pf"]);
    t.chmod("a", 0o600).chmod("sub/b", 0o604).mtime("sub/b", BASE_TIME + 500);
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64 - 1;
    let (r, f) = both(tmp.path(), &["-ai", "--no-p", "--no-t", "src/", "{dst}"], "ur", "uf").unwrap();
    same_output(&r, &f, &[], "update --no-p --no-t");
    assert_same_tree(&snap_old_times(&tmp.path().join("ur"), since), &snap_old_times(&tmp.path().join("uf"), since));
    let (r, f) = both_remote(tmp.path(), &["-ai", "--no-p", "--no-o", "--no-g", "src/", "{host}{dst}"], "pr", "pf").unwrap();
    same_output(&r, &f, &[], "push --no-p --no-o --no-g");
    assert_same_tree(&snap(&tmp.path().join("pr"), true), &snap(&tmp.path().join("pf"), true));
}

// ---------------------------------------------------------------------------
// --delete-before/-during/-delay/-after, --max-delete
// ---------------------------------------------------------------------------

/// Extraneous entries in each destination: files and directories, nested, at the root
/// and below, named so that rsync's deletion order matters.
fn deletion_fixture(root: &Path, dsts: &[&str]) {
    Tree::new(root.join("src")).file("a", "a").file("keep/k", "k").file("keep/sub/s", "s").settle_mtimes();
    seed(root, "src/", dsts);
    for d in dsts {
        Tree::new(root.join(d))
            .file("z", "z")
            .file("y", "y")
            .file("old/a", "1")
            .file("old/s/b", "2")
            .file("old/s/c", "3")
            .dir("x")
            .file("old-x/q", "4")
            .file("keep/gone", "5")
            .file("keep/sub/gone2", "6")
            .file("keep/sub/deep/gone3", "7")
            .same_mtimes(BASE_TIME + 700_000);
    }
}

#[test]
fn delete_timing_options_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let flags = ["--delete-before", "--delete-during", "--del", "--delete-delay", "--delete-after"];
    let dsts: Vec<String> = (0..flags.len()).flat_map(|i| [format!("r{i}"), format!("f{i}"), format!("pr{i}"), format!("pf{i}")]).collect();
    deletion_fixture(tmp.path(), &dsts.iter().map(String::as_str).collect::<Vec<_>>());
    for (i, flag) in flags.iter().enumerate() {
        let (r, f) = both(tmp.path(), &["-ai", flag, "src/", "{dst}"], &format!("r{i}"), &format!("f{i}")).unwrap();
        same_output(&r, &f, &[], flag);
        assert_same_tree(&snap_no_dir_times(&tmp.path().join(format!("r{i}"))), &snap_no_dir_times(&tmp.path().join(format!("f{i}"))));
        let (r, f) = both_remote(tmp.path(), &["-ai", flag, "src/", "{host}{dst}"], &format!("pr{i}"), &format!("pf{i}")).unwrap();
        same_output(&r, &f, &[], &format!("push {flag}"));
        assert_same_tree(&snap_no_dir_times(&tmp.path().join(format!("pr{i}"))), &snap_no_dir_times(&tmp.path().join(format!("pf{i}"))));
    }
    // Two different times, and --delete without -r or -d, are refused.
    for bad in [&["-a", "--delete-before", "--delete-after"][..], &["-a", "--del", "--delete-after"], &["--delete"], &["-a", "--no-r", "--delete"]] {
        let mut a = bad.to_vec();
        a.extend(["src/", "{dst}"]);
        let (r, f) = both(tmp.path(), &a, "br", "bf").unwrap();
        assert_eq!((r.code, f.code), (1, 1), "{bad:?}\nrsync: {}\nbarqsync: {}", r.stderr, f.stderr);
    }
}

#[test]
fn max_delete_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let limits = ["0", "1", "2", "4", "-3", "100"];
    let dsts: Vec<String> = (0..limits.len()).flat_map(|i| [format!("r{i}"), format!("f{i}"), format!("pr{i}"), format!("pf{i}"), format!("lr{i}"), format!("lf{i}")]).collect();
    deletion_fixture(tmp.path(), &dsts.iter().map(String::as_str).collect::<Vec<_>>());
    for (i, n) in limits.iter().enumerate() {
        let max = format!("--max-delete={n}");
        let (r, f) = both(tmp.path(), &["-ai", "--delete", &max, "src/", "{dst}"], &format!("r{i}"), &format!("f{i}")).unwrap();
        same_output(&r, &f, &[], &max);
        assert_same_tree(&snap_no_dir_times(&tmp.path().join(format!("r{i}"))), &snap_no_dir_times(&tmp.path().join(format!("f{i}"))));
        if f.code == 25 {
            assert!(f.stderr.contains("Deletions stopped due to --max-delete limit"), "{}", f.stderr);
        }
        let (r, f) = both_remote(tmp.path(), &["-ai", "--delete", &max, "src/", "{host}{dst}"], &format!("pr{i}"), &format!("pf{i}")).unwrap();
        same_output(&r, &f, &[], &format!("push {max}"));
        assert_same_tree(&snap_no_dir_times(&tmp.path().join(format!("pr{i}"))), &snap_no_dir_times(&tmp.path().join(format!("pf{i}"))));
        let (r, f) = both_remote(tmp.path(), &["-ai", "--delete", &max, "{host}src/", "{dst}"], &format!("lr{i}"), &format!("lf{i}")).unwrap();
        same_output(&r, &f, &[], &format!("pull {max}"));
        assert_same_tree(&snap_no_dir_times(&tmp.path().join(format!("lr{i}"))), &snap_no_dir_times(&tmp.path().join(format!("lf{i}"))));
    }
    // Without --delete it changes nothing.
    let (r, f) = both(tmp.path(), &["-ai", "--max-delete=1", "src/", "{dst}"], "nr", "nf").unwrap();
    same_output(&r, &f, &[], "no --delete");
}

// ---------------------------------------------------------------------------
// -m / --prune-empty-dirs
// ---------------------------------------------------------------------------

fn prune_fixture(root: &Path) {
    Tree::new(root.join("src"))
        .dir("a/b/c")
        .file("d/e/f.txt", "f")
        .file("g/h.log", "h")
        .dir("only-dirs/x/y")
        .file("only-dirs/z/w.txt", "w")
        .file("top.txt", "t")
        .symlink("lnkdir/l", "../top.txt")
        .settle_mtimes();
}

#[test]
fn prune_empty_dirs_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    prune_fixture(tmp.path());
    let cases: &[&[&str]] = &[
        &["-aim"],
        &["-aim", "--exclude=*.log"],
        &["-aim", "-f", "+ */", "-f", "+ *.txt", "-f", "- *"],
        &["-aim", "--delete", "--exclude=*.log"],
        &["-aim", "--no-l"],
        // -t: without it both copies get the time of the copy, which can differ.
        &["-dtim"],
        &["-aimn"],
        // A protected directory stays, empty or not, and so do the ones leading to it.
        &["-aim", "-f", "P c/"],
        &["-aim", "-f", "P only-dirs/x/"],
    ];
    for (i, flags) in cases.iter().enumerate() {
        for (remote, dr, df) in [(0, format!("r{i}"), format!("f{i}")), (1, format!("pr{i}"), format!("pf{i}")), (2, format!("lr{i}"), format!("lf{i}"))] {
            // Destinations already holding some of what is pruned.
            for d in [&dr, &df] {
                Tree::new(tmp.path().join(d)).dir("a/b").file("g/old", "o").dir("only-dirs/x").same_mtimes(BASE_TIME + 900_000);
            }
            let mut a = flags.to_vec();
            let (src, dst) = match remote {
                0 => ("src/", "{dst}"),
                1 => ("src/", "{host}{dst}"),
                _ => ("{host}src/", "{dst}"),
            };
            a.extend([src, dst]);
            let (r, f) = if remote == 0 { both(tmp.path(), &a, &dr, &df) } else { both_remote(tmp.path(), &a, &dr, &df) }.unwrap();
            same_output(&r, &f, &[(&dr, &df)], &format!("remote={remote} {flags:?}"));
            assert_same_tree(&snap_no_dir_times(&tmp.path().join(&dr)), &snap_no_dir_times(&tmp.path().join(&df)));
        }
    }
}

/// `-m` with `--files-from`: what the list names and is missing is reported (23) even
/// when its directory is pruned, as rsync's sender reports it.
#[test]
fn prune_empty_dirs_reports_missing_listed_files_like_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("sub/f", "f").dir("other/empty").settle_mtimes();
    fs::write(tmp.path().join("gone"), "sub/missing\n").unwrap();
    fs::write(tmp.path().join("some"), "sub/missing\nsub/f\nother/empty/\n").unwrap();
    for (i, list) in ["gone", "some"].iter().enumerate() {
        for (remote, dr, df) in [(0, format!("r{i}"), format!("f{i}")), (1, format!("pr{i}"), format!("pf{i}")), (2, format!("lr{i}"), format!("lf{i}"))] {
            // rsync leaves out "created directory" after a file-list error: an existing DEST.
            Tree::new(tmp.path().join(&dr));
            Tree::new(tmp.path().join(&df));
            let from = format!("--files-from={list}");
            let a: Vec<&str> = match remote {
                0 => vec!["-ai", "-rm", &from, "src", "{dst}"],
                1 => vec!["-ai", "-rm", &from, "src", "{host}{dst}"],
                _ => vec!["-ai", "-rm", &from, "{host}src", "{dst}"],
            };
            let (r, f) = if remote == 0 { both(tmp.path(), &a, &dr, &df) } else { both_remote(tmp.path(), &a, &dr, &df) }.unwrap();
            same_output(&r, &f, &[(&dr, &df)], &format!("remote={remote} {list}"));
            assert!(f.stderr.contains("sub/missing"), "{}", f.stderr);
            assert_same_tree(&snap_no_dir_times(&tmp.path().join(&dr)), &snap_no_dir_times(&tmp.path().join(&df)));
        }
    }
}

// ---------------------------------------------------------------------------
// --mkpath, -s
// ---------------------------------------------------------------------------

/// The reference rsync's version, as (3, 5, 1).
fn rsync_version() -> (u32, u32, u32) {
    let out = std::process::Command::new(reference_rsync().unwrap()).arg("--version").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut n = text.split_whitespace().nth(2).unwrap_or("").split('.').map(|p| p.parse().unwrap_or(0));
    (n.next().unwrap_or(0), n.next().unwrap_or(0), n.next().unwrap_or(0))
}

#[test]
fn mkpath_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("f", "f").file("sub/g", "g").settle_mtimes();
    let cases: &[&[&str]] = &[
        &["-ai", "--mkpath", "src/", "{dst}/b/c"],
        &["-ai", "--mkpath", "src/", "{dst}/b/c/"],
        &["-ai", "--mkpath", "src/f", "{dst}/y/z"],
        &["-ai", "--mkpath", "src/f", "{dst}/y/"],
        &["-ai", "--mkpath", "src/", "{dst}"],
        &["-ain", "--mkpath", "src/", "{dst}/b/c"],
        &["-ain", "--mkpath", "src/f", "{dst}/y/z"],
        &["-a", "--mkpath", "src/", "{dst}/q"],
        // Without --mkpath the missing parent is an error (11).
        &["-ai", "src/", "{dst}/b/c"],
    ];
    // rsync fixed --mkpath in dry runs in 3.4.4 (its #880); older ones are no reference.
    let dry_ok = rsync_version() >= (3, 4, 4);
    for (i, args) in cases.iter().enumerate() {
        if args.contains(&"-ain") && !dry_ok {
            continue;
        }
        let (dr, df) = (format!("r{i}"), format!("f{i}"));
        let (r, f) = both(tmp.path(), args, &dr, &df).unwrap();
        same_output(&r, &f, &[(&dr, &df)], &format!("{args:?}"));
        let (pr, pf) = (tmp.path().join(&dr), tmp.path().join(&df));
        assert_eq!(pr.exists(), pf.exists(), "{args:?}");
        if pr.exists() {
            assert_same_tree(&snap_no_dir_times(&pr), &snap_no_dir_times(&pf));
        }
    }
    for (i, args) in [&["-ai", "--mkpath", "src/", "{host}{dst}/b/c"][..], &["-ai", "--mkpath", "src/f", "{host}{dst}/y/z"], &["-ai", "--mkpath", "{host}src/", "{dst}/b/c"]].iter().enumerate() {
        let (dr, df) = (format!("pr{i}"), format!("pf{i}"));
        let (r, f) = both_remote(tmp.path(), args, &dr, &df).unwrap();
        same_output(&r, &f, &[(&dr, &df)], &format!("{args:?}"));
        assert_same_tree(&snap_no_dir_times(&tmp.path().join(&dr)), &snap_no_dir_times(&tmp.path().join(&df)));
    }
}

#[test]
fn secluded_args_are_accepted() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src dir")).file("a b", "x").file("c", "y").settle_mtimes();
    for flag in ["-s", "--protect-args", "--secluded-args"] {
        let (dr, df) = (format!("r{flag}"), format!("f{flag}"));
        let (r, f) = both_remote(tmp.path(), &["-ai", flag, "{host}src dir/", "{dst} x"], &dr, &df).unwrap();
        same_output(&r, &f, &[], flag);
        assert_same_tree(&snap(&tmp.path().join(format!("{dr} x")), true), &snap(&tmp.path().join(format!("{df} x")), true));
    }
}

// ---------------------------------------------------------------------------
// --filter / -f, -F, -C
// ---------------------------------------------------------------------------

fn filter_fixture(root: &Path) {
    Tree::new(root.join("src"))
        .file("a.txt", "a")
        .file("b.tmp", "b")
        .file("keep.tmp", "k")
        .file("x.o", "o")
        .file("core", "c")
        .file("notes~", "n")
        .file("anch", "1")
        .file(".rsync-filter", "- *.tmp\n+ keep.tmp\n")
        .file("sub/anch", "2")
        .file("sub/c.tmp", "c")
        .file("sub/d.txt", "d")
        .file("sub/e.log", "e")
        .file("sub/.rsync-filter", "- /anch\n- *.log\n")
        .file("deep/x/.cvsignore", "ignored *.dat\n")
        .file("deep/x/ignored", "i")
        .file("deep/x/y.dat", "y")
        .file("deep/x/kept", "k")
        .file("deep/.git/config", "g")
        .settle_mtimes();
    // Order matters: the first match wins, and `+ keep.tmp` comes after `- *.tmp`.
    fs::write(root.join("rules"), "# merged\n- *.o\n+ sub/\n+ sub/d.txt\n- sub/*\n").unwrap();
}

#[test]
fn filter_rules_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    filter_fixture(tmp.path());
    let cases: &[&[&str]] = &[
        &["-f", "- *.tmp"],
        &["-f", "-_*.tmp", "-f", "exclude *.o"],
        &["--filter=+ keep.tmp", "--filter=- *.tmp"],
        &["-f", "- *.tmp", "-f", "!", "-f", "- *.o"],
        &["-f", ". rules"],
        &["-f", "merge,- rules-ex"],
        &["-f", ": .rsync-filter"],
        &["-f", "dir-merge,n .rsync-filter"],
        &["-f", ":e .rsync-filter"],
        &["-F"],
        &["-FF"],
        &["-C"],
        &["-f", ":C"],
        &["-f", "-C"],
        &["-f", "-! */"],
        &["-f", "+ */", "-f", "+ *.txt", "-f", "- *"],
        &["-f", "H *.o", "-f", "S *"],
        &["--exclude=+ a.txt", "--exclude=*.txt"],
        &["--include=- *.o", "--exclude=*.tmp"],
    ];
    fs::write(tmp.path().join("rules-ex"), "*.o\ncore\n").unwrap();
    for (i, flags) in cases.iter().enumerate() {
        let (dr, df) = (format!("r{i}"), format!("f{i}"));
        let mut a = vec!["-ai"];
        a.extend_from_slice(flags);
        a.extend(["src/", "{dst}"]);
        let (r, f) = both(tmp.path(), &a, &dr, &df).unwrap();
        same_output(&r, &f, &[(&dr, &df)], &format!("{flags:?}"));
        assert_same_tree(&snap(&tmp.path().join(&dr), true), &snap(&tmp.path().join(&df), true));
    }
    // Over the wire, where the sender applies them (and the receiver protects).
    for (i, flags) in [&["-F"][..], &["-FF", "-C"], &["-f", ". rules"], &["-f", "- *.tmp", "-f", "P *.keep"]].iter().enumerate() {
        for (pull, dr, df) in [(false, format!("pr{i}"), format!("pf{i}")), (true, format!("lr{i}"), format!("lf{i}"))] {
            let mut a = vec!["-ai"];
            a.extend_from_slice(flags);
            a.extend(if pull { ["{host}src/", "{dst}"] } else { ["src/", "{host}{dst}"] });
            let (r, f) = both_remote(tmp.path(), &a, &dr, &df).unwrap();
            same_output(&r, &f, &[(&dr, &df)], &format!("pull={pull} {flags:?}"));
            assert_same_tree(&snap(&tmp.path().join(&dr), true), &snap(&tmp.path().join(&df), true));
        }
    }
    // Rules that can't be parsed are refused before anything is done (1), rules files
    // that can't be read too, as file I/O errors (11).
    for bad in ["bogus", "- ", ": ", "+,q x"] {
        let (r, f) = both(tmp.path(), &["-a", "-f", bad, "src/", "{dst}"], "br", "bf").unwrap();
        assert_eq!((r.code, f.code), (1, 1), "{bad:?}\nrsync: {}\nbarqsync: {}", r.stderr, f.stderr);
    }
    for bad in [&["-f", "merge nonexistent"][..], &["--exclude-from=nonexistent"], &["--include-from", "nonexistent"]] {
        let mut a = vec!["-a"];
        a.extend_from_slice(bad);
        a.extend(["src/", "{dst}"]);
        let (r, f) = both(tmp.path(), &a, "br", "bf").unwrap();
        assert_eq!((r.code, f.code), (11, 11), "{bad:?}\nrsync: {}\nbarqsync: {}", r.stderr, f.stderr);
    }
}

/// `--from0` also applies to per-directory filter files. (rsync tells its server only
/// with `--files-from`, so a pull's sender would read them line by line there; barqsync
/// sends it along, which a pull checks on its own.)
#[test]
fn from0_applies_to_per_directory_filter_files() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src"))
        .file(".rsync-filter", "- secret\0+ keep.tmp\0- *.tmp\0")
        .file("secret", "s")
        .file("keep.tmp", "k")
        .file("drop.tmp", "d")
        .file("sub/.rsync-filter", "- /x\0")
        .file("sub/x", "x")
        .file("sub/y", "y")
        .settle_mtimes();
    let (r, f) = both(tmp.path(), &["-ai", "-0", "-F", "src/", "{dst}"], "dr", "df").unwrap();
    same_output(&r, &f, &[], "local");
    assert_same_tree(&snap(&tmp.path().join("dr"), true), &snap(&tmp.path().join("df"), true));
    assert!(!tmp.path().join("df/secret").exists() && tmp.path().join("df/keep.tmp").exists());
    let (r, f) = both_remote(tmp.path(), &["-ai", "-0", "-F", "src/", "{host}{dst}"], "pr", "pf").unwrap();
    same_output(&r, &f, &[], "push");
    assert_same_tree(&snap(&tmp.path().join("pr"), true), &snap(&tmp.path().join("pf"), true));
    let f = barqsync_remote(tmp.path(), &["-a", "-0", "-F", "{host}src/", "lf"]);
    assert_eq!(f.code, 0, "{}", f.stderr);
    assert_same_tree(&snap(&tmp.path().join("df"), true), &snap(&tmp.path().join("lf"), true));
}

#[test]
fn filter_rules_protect_from_delete_like_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    filter_fixture(tmp.path());
    let cases: &[&[&str]] = &[
        &["--delete", "-f", "- *.tmp"],
        &["--delete", "-f", "P *.keep", "-f", "- *.tmp"],
        &["--delete", "--delete-excluded", "-f", "P *.keep", "-f", "- *.tmp"],
        &["--delete", "-f", "-p *.tmp"],
        &["--delete", "-f", "R stale.keep", "-f", "P *.keep"],
        &["--delete", "-F"],
        &["--delete-after", "-FF"],
        &["--delete", "-C"],
        &["--delete", "--exclude=*.tmp", "--max-delete=2"],
        // Backups (`*~`) are protected, also inside a directory being deleted.
        &["--delete", "-b"],
    ];
    for (i, flags) in cases.iter().enumerate() {
        for (remote, dr, df) in [(0, format!("r{i}"), format!("f{i}")), (1, format!("pr{i}"), format!("pf{i}")), (2, format!("lr{i}"), format!("lf{i}"))] {
            for d in [&dr, &df] {
                Tree::new(tmp.path().join(d))
                    .file("stale.keep", "k")
                    .file("stale.tmp", "t")
                    .file("old/k.keep", "k")
                    .file("old/t.tmp", "t")
                    .file("old/x", "x")
                    .file("old/core", "c")
                    .file("old/in/deeper.tmp", "d")
                    .file("old/in/bak~", "b")
                    .file("old2/only.tmp", "t")
                    .file("sub/stale.log", "l")
                    .file("sub/.rsync-filter", "- *.keep\n")
                    .same_mtimes(BASE_TIME + 900_000);
            }
            let mut a = vec!["-ai"];
            a.extend_from_slice(flags);
            a.extend(match remote {
                0 => ["src/", "{dst}"],
                1 => ["src/", "{host}{dst}"],
                _ => ["{host}src/", "{dst}"],
            });
            let (r, f) = if remote == 0 { both(tmp.path(), &a, &dr, &df) } else { both_remote(tmp.path(), &a, &dr, &df) }.unwrap();
            same_output(&r, &f, &[(&dr, &df)], &format!("remote={remote} {flags:?}"));
            assert_same_tree(&snap_no_dir_times(&tmp.path().join(&dr)), &snap_no_dir_times(&tmp.path().join(&df)));
        }
    }
}

// ---------------------------------------------------------------------------
// Vanished files, --timeout, --stats
// ---------------------------------------------------------------------------

/// barqsync with extra environment; `{host}` is the fake ssh's host, as in
/// [`barqsync_remote`].
fn barqsync_env(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> RunOut {
    let shim = fake_ssh(cwd);
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_barqsync"));
    cmd.env("LC_ALL", TEST_LOCALE).arg("-e").arg(&shim).arg(format!("--remote-path={}", env!("CARGO_BIN_EXE_barqsync")));
    cmd.args(args.iter().map(|a| a.replace("{host}", "fakehost:")));
    for (k, v) in env {
        cmd.env(k, v);
    }
    run(&mut cmd, cwd)
}

/// A file that disappears between the scan and its transfer is a warning and exit 24,
/// as rsync's "file has vanished"; an error still makes it 23. (rsync can't be raced
/// reliably, so a test hook removes the file just before it is opened.)
#[test]
fn vanished_files_exit_24() {
    let tmp = tempfile::tempdir().unwrap();
    for (i, (src, dst)) in [("src/", "d0"), ("src/", "{host}d1"), ("{host}src/", "d2")].iter().enumerate() {
        Tree::new(tmp.path().join("src")).file("a", "a").file("gone", "g").file("big", vec![1u8; 300_000]).settle_mtimes();
        Tree::new(tmp.path().join(format!("d{i}"))).file("extra", "e");
        let f = barqsync_env(tmp.path(), &["-a", "--delete", src, dst], &[("BARQSYNC_TEST_VANISH", "gone")]);
        assert_eq!(f.code, 24, "{src} {dst}\n{}", f.stderr);
        assert!(f.stderr.contains("file has vanished: \"") && f.stderr.contains("src/gone\""), "{}", f.stderr);
        let d = tmp.path().join(format!("d{i}"));
        assert!(d.join("a").exists() && d.join("big").exists() && !d.join("gone").exists());
        assert!(!d.join("extra").exists(), "a vanished file doesn't stop deletions");
        // A large file is sent another way (on its own, not packed with small ones).
        Tree::new(tmp.path().join("src")).file("gone", "g").file("big", vec![2u8; 300_000]);
        let f = barqsync_env(tmp.path(), &["-a", src, dst], &[("BARQSYNC_TEST_VANISH", "big")]);
        assert_eq!(f.code, 24, "{src} {dst}\n{}", f.stderr);
        assert!(d.join("gone").exists() && fs::read(d.join("big")).unwrap()[0] == 1);
    }
    // A real error on top: 23.
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", "a").file("gone", "g").settle_mtimes();
    let f = barqsync_env(tmp.path(), &["-a", "src/", "src/nonexistent", "d"], &[("BARQSYNC_TEST_VANISH", "gone")]);
    assert_eq!(f.code, 23, "{}", f.stderr);
}

/// A remote shell that forwards `CUT_BYTES` bytes of the server's output, then nothing,
/// keeping the connection open: a stalled link.
fn stall_ssh(dir: &Path) -> std::path::PathBuf {
    let p = dir.join("stall-ssh");
    fs::write(
        &p,
        r#"#!/bin/sh
shift
fifo=$(mktemp -u "${TMPDIR:-/tmp}/stall.XXXXXX")
mkfifo "$fifo"
exec 3<&0
/bin/sh -c "exec $*" <&3 >"$fifo" &
pid=$!
exec 3<&-
perl -e '
  $n = shift;
  while ($n > 0 && ($r = sysread(STDIN, $buf, $n < 65536 ? $n : 65536)) > 0) { $n -= $r; syswrite(STDOUT, $buf, $r) }
  sleep 15' "${CUT_BYTES:-4000}" <"$fifo"
kill -9 $pid 2>/dev/null
rm -f "$fifo"
"#,
    )
    .unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// Run `cmd`, killing it after `limit`; returns its exit code, stderr and how long it
/// took. Stderr goes to a file: what the remote shell left running may hold it open.
fn run_limited(cmd: &mut std::process::Command, limit: std::time::Duration) -> (i32, String, std::time::Duration) {
    let errs = tempfile::NamedTempFile::new().unwrap();
    let start = std::time::Instant::now();
    let mut child = cmd.stdout(std::process::Stdio::null()).stderr(errs.reopen().unwrap()).spawn().unwrap();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let took = start.elapsed();
            return (status.code().unwrap_or(-1), fs::read_to_string(errs.path()).unwrap(), took);
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return (-1, "killed".into(), start.elapsed());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn timeout_ends_a_stalled_transfer_with_30() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("big", noise(4 << 20, 3)).file("small", "s").settle_mtimes();
    let deaf = tmp.path().join("deaf-ssh");
    // Keeps its stdout open and says nothing.
    fs::write(&deaf, "#!/bin/sh\ncat >/dev/null\n").unwrap();
    fs::set_permissions(&deaf, fs::Permissions::from_mode(0o755)).unwrap();
    let stall = stall_ssh(tmp.path());
    let limit = std::time::Duration::from_secs(20);
    let rsync_bin = reference_rsync().unwrap();
    let barq = |shim: &Path, src: &str, dst: &str| {
        let mut f = std::process::Command::new(env!("CARGO_BIN_EXE_barqsync"));
        f.current_dir(tmp.path()).env("BARQSYNC_TRANSPORT", "ssh").args(["-a", "--timeout=2", "-e"]).arg(shim);
        f.arg(format!("--remote-path={}", env!("CARGO_BIN_EXE_barqsync"))).args([src, dst]);
        run_limited(&mut f, limit)
    };
    // A remote side that never answers.
    let mut r = std::process::Command::new(&rsync_bin);
    r.current_dir(tmp.path()).args(["-a", "--timeout=2", "-e"]).arg(&deaf).arg(format!("--rsync-path={}", rsync_bin.display()));
    let (rc, rerr, _) = run_limited(r.args(["src/", "fakehost:xr"]), limit);
    let (fc, ferr, took) = barq(&deaf, "src/", "fakehost:xf");
    assert_eq!((rc, fc), (30, 30), "rsync: {rerr}\nbarqsync: {ferr}");
    assert!(ferr.contains("io timeout after 2 seconds"), "{ferr}");
    assert!(took < std::time::Duration::from_secs(10), "took {took:?}");
    // A pull whose data stops arriving midway. (rsync 3.5.1 doesn't notice this one: its
    // receiving process leaves timeouts to the generator, which still hears from it.)
    let (fc, ferr, took) = barq(&stall, "fakehost:src/", "y");
    assert_eq!(fc, 30, "{ferr}");
    assert!(ferr.contains("io timeout after"), "{ferr}");
    assert!(took < std::time::Duration::from_secs(10), "took {took:?}");
    // A healthy transfer is not cut short. (A longer timeout than above: under load, the
    // remote side alone can take a second to start.)
    let f = barqsync_remote(tmp.path(), &["-a", "--timeout=5", "src/", "{host}ok"]);
    assert_eq!(f.code, 0, "{}", f.stderr);
}

#[test]
fn stats_print_rsync_byte_counts() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", vec![5u8; 200_000]).file("b", "bb").settle_mtimes();
    let pats = [
        r"^Total bytes sent: [0-9,]+$",
        r"^Total bytes received: [0-9,]+$",
        r"^sent [0-9,]+ bytes  received [0-9,]+ bytes  [0-9,]+\.[0-9]{2} bytes/sec$",
        r"^total size is [0-9,]+  speedup is [0-9,]+\.[0-9]{2}$",
    ];
    let lines = |out: &str, pat: &str| -> Vec<String> {
        let re = regex_lite(pat);
        out.lines().filter(|l| re(l)).map(str::to_string).collect()
    };
    let num = |line: &str, after: &str| -> u64 {
        let rest = &line[line.find(after).unwrap() + after.len()..];
        rest.chars().take_while(|c| c.is_ascii_digit() || *c == ',').filter(char::is_ascii_digit).collect::<String>().parse().unwrap()
    };
    let (r, f) = both(tmp.path(), &["-a", "--stats", "src/", "{dst}"], "dr", "df").unwrap();
    let (pr, pf) = both_remote(tmp.path(), &["-a", "--stats", "src/", "{host}{dst}"], "pr", "pf").unwrap();
    for (out, what) in [(&r, "rsync"), (&f, "local"), (&pr, "rsync push"), (&pf, "push")] {
        for pat in pats {
            assert_eq!(lines(&out.stdout, pat).len(), 1, "{what}: {pat}\n{}", out.stdout);
        }
    }
    // A push sends the data, and some protocol around it.
    let sent = num(&lines(&pf.stdout, pats[0])[0], "sent: ");
    let received = num(&lines(&pf.stdout, pats[1])[0], "received: ");
    assert!(sent > 200_000 && received > 0, "{}", pf.stdout);
    // -v alone prints the two summary lines, a dry run says so.
    let (r, f) = both(tmp.path(), &["-avn", "src/", "{dst}"], "vr", "vf").unwrap();
    for out in [&r, &f] {
        assert_eq!(lines(&out.stdout, pats[2]).len(), 1, "{}", out.stdout);
        assert!(out.stdout.lines().any(|l| l.starts_with("total size is ") && l.ends_with(" (DRY RUN)")), "{}", out.stdout);
        assert!(lines(&out.stdout, pats[0]).is_empty());
    }
}

/// A tiny matcher for the patterns above: `^...$` with `[0-9,]+`, `[0-9]{2}`, `\.`.
fn regex_lite(pat: &str) -> impl Fn(&str) -> bool + '_ {
    move |s: &str| {
        fn m(p: &[u8], t: &[u8]) -> bool {
            if p.is_empty() || p == b"$" {
                return t.is_empty();
            }
            if let Some(rest) = p.strip_prefix(b"[0-9,]+") {
                let n = t.iter().take_while(|c| c.is_ascii_digit() || **c == b',').count();
                return (1..=n).rev().any(|k| m(rest, &t[k..]));
            }
            if let Some(rest) = p.strip_prefix(b"[0-9]{2}") {
                return t.len() >= 2 && t[..2].iter().all(u8::is_ascii_digit) && m(rest, &t[2..]);
            }
            if let Some(rest) = p.strip_prefix(b"\\.") {
                return t.first() == Some(&b'.') && m(rest, &t[1..]);
            }
            t.first() == p.first() && m(&p[1..], &t[1..])
        }
        m(pat.strip_prefix('^').unwrap_or(pat).as_bytes(), s.as_bytes())
    }
}

// ---------------------------------------------------------------------------
// --bwlimit
// ---------------------------------------------------------------------------

/// The first line of a tool's stderr without its `rsync: `/`barqsync: ` prefix (both say
/// the same after it).
fn message(stderr: &str) -> &str {
    let line = stderr.lines().next().unwrap_or("");
    line.strip_prefix("rsync: ").or_else(|| line.strip_prefix("barqsync: ")).unwrap_or(line)
}

/// rsync and barqsync with the same arguments, one after the other, each with how long
/// it took. `{dst}` is the per-tool name, `{host}` the fake ssh's host.
fn both_timed(cwd: &Path, args: &[&str], dst_r: &str, dst_f: &str) -> ((RunOut, std::time::Duration), (RunOut, std::time::Duration)) {
    let rsync_bin = reference_rsync().unwrap();
    let shim = fake_ssh(cwd);
    let sub = |d: &str| -> Vec<String> { args.iter().map(|a| a.replace("{dst}", d).replace("{host}", "fakehost:")).collect() };
    let mut r = std::process::Command::new(&rsync_bin);
    r.env("LC_ALL", TEST_LOCALE).arg("-e").arg(&shim).arg(format!("--rsync-path={}", rsync_bin.display())).args(sub(dst_r));
    let mut f = std::process::Command::new(env!("CARGO_BIN_EXE_barqsync"));
    f.env("LC_ALL", TEST_LOCALE).arg("-e").arg(&shim).arg(format!("--remote-path={}", env!("CARGO_BIN_EXE_barqsync")));
    f.env("BARQSYNC_TRANSPORT", "ssh").args(sub(dst_f));
    let start = std::time::Instant::now();
    let ro = run(&mut r, cwd);
    let rt = start.elapsed();
    let start = std::time::Instant::now();
    let fo = run(&mut f, cwd);
    ((ro, rt), (fo, start.elapsed()))
}

#[test]
fn bwlimit_paces_transfers_like_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // Incompressible data, so -z doesn't shrink what is paced.
    Tree::new(tmp.path().join("src")).file("big", noise(512 << 10, 7)).file("small", "s").settle_mtimes();
    // 512 KiB at 512 KiB/s is about a second, of which both let a tenth go unpaced.
    let least = std::time::Duration::from_millis(700);
    for (i, (src, dst)) in [("src/", "{dst}"), ("src/", "{host}{dst}"), ("{host}src/", "{dst}")].into_iter().enumerate() {
        let (dr, df) = (format!("r{i}"), format!("f{i}"));
        let ((r, rt), (f, ft)) = both_timed(tmp.path(), &["-aiz", "--bwlimit=512", src, dst], &dr, &df);
        same_output(&r, &f, &[], &format!("{src} {dst}"));
        assert!(rt >= least && ft >= least, "{src} {dst}: rsync took {rt:?}, barqsync {ft:?}");
        assert_same_tree(&snap(&tmp.path().join(&dr), true), &snap(&tmp.path().join(&df), true));
    }
    // A frame that takes longer than --timeout to arrive at this rate still moves, so it
    // is no timeout: 96 KiB at 32 KiB/s, with one second allowed between bytes. Through
    // both I/O layers.
    Tree::new(tmp.path().join("slow")).file("f", noise(96 << 10, 8)).settle_mtimes();
    for io in ["threads", "tokio"] {
        for (i, (src, dst)) in [("slow/", "{host}t1"), ("{host}slow/", "t2")].into_iter().enumerate() {
            let args = ["-a", "--bwlimit=32", "--timeout=1", src, dst];
            let f = barqsync_env(tmp.path(), &args, &[("BARQSYNC_NET_IO", io), ("BARQSYNC_TRANSPORT", "ssh")]);
            assert_eq!(f.code, 0, "{io} {src} {dst}: {}", f.stderr);
            assert_eq!(fs::read(tmp.path().join(format!("t{}/f", i + 1))).unwrap(), noise(96 << 10, 8));
            let _ = fs::remove_dir_all(tmp.path().join(format!("t{}", i + 1)));
        }
    }
    // rsync's size syntax, in KiB by default; 0 is no limit.
    let (r, f) = both(tmp.path(), &["-ai", "--bwlimit=0", "src/", "{dst}"], "zr", "zf").unwrap();
    same_output(&r, &f, &[], "--bwlimit=0");
    // (A negative rate is "too large" for rsync 3.5, "invalid" for 3.2: not compared.)
    for bad in ["--bwlimit=100b", "--bwlimit=511b", "--bwlimit=x", "--bwlimit=1q"] {
        let (r, f) = both(tmp.path(), &["-a", bad, "src/", "{dst}"], "er", "ef").unwrap();
        assert_eq!((r.code, f.code), (1, 1), "{bad}: {}", f.stderr);
        assert_eq!(message(&r.stderr), message(&f.stderr), "{bad}");
    }
}

// ---------------------------------------------------------------------------
// --chown, --usermap, --groupmap
// ---------------------------------------------------------------------------

fn group_name(gid: u32) -> Option<String> {
    let gr = unsafe { libc::getgrgid(gid) };
    (!gr.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr((*gr).gr_name) }.to_string_lossy().into_owned())
}

/// A group other than `not` that this user may give files (one `chown` accepts), and its
/// name.
fn another_group(dir: &Path, not: u32) -> Option<(u32, String)> {
    let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    let mut groups = vec![0 as libc::gid_t; n.max(0) as usize];
    let n = unsafe { libc::getgroups(n, groups.as_mut_ptr()) };
    groups.truncate(n.max(0) as usize);
    let probe = dir.join("group-probe");
    fs::write(&probe, "").unwrap();
    let found = groups.into_iter().filter(|&g| g != not).find_map(|g| {
        let name = group_name(g)?;
        std::os::unix::fs::lchown(&probe, None, Some(g)).ok().map(|()| (g, name))
    });
    fs::remove_file(probe).unwrap();
    found
}

#[test]
fn id_maps_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = Tree::new(tmp.path().join("src"));
    t.file("a", "a").file("sub/b", "b").symlink("lnk", "a").settle_mtimes();
    let src_gid = fs::metadata(t.path("a")).unwrap().gid();
    let (Some(src_group), Some((gid, group))) = (group_name(src_gid), another_group(tmp.path(), src_gid)) else {
        eprintln!("SKIP: this user is in no second group to map to");
        return;
    };
    let me = unsafe { std::ffi::CStr::from_ptr((*libc::getpwuid(libc::getuid())).pw_name) }.to_string_lossy().into_owned();
    let cases: Vec<Vec<String>> = vec![
        vec!["-ai".into(), format!("--chown=:{group}")],
        vec!["-ai".into(), format!("--chown={me}:{group}")],
        vec!["-ai".into(), format!("--groupmap=*:{group}")],
        vec!["-ai".into(), format!("--groupmap={src_group}:{group}")],
        vec!["-ai".into(), format!("--groupmap={src_gid}:{gid}")],
        vec!["-ai".into(), format!("--groupmap=0-{}:{group}", src_gid.max(1))],
        vec!["-ai".into(), format!("--groupmap=no-such-group-x:{src_group},*:{group}")],
        vec!["-ai".into(), format!("--groupmap=*:{group},{src_group}:{src_gid}")],
        // No names travel with --numeric-ids: only numbers and an empty or wildcard name match.
        vec!["-ai".into(), "--numeric-ids".into(), format!("--groupmap={src_group}:{group}")],
        vec!["-ai".into(), "--numeric-ids".into(), format!("--groupmap=:{group}")],
        // A name the receiver doesn't know is reported and its pair left out.
        vec!["-ai".into(), "--groupmap=*:no-such-group-barqsync".into()],
        // --chown implies -g where it is given (a later --no-g turns that off again: see below).
        vec!["-ri".into(), format!("--chown=:{group}")],
        vec!["-ai".into(), "--no-g".into(), format!("--chown=:{group}")],
        // Without root, owners stay as they are.
        vec!["-ai".into(), format!("--usermap=*:{me}")],
    ];
    for (i, flags) in cases.iter().enumerate() {
        for (j, (src, dst)) in [("src/", "{dst}"), ("src/", "{host}{dst}"), ("{host}src/", "{dst}")].into_iter().enumerate() {
            let (dr, df) = (format!("r{i}-{j}"), format!("f{i}-{j}"));
            let mut a: Vec<&str> = flags.iter().map(String::as_str).collect();
            a.extend([src, dst]);
            let (r, f) = both_remote(tmp.path(), &a, &dr, &df).unwrap();
            same_output(&r, &f, &[(&dr, &df)], &format!("{flags:?} {src} {dst}"));
            assert_eq!(r.stderr, f.stderr, "{flags:?} {src} {dst}");
            assert_eq!(gids(&tmp.path().join(&dr)), gids(&tmp.path().join(&df)), "{flags:?} {src} {dst}");
        }
    }
    // A --no-g after --chown wins. (Only locally: rsync's server turns -g back on when it
    // is given the map, while the client sends no groups, and it fails to chgrp.)
    let no_g = format!("--chown=:{group}");
    let (r, f) = both(tmp.path(), &["-ai", &no_g, "--no-g", "src/", "{dst}"], "nr", "nf").unwrap();
    same_output(&r, &f, &[("nr", "nf")], "--chown --no-g");
    assert_eq!(gids(&tmp.path().join("nr")), gids(&tmp.path().join("nf")));
    assert_eq!(fs::metadata(tmp.path().join("nf/a")).unwrap().gid(), src_gid);
    // An update changes the group of what is there (`.f.....g...`).
    for (j, (src, dst)) in [("src/", "{dst}"), ("src/", "{host}{dst}"), ("{host}src/", "{dst}")].into_iter().enumerate() {
        let (dr, df) = (format!("ur{j}"), format!("uf{j}"));
        seed(tmp.path(), "src/", &[&dr, &df]);
        let chown = format!("--chown=:{group}");
        let (r, f) = both_remote(tmp.path(), &["-ai", &chown, src, dst], &dr, &df).unwrap();
        same_output(&r, &f, &[], &format!("update {src} {dst}"));
        assert!(f.stdout.contains(".f.....g... a"), "{}", f.stdout);
        assert_eq!(gids(&tmp.path().join(&dr)), gids(&tmp.path().join(&df)));
        assert_eq!(fs::metadata(tmp.path().join(&df).join("sub/b")).unwrap().gid(), gid);
    }
    // rsync's errors: each map once, --chown not mixed with them, and the maps' syntax.
    let bad: &[&[&str]] = &[
        &["--chown=a:b", "--usermap=x:y"],
        &["--usermap=x:y", "--chown=a:b"],
        &["--groupmap=x:y", "--chown=:b"],
        &["--usermap=x:y", "--usermap=z:w"],
        &["--chown=a:b", "--chown=c:d"],
        &["--chown=:b", "--chown=:d"],
        &["--usermap=x"],
        &["--usermap=x:"],
        &["--groupmap=1a:x"],
        &["--groupmap=1-:x"],
    ];
    for flags in bad {
        let mut a = vec!["-a"];
        a.extend(*flags);
        a.extend(["src/", "{dst}"]);
        let (r, f) = both(tmp.path(), &a, "er", "ef").unwrap();
        assert_eq!((r.code, f.code), (1, 1), "{flags:?}: {}", f.stderr);
        assert_eq!(message(&r.stderr), message(&f.stderr), "{flags:?}");
    }
}

// ---------------------------------------------------------------------------
// --append, --append-verify
// ---------------------------------------------------------------------------

/// Sources and destinations for `--append`: a file that grew, one unchanged, one that is
/// shorter and one of the same size but newer at the source, a new one, one whose start
/// differs (only `--append-verify` notices), and a big one that grew.
fn append_fixture(root: &Path, dsts: &[&str]) {
    let big = noise(300_000, 9);
    let src = Tree::new(root.join("src"));
    src.file("grown", "0123456789abcdef")
        .file("same", "same")
        .file("shrunk", "shrunk")
        .file("samesize", "SAMESIZE")
        .file("new", "new")
        .file("mismatch", "XXXXmore")
        .file("sub/big", &big)
        .chmod("grown", 0o600)
        .settle_mtimes();
    let same = fs::metadata(src.path("same")).unwrap().mtime() as u64;
    for d in dsts {
        Tree::new(root.join(d))
            .file("grown", "0123")
            .file("same", "same")
            .file("shrunk", "shrunk, and longer")
            .file("samesize", "samesiz2")
            .file("mismatch", "YYYY")
            .file("sub/big", &big[..100_000])
            .same_mtimes(BASE_TIME + 100_000)
            .mtime("same", same);
    }
}

#[test]
fn append_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    for (i, mode) in ["--append", "--append-verify"].into_iter().enumerate() {
        for (j, (src, dst)) in [("src/", "{dst}"), ("src/", "{host}{dst}"), ("{host}src/", "{dst}")].into_iter().enumerate() {
            let (dr, df) = (format!("r{i}-{j}"), format!("f{i}-{j}"));
            append_fixture(tmp.path(), &[&dr, &df]);
            let (r, f) = both_remote(tmp.path(), &["-ai", mode, src, dst], &dr, &df).unwrap();
            let what = format!("{mode} {src} {dst}");
            same_output(&r, &f, &[], &what);
            // --append-verify: "WARNING: mismatch failed verification -- update retained (will try again)."
            let warnings = |o: &RunOut| o.stderr.lines().map(str::to_string).collect::<Vec<_>>();
            assert_eq!(warnings(&r), warnings(&f), "{what}");
            assert_same_tree(&snap(&tmp.path().join(&dr), true), &snap(&tmp.path().join(&df), true));
            // What --append keeps: shorter files are left alone, attributes too.
            let d = tmp.path().join(&df);
            assert_eq!(fs::read(d.join("shrunk")).unwrap(), b"shrunk, and longer", "{what}");
            assert_eq!(fs::metadata(d.join("samesize")).unwrap().mtime() as u64, BASE_TIME + 100_000, "{what}");
            let mismatch: &[u8] = if mode == "--append" { b"YYYYmore" } else { b"XXXXmore" };
            assert_eq!(fs::read(d.join("mismatch")).unwrap(), mismatch, "{what}");
        }
    }
    // A second run has nothing left to append.
    let (r, f) = both(tmp.path(), &["-ai", "--append", "src/", "{dst}"], "r0-0", "f0-0").unwrap();
    same_output(&r, &f, &[], "again");
    // rsync's conflicts: --append is in place and delta-based.
    for flags in [&["--append", "-W"][..], &["--append-verify", "--partial-dir=pd"], &["--append", "--no-append", "-W"]] {
        let mut a = vec!["-a"];
        a.extend(flags);
        a.extend(["src/", "{dst}"]);
        let (r, f) = both(tmp.path(), &a, "er", "ef").unwrap();
        assert_eq!(r.code, f.code, "{flags:?}: {}", f.stderr);
        assert_eq!(message(&r.stderr), message(&f.stderr), "{flags:?}");
    }
}

// ---------------------------------------------------------------------------
// -T / --temp-dir, --delay-updates
// ---------------------------------------------------------------------------

/// Whether a tree holds any partial-dir or temp leftovers.
fn leftovers(root: &Path) -> Vec<String> {
    snapshot(root, false)
        .keys()
        .map(|k| String::from_utf8_lossy(k).into_owned())
        .filter(|k| k.contains(".~tmp~") || k.split('/').next_back().is_some_and(|n| n.starts_with('.') && n.len() > 7 && n.as_bytes()[n.len() - 7] == b'.'))
        .collect()
}

#[test]
fn temp_dir_and_delay_updates_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = Tree::new(tmp.path().join("src"));
    t.file("a", "alpha").file("same", "same").file("sub/b", noise(300_000, 4)).file("sub/c", "c").symlink("lnk", "a").settle_mtimes();
    let cases: &[&[&str]] = &[
        &["-ai", "-T", "scratch"],
        &["-ai", "--temp-dir=scratch", "--partial"],
        &["-ai", "--delay-updates"],
        &["-ai", "--delay-updates", "-T", "scratch"],
        &["-ai", "--delay-updates", "--partial-dir=.pd"],
        &["-ai", "--delay-updates", "--no-delay-updates"],
    ];
    for (i, flags) in cases.iter().enumerate() {
        for (j, (src, dst)) in [("src/", "{dst}"), ("src/", "{host}{dst}"), ("{host}src/", "{dst}")].into_iter().enumerate() {
            let (dr, df) = (format!("r{i}-{j}"), format!("f{i}-{j}"));
            // An update: a changed file, an unchanged one, a new one; and -T's directory,
            // which is relative to the destination.
            for d in [&dr, &df] {
                Tree::new(tmp.path().join(d)).file("a", "old").file("same", "same").file("sub/b", noise(300_000, 5)).dir("scratch");
                set_mtime(&tmp.path().join(d).join("same"), fs::metadata(t.path("same")).unwrap().mtime() as u64);
            }
            let mut a = flags.to_vec();
            a.extend([src, dst]);
            let (r, f) = both_remote(tmp.path(), &a, &dr, &df).unwrap();
            let what = format!("{flags:?} {src} {dst}");
            same_output(&r, &f, &[], &what);
            assert_same_tree(&snap_no_dir_times(&tmp.path().join(&dr)), &snap_no_dir_times(&tmp.path().join(&df)));
            assert_eq!(leftovers(&tmp.path().join(&df)), Vec::<String>::new(), "{what}");
            assert!(fs::read_dir(tmp.path().join(&df).join("scratch")).unwrap().next().is_none(), "{what}: the temp dir is left empty");
        }
    }
    // rsync's checks: the temp dir must exist and be a directory, relative to the
    // destination; in-place updates have no use for --delay-updates.
    Tree::new(tmp.path().join("ed")).file("plainfile", "x");
    for flags in [
        &["-a", "-T", "nope"][..],
        &["-a", "-T", "plainfile"],
        &["-a", "--inplace", "--delay-updates"],
        &["-a", "--append", "--delay-updates"],
        &["-a", "--delay-updates", "--inplace"],
    ] {
        let mut a = flags.to_vec();
        a.extend(["src/", "ed"]);
        let r = rsync(tmp.path(), &a).unwrap();
        let f = barqsync(tmp.path(), &a);
        assert_eq!((r.code, f.code), (1, 1), "{flags:?}: {}", f.stderr);
        assert_eq!(message(&r.stderr), message(&f.stderr), "{flags:?}");
    }
    // A pull checks it too, on the receiving (client) side.
    let f = barqsync_remote(tmp.path(), &["-a", "-T", "nope", "{host}src/", "ed"]);
    assert_eq!(f.code, 1, "{}", f.stderr);
    assert!(f.stderr.contains("The temp-dir does not exist: nope"), "{}", f.stderr);
}

// ---------------------------------------------------------------------------
// --force, --ignore-errors, --ignore-missing-args, --delete-missing-args
// ---------------------------------------------------------------------------

/// Each distinct stderr line, without the tool's name, the temp dir's path, or rsync's
/// closing "rsync error/warning: ..." line (barqsync has none).
fn messages(o: &RunOut, tmp: &Path) -> std::collections::BTreeSet<String> {
    let t = tmp.to_str().unwrap();
    o.stderr
        .lines()
        .filter(|l| !l.starts_with("rsync error") && !l.starts_with("rsync warning"))
        .map(|l| l.strip_prefix("rsync: ").or_else(|| l.strip_prefix("barqsync: ")).unwrap_or(l))
        .map(|l| l.replace("[sender] ", "").replace("[generator] ", "").replace("[receiver] ", "").replace(t, "").replace("/private", ""))
        .collect()
}

#[test]
fn force_and_missing_args_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", "a").file("x", "x").settle_mtimes();
    // A directory deleted at the source whose files are still listed (rsync: they are
    // deleted, the directory "vanished", exit 24).
    fs::write(tmp.path().join("list"), "a\ngone\nsub/gone2\nsub/deep\nsub/deep/gone3\n").unwrap();
    let cases: &[&[&str]] = &[
        // A directory in the way of a file goes only with --delete or --force.
        &["-ai", "src/", "{dst}"],
        &["-ai", "--force", "src/", "{dst}"],
        &["-ai", "--force", "--no-force", "src/", "{dst}"],
        &["-ai", "--ignore-missing-args", "src/a", "src/gone", "src/x", "{dst}/"],
        &["-ai", "--delete-missing-args", "src/a", "src/gone", "src/gonedir", "{dst}/"],
        &["-ai", "--delete-missing-args", "--force", "src/a", "src/gonedir", "{dst}/"],
        &["-ai", "--delete-missing-args", "--ignore-missing-args", "src/gone", "{dst}/"],
        // A rule leaves a missing source's destination alone. (Only compared locally:
        // over ssh rsync's sender sends the excluded name anyway, which its server then
        // deletes in a push, and its client refuses in a pull, exit 4.)
        &["-ai", "--delete-missing-args", "--exclude=gone", "src/a", "src/gone", "{dst}/"],
        &["-ai", "--ignore-missing-args", "--files-from=list", "src/", "{dst}/"],
        &["-ai", "--delete-missing-args", "--files-from=list", "src/", "{dst}/"],
        // A single missing file copied to a new name deletes that name.
        &["-ai", "--delete-missing-args", "src/gone", "{dst}/target"],
    ];
    for (i, flags) in cases.iter().enumerate() {
        for (j, remote) in ["", "push", "pull"].into_iter().enumerate() {
            if !remote.is_empty() && flags.contains(&"--exclude=gone") {
                continue;
            }
            let (dr, df) = (format!("r{i}-{j}"), format!("f{i}-{j}"));
            for d in [&dr, &df] {
                Tree::new(tmp.path().join(d))
                    .file("x/inner/f", "i")
                    .file("gone", "g")
                    .file("target", "t")
                    .file("gonedir/k/q", "q")
                    .file("sub/gone2", "g2")
                    .file("sub/deep/gone3", "g3");
            }
            let a: Vec<String> = flags
                .iter()
                .map(|a| match (remote, a.starts_with("src/"), a.starts_with("{dst}")) {
                    ("push", _, true) => format!("{{host}}{a}"),
                    ("pull", true, _) => format!("{{host}}{a}"),
                    _ => a.to_string(),
                })
                .collect();
            let a: Vec<&str> = a.iter().map(String::as_str).collect();
            let (r, f) = both_remote(tmp.path(), &a, &dr, &df).unwrap();
            let what = format!("{flags:?} {remote}");
            same_output(&r, &f, &[(&dr, &df)], &what);
            assert_same_tree(&snap_no_dir_times(&tmp.path().join(&dr)), &snap_no_dir_times(&tmp.path().join(&df)));
            let (rm, fm) = (messages(&r, tmp.path()), messages(&f, tmp.path()));
            assert_eq!(rm.iter().map(|l| l.replace(&dr, "D")).collect::<Vec<_>>(), fm.iter().map(|l| l.replace(&df, "D")).collect::<Vec<_>>(), "{what}");
        }
    }
}

#[test]
fn ignore_errors_deletes_after_errors_like_rsync() {
    if skip_without_rsync() || unsafe { libc::geteuid() } == 0 {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // An unreadable source directory is an I/O error: --delete then deletes nothing,
    // unless --ignore-errors. (--delete-after: rsync's default --delete-during deletes
    // in the directories it reached before the error, barqsync only after a complete
    // read; see the known differences.)
    Tree::new(tmp.path().join("src")).file("a", "a").file("locked/f", "f").settle_mtimes();
    fs::set_permissions(tmp.path().join("src/locked"), fs::Permissions::from_mode(0o000)).unwrap();
    let cases: [&[&str]; 4] = [
        &["-ai", "--delete-after"],
        &["-ai", "--delete-after", "--ignore-errors"],
        &["-ai", "--delete", "--ignore-errors"],
        &["-ai", "--delete-after", "--ignore-errors", "--no-ignore-errors"],
    ];
    for (i, flags) in cases.iter().enumerate() {
        for (j, (src, dst)) in [("src/", "{dst}"), ("src/", "{host}{dst}"), ("{host}src/", "{dst}")].into_iter().enumerate() {
            let (dr, df) = (format!("r{i}-{j}"), format!("f{i}-{j}"));
            for d in [&dr, &df] {
                Tree::new(tmp.path().join(d)).file("a", "a").file("extra", "e").dir("old/deep");
            }
            let mut a = flags.to_vec();
            a.extend([src, dst]);
            let (r, f) = both_remote(tmp.path(), &a, &dr, &df).unwrap();
            let what = format!("{flags:?} {src} {dst}");
            same_output(&r, &f, &[], &what);
            // The copy of the unreadable directory is unreadable too.
            for d in [&dr, &df] {
                let _ = fs::set_permissions(tmp.path().join(d).join("locked"), fs::Permissions::from_mode(0o755));
            }
            assert_same_tree(&snap_no_dir_times(&tmp.path().join(&dr)), &snap_no_dir_times(&tmp.path().join(&df)));
            assert_eq!(tmp.path().join(&df).join("extra").exists(), flags.len() != 3, "{what}");
        }
    }
    fs::set_permissions(tmp.path().join("src/locked"), fs::Permissions::from_mode(0o755)).unwrap();
}

// ---------------------------------------------------------------------------
// --safe-links, --copy-unsafe-links, --munge-links
// ---------------------------------------------------------------------------

#[test]
fn symlink_safety_options_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let top = tmp.path().join("top");
    Tree::new(top.join("outside")).file("file", "outside");
    // Links inside the transfer and out of it, as `src/` (where `../` leaves it at once)
    // and `src` (one level more) see them. (No `a/../b` targets: rsync 3.4 and later call
    // those unsafe, 3.2 doesn't.)
    Tree::new(top.join("src"))
        .file("a", "a")
        .file("sub/b", "b")
        .symlink("rel_ok", "a")
        .symlink("sub/up_ok", "../a")
        .symlink("rel_out", "../outside/file")
        .symlink("sub/up_out", "../../outside/file")
        .symlink("abs", "/etc/hosts")
        .symlink("dir_out", "../outside")
        .symlink("dangling_out", "../missing")
        .symlink("munged", "/rsyncd-munged/a")
        .settle_mtimes();
    let cases: &[&[&str]] = &[
        &["-ai", "--safe-links"],
        &["-av", "--safe-links"],
        &["-ai", "--copy-unsafe-links"],
        &["-av", "--copy-unsafe-links"],
        &["-ai", "--safe-links", "--copy-unsafe-links"],
        // Each side munges on its own: a sending client unmunges, a receiving client
        // munges, a local copy (whose client sends) unmunges.
        &["-ai", "--munge-links"],
        &["-ai", "--munge-links", "--safe-links"],
        &["-ai", "--munge-links", "--no-munge-links"],
    ];
    for (i, flags) in cases.iter().enumerate() {
        for (j, spec) in ["src/", "src"].into_iter().enumerate() {
            for (k, (src, dst)) in [("", "{dst}/"), ("", "{host}{dst}/"), ("{host}", "{dst}/")].into_iter().enumerate() {
                let (dr, df) = (format!("r{i}-{j}-{k}"), format!("f{i}-{j}-{k}"));
                let source = format!("{src}{spec}");
                let mut a = flags.to_vec();
                a.extend([source.as_str(), dst]);
                let (r, f) = both_remote(&top, &a, &dr, &df).unwrap();
                let what = format!("{flags:?} {source} {dst}");
                same_output(&r, &f, &[(&dr, &df)], &what);
                // (Without the destination itself, which each tool created a moment apart.)
                assert_same_tree(&snap(&top.join(&dr), false), &snap(&top.join(&df), false));
            }
        }
    }
}

/// A source cut short after the scan is skipped with `--append`, as rsync's sender skips
/// a "diminished" file: the destination keeps its data and attributes, and
/// `--remove-source-files` leaves the source. (rsync can't be raced reliably, so a test
/// hook cuts the file to half its length just before it is opened.)
#[test]
fn append_skips_a_diminished_source() {
    let tmp = tempfile::tempdir().unwrap();
    let data = noise(300_000, 4);
    for (i, (src, dst)) in [("src/", "d0"), ("src/", "{host}d1"), ("{host}src/", "d2")].into_iter().enumerate() {
        for mode in ["--append", "--append-verify"] {
            Tree::new(tmp.path().join("src")).file("log", &data).file("ok", "complete").settle_mtimes();
            let name = format!("d{i}");
            Tree::new(tmp.path().join(&name)).file("log", &data[..200_000]).file("ok", "comp").same_mtimes(BASE_TIME + 7);
            let args = ["-a", mode, "--remove-source-files", src, dst];
            let f = barqsync_env(tmp.path(), &args, &[("BARQSYNC_TEST_SHRINK", "log")]);
            let what = format!("{mode} {src} {dst}");
            assert_eq!(f.code, 0, "{what}\n{}", f.stderr);
            assert!(f.stderr.contains("skipped diminished file: \"") && f.stderr.contains("src/log\""), "{what}: {}", f.stderr);
            let d = tmp.path().join(&name);
            assert_eq!(fs::read(d.join("log")).unwrap(), &data[..200_000], "{what}");
            assert_eq!(fs::metadata(d.join("log")).unwrap().mtime() as u64, BASE_TIME + 7, "{what}");
            assert_eq!(fs::metadata(tmp.path().join("src/log")).unwrap().len(), 150_000, "{what}: the source stays");
            assert_eq!(fs::read(d.join("ok")).unwrap(), b"complete", "{what}");
            assert!(!tmp.path().join("src/ok").exists(), "{what}");
        }
    }
}
