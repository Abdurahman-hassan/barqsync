//! End-to-end local sync tests. Most are differential: the same command runs through
//! the reference rsync and barqsync, and both the printed items and the resulting
//! trees must match.

mod common;

use common::*;
use std::fs;

#[test]
fn fresh_copy_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    rich_tree(&tmp.path().join("src"));
    let (r, f) = both(tmp.path(), &["-av", "src/", "{dst}"], "dr", "df").unwrap();
    assert_eq!(f.code, 0, "stderr: {}", f.stderr);
    assert_eq!(normalize(r.items(), "dr", "D"), normalize(f.items(), "df", "D"));
    assert_same_tree(&snapshot(&tmp.path().join("dr"), true), &snapshot(&tmp.path().join("df"), true));
}

#[test]
fn no_trailing_slash_copies_the_directory_itself() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    rich_tree(&tmp.path().join("src"));
    let (r, f) = both(tmp.path(), &["-av", "src", "{dst}"], "dr", "df").unwrap();
    assert_eq!(f.code, 0, "stderr: {}", f.stderr);
    assert_eq!(normalize(r.items(), "dr", "D"), normalize(f.items(), "df", "D"));
    assert_same_tree(&snapshot(&tmp.path().join("dr"), false), &snapshot(&tmp.path().join("df"), false));
}

#[test]
fn second_run_is_a_noop() {
    let tmp = tempfile::tempdir().unwrap();
    rich_tree(&tmp.path().join("src"));
    let first = barqsync(tmp.path(), &["-a", "src/", "dst"]);
    assert_eq!(first.code, 0, "{}", first.stderr);
    let before = snapshot(&tmp.path().join("dst"), true);
    let second = barqsync(tmp.path(), &["-a", "--out-format=%n", "src/", "dst"]);
    assert_eq!(second.code, 0, "{}", second.stderr);
    assert_eq!(second.stdout, "", "nothing should be transferred on a no-op sync");
    assert_eq!(before, snapshot(&tmp.path().join("dst"), true));
}

#[test]
fn incremental_changes_with_delete_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = rich_tree(&tmp.path().join("src"));
    for d in ["dr", "df"] {
        let r = barqsync(tmp.path(), &["-a", "src/", d]);
        assert_eq!(r.code, 0, "{}", r.stderr);
    }
    // Mutate the source in every way the quick check must notice.
    t.file("a.txt", "ALPHA\n") // same size, new mtime below
        .file("sub/b.txt", "bravo-appended")
        .file("new/nested/file", "new")
        .remove("sub/deep")
        .remove("empty")
        .file("empty", "now a file again")
        .remove("emptydir")
        .file("emptydir", "dir became file")
        .remove("with space.txt")
        .dir("with space.txt")
        .file("with space.txt/inside", "file became dir")
        .symlink("link", "sub/b.txt")
        .chmod("big.bin", 0o640)
        .settle_mtimes();
    // Extraneous destination entries.
    for d in ["dr", "df"] {
        fs::write(tmp.path().join(d).join("extra.txt"), "x").unwrap();
        fs::create_dir_all(tmp.path().join(d).join("olddir/deeper")).unwrap();
        fs::write(tmp.path().join(d).join("olddir/deeper/f"), "y").unwrap();
    }
    let (r, f) = both(tmp.path(), &["-av", "--delete", "src/", "{dst}"], "dr", "df").unwrap();
    assert_eq!(f.code, 0, "stderr: {}", f.stderr);
    assert_eq!(r.items(), f.items());
    assert_same_tree(&snapshot(&tmp.path().join("dr"), true), &snapshot(&tmp.path().join("df"), true));
}

#[test]
fn excludes_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = Tree::new(tmp.path().join("src"));
    t.file("keep.log", "k")
        .file("drop.log", "d")
        .file("build/out.o", "o")
        .file("src/build/x.c", "c")
        .file("src/main.c", "m")
        .file("src/main.o", "o")
        .file("cache/a", "a")
        .file("docs/cache", "file named cache")
        .file("node_modules/pkg/index.js", "js")
        .settle_mtimes();
    let cases: &[&[&str]] = &[
        &["--exclude=*.o"],
        &["--include=keep.log", "--exclude=*.log"],
        &["--exclude=/build"],
        &["--exclude=build/"],
        &["--exclude=cache/"],
        &["--exclude=src/*.c"],
        &["--exclude=/node_modules/***"],
        &["--exclude=**/x.c"],
        &["--include=*/", "--include=*.c", "--exclude=*"],
    ];
    for (i, flags) in cases.iter().enumerate() {
        let (dr, df) = (format!("r{i}"), format!("f{i}"));
        // Pre-populate destinations with an excluded file so --delete protection is tested.
        for d in [&dr, &df] {
            Tree::new(tmp.path().join(d)).file("drop.log", "old").file("stale", "s");
        }
        let mut args = vec!["-av", "--delete"];
        args.extend_from_slice(flags);
        args.extend_from_slice(&["src/", "{dst}"]);
        let (r, f) = both(tmp.path(), &args, &dr, &df).unwrap();
        assert_eq!(f.code, 0, "{flags:?}: {}", f.stderr);
        assert_eq!(r.items(), f.items(), "output differs for {flags:?}");
        assert_same_tree(&snapshot(&tmp.path().join(&dr), false), &snapshot(&tmp.path().join(&df), false));
    }
}

#[test]
fn delete_excluded_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a.log", "a").file("b.txt", "b").settle_mtimes();
    for d in ["dr", "df"] {
        Tree::new(tmp.path().join(d)).file("a.log", "old").file("c.log", "c");
    }
    let (r, f) =
        both(tmp.path(), &["-av", "--delete-excluded", "--exclude=*.log", "src/", "{dst}"], "dr", "df").unwrap();
    assert_eq!(r.items(), f.items());
    assert_same_tree(&snapshot(&tmp.path().join("dr"), false), &snapshot(&tmp.path().join("df"), false));
}

#[test]
fn dry_run_changes_nothing_and_matches_rsync_output() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    rich_tree(&tmp.path().join("src"));
    for d in ["dr", "df"] {
        Tree::new(tmp.path().join(d)).file("extra", "x").file("a.txt", "different");
    }
    let before = snapshot(&tmp.path().join("df"), true);
    let (r, f) = both(tmp.path(), &["-avn", "--delete", "src/", "{dst}"], "dr", "df").unwrap();
    assert_eq!(f.code, 0, "{}", f.stderr);
    assert_eq!(r.items(), f.items());
    assert_eq!(before, snapshot(&tmp.path().join("df"), true), "dry run modified the destination");
    // Dry run into a destination that doesn't exist yet.
    let (r, f) = both(tmp.path(), &["-avn", "src", "{dst}"], "nr", "nf").unwrap();
    assert_eq!(normalize(r.items(), "nr", "N"), normalize(f.items(), "nf", "N"));
    assert!(!tmp.path().join("nf").exists());
}

#[test]
fn recursive_without_times_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    rich_tree(&tmp.path().join("src"));
    let (r, f) = both(tmp.path(), &["-rv", "src/", "{dst}"], "dr", "df").unwrap();
    assert_eq!(normalize(r.items(), "dr", "D"), normalize(f.items(), "df", "D"));
    assert_same_tree(
        &without_mtimes(snapshot(&tmp.path().join("dr"), false)),
        &without_mtimes(snapshot(&tmp.path().join("df"), false)),
    );
}

#[test]
fn single_file_copies_and_renames() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path()).file("one.txt", "1").dir("into");
    let r = barqsync(tmp.path(), &["-a", "one.txt", "renamed.txt"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(fs::read_to_string(tmp.path().join("renamed.txt")).unwrap(), "1");
    let r = barqsync(tmp.path(), &["-a", "one.txt", "into"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(fs::read_to_string(tmp.path().join("into/one.txt")).unwrap(), "1");
    let r = barqsync(tmp.path(), &["-a", "one.txt", "newdir/"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(fs::read_to_string(tmp.path().join("newdir/one.txt")).unwrap(), "1");
}

#[test]
fn multiple_sources_merge_into_destination() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("s1")).file("a", "1").file("d/x", "x").settle_mtimes();
    Tree::new(tmp.path().join("s2")).file("b", "2").settle_mtimes();
    Tree::new(tmp.path()).file("loose.txt", "l").mtime("loose.txt", BASE_TIME);
    let (r, f) = both(tmp.path(), &["-av", "s1", "s2/", "loose.txt", "{dst}"], "dr", "df").unwrap();
    assert_eq!(normalize(r.items(), "dr", "D"), normalize(f.items(), "df", "D"));
    assert_same_tree(&snapshot(&tmp.path().join("dr"), false), &snapshot(&tmp.path().join("df"), false));
}

#[test]
fn read_only_source_directories_are_filled_then_locked() {
    let tmp = tempfile::tempdir().unwrap();
    let t = Tree::new(tmp.path().join("src"));
    t.file("ro/inner/f", "data").chmod("ro/inner", 0o555).chmod("ro", 0o500).settle_mtimes();
    let r = barqsync(tmp.path(), &["-a", "src/", "dst"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(fs::read_to_string(tmp.path().join("dst/ro/inner/f")).unwrap(), "data");
    let snap_src = snapshot(&tmp.path().join("src"), true);
    let snap_dst = snapshot(&tmp.path().join("dst"), true);
    // Restore permissions so the tempdir can be cleaned up.
    for base in ["src", "dst"] {
        t.chmod(&format!("../{base}/ro"), 0o755).chmod(&format!("../{base}/ro/inner"), 0o755);
    }
    assert_same_tree(&snap_src, &snap_dst);
}

#[test]
fn quick_check_variants_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = Tree::new(tmp.path().join("src"));
    t.file("same_size", "aaaa").file("newer_dst", "src").file("older_dst", "src").file("only_src", "x");
    t.settle_mtimes();
    for flags in [&["-u"][..], &["--ignore-existing"], &["--size-only"], &["-I"], &["--modify-window=-1"]] {
        let (dr, df) = (format!("r{}", flags[0]), format!("f{}", flags[0]));
        for d in [&dr, &df] {
            let dt = Tree::new(tmp.path().join(d));
            dt.file("same_size", "bbbb").file("newer_dst", "dst-newer").file("older_dst", "dst-older");
            dt.mtime("same_size", BASE_TIME + 999_999).mtime("newer_dst", BASE_TIME + 999_999).mtime("older_dst", 1);
        }
        let mut args = vec!["-av"];
        args.extend_from_slice(flags);
        args.extend_from_slice(&["src/", "{dst}"]);
        let (r, f) = both(tmp.path(), &args, &dr, &df).unwrap();
        assert_eq!(r.items(), f.items(), "output differs for {flags:?}");
        assert_same_tree(&snapshot(&tmp.path().join(&dr), false), &snapshot(&tmp.path().join(&df), false));
    }
}

#[test]
fn inplace_and_fsync_produce_correct_files() {
    let tmp = tempfile::tempdir().unwrap();
    let t = Tree::new(tmp.path().join("src"));
    t.file("f", "first").settle_mtimes();
    assert_eq!(barqsync(tmp.path(), &["-a", "src/", "dst"]).code, 0);
    t.file("f", "second version").settle_mtimes();
    let r = barqsync(tmp.path(), &["-a", "--inplace", "--fsync", "src/", "dst"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_same_tree(&snapshot(&tmp.path().join("src"), true), &snapshot(&tmp.path().join("dst"), true));
}

/// Files of 8 MiB+ go through the page-cache-hinted copy paths (buffered copy,
/// `--inplace` overwrite, clone or `copy_file_range`); the bytes must not change.
#[test]
fn big_files_with_and_without_cache_hints() {
    let tmp = tempfile::tempdir().unwrap();
    let t = Tree::new(tmp.path().join("src"));
    let big: Vec<u8> = (0..(9u64 << 20) + 777).map(|i| (i * 31 + i / 4099) as u8).collect();
    t.file("big.img", &big).file("small", "x").settle_mtimes();
    for (dst, extra) in [("d1", &["--no-clone"][..]), ("d2", &["--no-clone", "--no-drop-cache"]), ("d3", &[])] {
        let mut args = vec!["-a"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["src/", dst]);
        let r = barqsync(tmp.path(), &args);
        assert_eq!(r.code, 0, "{extra:?}: {}", r.stderr);
        assert_same_tree(&snapshot(&tmp.path().join("src"), true), &snapshot(&tmp.path().join(dst), true));
    }
    // Overwrite in place with a shorter, different version.
    t.file("big.img", &big[1000..(8 << 20) + 1000 + 99]).settle_mtimes();
    for extra in [&["--inplace", "--no-clone"][..], &["--inplace", "--no-clone", "--drop-cache"]] {
        for dst in ["d1", "d2"] {
            let mut args = vec!["-a"];
            args.extend_from_slice(extra);
            args.extend_from_slice(&["src/", dst]);
            let r = barqsync(tmp.path(), &args);
            assert_eq!(r.code, 0, "{extra:?}: {}", r.stderr);
            assert_same_tree(&snapshot(&tmp.path().join("src"), true), &snapshot(&tmp.path().join(dst), true));
        }
    }
}

#[test]
fn missing_source_reports_partial_transfer() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("ok", "1");
    let r = barqsync(tmp.path(), &["-a", "src", "nope", "dst"]);
    assert_eq!(r.code, 23);
    assert!(r.stderr.contains("nope"), "{}", r.stderr);
    assert!(tmp.path().join("dst/src/ok").exists());
}

#[test]
fn no_temp_files_left_behind() {
    let tmp = tempfile::tempdir().unwrap();
    rich_tree(&tmp.path().join("src"));
    assert_eq!(barqsync(tmp.path(), &["-a", "src/", "dst"]).code, 0);
    let snap = snapshot(&tmp.path().join("dst"), false);
    let stray: Vec<_> = snap
        .keys()
        .map(|k| String::from_utf8_lossy(k).into_owned())
        .filter(|k| {
            let name = k.rsplit('/').next().unwrap();
            name.starts_with('.') && name != ".hidden"
        })
        .collect();
    assert!(stray.is_empty(), "temp files left: {stray:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_names_round_trip() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    for name in [&b"bad-\xff\xfe-1"[..], b"bad-\xff\xfe-2", b"\xc3\x28"] {
        fs::write(src.join(OsStr::from_bytes(name)), name).unwrap();
    }
    let r = barqsync_os(tmp.path(), &[OsStr::new("-a"), OsStr::new("src/"), OsStr::new("dst")]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_same_tree(&snapshot(&src, true), &snapshot(&tmp.path().join("dst"), true));
}

/// Build `src` with a file, a directory (holding `x`) and a symlink per name (its target
/// is the name plus an invalid UTF-8 byte), then compare rsync's and barqsync's raw
/// output under LC_ALL=`locale`, with and without `-8`, for `-rv --delete` (symlinks are
/// skipped as non-regular files) and `-av --delete` (`name -> target`). Each destination
/// starts with a stale file per name, so `deleting` lines are covered too.
fn assert_names_escaped_like_rsync(names: &[&[u8]], locale: &str) {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let tmp = tempfile::tempdir().unwrap();
    let at = |dir: &str, prefix: &[u8], name: &[u8]| tmp.path().join(dir).join(OsStr::from_bytes(&[prefix, name].concat()));
    fs::create_dir(tmp.path().join("src")).unwrap();
    for n in names {
        fs::write(at("src", b"f-", n), n).unwrap();
        fs::create_dir(at("src", b"d-", n)).unwrap();
        fs::write(at("src", b"d-", n).join("x"), "x").unwrap();
        std::os::unix::fs::symlink(OsStr::from_bytes(&[b"t-", *n, b"\xff"].concat()), at("src", b"l-", n)).unwrap();
    }
    let shown = |lines: Vec<Vec<u8>>| -> Vec<String> { lines.iter().map(|l| l.escape_ascii().to_string()).collect() };
    for (i, flags) in [["-rv", "--delete"], ["-av", "--delete"]].iter().enumerate() {
        for eight_bit in [false, true] {
            let (dr, df) = (format!("r{i}-{eight_bit}"), format!("f{i}-{eight_bit}"));
            for d in [&dr, &df] {
                fs::create_dir(tmp.path().join(d)).unwrap();
                for n in names {
                    fs::write(at(d, b"s-", n), "stale").unwrap();
                }
            }
            let mut args = flags.to_vec();
            if eight_bit {
                args.push("-8");
            }
            args.extend(["src/", "{dst}"]);
            let (r, f) = both_in_locale(tmp.path(), locale, &args, &dr, &df).unwrap();
            assert_eq!((r.code, f.code), (0, 0), "{args:?}\nrsync: {}\nbarqsync: {}", r.stderr, f.stderr);
            assert_eq!(shown(r.raw_items()), shown(f.raw_items()), "LC_ALL={locale} {args:?}");
            assert!(
                !f.raw.iter().any(|&b| b < b' ' && b != b'\n' && b != b'\t'),
                "raw control character in output: {}",
                f.raw.escape_ascii()
            );
            assert_same_tree(
                &without_mtimes(snapshot(&tmp.path().join(&dr), false)),
                &without_mtimes(snapshot(&tmp.path().join(&df), false)),
            );
        }
    }
}

#[test]
fn control_characters_in_names_are_escaped_like_rsync() {
    if skip_without_rsync() {
        return;
    }
    // Valid UTF-8 only, so the names can exist on APFS too.
    let names: &[&[u8]] = &[
        b"nl\nx",
        b"esc\x1b[31mred",
        b"ctl\x01",
        b"tab\tx",
        b"del\x7fx",
        br"lit\#123x",
        "café".as_bytes(),
        "c1\u{9b}x".as_bytes(),
    ];
    assert_names_escaped_like_rsync(names, TEST_LOCALE);
}

#[cfg(target_os = "linux")]
#[test]
fn non_ascii_names_are_escaped_per_locale_like_rsync() {
    if skip_without_rsync() {
        return;
    }
    let names: &[&[u8]] = &[
        b"caf\xc3\xa9",
        b"c1\xc2\x9bx",
        b"bad\xffx",
        b"lat\xe9x",
        b"overlong\xc0\x80",
        b"surr\xed\xa0\x80",
        b"big\xf4\x90\x80\x80",
        b"five\xf8\x88\x80\x80\x80",
        b"trunc\xe2\x82",
        b"trunc2\xe2\x82(",
        b"cut\\#\xff123",
        b"ctl\x01\xe9",
    ];
    // No system has xx_XX installed: glibc falls back to the C locale for both tools.
    for locale in ["C", "C.UTF-8", "xx_XX.UTF-8"] {
        assert_names_escaped_like_rsync(names, locale);
    }
}

/// Names in error messages are escaped from their raw bytes too: an invalid UTF-8
/// byte shows as `\#377` (as in rsync), not as a U+FFFD that went through `Path::display()`.
/// The source is missing, so the name never has to exist (APFS would refuse it).
#[test]
fn error_messages_escape_invalid_utf8_like_rsync() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("ok", "1");
    let args = |dst: &'static str| [OsStr::new("-a"), OsStr::new("src/ok"), OsStr::from_bytes(b"src/no\xff"), OsStr::new(dst)];
    for locale in ["C.UTF-8", "C"] {
        let f = barqsync_os_in_locale(tmp.path(), locale, &args("dst-f"));
        assert_eq!(f.code, 23, "LC_ALL={locale}: {}", f.stderr);
        assert!(f.stderr.contains(r#"link_stat "src/no\#377" failed"#), "LC_ALL={locale}: {}", f.stderr);
        if let Some(r) = rsync_os_in_locale(tmp.path(), locale, &args("dst-r")) {
            assert!(r.stderr.contains(r#"/src/no\#377" failed"#), "rsync, LC_ALL={locale}: {}", r.stderr);
        }
    }
}
