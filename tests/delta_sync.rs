//! Delta transfer (`--no-whole-file`) end to end: results must match rsync's, and the
//! stats must show that unchanged data was reused rather than copied.

mod common;

use common::*;
use std::path::Path;

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

const MIB: usize = 1 << 20;

/// Old and new versions of several files, each exercising a different kind of change.
fn versions() -> Vec<(&'static str, Vec<u8>, Vec<u8>)> {
    let base = noise(6 * MIB, 1);
    let mut overwritten = base.clone();
    overwritten[1_000_000..1_000_000 + 4096].copy_from_slice(&noise(4096, 2));
    overwritten[5_000_000..5_000_000 + 100].copy_from_slice(&noise(100, 3));
    let mut appended = base.clone();
    appended.extend(noise(300_000, 4));
    let mut inserted = noise(777, 5);
    inserted.extend_from_slice(&base);
    let mut middle_cut = base.clone();
    middle_cut.drain(2 * MIB..2 * MIB + 12_345);
    let truncated = base[..4 * MIB + 17].to_vec();
    // Big enough (8 MiB+) for page-cache hints on the hashing reads and rebuild writes.
    let big = noise(9 * MIB + 4321, 9);
    let mut big_edited = big.clone();
    big_edited[8 * MIB + 5..8 * MIB + 5000].copy_from_slice(&noise(4995, 10));
    vec![
        ("in_place.img", base.clone(), overwritten),
        ("appended.log", base.clone(), appended),
        ("inserted.bin", base.clone(), inserted),
        ("middle_cut.bin", base.clone(), middle_cut),
        ("truncated.bin", base.clone(), truncated),
        ("unrelated.bin", base.clone(), noise(3 * MIB, 6)),
        ("grown_from_small.bin", noise(70_000, 7), noise(2 * MIB, 8)),
        ("big.img", big, big_edited),
    ]
}

fn setup(root: &Path, dests: &[&str]) {
    let src = Tree::new(root.join("src"));
    for d in dests {
        let t = Tree::new(root.join(d));
        for (name, old, _) in versions() {
            t.file(name, &old);
        }
        t.settle_mtimes();
    }
    for (name, _, new) in versions() {
        src.file(name, &new);
    }
    src.settle_mtimes();
    // Make sure every source mtime differs from its destination (quick check must fire).
    for (i, (name, _, _)) in versions().iter().enumerate() {
        src.mtime(name, BASE_TIME + 500_000 + i as u64);
    }
}

fn stat_bytes(stdout: &str, label: &str) -> u64 {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix(label))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no {label:?} in output:\n{stdout}"))
}

fn check_mode(extra: &[&str]) {
    let tmp = tempfile::tempdir().unwrap();
    let have_rsync = reference_rsync().is_some();
    setup(tmp.path(), &["dr", "df"]);

    let mut args = vec!["-a", "--no-whole-file", "--stats"];
    args.extend_from_slice(extra);
    args.extend_from_slice(&["src/", "df"]);
    let f = barqsync(tmp.path(), &args);
    assert_eq!(f.code, 0, "{}\n{}", f.stdout, f.stderr);

    // Byte-exact result, attributes included.
    let mut src_snap = snapshot(&tmp.path().join("src"), true);
    let dst_snap = snapshot(&tmp.path().join("df"), true);
    src_snap.get_mut(b".".as_slice()).unwrap().mtime = dst_snap[b".".as_slice()].mtime;
    assert_same_tree(&src_snap, &dst_snap);

    // Most data must have been reused.
    let literal = stat_bytes(&f.stdout, "Literal data: ");
    let matched = stat_bytes(&f.stdout, "Matched data: ");
    let total_new: usize = versions().iter().map(|(_, _, n)| n.len()).sum();
    assert!(matched > (total_new as u64) / 2, "matched {matched} of {total_new} (literal {literal})");

    if have_rsync {
        let mut rargs = vec!["-a", "--no-whole-file"];
        rargs.extend(extra.iter().filter(|a| **a != "--no-clone" && !a.starts_with("--delta")));
        rargs.extend_from_slice(&["src/", "dr"]);
        let r = rsync(tmp.path(), &rargs).unwrap();
        assert_eq!(r.code, 0, "{}", r.stderr);
        assert_same_tree(&snapshot(&tmp.path().join("dr"), true), &snapshot(&tmp.path().join("df"), true));
    } else {
        eprintln!("SKIP rsync comparison: no reference rsync 3.x");
    }
}

#[test]
fn delta_with_clone_and_patch() {
    check_mode(&[]);
}

#[test]
fn delta_inplace() {
    check_mode(&["--inplace"]);
}

#[test]
fn delta_rebuild_without_clones() {
    check_mode(&["--no-clone"]);
}

#[test]
fn cdc_delta_with_clone_and_patch() {
    check_mode(&["--delta=cdc"]);
}

#[test]
fn cdc_delta_inplace() {
    check_mode(&["--delta=cdc", "--inplace"]);
}

#[test]
fn cdc_delta_rebuild_without_clones() {
    check_mode(&["--delta=cdc", "--no-clone"]);
}

#[test]
fn whole_file_is_still_the_local_default() {
    let tmp = tempfile::tempdir().unwrap();
    setup(tmp.path(), &["df"]);
    let f = barqsync(tmp.path(), &["-a", "--stats", "src/", "df"]);
    assert_eq!(f.code, 0, "{}", f.stderr);
    assert_eq!(stat_bytes(&f.stdout, "Matched data: "), 0);
}
