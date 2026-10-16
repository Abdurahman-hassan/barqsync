//! Test helpers: tree builders, snapshots, and running fastrsync / reference rsync.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

pub const BASE_TIME: u64 = 1_600_000_000;

pub struct Tree {
    pub root: PathBuf,
}

impl Tree {
    pub fn new(root: impl Into<PathBuf>) -> Tree {
        let root = root.into();
        fs::create_dir_all(&root).unwrap();
        Tree { root }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    pub fn file(&self, rel: &str, content: impl AsRef<[u8]>) -> &Self {
        let p = self.path(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
        self
    }

    pub fn dir(&self, rel: &str) -> &Self {
        fs::create_dir_all(self.path(rel)).unwrap();
        self
    }

    pub fn symlink(&self, rel: &str, target: &str) -> &Self {
        let p = self.path(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let _ = fs::remove_file(&p);
        std::os::unix::fs::symlink(target, &p).unwrap();
        self
    }

    pub fn fifo(&self, rel: &str) -> &Self {
        let p = self.path(rel);
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        self
    }

    pub fn chmod(&self, rel: &str, mode: u32) -> &Self {
        fs::set_permissions(self.path(rel), fs::Permissions::from_mode(mode)).unwrap();
        self
    }

    pub fn remove(&self, rel: &str) -> &Self {
        let p = self.path(rel);
        if fs::symlink_metadata(&p).unwrap().is_dir() {
            fs::remove_dir_all(p).unwrap();
        } else {
            fs::remove_file(p).unwrap();
        }
        self
    }

    pub fn mtime(&self, rel: &str, secs: u64) -> &Self {
        set_mtime(&self.path(rel), secs);
        self
    }

    /// Give every file and directory a distinct, old mtime (directories last,
    /// deepest first) so quick-check results never depend on wall-clock timing.
    pub fn settle_mtimes(&self) -> &Self {
        let mut all = Vec::new();
        walk(&self.root, &mut all);
        all.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
        for (i, p) in all.iter().enumerate() {
            let md = fs::symlink_metadata(p).unwrap();
            if md.file_type().is_symlink() || md.file_type().is_fifo_like() {
                continue;
            }
            set_mtime(p, BASE_TIME + i as u64 * 7);
        }
        set_mtime(&self.root, BASE_TIME - 1);
        self
    }
}

trait FifoLike {
    fn is_fifo_like(&self) -> bool;
}
impl FifoLike for fs::FileType {
    fn is_fifo_like(&self) -> bool {
        use std::os::unix::fs::FileTypeExt;
        self.is_fifo() || self.is_socket() || self.is_char_device() || self.is_block_device()
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let md = fs::symlink_metadata(&p).unwrap();
        if md.is_dir() {
            walk(&p, out);
        }
        out.push(p);
    }
}

pub fn set_mtime(p: &Path, secs: u64) {
    let f = File::open(p).unwrap();
    f.set_modified(UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub kind: char,
    pub mode: u32,
    pub mtime: Option<i64>,
    pub size: u64,
    pub content: Option<[u8; 32]>,
    pub link: Option<Vec<u8>>,
}

/// Snapshot of a tree keyed by relative path. `with_root` includes the root
/// directory's own attributes (relevant for `src/` copies, not for `src` copies).
pub fn snapshot(root: &Path, with_root: bool) -> BTreeMap<Vec<u8>, Node> {
    let mut map = BTreeMap::new();
    if with_root {
        let md = fs::symlink_metadata(root).unwrap();
        map.insert(b".".to_vec(), node(root, &md));
    }
    snap(root, root, &mut map);
    map
}

fn node(p: &Path, md: &fs::Metadata) -> Node {
    use std::os::unix::fs::FileTypeExt;
    let ft = md.file_type();
    let kind = if ft.is_dir() {
        'd'
    } else if ft.is_symlink() {
        'l'
    } else if ft.is_file() {
        'f'
    } else if ft.is_fifo() {
        'p'
    } else {
        '?'
    };
    Node {
        kind,
        mode: md.mode() & 0o7777,
        mtime: (kind != 'l').then_some(md.mtime()),
        size: if kind == 'f' { md.len() } else { 0 },
        content: (kind == 'f').then(|| *blake3::hash(&fs::read(p).unwrap()).as_bytes()),
        link: (kind == 'l').then(|| fs::read_link(p).unwrap().as_os_str().as_bytes().to_vec()),
    }
}

fn snap(root: &Path, dir: &Path, map: &mut BTreeMap<Vec<u8>, Node>) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let md = fs::symlink_metadata(&p).unwrap();
        let rel = p.strip_prefix(root).unwrap().as_os_str().as_bytes().to_vec();
        map.insert(rel, node(&p, &md));
        if md.is_dir() {
            snap(root, &p, map);
        }
    }
}

/// Snapshot comparison ignoring mtimes (for runs without -t).
pub fn without_mtimes(mut m: BTreeMap<Vec<u8>, Node>) -> BTreeMap<Vec<u8>, Node> {
    for n in m.values_mut() {
        n.mtime = None;
    }
    m
}

pub fn assert_same_tree(a: &BTreeMap<Vec<u8>, Node>, b: &BTreeMap<Vec<u8>, Node>) {
    if a == b {
        return;
    }
    let mut msg = String::from("trees differ:\n");
    for (k, v) in a {
        match b.get(k) {
            None => msg += &format!("  only in rsync:     {} {:?}\n", String::from_utf8_lossy(k), v),
            Some(w) if w != v => {
                msg += &format!("  {}:\n    rsync:     {:?}\n    fastrsync: {:?}\n", String::from_utf8_lossy(k), v, w)
            }
            _ => {}
        }
    }
    for (k, v) in b {
        if !a.contains_key(k) {
            msg += &format!("  only in fastrsync: {} {:?}\n", String::from_utf8_lossy(k), v);
        }
    }
    panic!("{msg}");
}

pub struct RunOut {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl RunOut {
    /// Item/deletion lines only, sorted (drops headers and summaries of both tools).
    pub fn items(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .stdout
            .lines()
            .filter(|l| {
                !(l.is_empty()
                    || l.starts_with("sending incremental file list")
                    || l.starts_with("building file list")
                    || l.starts_with("sent ")
                    || l.starts_with("total size is")
                    || l.starts_with("transferred ")
                    || l.starts_with("Number of")
                    || l.starts_with("Total ")
                    || l.starts_with("Elapsed")
                    || l.contains("(DRY RUN)"))
            })
            .map(str::to_string)
            .collect();
        v.sort();
        v
    }
}

fn run(cmd: &mut Command, cwd: &Path) -> RunOut {
    let o = cmd.current_dir(cwd).output().expect("spawn");
    RunOut {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

pub fn fastrsync(cwd: &Path, args: &[&str]) -> RunOut {
    run(Command::new(env!("CARGO_BIN_EXE_fastrsync")).args(args), cwd)
}

pub fn fastrsync_os(cwd: &Path, args: &[&OsStr]) -> RunOut {
    run(Command::new(env!("CARGO_BIN_EXE_fastrsync")).args(args), cwd)
}

/// Reference rsync (3.x, not openrsync). `RSYNC=/path` overrides discovery.
pub fn reference_rsync() -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = std::env::var_os("RSYNC")
        .map(|p| vec![PathBuf::from(p)])
        .unwrap_or_else(|| {
            ["/opt/homebrew/bin/rsync", "/usr/local/bin/rsync", "/usr/bin/rsync"]
                .iter()
                .map(PathBuf::from)
                .collect()
        });
    candidates.into_iter().find(|p| {
        Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).starts_with("rsync  version 3"))
    })
}

pub fn rsync(cwd: &Path, args: &[&str]) -> Option<RunOut> {
    let r = reference_rsync()?;
    Some(run(Command::new(r).args(args), cwd))
}

/// Run the same arguments through rsync and fastrsync, substituting `{dst}`
/// with a per-tool destination. Returns (rsync, fastrsync) results, or None
/// when no reference rsync is installed.
pub fn both(cwd: &Path, args: &[&str], dst_r: &str, dst_f: &str) -> Option<(RunOut, RunOut)> {
    reference_rsync()?;
    let sub = |d: &str| -> Vec<String> { args.iter().map(|a| a.replace("{dst}", d)).collect() };
    let ra = sub(dst_r);
    let fa = sub(dst_f);
    let r = rsync(cwd, &ra.iter().map(String::as_str).collect::<Vec<_>>())?;
    let f = fastrsync(cwd, &fa.iter().map(String::as_str).collect::<Vec<_>>());
    Some((r, f))
}

pub fn skip_without_rsync() -> bool {
    if reference_rsync().is_none() {
        eprintln!("SKIP: no reference rsync 3.x found (set RSYNC=/path/to/rsync)");
        return true;
    }
    false
}

/// Replace per-tool destination names in output so lines can be compared.
pub fn normalize(items: Vec<String>, from: &str, to: &str) -> Vec<String> {
    let mut v: Vec<String> = items.into_iter().map(|l| l.replace(from, to)).collect();
    v.sort();
    v
}
