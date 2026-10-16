//! Itemized output (`-i`), hard links (`-H`) and progress, compared with rsync where
//! the output is defined by rsync.

mod common;

use common::*;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

fn base_tree(root: &Path) -> Tree {
    let t = Tree::new(root);
    t.file("a", "a\n")
        .file("sub/b", "b\n")
        .file("keep", "same")
        .dir("dir2")
        .symlink("link", "a")
        .fifo("fifo")
        .chmod("a", 0o640)
        .settle_mtimes();
    t
}

#[test]
fn itemize_matches_rsync_through_a_sequence_of_changes() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = base_tree(&tmp.path().join("src"));
    let run = |args: &[&str]| {
        let mut a = args.to_vec();
        a.extend_from_slice(&["src/", "{dst}"]);
        let (r, f) = both(tmp.path(), &a, "dr", "df").unwrap();
        assert_eq!(f.code, 0, "{args:?}: {}", f.stderr);
        assert_eq!(normalize(r.items(), "dr", "D"), normalize(f.items(), "df", "D"), "{args:?}");
    };
    run(&["-ai"]); // fresh
    run(&["-ai"]); // no-op
    t.file("a", "AAAA\n")
        .chmod("sub/b", 0o600)
        .symlink("link", "sub/b")
        .remove("fifo")
        .dir("newdir")
        .settle_mtimes();
    for d in ["dr", "df"] {
        fs::write(tmp.path().join(d).join("extra"), "x").unwrap();
    }
    run(&["-ai", "--delete"]); // content, perms, symlink target, deletions, new dir
    t.chmod("keep", 0o600);
    run(&["-ai"]); // attribute-only change
    run(&["-aii"]); // unchanged items listed
    run(&["-rlpi"]); // without -t: T instead of t
}

#[test]
fn itemize_remote_push_shows_sender_view() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    base_tree(&tmp.path().join("src"));
    let (r, f) = both_remote(tmp.path(), &["-ai", "src/", "{host}{dst}"], "dr", "df").unwrap();
    assert_eq!(f.code, 0, "{}", f.stderr);
    assert!(f.items().iter().any(|l| l.starts_with("<f+++++++++")), "{:?}", f.items());
    assert_eq!(normalize(r.items(), "dr", "D"), normalize(f.items(), "df", "D"));
}

/// Groups of names sharing an inode (only groups with more than one name).
fn link_groups(root: &Path) -> BTreeSet<BTreeSet<String>> {
    let mut by_ino: BTreeMap<u64, BTreeSet<String>> = BTreeMap::new();
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<u64, BTreeSet<String>>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let md = fs::symlink_metadata(&p).unwrap();
            if md.is_dir() {
                walk(root, &p, out);
            } else if md.is_file() {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                out.entry(md.ino()).or_default().insert(rel);
            }
        }
    }
    walk(root, root, &mut by_ino);
    by_ino.into_values().filter(|g| g.len() > 1).collect()
}

fn hardlink_tree(root: &Path) {
    let t = Tree::new(root);
    t.file("h1", "shared\n").file("single", "s").file("g1", "other").dir("sub").dir("dir2");
    fs::hard_link(root.join("h1"), root.join("sub/h2")).unwrap();
    fs::hard_link(root.join("h1"), root.join("dir2/h3")).unwrap();
    fs::hard_link(root.join("g1"), root.join("sub/g2")).unwrap();
    t.settle_mtimes();
}

/// Item lines with the hard-link target dropped: which name leads a group depends on
/// traversal order (rsync's file list vs. our parallel walk).
fn without_link_targets(items: Vec<String>) -> Vec<String> {
    let mut v: Vec<String> = items.into_iter().map(|l| l.split(" => ").next().unwrap().to_string()).collect();
    v.sort();
    v
}

#[test]
fn hard_links_match_rsync_locally() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    hardlink_tree(&tmp.path().join("src"));
    let (r, f) = both(tmp.path(), &["-aHi", "src/", "{dst}"], "dr", "df").unwrap();
    assert_eq!(f.code, 0, "{}", f.stderr);
    // Exactly the same names are transferred or linked; only the leader may differ.
    let strip = |v: Vec<String>| {
        let mut v: Vec<String> = without_link_targets(v)
            .into_iter()
            .map(|l| l.replacen(">f+++++++++", "?f+++++++++", 1).replacen("hf+++++++++", "?f+++++++++", 1))
            .collect();
        v.sort();
        v
    };
    assert_eq!(strip(normalize(r.items(), "dr", "D")), strip(normalize(f.items(), "df", "D")));
    let groups = link_groups(&tmp.path().join("df"));
    assert_eq!(link_groups(&tmp.path().join("dr")), groups);
    assert_eq!(groups.len(), 2);
    assert_same_tree(&snapshot(&tmp.path().join("dr"), true), &snapshot(&tmp.path().join("df"), true));

    // A second run changes nothing; a broken link is repaired.
    let again = fastrsync(tmp.path(), &["-aHi", "src/", "df"]);
    assert_eq!(again.items(), Vec::<String>::new());
    fs::remove_file(tmp.path().join("df/dir2/h3")).unwrap();
    fs::write(tmp.path().join("df/dir2/h3"), "different content").unwrap();
    let fix = fastrsync(tmp.path(), &["-aHi", "src/", "df"]);
    assert_eq!(fix.code, 0, "{}", fix.stderr);
    assert!(fix.items().iter().any(|l| l.starts_with("hf") && l.contains("dir2/h3 => ")), "{:?}", fix.items());
    assert_eq!(link_groups(&tmp.path().join("df")), groups);
}

#[test]
fn hard_links_over_the_wire() {
    let tmp = tempfile::tempdir().unwrap();
    hardlink_tree(&tmp.path().join("src"));
    for (args, dst) in [(["-aH", "src/", "{host}pushed"], "pushed"), (["-aH", "{host}src/", "pulled"], "pulled")] {
        let f = fastrsync_remote(tmp.path(), &args);
        assert_eq!(f.code, 0, "{}", f.stderr);
        assert_eq!(link_groups(&tmp.path().join(dst)), link_groups(&tmp.path().join("src")));
        assert_same_tree(&snapshot(&tmp.path().join("src"), true), &snapshot(&tmp.path().join(dst), true));
    }
}

#[test]
fn without_hard_links_flag_names_are_separate_files() {
    let tmp = tempfile::tempdir().unwrap();
    hardlink_tree(&tmp.path().join("src"));
    assert_eq!(fastrsync(tmp.path(), &["-a", "src/", "df"]).code, 0);
    assert!(link_groups(&tmp.path().join("df")).is_empty());
}

#[test]
fn progress_reports_final_totals() {
    let tmp = tempfile::tempdir().unwrap();
    let t = Tree::new(tmp.path().join("src"));
    for i in 0..50 {
        t.file(&format!("d/f{i}"), vec![b'x'; 1000]);
    }
    for flag in ["--progress", "--info=progress2", "-P"] {
        let dst = format!("dst{flag}");
        let f = fastrsync(tmp.path(), &["-a", flag, "src/", &dst]);
        assert_eq!(f.code, 0, "{}", f.stderr);
        let last = f.stdout.lines().last().unwrap_or("").trim_start_matches('\r').to_string();
        assert!(last.contains("50,000 100%"), "{flag}: {last:?}");
        assert!(last.ends_with("(xfr#50, to-chk=0/50)"), "{flag}: {last:?}");
    }
}
