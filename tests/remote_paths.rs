//! CLI locations must preserve filenames and never turn a host into SSH options.
mod support;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use support::{fake_ssh, run};

#[test]
fn ipv6_push_pull_preserves_paths_and_quotes_the_program() {
    check_round_trip("source café '$(touch INJECTED)".as_bytes(), "file café 'quote'".as_bytes(),
        "destination café '$(touch INJECTED)".as_bytes());
}

// APFS rejects invalid UTF-8 filenames; the parser's raw-byte cases run everywhere.
#[cfg(target_os = "linux")]
#[test]
fn ipv6_push_pull_preserves_non_utf8_paths() {
    check_round_trip(b"source \xff '$(touch INJECTED)", b"file \xfe 'quote'", b"destination \xfd '$(touch INJECTED)");
}

fn check_round_trip(source_name: &[u8], file_name: &[u8], destination_name: &[u8]) {
    for backend in ["threads", "tokio"] {
        for wire in ["postcard", "rkyv"] {
            let tmp = tempfile::tempdir().unwrap();
            fake_ssh(tmp.path());
            fs::write(tmp.path().join("fake-ssh"),
                "#!/bin/sh\nprintf '%s\\n' \"$1\" > ssh-host\nshift\nexec /bin/sh -c \"$*\"\n").unwrap();
            let bin = tmp.path().join("bin '$(touch EXECUTED)");
            fs::create_dir(&bin).unwrap();
            let program = bin.join("barqsync");
            std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_barqsync"), &program).unwrap();
            let src = OsStr::from_bytes(source_name);
            fs::create_dir(tmp.path().join(src)).unwrap();
            let filename = OsStr::from_bytes(file_name);
            fs::write(tmp.path().join(src).join(filename), b"literal path bytes").unwrap();
            let dst = tmp.path().join(OsStr::from_bytes(destination_name));
            let mut remote = b"user@[::1]:".to_vec();
            remote.extend_from_slice(dst.as_os_str().as_bytes());
            remote.push(b'/');
            let remote = OsString::from_vec(remote);
            let mut source = src.as_bytes().to_vec();
            source.push(b'/');
            let source = OsString::from_vec(source);
            let base: Vec<OsString> = vec!["-a".into(), "--threads=2".into(), format!("--wire={wire}").into(),
                "-e".into(), "./fake-ssh".into(), "--remote-path".into(), program.into_os_string()];
            for (from, to) in [(&source, &remote), (&remote, &OsString::from("back/"))] {
                let mut args = base.clone();
                args.extend([from.clone(), to.clone()]);
                let (code, output) = run(tmp.path(), &args, backend);
                assert_eq!(code, 0, "{backend} {wire}: {output}");
                assert_eq!(fs::read(tmp.path().join("ssh-host")).unwrap(), b"user@::1\n");
            }
            for directory in [&dst, &tmp.path().join("back")] {
                assert_eq!(fs::read(directory.join(filename)).unwrap(), b"literal path bytes");
            }
            assert!(!tmp.path().join("INJECTED").exists());
            assert!(!tmp.path().join("EXECUTED").exists());
        }
    }
}

#[test]
fn malformed_hosts_and_daemon_forms_fail_before_remote_shell() {
    let tmp = tempfile::tempdir().unwrap();
    fake_ssh(tmp.path());
    fs::write(tmp.path().join("fake-ssh"), "#!/bin/sh\ntouch ssh-called\nexit 99\n").unwrap();
    fs::write(tmp.path().join("source"), b"preserve me").unwrap();
    let malformed = ["-oProxyCommand=touch:target", "user@-option:target", "@host:target", "user@:target",
        "a@b@host:target", "host name:target", "host;touch:target", "host\nname:target",
        "[::1:target", "[::1]junk:target", "[invalid]:target", "[fe80::1%]:target",
        "host::module", "[::1]::module", "rsync://host/module"];
    for bad in malformed {
        for (from, to) in [("source", bad), (bad, "target")] {
            let (code, output) = run(tmp.path(), &["-a", "--threads=2", "-e", "./fake-ssh", "--", from, to], "threads");
            assert_eq!(code, barqsync::exit::SYNTAX, "{bad:?}: {output}");
            assert!(output.contains("invalid remote location"), "{output}");
        }
    }
    assert!(!tmp.path().join("ssh-called").exists());
    assert!(!tmp.path().join("target").exists());
    assert_eq!(fs::read(tmp.path().join("source")).unwrap(), b"preserve me");
}
