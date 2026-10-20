//! Regressions for the destructive-operation findings in IMPLEMENTATION_PLAN.md.
//! All CLI cases have a deadline and use disposable files, including fake SSH.

mod support;
use support::{fake_ssh, run};

use barqsync::{cli::Options, fsops, local::flags::remove_source};
use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

#[test]
fn same_file_paths_and_hard_links_preserve_source() {
    for scanner in ["--scanner=dirtask", "--scanner=jwalk"] {
        for flags in [
            vec!["--inplace", "-I"],
            vec!["--remove-source-files"],
            vec!["--inplace", "-I", "-b"],
            vec!["--inplace", "-I", "--no-whole-file"],
        ] {
            for alias in ["f", "./f", "alias"] {
                let tmp = tempfile::tempdir().unwrap();
                let data = vec![b'x'; 2 << 20];
                fs::write(tmp.path().join("f"), &data).unwrap();
                fs::hard_link(tmp.path().join("f"), tmp.path().join("alias")).unwrap();
                let inode = fs::metadata(tmp.path().join("f")).unwrap().ino();
                let mut args = vec!["-a", scanner];
                args.extend_from_slice(&flags);
                args.extend(["f", alias]);
                let (code, text) = run(tmp.path(), &args, "threads");
                assert_eq!(code, barqsync::exit::PARTIAL, "{args:?}: {text}");
                assert!(text.contains("same file"), "{args:?}: {text}");
                for path in ["f", "alias"] {
                    assert_eq!(fs::read(tmp.path().join(path)).unwrap(), data);
                    assert_eq!(fs::metadata(tmp.path().join(path)).unwrap().ino(), inode);
                }
                assert!(!tmp.path().join("f~").exists());
                assert!(!tmp.path().join("alias~").exists());
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

#[test]
fn descriptor_guard_precedes_truncation_and_normal_inplace_still_works() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let alias = tmp.path().join("alias");
    fs::write(&src, b"valuable").unwrap();
    fs::hard_link(&src, &alias).unwrap();
    assert!(fsops::copy_into_existing(&src, &alias, 8).is_err());
    assert_eq!(fs::read(&src).unwrap(), b"valuable");
    fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&src, &alias).unwrap();
    assert!(fsops::copy_into_existing(&src, &alias, 8).is_err());
    assert_eq!(fs::read(&src).unwrap(), b"valuable");
    fs::remove_file(&alias).unwrap();
    fs::write(&alias, b"old content that must be truncated").unwrap();
    fsops::copy_into_existing(&src, &alias, 8).unwrap();
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
                    fs::write(tmp.path().join("src/f"), vec![b'N'; size]).unwrap();
                    fs::write(tmp.path().join("dst/f"), b"original").unwrap();
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
                    assert_eq!(fs::read(tmp.path().join("dst/f")).unwrap(), b"original");
                    assert_eq!(fs::read(tmp.path().join("src/f")).unwrap(), vec![b'N'; size]);
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
            use std::os::unix::fs::FileTypeExt;
            assert!(fs::symlink_metadata(tmp.path().join("dst/f")).unwrap().file_type().is_fifo());
        } else {
            assert_eq!(fs::read_link(tmp.path().join("dst/f")).unwrap(), Path::new("new-target"));
        }
    }
}

#[test]
fn backup_alias_and_failed_commit_preserve_original_and_prior_backup() {
    for alias in [true, false] {
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
        let (code, text) = run(tmp.path(), &["-aI", "-b", "src/", "dst/"], "threads");
        assert_eq!(code, barqsync::exit::PARTIAL, "{text}");
        assert_eq!(fs::read(tmp.path().join("dst/f")).unwrap(), b"original");
        if alias {
            assert_eq!(fs::read(tmp.path().join("dst/f~")).unwrap(), b"original");
        } else {
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
    fs::set_permissions(tmp.path().join("dst/f"), fs::Permissions::from_mode(0)).unwrap();
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

#[test]
fn remove_source_requires_authorization_and_trusted_unchanged_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("f");
    fs::write(&path, b"OLD!").unwrap();
    fsops::set_mtime(&path, 1_600_000_000, 100).unwrap();
    let meta = fsops::lstat(&path).unwrap();
    let mut opts = Options::parse_from(["barqsync", "-a", "s/", "d/"]).unwrap();
    assert!(remove_source(&opts, &path, &meta).is_err());
    opts.remove_source_files = true;
    opts.dry_run = true;
    assert!(remove_source(&opts, &path, &meta).is_err());
    opts.dry_run = false;
    fs::write(&path, b"NEW!").unwrap();
    fsops::set_mtime(&path, meta.mtime, 900_000_000).unwrap();
    assert!(remove_source(&opts, &path, &meta).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"NEW!");
    // Holding the old inode prevents reuse, making this replacement deterministic.
    let held = File::open(&path).unwrap();
    fs::remove_file(&path).unwrap();
    fs::write(&path, b"NEW!").unwrap();
    fsops::set_mtime(&path, meta.mtime, meta.mtime_ns).unwrap();
    assert!(remove_source(&opts, &path, &meta).is_err());
    drop(held);
    let current = fsops::lstat(&path).unwrap();
    remove_source(&opts, &path, &current).unwrap();
    assert!(!path.exists());
}
