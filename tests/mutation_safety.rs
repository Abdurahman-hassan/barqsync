//! Destructive operations that must not destroy what they work from: a source that is
//! also the destination (`f f`, another path to it, a hard link), backups that fail or
//! would land on the file itself or on the source, and the `--remove-source-files`
//! check of whether a source changed since it was listed.

mod common;

use barqsync::{cli::Options, fsops, local::flags::remove_source};
use common::*;
use std::fs::{self, File};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;

fn read(p: impl AsRef<Path>) -> Vec<u8> {
    fs::read(p).unwrap()
}

/// `f f`, `f ./f` and `f alias` (a hard link): in-place writes leave the data alone, and
/// `--remove-source-files` refuses to remove the destination, as rsync does. (Rewritten
/// with `-I` and no `--inplace`, `alias` becomes a copy, and then `f` can go.)
#[test]
fn a_source_that_is_the_destination_survives() {
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
            let mut args = vec!["-a"];
            args.extend_from_slice(flags);
            args.extend(["f", alias]);
            let out = barqsync(tmp.path(), &args);
            let removing = flags.contains(&"--remove-source-files");
            let copied = alias == "alias" && flags.contains(&"-I") && !flags.contains(&"--inplace");
            if removing && copied {
                assert_eq!(out.code, 0, "{args:?}: {}", out.stderr);
                assert!(!tmp.path().join("f").exists(), "{args:?}");
                assert!(read(tmp.path().join("alias")) == data, "{args:?}: alias changed");
                continue;
            }
            assert_eq!(out.code, if removing { 23 } else { 0 }, "{args:?}: {}", out.stderr);
            if removing {
                assert!(out.stderr.contains("Skipping sender remove of destination file"), "{args:?}: {}", out.stderr);
            }
            for name in ["f", "alias"] {
                assert!(read(tmp.path().join(name)) == data, "{args:?}: {name} changed");
            }
        }
    }
}

/// The same for trees: a directory synced onto itself, and a destination tree whose
/// files are hard links to the source's.
#[test]
fn a_source_tree_that_is_the_destination_survives() {
    for scanner in ["dirtask", "jwalk"] {
        let tmp = tempfile::tempdir().unwrap();
        Tree::new(tmp.path().join("src")).file("a", "valuable a").file("sub/b", "valuable b").symlink("l", "a");
        let scan = format!("--scanner={scanner}");
        let out = barqsync(tmp.path(), &["-a", &scan, "--remove-source-files", "src/", "src/"]);
        assert_eq!(out.code, 23, "{scanner}: {}", out.stderr);
        assert_eq!(read(tmp.path().join("src/a")), b"valuable a");
        assert_eq!(read(tmp.path().join("src/sub/b")), b"valuable b");
        assert!(fs::symlink_metadata(tmp.path().join("src/l")).is_ok());

        fs::create_dir_all(tmp.path().join("dst/sub")).unwrap();
        for f in ["a", "sub/b"] {
            fs::hard_link(tmp.path().join("src").join(f), tmp.path().join("dst").join(f)).unwrap();
        }
        let out = barqsync(tmp.path(), &["-a", &scan, "--inplace", "-I", "src/", "dst/"]);
        assert_eq!(out.code, 0, "{scanner}: {}", out.stderr);
        assert_eq!(read(tmp.path().join("src/a")), b"valuable a");
        assert_eq!(read(tmp.path().join("src/sub/b")), b"valuable b");
        let out = barqsync(tmp.path(), &["-a", &scan, "--remove-source-files", "src/", "dst/"]);
        assert_eq!(out.code, 23, "{scanner}: {}", out.stderr);
        assert_eq!(read(tmp.path().join("dst/a")), b"valuable a");
        assert_eq!(read(tmp.path().join("src/sub/b")), b"valuable b");
    }
}

/// The in-place copy checks the files it opened, so a link to the source is caught too.
#[test]
fn copy_into_existing_never_truncates_its_source() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let alias = tmp.path().join("alias");
    fs::write(&src, b"valuable").unwrap();
    fs::hard_link(&src, &alias).unwrap();
    fsops::copy_into_existing(&src, &alias, 8).unwrap();
    assert_eq!(read(&src), b"valuable");
    fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&src, &alias).unwrap();
    fsops::copy_into_existing(&src, &alias, 8).unwrap();
    assert_eq!(read(&src), b"valuable");
    // An ordinary in-place copy still replaces the old data and length.
    fs::remove_file(&alias).unwrap();
    fs::write(&alias, b"old content, longer than the new").unwrap();
    fsops::copy_into_existing(&src, &alias, 8).unwrap();
    assert_eq!(read(&alias), b"valuable");
}

/// A backup that can't be made (its directory is a file) stops the replacement: local,
/// push and pull, for small files, large ones, deltas and in-place writes.
#[test]
fn a_failed_backup_keeps_the_destination() {
    let mut variants = vec![&[][..], &["--inplace"], &["--no-whole-file"], &["--whole-file"]];
    if cfg!(target_os = "linux") {
        variants.push(&["--io-uring"]);
    }
    for direction in ["local", "push", "pull"] {
        for flags in &variants {
            for size in [8, 2 << 20] {
                let tmp = tempfile::tempdir().unwrap();
                // Mostly the old data, so the large file goes as a delta where deltas are on.
                let old: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
                let mut new = old.clone();
                new[size / 2] ^= 0xff;
                Tree::new(tmp.path().join("src")).file("f", &new);
                Tree::new(tmp.path().join("dst")).file("f", &old).file("blocked", "not a directory");
                let (src, dst) = match direction {
                    "push" => ("src/", "{host}dst/"),
                    "pull" => ("{host}src/", "dst/"),
                    _ => ("src/", "dst/"),
                };
                let mut args = vec!["-aI", "--backup-dir=blocked/sub"];
                args.extend_from_slice(flags);
                args.extend([src, dst]);
                let out = if direction == "local" { barqsync(tmp.path(), &args) } else { barqsync_remote(tmp.path(), &args) };
                let what = format!("{direction} {flags:?} {size}");
                assert_eq!(out.code, 23, "{what}: {}", out.stderr);
                assert!(out.stderr.contains("make_backup"), "{what}: {}", out.stderr);
                assert!(read(tmp.path().join("dst/f")) == old, "{what}: replaced without a backup");
                assert!(read(tmp.path().join("src/f")) == new, "{what}");
            }
        }
    }
}

/// Symlinks, special files and hard-link followers replace an existing file only once
/// it is backed up (rsync backs them up too), and not at all when that fails.
#[test]
fn every_kind_of_replacement_is_backed_up_first() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", "A").symlink("l", "a").fifo("p");
    fs::hard_link(tmp.path().join("src/a"), tmp.path().join("src/b")).unwrap();
    let dst = Tree::new(tmp.path().join("dst"));
    dst.file("b", "old b").file("l", "old l").file("p", "old p").file("blocked", "x");
    let out = barqsync(tmp.path(), &["-aH", "--specials", "--backup-dir=blocked/sub", "src/", "dst/"]);
    assert_eq!(out.code, 23, "{}", out.stderr);
    for f in ["b", "l", "p"] {
        assert_eq!(read(dst.path(f)), format!("old {f}").as_bytes(), "{f} replaced without a backup");
    }
    let out = barqsync(tmp.path(), &["-aHb", "--specials", "src/", "dst/"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    for f in ["b", "l", "p"] {
        assert_eq!(read(dst.path(&format!("{f}~"))), format!("old {f}").as_bytes(), "{f}");
    }
    assert_eq!(fs::metadata(dst.path("b")).unwrap().ino(), fs::metadata(dst.path("a")).unwrap().ino());
    assert_eq!(fs::read_link(dst.path("l")).unwrap(), Path::new("a"));
    assert!(fs::symlink_metadata(dst.path("p")).unwrap().file_type().is_fifo());
}

/// The previous backup is replaced only by a complete new one: a directory in the way or
/// an unreadable file leaves it (and the destination) alone, and a retry then works.
#[test]
fn a_failed_backup_keeps_the_previous_backup() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("f", "new version");
    let dst = Tree::new(tmp.path().join("dst"));
    dst.file("f", "original").file("f~/previous", "prior backup");
    let out = barqsync(tmp.path(), &["-aI", "-b", "src/", "dst/"]);
    assert_eq!(out.code, 23, "{}", out.stderr);
    assert_eq!(read(dst.path("f")), b"original");
    assert_eq!(read(dst.path("f~/previous")), b"prior backup");
    assert_eq!(fs::read_dir(&dst.root).unwrap().count(), 2, "a temp file was left behind");

    if unsafe { libc::geteuid() } == 0 {
        return; // root reads the unreadable file anyway
    }
    dst.remove("f~").file("f~", "prior backup").chmod("f", 0);
    // --inplace copies the old version (a hard link would change with it).
    let args = ["-aI", "-b", "--inplace", "src/", "dst/"];
    let out = barqsync(tmp.path(), &args);
    dst.chmod("f", 0o644);
    assert_eq!(out.code, 23, "{}", out.stderr);
    assert_eq!(read(dst.path("f")), b"original");
    assert_eq!(read(dst.path("f~")), b"prior backup");
    assert_eq!(fs::read_dir(&dst.root).unwrap().count(), 2, "a temp file was left behind");
    let out = barqsync(tmp.path(), &args);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(read(dst.path("f")), b"new version");
    assert_eq!(read(dst.path("f~")), b"original");
}

/// A backup name that is already a hard link to the file (left by an earlier run that
/// failed after backing up) holds that version: the transfer goes ahead.
#[test]
fn a_backup_that_is_already_a_link_is_kept() {
    for inplace in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        Tree::new(tmp.path().join("src")).file("f", "NEW");
        let dst = Tree::new(tmp.path().join("dst"));
        dst.file("f", "original");
        fs::hard_link(dst.path("f"), dst.path("f~")).unwrap();
        let args: &[&str] = if inplace { &["-aI", "-b", "--inplace", "src/", "dst/"] } else { &["-aI", "-b", "src/", "dst/"] };
        let out = barqsync(tmp.path(), args);
        assert_eq!(out.code, 0, "inplace={inplace}: {}", out.stderr);
        assert_eq!(read(dst.path("f")), b"NEW");
        assert_eq!(read(dst.path("f~")), b"original", "inplace={inplace}");
    }
}

/// A backup must not replace the source it is about to copy (`-b f~ f`) or the file
/// itself (an empty suffix with a backup dir leading back to the same directory).
#[test]
fn a_backup_never_lands_on_the_source_or_the_file_itself() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("h~"), "SRC").unwrap();
    fs::write(tmp.path().join("h"), "OLD").unwrap();
    let out = barqsync(tmp.path(), &["-a", "-b", "-I", "h~", "h"]);
    assert_eq!(out.code, 23, "{}", out.stderr);
    assert_eq!(read(tmp.path().join("h~")), b"SRC");
    assert_eq!(read(tmp.path().join("h")), b"OLD");

    let dst = Tree::new(tmp.path().join("dst"));
    dst.file("f", "OLD").dir("sub");
    fs::write(tmp.path().join("f"), "NEW").unwrap();
    for dir in [".", "sub/.."] {
        let out = barqsync(tmp.path(), &["-aI", &format!("--backup-dir={dir}"), "--suffix=", "f", "dst/"]);
        assert_eq!(out.code, 23, "{dir}: {}", out.stderr);
        assert_eq!(read(dst.path("f")), b"OLD", "{dir}");
    }
    // Without a backup dir, an empty suffix is refused up front, as rsync does.
    let out = barqsync(tmp.path(), &["-a", "-b", "--suffix=", "-I", "f", "f"]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert_eq!(read(tmp.path().join("f")), b"NEW");
}

/// `--remove-source-files` removes a source only when asked to (not in a dry run), and
/// only if it is still the file that was listed: the same type, device and inode, size
/// and modification time to the nanosecond.
#[test]
fn source_removal_checks_what_was_listed() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("f");
    fs::write(&path, b"OLD!").unwrap();
    fsops::set_mtime(&path, 1_600_000_000, 100).unwrap();
    let listed = fsops::lstat(&path).unwrap();
    let mut opts = Options::parse_from(["barqsync", "-a", "s/", "d/"]).unwrap();
    assert!(remove_source(&opts, &path, &listed, None).is_err());
    opts.remove_source_files = true;
    opts.dry_run = true;
    assert!(remove_source(&opts, &path, &listed, None).is_err());
    opts.dry_run = false;
    assert!(path.exists());

    // Changed within the same second, at the same size.
    fs::write(&path, b"NEW!").unwrap();
    fsops::set_mtime(&path, listed.mtime, 900_000_000).unwrap();
    assert!(remove_source(&opts, &path, &listed, None).is_err());
    assert_eq!(read(&path), b"NEW!");

    // Replaced by another file with the same size and time. Holding the old one open
    // keeps its inode from being reused.
    fsops::set_mtime(&path, listed.mtime, listed.mtime_ns).unwrap();
    let held = File::open(&path).unwrap();
    let listed = fsops::lstat(&path).unwrap();
    fs::remove_file(&path).unwrap();
    fs::write(&path, b"NEW!").unwrap();
    fsops::set_mtime(&path, listed.mtime, listed.mtime_ns).unwrap();
    assert!(remove_source(&opts, &path, &listed, None).is_err());
    assert!(path.exists());
    drop(held);

    // A symlink swapped for a file.
    let link = tmp.path().join("l");
    std::os::unix::fs::symlink("abcd", &link).unwrap();
    let listed_link = fsops::lstat(&link).unwrap();
    fs::remove_file(&link).unwrap();
    fs::write(&link, b"abcd").unwrap();
    assert!(remove_source(&opts, &link, &listed_link, None).is_err());
    assert!(link.exists());

    // The destination itself.
    let current = fsops::lstat(&path).unwrap();
    let err = remove_source(&opts, &path, &current, Some(&path)).unwrap_err();
    assert!(String::from_utf8_lossy(&err).contains("destination file"));
    assert!(path.exists());

    remove_source(&opts, &path, &current, None).unwrap();
    assert!(!path.exists());
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755)).unwrap();
}

/// Authorized removal still removes everything sent: files, hard links and symlinks,
/// locally, in a push and in a pull.
#[test]
fn authorized_removal_still_removes_what_was_sent() {
    for direction in ["local", "push", "pull"] {
        let tmp = tempfile::tempdir().unwrap();
        Tree::new(tmp.path().join("src")).file("a", "A").file("sub/c", "C").symlink("l", "a");
        fs::hard_link(tmp.path().join("src/a"), tmp.path().join("src/b")).unwrap();
        let (src, dst) = match direction {
            "push" => ("src/", "{host}dst/"),
            "pull" => ("{host}src/", "dst/"),
            _ => ("src/", "dst/"),
        };
        let args = ["-aH", "--remove-source-files", src, dst];
        let out = if direction == "local" { barqsync(tmp.path(), &args) } else { barqsync_remote(tmp.path(), &args) };
        assert_eq!(out.code, 0, "{direction}: {}", out.stderr);
        for f in ["a", "b", "l", "sub/c"] {
            assert!(fs::symlink_metadata(tmp.path().join("src").join(f)).is_err(), "{direction}: src/{f} is still there");
        }
        assert!(tmp.path().join("src/sub").is_dir());
        let dst = tmp.path().join("dst");
        assert_eq!(read(dst.join("b")), b"A");
        assert_eq!(fs::metadata(dst.join("a")).unwrap().ino(), fs::metadata(dst.join("b")).unwrap().ino(), "{direction}");
        assert_eq!(fs::read_link(dst.join("l")).unwrap(), Path::new("a"));
    }
}
