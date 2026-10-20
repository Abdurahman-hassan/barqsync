//! Test helpers: tree builders, snapshots, and running barqsync / reference rsync.

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

    /// Give every regular file the same old mtime. For destination trees made once per
    /// tool: files the sync leaves alone must not differ by the second they were created in.
    pub fn same_mtimes(&self, secs: u64) -> &Self {
        let mut all = Vec::new();
        walk(&self.root, &mut all);
        for p in all.iter().filter(|p| fs::symlink_metadata(p).unwrap().is_file()) {
            set_mtime(p, secs);
        }
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
            if md.file_type().is_symlink() {
                continue;
            }
            if md.file_type().is_fifo_like() {
                // Opening a FIFO would block: set its time by path.
                set_mtime_by_path(p, BASE_TIME + i as u64 * 7);
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

/// `utimensat` without opening the file (FIFOs, devices).
pub fn set_mtime_by_path(p: &Path, secs: u64) {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
    let t = libc::timespec { tv_sec: secs as libc::time_t, tv_nsec: 0 };
    let r = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), [t, t].as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
    assert_eq!(r, 0, "utimensat {}: {}", p.display(), std::io::Error::last_os_error());
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
                msg += &format!("  {}:\n    rsync:     {:?}\n    barqsync: {:?}\n", String::from_utf8_lossy(k), v, w)
            }
            _ => {}
        }
    }
    for (k, v) in b {
        if !a.contains_key(k) {
            msg += &format!("  only in barqsync: {} {:?}\n", String::from_utf8_lossy(k), v);
        }
    }
    panic!("{msg}");
}

pub struct RunOut {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    /// `stdout` as bytes, before the lossy UTF-8 conversion.
    pub raw: Vec<u8>,
}

impl RunOut {
    /// Item/deletion lines only, sorted (drops headers and summaries of both tools).
    pub fn items(&self) -> Vec<String> {
        let mut v: Vec<String> =
            self.stdout.lines().filter(|l| is_item_line(l.as_bytes())).map(str::to_string).collect();
        v.sort();
        v
    }

    /// [`items`](Self::items) as raw bytes, for output that is not valid UTF-8.
    pub fn raw_items(&self) -> Vec<Vec<u8>> {
        let mut v: Vec<Vec<u8>> = self.raw.split(|&b| b == b'\n').filter(|l| is_item_line(l)).map(<[u8]>::to_vec).collect();
        v.sort();
        v
    }
}

fn is_item_line(l: &[u8]) -> bool {
    const SUMMARY: [&[u8]; 10] = [
        b"sending incremental file list",
        b"receiving incremental file list",
        b"receiving file list",
        b"building file list",
        b"sent ",
        b"total size is",
        b"transferred ",
        b"Number of",
        b"Total ",
        b"Elapsed",
    ];
    !(l.is_empty() || SUMMARY.iter().any(|p| l.starts_with(p)) || l.windows(9).any(|w| w == b"(DRY RUN)"))
}

pub fn run(cmd: &mut Command, cwd: &Path) -> RunOut {
    let o = cmd.current_dir(cwd).output().expect("spawn");
    RunOut {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        raw: o.stdout,
    }
}

/// Locale (LC_ALL) for both tools in differential runs. Under C/POSIX, the default in
/// containers and CI, glibc rsync and barqsync on Linux escape non-ASCII names as
/// `\#ooo`; UTF-8 keeps them readable. Tests of that escaping pick their own locale.
pub const TEST_LOCALE: &str = "C.UTF-8";

pub fn barqsync(cwd: &Path, args: &[&str]) -> RunOut {
    barqsync_in_locale(cwd, TEST_LOCALE, args)
}

pub fn barqsync_in_locale(cwd: &Path, locale: &str, args: &[&str]) -> RunOut {
    run(Command::new(env!("CARGO_BIN_EXE_barqsync")).env("LC_ALL", locale).args(args), cwd)
}

pub fn barqsync_os(cwd: &Path, args: &[&OsStr]) -> RunOut {
    barqsync_os_in_locale(cwd, TEST_LOCALE, args)
}

pub fn barqsync_os_in_locale(cwd: &Path, locale: &str, args: &[&OsStr]) -> RunOut {
    run(Command::new(env!("CARGO_BIN_EXE_barqsync")).env("LC_ALL", locale).args(args), cwd)
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
    rsync_in_locale(cwd, TEST_LOCALE, args)
}

pub fn rsync_in_locale(cwd: &Path, locale: &str, args: &[&str]) -> Option<RunOut> {
    let r = reference_rsync()?;
    Some(run(Command::new(r).env("LC_ALL", locale).args(args), cwd))
}

pub fn rsync_os_in_locale(cwd: &Path, locale: &str, args: &[&OsStr]) -> Option<RunOut> {
    let r = reference_rsync()?;
    Some(run(Command::new(r).env("LC_ALL", locale).args(args), cwd))
}

/// Run the same arguments through rsync and barqsync, substituting `{dst}`
/// with a per-tool destination. Returns (rsync, barqsync) results, or None
/// when no reference rsync is installed.
pub fn both(cwd: &Path, args: &[&str], dst_r: &str, dst_f: &str) -> Option<(RunOut, RunOut)> {
    both_in_locale(cwd, TEST_LOCALE, args, dst_r, dst_f)
}

/// [`both`] with LC_ALL=`locale` for both tools.
pub fn both_in_locale(cwd: &Path, locale: &str, args: &[&str], dst_r: &str, dst_f: &str) -> Option<(RunOut, RunOut)> {
    reference_rsync()?;
    let sub = |d: &str| -> Vec<String> { args.iter().map(|a| a.replace("{dst}", d)).collect() };
    let ra = sub(dst_r);
    let fa = sub(dst_f);
    let r = rsync_in_locale(cwd, locale, &ra.iter().map(String::as_str).collect::<Vec<_>>())?;
    let f = barqsync_in_locale(cwd, locale, &fa.iter().map(String::as_str).collect::<Vec<_>>());
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

/// A tree exercising every entry type and some awkward names and permissions.
pub fn rich_tree(root: &Path) -> Tree {
    let t = Tree::new(root);
    t.file("a.txt", "alpha\n")
        .file("empty", "")
        .file(".hidden", "dot")
        .file("with space.txt", "spaces")
        .file("ünïcødé/файл.txt", "unicode")
        .file("sub/b.txt", "bravo")
        .file("sub/deep/er/c.txt", "charlie")
        .file("big.bin", vec![7u8; 3 * 1024 * 1024 + 17])
        .dir("emptydir")
        .dir("sub/emptyinner")
        .symlink("link", "a.txt")
        .symlink("sub/dangling", "../nowhere")
        .symlink("dirlink", "sub")
        .fifo("pipe")
        .chmod("a.txt", 0o600)
        .chmod("sub/b.txt", 0o755)
        .chmod("sub/deep", 0o750)
        .settle_mtimes();
    t
}

/// A stand-in for ssh: drops the host argument and runs the remote command locally,
/// so remote transfers can be tested without an ssh server.
pub fn fake_ssh(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake-ssh");
    fs::write(&p, "#!/bin/sh\nshift\nexec /bin/sh -c \"$*\"\n").unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// Run `args` through rsync and barqsync as a remote transfer via [`fake_ssh`].
/// `{dst}` becomes the per-tool name and `{host}` the `fakehost:` prefix.
pub fn both_remote(cwd: &Path, args: &[&str], dst_r: &str, dst_f: &str) -> Option<(RunOut, RunOut)> {
    both_remote_env(cwd, args, dst_r, dst_f, &[])
}

/// [`both_remote`] with extra environment for barqsync.
pub fn both_remote_env(cwd: &Path, args: &[&str], dst_r: &str, dst_f: &str, env: &[(&str, &str)]) -> Option<(RunOut, RunOut)> {
    let rsync_bin = reference_rsync()?;
    let shim = fake_ssh(cwd);
    let shim = shim.to_str().unwrap();
    let sub = |d: &str| -> Vec<String> {
        args.iter().map(|a| a.replace("{dst}", d).replace("{host}", "fakehost:")).collect()
    };
    let mut ra = vec!["-e".to_string(), shim.to_string(), format!("--rsync-path={}", rsync_bin.display())];
    ra.extend(sub(dst_r));
    let mut fa = vec!["-e".to_string(), shim.to_string(), format!("--remote-path={}", env!("CARGO_BIN_EXE_barqsync"))];
    fa.extend(sub(dst_f));
    let r = rsync(cwd, &ra.iter().map(String::as_str).collect::<Vec<_>>())?;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_barqsync"));
    cmd.env("LC_ALL", TEST_LOCALE).args(&fa);
    for (k, v) in env {
        cmd.env(k, v);
    }
    Some((r, run(&mut cmd, cwd)))
}

/// A [`fake_ssh`] that cuts the remote command's output after `CUT_BYTES` bytes (a
/// dropped connection in the middle of a transfer). Copies with an unbuffered loop,
/// tolerating the non-blocking pipe rsync hands its remote shell.
pub fn cut_ssh(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("cut-ssh");
    fs::write(
        &p,
        r#"#!/bin/sh
shift
fifo=$(mktemp -u "${TMPDIR:-/tmp}/cut.XXXXXX")
mkfifo "$fifo"
exec 3<&0
/bin/sh -c "exec $*" <&3 >"$fifo" &
pid=$!
exec 3<&-
perl -MErrno=EAGAIN -e '
  $n = shift;
  while ($n > 0 && ($r = sysread(STDIN, $buf, $n < 65536 ? $n : 65536)) > 0) {
    $n -= $r;
    for ($o = 0; $o < $r;) {
      $w = syswrite(STDOUT, $buf, $r - $o, $o);
      if (defined $w) { $o += $w } elsif ($! == EAGAIN) { select(undef, undef, undef, 0.01) } else { exit 1 }
    }
  }' "${CUT_BYTES:-5000000}" <"$fifo"
kill -9 $pid 2>/dev/null
rm -f "$fifo"
"#,
    )
    .unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// [`both_remote`] through a given remote-shell stand-in. barqsync stays on the ssh
/// pipe with the fixed strategy (whatever `BARQSYNC_TRANSPORT` / `BARQSYNC_STRATEGY`
/// say), so the shim sees all of its traffic and no link probe.
pub fn both_remote_via(cwd: &Path, shim: &Path, args: &[&str], dst_r: &str, dst_f: &str) -> Option<(RunOut, RunOut)> {
    let rsync_bin = reference_rsync()?;
    let shim = shim.to_str().unwrap();
    let sub = |d: &str| -> Vec<String> {
        args.iter().map(|a| a.replace("{dst}", d).replace("{host}", "fakehost:")).collect()
    };
    let mut ra = vec!["-e".to_string(), shim.to_string(), format!("--rsync-path={}", rsync_bin.display())];
    ra.extend(sub(dst_r));
    let mut fa = vec!["-e".to_string(), shim.to_string(), format!("--remote-path={}", env!("CARGO_BIN_EXE_barqsync"))];
    fa.extend(sub(dst_f));
    let r = rsync(cwd, &ra.iter().map(String::as_str).collect::<Vec<_>>())?;
    let f = run(
        Command::new(env!("CARGO_BIN_EXE_barqsync"))
            .env("LC_ALL", TEST_LOCALE)
            .env("BARQSYNC_TRANSPORT", "ssh")
            .env("BARQSYNC_STRATEGY", "fixed")
            .args(&fa),
        cwd,
    );
    Some((r, f))
}

/// barqsync alone as a remote transfer via [`fake_ssh`] (`{host}` = `fakehost:`).
pub fn barqsync_remote(cwd: &Path, args: &[&str]) -> RunOut {
    let shim = fake_ssh(cwd);
    let mut fa = vec!["-e".to_string(), shim.to_str().unwrap().to_string()];
    fa.push(format!("--remote-path={}", env!("CARGO_BIN_EXE_barqsync")));
    fa.extend(args.iter().map(|a| a.replace("{host}", "fakehost:")));
    barqsync(cwd, &fa.iter().map(String::as_str).collect::<Vec<_>>())
}
