//! Protocol coordination must leave the configured workers available for I/O.
mod support;
use support::{fake_ssh, run};
use std::fs;

#[test]
fn one_and_two_workers_finish_push_pull_packs_deltas_and_noops() {
    for threads in ["--threads=1", "--threads=2"] {
        for backend in ["threads", "tokio"] {
            for direction in ["push", "pull"] {
                for wire in ["--wire=postcard", "--wire=rkyv"] {
                    let tmp = tempfile::tempdir().unwrap();
                    fake_ssh(tmp.path());
                    fs::create_dir_all(tmp.path().join("src/sub")).unwrap();
                    for i in 0..40 {
                        fs::write(tmp.path().join(format!("src/sub/{i}")), format!("small-{i}")).unwrap();
                    }
                    let mut large = vec![0; 2 << 20];
                    for (i, b) in large.iter_mut().enumerate() {
                        *b = (i.wrapping_mul(37) ^ (i >> 8)) as u8;
                    }
                    fs::write(tmp.path().join("src/large"), &large).unwrap();
                    let (src, dst) = if direction == "push" { ("src/", "fake:dst/") } else { ("fake:src/", "dst/") };
                    let args = ["-a", threads, wire, "--strategy=fixed", "-e", "./fake-ssh",
                        "--remote-path", env!("CARGO_BIN_EXE_barqsync"), src, dst];
                    // Fresh whole-file transfer and packs, then a no-op, then a delta.
                    for phase in 0..3 {
                        if phase == 2 {
                            large[600_000..604_096].fill(b'Z');
                            fs::write(tmp.path().join("src/large"), &large).unwrap();
                            // Avoid second-resolution quick-check ambiguity.
                            barqsync::fsops::set_mtime(&tmp.path().join("src/large"), 1_700_000_000, 0).unwrap();
                        }
                        let (code, text) = run(tmp.path(), &args, backend);
                        assert_eq!(code, 0, "{threads} {backend} {direction} {wire} phase={phase}: {text}");
                        assert_eq!(fs::read(tmp.path().join("dst/large")).unwrap(), large);
                        for i in 0..40 {
                            assert_eq!(fs::read(tmp.path().join(format!("dst/sub/{i}"))).unwrap(), format!("small-{i}").as_bytes());
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn one_worker_removal_waits_for_hard_links_and_deferred_xattrs() {
    for backend in ["threads", "tokio"] {
        for direction in ["push", "pull"] {
            let tmp = tempfile::tempdir().unwrap();
            fake_ssh(tmp.path());
            fs::create_dir(tmp.path().join("src")).unwrap();
            let first = tmp.path().join("src/a");
            fs::write(&first, b"content").unwrap();
            fs::hard_link(&first, tmp.path().join("src/b")).unwrap();
            std::os::unix::fs::symlink("a", tmp.path().join("src/link")).unwrap();
            let (src, dst) = if direction == "push" { ("src/", "fake:dst/") } else { ("fake:src/", "dst/") };
            let base = ["--threads=1", "-e", "./fake-ssh", "--remote-path", env!("CARGO_BIN_EXE_barqsync")];
            let mut args = vec!["-aH"];
            args.extend(base);
            args.extend([src, dst]);
            let (code, text) = run(tmp.path(), &args, backend);
            assert_eq!(code, 0, "{text}");
            let name: &[u8] = if cfg!(target_os = "linux") { b"user.barqsync-test" } else { b"barqsync-test" };
            let value = vec![b'X'; 1024];
            barqsync::fsops::xattr::set(&first, name, &value).unwrap();
            let mut args = vec!["-aHX", "--remove-source-files"];
            args.extend(base);
            args.extend([src, dst]);
            let (code, text) = run(tmp.path(), &args, backend);
            assert_eq!(code, 0, "{backend} {direction}: {text}");
            assert_eq!(fs::read_dir(tmp.path().join("src")).unwrap().count(), 0);
            for file in ["a", "b"] {
                let path = tmp.path().join("dst").join(file);
                assert_eq!(fs::read(&path).unwrap(), b"content");
                assert_eq!(barqsync::fsops::xattr::get(&path, name).unwrap(), Some(value.clone()));
            }
            assert_eq!(fs::read_link(tmp.path().join("dst/link")).unwrap(), std::path::Path::new("a"));
        }
    }
}
