//! Regressions for the destructive-operation findings in docs/audit/findings.md: a source
//! that is also the destination, backups that fail or would replace the file itself or
//! the source, and the `--remove-source-files` check of whether a source changed.
//! All CLI cases have a deadline and use disposable files, including fake SSH.

mod support;
use support::{fake_ssh, run};

use barqsync::{cli::Options, fsops, local::flags::remove_source};
use std::fs::{self, File};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;

/// `f f`, `f ./f` and `f alias` (a hard link): in-place writes leave the data alone and
/// succeed, and `--remove-source-files` keeps the destination and reports it, as rsync
/// does. (Rewritten with `-I` and no `--inplace`, `alias` becomes a copy; then `f` can go.)
#[test]
fn same_file_paths_and_hard_links_preserve_source() {
    let data: Vec<u8> = (0..(2u32 << 20)).map(|i| (i % 251) as u8).collect();
    for flags in [
        &["--inplace", "-I"][..],
        &["--inplace", "-I", "--no-whole-file"],
        &["--inplace", "-I", "-b"],
        &["--remove-source-files"],
        &["--remove-source-files", "-I"],
        &["--remove-source-files", "--inplace", "-I"],
    ] {
        for alias in ["f", "./f", "alias"] {
            let tmp = tempfile::tempdir().unwrap();
            fs::write(tmp.path().join("f"), &data).unwrap();
            fs::hard_link(tmp.path().join("f"), tmp.path().join("alias")).unwrap();
            let inode = fs::metadata(tmp.path().join("f")).unwrap().ino();
            let mut args = vec!["-a"];
            args.extend_from_slice(flags);
            args.extend(["f", alias]);
            let (code, text) = run(tmp.path(), &args, "threads");
            let removing = flags.contains(&"--remove-source-files");
            if removing && alias == "alias" && flags.contains(&"-I") && !flags.contains(&"--inplace") {
                assert_eq!(code, 0, "{args:?}: {text}");
                assert!(!tmp.path().join("f").exists(), "{args:?}");
                assert!(fs::read(tmp.path().join("alias")).unwrap() == data, "{args:?}: alias changed");
                continue;
            }
            assert_eq!(code, if removing { barqsync::exit::PARTIAL } else { 0 }, "{args:?}: {text}");
            if removing {
                assert!(text.contains("Skipping sender remove of destination file"), "{args:?}: {text}");
            }
            for path in ["f", "alias"] {
                assert!(fs::read(tmp.path().join(path)).unwrap() == data, "{args:?}: {path} changed");
            }
            if flags.contains(&"--inplace") {
                assert_eq!(fs::metadata(tmp.path().join("alias")).unwrap().ino(), inode, "{args:?}");
            }
        }
    }
}

#[test]
fn directory_self_sync_cannot_remove_its_files() {
    for scanner in ["--scanner=dirtask", "--scanner=jwalk"] {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("src/sub")).unwrap();
        fs::write(tmp.path().join("src/sub/f"), b"valuable").unwrap();
        let (code, text) = run(tmp.path(), &["-a", scanner, "--remove-source-files", "src/", "src/"], "threads");
        assert_eq!(code, barqsync::exit::PARTIAL, "{text}");
        assert_eq!(fs::read(tmp.path().join("src/sub/f")).unwrap(), b"valuable");
    }
}

/// Syncing a tree onto itself, or onto a tree of hard links to it, changes nothing and is
/// no error (rsync's quick check finds every file up to date); `--inplace -I` writes
/// nothing into the shared files; `--remove-source-files` keeps them.
#[test]
fn trees_sharing_files_with_the_source_are_left_intact() {
    for scanner in ["--scanner=dirtask", "--scanner=jwalk"] {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("src/sub")).unwrap();
        fs::create_dir_all(tmp.path().join("dst/sub")).unwrap();
        for (f, data) in [("a", "valuable a"), ("sub/b", "valuable b")] {
            fs::write(tmp.path().join("src").join(f), data).unwrap();
            fs::hard_link(tmp.path().join("src").join(f), tmp.path().join("dst").join(f)).unwrap();
        }
        let intact = || {
            assert_eq!(fs::read(tmp.path().join("src/a")).unwrap(), b"valuable a");
            assert_eq!(fs::read(tmp.path().join("src/sub/b")).unwrap(), b"valuable b");
        };
        for args in [&["-a", scanner, "src/", "src/"][..], &["-a", scanner, "src/", "dst/"], &["-a", scanner, "--inplace", "-I", "src/", "dst/"]] {
            let (code, text) = run(tmp.path(), args, "threads");
            assert_eq!(code, 0, "{args:?}: {text}");
            intact();
        }
        let (code, text) = run(tmp.path(), &["-a", scanner, "--remove-source-files", "src/", "dst/"], "threads");
        assert_eq!(code, barqsync::exit::PARTIAL, "{text}");
        intact();
    }
}

/// The in-place copy checks the files it opened, so another name for the source is caught
/// too: nothing is written (the file already holds the data), and nothing is truncated.
#[test]
fn copy_into_existing_never_truncates_its_source() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let alias = tmp.path().join("alias");
    fs::write(&src, b"valuable").unwrap();
    fs::hard_link(&src, &alias).unwrap();
    fsops::copy_into_existing(&fs::File::open(&src).unwrap(), &alias, 8).unwrap();
    assert_eq!(fs::read(&src).unwrap(), b"valuable");
    fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&src, &alias).unwrap();
    fsops::copy_into_existing(&fs::File::open(&src).unwrap(), &alias, 8).unwrap();
    assert_eq!(fs::read(&src).unwrap(), b"valuable");
    fs::remove_file(&alias).unwrap();
    fs::write(&alias, b"old content that must be truncated").unwrap();
    fsops::copy_into_existing(&fs::File::open(&src).unwrap(), &alias, 8).unwrap();
    assert_eq!(fs::read(&alias).unwrap(), b"valuable");
}

#[test]
fn failed_backups_preserve_destination_for_local_push_and_pull() {
    for direction in ["local", "push", "pull"] {
        for backend in ["threads", "tokio"] {
            for flags in [vec![], vec!["--inplace"], vec!["--no-whole-file"], vec!["--io-uring"]] {
                for size in [8, 2 << 20] {
                    let tmp = tempfile::tempdir().unwrap();
                    fake_ssh(tmp.path());
                    fs::create_dir(tmp.path().join("src")).unwrap();
                    fs::create_dir(tmp.path().join("dst")).unwrap();
                    // Mostly the old data, so the large file goes as a delta where deltas are on.
                    let old: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
                    let mut new = old.clone();
                    new[size / 2] ^= 0xff;
                    fs::write(tmp.path().join("src/f"), &new).unwrap();
                    fs::write(tmp.path().join("dst/f"), &old).unwrap();
                    fs::write(tmp.path().join("dst/blocked"), b"not a directory").unwrap();
                    let (src, dst) = match direction {
                        "push" => ("src/", "fake:dst/"),
                        "pull" => ("fake:src/", "dst/"),
                        _ => ("src/", "dst/"),
                    };
                    let mut args = vec!["-aI", "-b", "--backup-dir=blocked/sub", "-e", "./fake-ssh",
                        "--remote-path", env!("CARGO_BIN_EXE_barqsync")];
                    args.extend_from_slice(&flags);
                    args.extend([src, dst]);
                    let (code, text) = run(tmp.path(), &args, backend);
                    assert_eq!(code, barqsync::exit::PARTIAL, "{direction} {backend} {flags:?}: {text}");
                    assert!(fs::read(tmp.path().join("dst/f")).unwrap() == old, "{direction} {backend} {flags:?} {size}");
                    assert!(fs::read(tmp.path().join("src/f")).unwrap() == new);
                }
            }
        }
    }
}

#[test]
fn backups_preserve_symlinks_and_special_files() {
    for special in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::create_dir(tmp.path().join("dst")).unwrap();
        if special {
            fsops::mknod(&tmp.path().join("src/f"), barqsync::entry::Kind::Fifo, 0o600, 0).unwrap();
        } else {
            std::os::unix::fs::symlink("new-target", tmp.path().join("src/f")).unwrap();
        }
        std::os::unix::fs::symlink("old-target", tmp.path().join("dst/f")).unwrap();
        fs::write(tmp.path().join("dst/blocked"), b"block").unwrap();
        let (code, text) = run(tmp.path(), &["-a", "-b", "--backup-dir=blocked/sub", "src/", "dst/"], "threads");
        assert_eq!(code, barqsync::exit::PARTIAL, "{text}");
        assert_eq!(fs::read_link(tmp.path().join("dst/f")).unwrap(), Path::new("old-target"));
        let (code, text) = run(tmp.path(), &["-a", "-b", "src/", "dst/"], "threads");
        assert_eq!(code, 0, "{text}");
        assert_eq!(fs::read_link(tmp.path().join("dst/f~")).unwrap(), Path::new("old-target"));
        if special {
            assert!(fs::symlink_metadata(tmp.path().join("dst/f")).unwrap().file_type().is_fifo());
        } else {
            assert_eq!(fs::read_link(tmp.path().join("dst/f")).unwrap(), Path::new("new-target"));
        }
    }
}

/// Hard-link followers and special files replace an existing file only once it is backed
/// up (rsync backs them up too), and not at all when that fails.
#[test]
fn hard_link_and_special_replacements_are_backed_up_first() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
    fs::create_dir(&src).unwrap();
    fs::create_dir(&dst).unwrap();
    fs::write(src.join("a"), b"A").unwrap();
    fs::hard_link(src.join("a"), src.join("b")).unwrap();
    fsops::mknod(&src.join("p"), barqsync::entry::Kind::Fifo, 0o600, 0).unwrap();
    for f in ["b", "p"] {
        fs::write(dst.join(f), format!("old {f}")).unwrap();
    }
    fs::write(dst.join("blocked"), b"block").unwrap();
    let (code, text) = run(tmp.path(), &["-aH", "--specials", "--backup-dir=blocked/sub", "src/", "dst/"], "threads");
    assert_eq!(code, barqsync::exit::PARTIAL, "{text}");
    for f in ["b", "p"] {
        assert_eq!(fs::read(dst.join(f)).unwrap(), format!("old {f}").as_bytes(), "{f} replaced without a backup");
    }
    let (code, text) = run(tmp.path(), &["-aHb", "--specials", "src/", "dst/"], "threads");
    assert_eq!(code, 0, "{text}");
    for f in ["b", "p"] {
        assert_eq!(fs::read(dst.join(format!("{f}~"))).unwrap(), format!("old {f}").as_bytes(), "{f}");
    }
    assert_eq!(fs::metadata(dst.join("b")).unwrap().ino(), fs::metadata(dst.join("a")).unwrap().ino());
    assert!(fs::symlink_metadata(dst.join("p")).unwrap().file_type().is_fifo());
}

/// A backup name that is already a hard link to the file (left by an earlier run that
/// failed after backing up) holds that version, so the transfer goes ahead (with
/// `--inplace` the backup becomes a copy first). A directory in its way stops the
/// transfer and stays as it was. No temp file is left behind either way.
#[test]
fn backup_alias_and_failed_commit_preserve_original_and_prior_backup() {
    for (alias, inplace) in [(true, false), (true, true), (false, false)] {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::create_dir(tmp.path().join("dst")).unwrap();
        fs::write(tmp.path().join("src/f"), b"NEW").unwrap();
        fs::write(tmp.path().join("dst/f"), b"original").unwrap();
        if alias {
            fs::hard_link(tmp.path().join("dst/f"), tmp.path().join("dst/f~")).unwrap();
        } else {
            fs::create_dir(tmp.path().join("dst/f~")).unwrap();
            fs::write(tmp.path().join("dst/f~/previous"), b"prior backup").unwrap();
        }
        let args: &[&str] = if inplace { &["-aI", "-b", "--inplace", "src/", "dst/"] } else { &["-aI", "-b", "src/", "dst/"] };
        let (code, text) = run(tmp.path(), args, "threads");
        if alias {
            assert_eq!(code, 0, "inplace={inplace}: {text}");
            assert_eq!(fs::read(tmp.path().join("dst/f")).unwrap(), b"NEW");
            assert_eq!(fs::read(tmp.path().join("dst/f~")).unwrap(), b"original", "inplace={inplace}");
        } else {
            assert_eq!(code, barqsync::exit::PARTIAL, "{text}");
            assert_eq!(fs::read(tmp.path().join("dst/f")).unwrap(), b"original");
            assert_eq!(fs::read(tmp.path().join("dst/f~/previous")).unwrap(), b"prior backup");
        }
        assert_eq!(fs::read_dir(tmp.path().join("dst")).unwrap().count(), 2, "temporary backup leaked");
    }
}

#[test]
fn failed_backup_copy_keeps_prior_backup_and_successful_retry_replaces_it() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("SKIP: root bypasses the unreadable-file backup fault");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join("src")).unwrap();
    fs::create_dir(tmp.path().join("dst")).unwrap();
    fs::write(tmp.path().join("src/f"), b"new version").unwrap();
    fs::write(tmp.path().join("dst/f"), b"original").unwrap();
    fs::write(tmp.path().join("dst/f~"), b"prior backup").unwrap();
    fs::set_permissions(tmp.path().join("dst/f"), fs::Permissions::from_mode(0o0)).unwrap();
    let args = ["-aI", "-b", "--inplace", "src/", "dst/"];
    let (code, text) = run(tmp.path(), &args, "threads");
    fs::set_permissions(tmp.path().join("dst/f"), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(code, barqsync::exit::PARTIAL, "{text}");
    assert_eq!(fs::read(tmp.path().join("dst/f")).unwrap(), b"original");
    assert_eq!(fs::read(tmp.path().join("dst/f~")).unwrap(), b"prior backup");
    let (code, text) = run(tmp.path(), &args, "threads");
    assert_eq!(code, 0, "{text}");
    assert_eq!(fs::read(tmp.path().join("dst/f")).unwrap(), b"new version");
    assert_eq!(fs::read(tmp.path().join("dst/f~")).unwrap(), b"original");
}

/// A backup must not replace the source it is about to copy (`-b f~ f`) or the file
/// itself (an empty suffix with a backup dir leading back to the same directory).
#[test]
fn a_backup_never_replaces_the_source_or_the_file_itself() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("h~"), "SRC").unwrap();
    fs::write(tmp.path().join("h"), "OLD").unwrap();
    let (code, text) = run(tmp.path(), &["-a", "-b", "-I", "h~", "h"], "threads");
    assert_eq!(code, barqsync::exit::PARTIAL, "{text}");
    assert_eq!(fs::read(tmp.path().join("h~")).unwrap(), b"SRC");
    assert_eq!(fs::read(tmp.path().join("h")).unwrap(), b"OLD");

    fs::create_dir_all(tmp.path().join("dst/sub")).unwrap();
    fs::write(tmp.path().join("dst/f"), "OLD").unwrap();
    fs::write(tmp.path().join("f"), "NEW").unwrap();
    for dir in ["--backup-dir=.", "--backup-dir=sub/.."] {
        let (code, text) = run(tmp.path(), &["-aI", dir, "--suffix=", "f", "dst/"], "threads");
        assert_eq!(code, barqsync::exit::PARTIAL, "{dir}: {text}");
        assert_eq!(fs::read(tmp.path().join("dst/f")).unwrap(), b"OLD", "{dir}");
    }
    // Without a backup dir, an empty suffix is refused up front, as rsync does.
    let (code, text) = run(tmp.path(), &["-a", "-b", "--suffix=", "-I", "f", "f"], "threads");
    assert_eq!(code, barqsync::exit::SYNTAX, "{text}");
    assert_eq!(fs::read(tmp.path().join("f")).unwrap(), b"NEW");
}

#[test]
fn remove_source_requires_authorization_and_trusted_unchanged_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("f");
    fs::write(&path, b"OLD!").unwrap();
    fsops::set_mtime(&path, 1_600_000_000, 100).unwrap();
    let meta = fsops::lstat(&path).unwrap();
    let mut opts = Options::parse_from(["barqsync", "-a", "s/", "d/"]).unwrap();
    assert!(remove_source(&opts, &path, &meta, None).is_err());
    opts.remove_source_files = true;
    opts.dry_run = true;
    assert!(remove_source(&opts, &path, &meta, None).is_err());
    opts.dry_run = false;
    fs::write(&path, b"NEW!").unwrap();
    fsops::set_mtime(&path, meta.mtime, 900_000_000).unwrap();
    assert!(remove_source(&opts, &path, &meta, None).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"NEW!");
    // Holding the old inode prevents reuse, making this replacement deterministic.
    let held = File::open(&path).unwrap();
    fs::remove_file(&path).unwrap();
    fs::write(&path, b"NEW!").unwrap();
    fsops::set_mtime(&path, meta.mtime, meta.mtime_ns).unwrap();
    assert!(remove_source(&opts, &path, &meta, None).is_err());
    drop(held);
    // A symlink swapped for a file of the same size.
    let link = tmp.path().join("l");
    std::os::unix::fs::symlink("abcd", &link).unwrap();
    let listed = fsops::lstat(&link).unwrap();
    fs::remove_file(&link).unwrap();
    fs::write(&link, b"abcd").unwrap();
    assert!(remove_source(&opts, &link, &listed, None).is_err());
    assert!(link.exists());
    // The destination itself.
    let current = fsops::lstat(&path).unwrap();
    let err = remove_source(&opts, &path, &current, Some(&path)).unwrap_err();
    assert!(String::from_utf8_lossy(&err).contains("destination file"));
    remove_source(&opts, &path, &current, None).unwrap();
    assert!(!path.exists());
}

/// Authorized removal still removes everything sent: files, hard links and symlinks,
/// locally, in a push and in a pull.
#[test]
fn authorized_removal_still_removes_what_was_sent() {
    for direction in ["local", "push", "pull"] {
        let tmp = tempfile::tempdir().unwrap();
        fake_ssh(tmp.path());
        let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a"), b"A").unwrap();
        fs::write(src.join("sub/c"), b"C").unwrap();
        fs::hard_link(src.join("a"), src.join("b")).unwrap();
        std::os::unix::fs::symlink("a", src.join("l")).unwrap();
        let (from, to) = match direction {
            "push" => ("src/", "fake:dst/"),
            "pull" => ("fake:src/", "dst/"),
            _ => ("src/", "dst/"),
        };
        let args = ["-aH", "--remove-source-files", "-e", "./fake-ssh", "--remote-path", env!("CARGO_BIN_EXE_barqsync"), from, to];
        let (code, text) = run(tmp.path(), &args, "threads");
        assert_eq!(code, 0, "{direction}: {text}");
        for f in ["a", "b", "l", "sub/c"] {
            assert!(fs::symlink_metadata(src.join(f)).is_err(), "{direction}: src/{f} is still there");
        }
        assert!(src.join("sub").is_dir());
        assert_eq!(fs::read(dst.join("b")).unwrap(), b"A");
        assert_eq!(fs::metadata(dst.join("a")).unwrap().ino(), fs::metadata(dst.join("b")).unwrap().ino(), "{direction}");
        assert_eq!(fs::read_link(dst.join("l")).unwrap(), Path::new("a"));
    }
}
