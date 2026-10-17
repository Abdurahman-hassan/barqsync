//! Opt-in tests against a real second machine over real ssh (key auth). Skipped unless
//! `FASTRSYNC_TEST_REMOTE=user@host` is set; prepare the host with `bench/remote_setup.sh`.
//!
//! - `FASTRSYNC_TEST_REMOTE_BIN`: fastrsync on the host (default `bin/fastrsync`)
//! - `FASTRSYNC_TEST_REMOTE_RSYNC`: rsync 3.x on the host, for the comparison test
//!
//! Each test round-trips a tree through the host (push, then pull back) and compares the
//! result with the source, for every transport; the comparison test also pushes with
//! rsync and checks both tools leave the same tree and print the same items.

mod common;

use common::*;
use std::path::Path;
use std::process::Command;

struct Host {
    addr: String,
    bin: String,
    rsync: Option<String>,
    dir: String,
}

impl Host {
    fn get(name: &str) -> Option<Host> {
        let addr = std::env::var("FASTRSYNC_TEST_REMOTE").ok()?;
        let bin = std::env::var("FASTRSYNC_TEST_REMOTE_BIN").unwrap_or_else(|_| "bin/fastrsync".into());
        let rsync = std::env::var("FASTRSYNC_TEST_REMOTE_RSYNC").ok();
        let dir = format!("/tmp/fastrsync-test-{name}-{}", std::process::id());
        let h = Host { addr, bin, rsync, dir };
        h.ssh(&format!("rm -rf '{0}' && mkdir -p '{0}'", h.dir));
        Some(h)
    }

    fn ssh(&self, cmd: &str) -> String {
        let o = Command::new("ssh").args(["-o", "BatchMode=yes", &self.addr, cmd]).output().unwrap();
        assert!(o.status.success(), "ssh {cmd}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn remote(&self, sub: &str) -> String {
        format!("{}:{}/{sub}", self.addr, self.dir)
    }

    fn fastrsync(&self, cwd: &Path, args: &[&str]) -> RunOut {
        let mut all = vec!["--remote-path".to_string(), self.bin.clone()];
        all.extend(args.iter().map(|a| a.to_string()));
        fastrsync(cwd, &all.iter().map(String::as_str).collect::<Vec<_>>())
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = Command::new("ssh").args(["-o", "BatchMode=yes", &self.addr, &format!("rm -rf '{}'", self.dir)]).status();
    }
}

fn transports() -> Vec<Vec<&'static str>> {
    vec![vec![], vec!["--streams", "4"]]
}

#[test]
fn round_trip_through_real_host() {
    let Some(h) = Host::get("roundtrip") else { return eprintln!("SKIP: FASTRSYNC_TEST_REMOTE not set") };
    let tmp = tempfile::tempdir().unwrap();
    rich_tree(&tmp.path().join("src"));
    for (i, t) in transports().iter().enumerate() {
        let sub = format!("t{i}");
        let mut push = vec!["-a"];
        push.extend(t);
        let target = format!("{}/", h.remote(&sub));
        push.extend(["src/", target.as_str()]);
        let r = h.fastrsync(tmp.path(), &push);
        assert_eq!(r.code, 0, "push {t:?}: {}", r.stderr);
        let back = format!("back{i}");
        let mut pull = vec!["-a"];
        pull.extend(t);
        pull.extend([target.as_str(), back.as_str()]);
        let r = h.fastrsync(tmp.path(), &pull);
        assert_eq!(r.code, 0, "pull {t:?}: {}", r.stderr);
        assert_same_tree(&snapshot(&tmp.path().join("src"), true), &snapshot(&tmp.path().join(&back), true));
    }
}

#[test]
fn delta_over_a_real_link() {
    let Some(h) = Host::get("delta") else { return eprintln!("SKIP: FASTRSYNC_TEST_REMOTE not set") };
    let tmp = tempfile::tempdir().unwrap();
    let t = Tree::new(tmp.path().join("src"));
    let mut data: Vec<u8> = (0..8u32 << 20).map(|i| (i.wrapping_mul(2654435761) >> 7) as u8).collect();
    t.file("big.img", &data).settle_mtimes();
    let target = format!("{}/", h.remote("d"));
    assert_eq!(h.fastrsync(tmp.path(), &["-a", "src/", &target]).code, 0);
    data[3_000_000..3_000_100].fill(0xAB);
    t.file("big.img", &data).mtime("big.img", BASE_TIME + 777);
    let r = h.fastrsync(tmp.path(), &["-a", "--stats", "--strategy", "fixed", "src/", &target]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let literal: u64 = r
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("Literal data: "))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .unwrap();
    assert!(literal < 1 << 20, "only the changed region should travel, sent {literal}");
    assert_eq!(h.fastrsync(tmp.path(), &["-a", &target, "back"]).code, 0);
    assert_same_tree(&snapshot(&tmp.path().join("src"), true), &snapshot(&tmp.path().join("back"), true));
}

#[test]
fn matches_rsync_between_the_same_machines() {
    let Some(h) = Host::get("vs-rsync") else { return eprintln!("SKIP: FASTRSYNC_TEST_REMOTE not set") };
    let (Some(remote_rsync), Some(local_rsync)) = (h.rsync.clone(), reference_rsync()) else {
        return eprintln!("SKIP: set FASTRSYNC_TEST_REMOTE_RSYNC and install rsync 3.x locally");
    };
    let tmp = tempfile::tempdir().unwrap();
    rich_tree(&tmp.path().join("src"));
    let r = Command::new(local_rsync)
        .args(["-ai", &format!("--rsync-path={remote_rsync}"), "src/", &format!("{}/", h.remote("r"))])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let f = h.fastrsync(tmp.path(), &["-ai", "src/", &format!("{}/", h.remote("f"))]);
    assert_eq!(f.code, 0, "{}", f.stderr);
    let rsync_items: Vec<String> = {
        let mut v: Vec<String> = String::from_utf8_lossy(&r.stdout).lines().map(str::to_string).filter(|l| !l.is_empty()).collect();
        v.sort();
        v
    };
    assert_eq!(rsync_items, f.items());
    // Bring both results back with the same tool and compare.
    for d in ["r", "f"] {
        assert_eq!(h.fastrsync(tmp.path(), &["-a", &format!("{}/", h.remote(d)), &format!("back-{d}")]).code, 0);
    }
    assert_same_tree(&snapshot(&tmp.path().join("back-r"), true), &snapshot(&tmp.path().join("back-f"), true));
}
