//! `-X/--xattrs` and `-A/--acls`, run through the reference rsync and barqsync: locally,
//! and pushed and pulled over the fake ssh on each transport. After every step the item
//! lines, exit codes, trees and the resulting attributes and ACLs must match.
//!
//! Skipped (with a message) when rsync was built without xattr or ACL support, or the
//! filesystem the tests run on has neither user attributes nor ACLs.

mod common;

use common::*;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

// ---------------------------------------------------------------------------
// Attributes and ACLs, set and read without barqsync's own code
// ---------------------------------------------------------------------------

fn c(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).unwrap()
}

fn cn(name: &[u8]) -> CString {
    CString::new(name).unwrap()
}

fn last_err() -> std::io::Error {
    std::io::Error::last_os_error()
}

#[cfg(target_os = "macos")]
fn set_x(p: &Path, name: &[u8], v: &[u8]) -> std::io::Result<()> {
    let r = unsafe { libc::setxattr(c(p).as_ptr(), cn(name).as_ptr(), v.as_ptr().cast(), v.len(), 0, libc::XATTR_NOFOLLOW) };
    if r == 0 { Ok(()) } else { Err(last_err()) }
}

#[cfg(target_os = "linux")]
fn set_x(p: &Path, name: &[u8], v: &[u8]) -> std::io::Result<()> {
    let r = unsafe { libc::lsetxattr(c(p).as_ptr(), cn(name).as_ptr(), v.as_ptr().cast(), v.len(), 0) };
    if r == 0 { Ok(()) } else { Err(last_err()) }
}

#[cfg(target_os = "macos")]
fn remove_x(p: &Path, name: &[u8]) {
    unsafe { libc::removexattr(c(p).as_ptr(), cn(name).as_ptr(), libc::XATTR_NOFOLLOW) };
}

#[cfg(target_os = "linux")]
fn remove_x(p: &Path, name: &[u8]) {
    unsafe { libc::lremovexattr(c(p).as_ptr(), cn(name).as_ptr()) };
}

fn list_x(p: &Path) -> Vec<Vec<u8>> {
    let mut buf = vec![0u8; 64 << 10];
    #[cfg(target_os = "macos")]
    let n = unsafe { libc::listxattr(c(p).as_ptr(), buf.as_mut_ptr().cast(), buf.len(), libc::XATTR_NOFOLLOW) };
    #[cfg(target_os = "linux")]
    let n = unsafe { libc::llistxattr(c(p).as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if n <= 0 {
        return Vec::new();
    }
    buf.truncate(n as usize);
    buf.split(|&b| b == 0).filter(|n| !n.is_empty()).map(<[u8]>::to_vec).collect()
}

fn get_x(p: &Path, name: &[u8]) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 256 << 10];
    #[cfg(target_os = "macos")]
    let n = unsafe { libc::getxattr(c(p).as_ptr(), cn(name).as_ptr(), buf.as_mut_ptr().cast(), buf.len(), 0, libc::XATTR_NOFOLLOW) };
    #[cfg(target_os = "linux")]
    let n = unsafe { libc::lgetxattr(c(p).as_ptr(), cn(name).as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    (n >= 0).then(|| {
        buf.truncate(n as usize);
        buf
    })
}

fn am_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// A test attribute name: Linux users only have the `user.` namespace; macOS has one.
fn nm(s: &str) -> Vec<u8> {
    if cfg!(target_os = "linux") { format!("user.{s}").into_bytes() } else { s.as_bytes().to_vec() }
}

/// The attributes a snapshot compares: what the run may copy, without the macOS
/// provenance tag the system puts on every new file.
fn compared(name: &[u8]) -> bool {
    if cfg!(target_os = "linux") {
        name.starts_with(b"user.") || (am_root() && name.starts_with(b"trusted."))
    } else {
        name != b"com.apple.provenance"
    }
}

/// An entry's ACLs as text: `ls -le` entry lines on macOS, the raw kernel encoding on Linux.
fn acl_text(p: &Path) -> String {
    if cfg!(target_os = "macos") {
        let o = std::process::Command::new("/bin/ls").arg("-lde").arg(p).output().unwrap();
        String::from_utf8_lossy(&o.stdout).lines().skip(1).collect::<Vec<_>>().join("\n")
    } else {
        let a = get_x(p, b"system.posix_acl_access").unwrap_or_default();
        let d = get_x(p, b"system.posix_acl_default").unwrap_or_default();
        format!("{a:?} {d:?}")
    }
}

type XSnap = BTreeMap<Vec<u8>, (Vec<(Vec<u8>, Vec<u8>)>, String)>;

/// Every entry's compared attributes (sorted) and ACLs, by relative path.
fn xsnap(root: &Path) -> XSnap {
    let mut map = BTreeMap::new();
    fn walk(root: &Path, p: &Path, map: &mut XSnap) {
        let md = fs::symlink_metadata(p).unwrap();
        let mut xs: Vec<(Vec<u8>, Vec<u8>)> =
            list_x(p).into_iter().filter(|n| compared(n)).map(|n| (n.clone(), get_x(p, &n).unwrap_or_default())).collect();
        xs.sort();
        let acl = if md.file_type().is_symlink() { String::new() } else { acl_text(p) };
        let rel = p.strip_prefix(root).unwrap().as_os_str().as_bytes().to_vec();
        map.insert(rel, (xs, acl));
        if md.is_dir() {
            for e in fs::read_dir(p).unwrap() {
                walk(root, &e.unwrap().path(), map);
            }
        }
    }
    walk(root, root, &mut map);
    map
}

fn assert_same_xsnap(r: &XSnap, f: &XSnap, what: &str) {
    if r == f {
        return;
    }
    let mut msg = format!("{what}: attributes or ACLs differ\n");
    for (k, v) in r {
        if f.get(k) != Some(v) {
            msg += &format!("  {}:\n    rsync:    {:?}\n    barqsync: {:?}\n", String::from_utf8_lossy(k), v, f.get(k));
        }
    }
    for k in f.keys().filter(|k| !r.contains_key(*k)) {
        msg += &format!("  only in barqsync: {}\n", String::from_utf8_lossy(k));
    }
    panic!("{msg}");
}

#[cfg(target_os = "macos")]
fn chmod_acl(p: &Path, args: &[&str]) {
    let o = std::process::Command::new("/bin/chmod").args(args).arg(p).output().unwrap();
    assert!(o.status.success(), "chmod {args:?} {}: {}", p.display(), String::from_utf8_lossy(&o.stderr));
}

/// A POSIX ACL in the kernel's encoding: `(tag, perm, id)` entries in kernel order.
#[cfg(target_os = "linux")]
fn posix(entries: &[(u16, u16, u32)]) -> Vec<u8> {
    let mut v = 2u32.to_le_bytes().to_vec();
    for &(tag, perm, id) in entries {
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&perm.to_le_bytes());
        v.extend_from_slice(&id.to_le_bytes());
    }
    v
}

#[cfg(target_os = "linux")]
mod tag {
    pub const USER_OBJ: u16 = 1;
    pub const USER: u16 = 2;
    pub const GROUP_OBJ: u16 = 4;
    pub const GROUP: u16 = 8;
    pub const MASK: u16 = 0x10;
    pub const OTHER: u16 = 0x20;
    pub const NONE: u32 = u32::MAX;
}

/// Give `p` an access ACL with a named user (`perm` for it, `group` for the owning group).
#[cfg(target_os = "linux")]
fn set_user_acl(p: &Path, uid: u32, perm: u16, mode_group: u16) {
    use tag::*;
    let v = posix(&[(USER_OBJ, 6, NONE), (USER, perm, uid), (GROUP_OBJ, 4, NONE), (MASK, mode_group, NONE), (OTHER, 4, NONE)]);
    set_x(p, b"system.posix_acl_access", &v).unwrap();
}

// ---------------------------------------------------------------------------
// Support checks
// ---------------------------------------------------------------------------

fn rsync_has(cap: &str) -> bool {
    let Some(r) = reference_rsync() else { return false };
    let o = std::process::Command::new(r).arg("--version").output().unwrap();
    String::from_utf8_lossy(&o.stdout).contains(cap)
}

/// Can files in `dir` carry user attributes?
fn fs_has_xattrs(dir: &Path) -> bool {
    let p = dir.join(".probe-x");
    fs::write(&p, b"").unwrap();
    let ok = set_x(&p, &nm("probe"), b"1").is_ok();
    fs::remove_file(&p).unwrap();
    ok
}

/// Can files in `dir` carry ACLs?
fn fs_has_acls(dir: &Path) -> bool {
    let p = dir.join(".probe-a");
    fs::write(&p, b"").unwrap();
    #[cfg(target_os = "macos")]
    let ok = std::process::Command::new("/bin/chmod").args(["+a", "user:nobody allow read"]).arg(&p).status().unwrap().success();
    #[cfg(target_os = "linux")]
    let ok = {
        use tag::*;
        set_x(&p, b"system.posix_acl_access", &posix(&[(USER_OBJ, 6, NONE), (USER, 4, 65534), (GROUP_OBJ, 4, NONE), (MASK, 4, NONE), (OTHER, 4, NONE)]))
            .is_ok()
    };
    fs::remove_file(&p).unwrap();
    ok
}

fn skip_x(dir: &Path) -> bool {
    if skip_without_rsync() {
        return true;
    }
    if !rsync_has("xattrs") {
        eprintln!("SKIP: the reference rsync has no xattr support");
        return true;
    }
    if !fs_has_xattrs(dir) {
        eprintln!("SKIP: {} has no user extended attributes", dir.display());
        return true;
    }
    false
}

fn skip_a(dir: &Path) -> bool {
    if skip_without_rsync() {
        return true;
    }
    if !rsync_has("ACLs") {
        eprintln!("SKIP: the reference rsync has no ACL support");
        return true;
    }
    if !fs_has_acls(dir) {
        eprintln!("SKIP: {} has no ACLs", dir.display());
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Running both tools
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum How {
    Local,
    Push(&'static str),
    Pull(&'static str),
}

const HOWS: [How; 7] = [
    How::Local,
    How::Push("ssh"),
    How::Pull("ssh"),
    How::Push("tcp"),
    How::Pull("tcp"),
    How::Push("quic"),
    How::Pull("quic"),
];

/// Sync `src/` into `{dst}` with both tools, `how` the transfer goes.
fn sync(cwd: &Path, how: How, flags: &[&str], dst_r: &str, dst_f: &str) -> (RunOut, RunOut) {
    let mut args: Vec<&str> = flags.to_vec();
    match how {
        How::Local => {
            args.extend(["src/", "{dst}"]);
            both(cwd, &args, dst_r, dst_f).unwrap()
        }
        How::Push(t) | How::Pull(t) => {
            if matches!(how, How::Push(_)) {
                args.extend(["src/", "{host}{dst}"]);
            } else {
                args.extend(["{host}src/", "{dst}"]);
            }
            both_remote_env(cwd, &args, dst_r, dst_f, &[("BARQSYNC_TRANSPORT", t)]).unwrap()
        }
    }
}

/// rsync failed only because it rewrote SELinux labels the filesystem won't take (as root
/// in a container: rsync rewrites every value, barqsync only those that differ).
fn rsync_only_failed_on_labels(r: &RunOut, f: &RunOut) -> bool {
    r.code == 23
        && f.code == 0
        && r.stderr.lines().all(|l| {
            l.contains("\"security.selinux\") failed: Operation not supported") || l.starts_with("rsync error: some files/attrs were not transferred")
        })
}

fn same_code(r: &RunOut, f: &RunOut, what: &str) {
    if !rsync_only_failed_on_labels(r, f) {
        assert_eq!(r.code, f.code, "{what}: exit codes differ\n--- rsync:\n{}\n--- barqsync:\n{}", r.stderr, f.stderr);
    }
}

/// Same exit code, item lines, tree and attributes/ACLs.
fn check(cwd: &Path, (r, f): &(RunOut, RunOut), dst_r: &str, dst_f: &str, what: &str) {
    same_code(r, f, what);
    let norm = |out: &RunOut, d: &str| -> Vec<String> {
        let items = out.items().into_iter().map(|l| if l.starts_with("created directory ") { "created directory".into() } else { l });
        normalize(items.collect(), d, "D")
    };
    assert_eq!(norm(r, dst_r), norm(f, dst_f), "{what}: items differ\nbarqsync stderr: {}", f.stderr);
    assert_same_tree(&snapshot(&cwd.join(dst_r), true), &snapshot(&cwd.join(dst_f), true));
    assert_same_xsnap(&xsnap(&cwd.join(dst_r)), &xsnap(&cwd.join(dst_f)), what);
}

/// First sync, a no-op, `mutate(src, [dst_r, dst_f])`, the update (which must itemize
/// some entries with `flag` in its column), and a no-op again.
fn scenario(cwd: &Path, how: How, flags: &[&str], tag: &str, flag: (usize, char), mutate: &dyn Fn(&Path, &[&Path])) {
    let (dr, df) = (format!("{tag}-r"), format!("{tag}-f"));
    let what = |step: &str| format!("{how:?} {flags:?} {step}");
    let first = sync(cwd, how, flags, &dr, &df);
    assert_eq!(first.1.code, 0, "{}: {}", what("first"), first.1.stderr);
    check(cwd, &first, &dr, &df, &what("first"));
    let noop = sync(cwd, how, flags, &dr, &df);
    check(cwd, &noop, &dr, &df, &what("no-op"));
    assert_eq!(noop.1.items(), Vec::<String>::new(), "{}", what("no-op"));
    mutate(&cwd.join("src"), &[&cwd.join(&dr), &cwd.join(&df)]);
    let update = sync(cwd, how, flags, &dr, &df);
    check(cwd, &update, &dr, &df, &what("update"));
    let flagged = update.1.items().iter().filter(|l| l.as_bytes().get(flag.0) == Some(&(flag.1 as u8))).count();
    assert!(flagged >= 3, "{}: few `{}` items: {:?}", what("update"), flag.1, update.1.items());
    let again = sync(cwd, how, flags, &dr, &df);
    check(cwd, &again, &dr, &df, &what("no-op after update"));
}

fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

// ---------------------------------------------------------------------------
// -X
// ---------------------------------------------------------------------------

/// Files, a directory, a big file (cloned on APFS) and, where possible, a symlink with
/// attributes: short values and ones long enough to be sent as checksums.
fn xattr_tree(root: &Path) {
    let t = Tree::new(root);
    t.file("f", "f data\n")
        .file("plain", "no attributes\n")
        .file("ro", "read-only\n")
        .file("sub/g", "g data\n")
        .file("sub/deep/h", "h data\n")
        .file("big.bin", bytes(700 << 10, 1))
        .file("mid.bin", bytes(200 << 10, 2))
        .symlink("link", "f");
    set_x(&t.path("f"), &nm("small"), b"v1").unwrap();
    set_x(&t.path("f"), &nm("long"), &bytes(100, 7)).unwrap();
    set_x(&t.path("ro"), &nm("small"), b"ro").unwrap();
    set_x(&t.path("sub"), &nm("dirattr"), b"d").unwrap();
    set_x(&t.path("sub/deep"), &nm("dirlong"), &bytes(300, 3)).unwrap();
    set_x(&t.path("sub/g"), &nm("long"), &bytes(900, 9)).unwrap();
    set_x(&t.path("big.bin"), &nm("onbig"), &bytes(500, 5)).unwrap();
    set_x(&t.path("mid.bin"), &nm("onmid"), b"m").unwrap();
    set_x(&t.path(""), &nm("rootattr"), &bytes(64, 4)).unwrap();
    if cfg!(target_os = "macos") {
        // User attributes on a symlink, the system's own names, and rsync's prefix rules.
        set_x(&t.path("link"), b"linkattr", b"lv").unwrap();
        set_x(&t.path("f"), b"com.apple.quarantine", b"0081;65000000;Safari;").unwrap();
        set_x(&t.path("f"), b"rsync.user.hidden", b"shown as hidden").unwrap();
        set_x(&t.path("f"), b"rsync.%stat", b"internal").unwrap();
        set_x(&t.path("f"), b"rsync.other", b"another namespace").unwrap();
        set_x(&t.path("mid.bin"), b"com.apple.ResourceFork", &bytes(100 << 10, 8)).unwrap();
    }
    // Root (with CAP_SYS_ADMIN, so not in most containers) also copies other namespaces;
    // on Linux symlinks only those.
    if am_root() && cfg!(target_os = "linux") && set_x(&t.path("f"), b"trusted.rootonly", b"t").is_ok() {
        set_x(&t.path("link"), b"trusted.onlink", b"l").unwrap();
    }
    t.chmod("ro", 0o444);
    t.settle_mtimes();
}

/// Only attributes change (values short and long, added, removed, on files, directories
/// and symlinks, on a read-only file), extra ones appear on the destination, and some
/// files change their data along with their attributes.
fn xattr_mutate(src: &Path, dsts: &[&Path]) {
    set_x(&src.join("f"), &nm("small"), b"v2").unwrap();
    set_x(&src.join("f"), &nm("long"), &bytes(100, 70)).unwrap();
    set_x(&src.join("plain"), &nm("added"), b"new").unwrap();
    set_x(&src.join("plain"), &nm("addedlong"), &bytes(400, 40)).unwrap();
    fs::set_permissions(src.join("ro"), std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    set_x(&src.join("ro"), &nm("small"), b"ro2").unwrap();
    fs::set_permissions(src.join("ro"), std::os::unix::fs::PermissionsExt::from_mode(0o444)).unwrap();
    remove_x(&src.join("sub"), &nm("dirattr"));
    set_x(&src.join("sub/deep"), &nm("dirlong"), &bytes(300, 30)).unwrap();
    set_x(&src.join("sub/deep"), &nm("dirnew"), &bytes(50, 50)).unwrap();
    remove_x(&src.join("sub/g"), &nm("long"));
    // Data and attributes together: a small file, a mid one (sent whole or as a delta)
    // and a big one.
    fs::write(src.join("sub/deep/h"), "h changed\n").unwrap();
    set_x(&src.join("sub/deep/h"), &nm("withdata"), &bytes(200, 20)).unwrap();
    let mtime = std::os::unix::fs::MetadataExt::mtime(&fs::metadata(src.join("mid.bin")).unwrap());
    let mut mid = fs::read(src.join("mid.bin")).unwrap();
    mid[100] ^= 0xff;
    fs::write(src.join("mid.bin"), mid).unwrap();
    set_mtime(&src.join("mid.bin"), mtime as u64 + 5);
    set_x(&src.join("mid.bin"), &nm("onmid"), &bytes(80, 80)).unwrap();
    set_x(&src.join("big.bin"), &nm("onbig"), &bytes(500, 55)).unwrap();
    for d in dsts {
        set_x(&d.join("sub/g"), &nm("destonly"), b"x").unwrap();
        set_x(&d.join("plain"), &nm("destlong"), &bytes(90, 90)).unwrap();
    }
    if cfg!(target_os = "macos") {
        set_x(&src.join("link"), b"linkattr", b"changed").unwrap();
        set_x(&src.join("link"), b"linklong", &bytes(77, 77)).unwrap();
        remove_x(&src.join("f"), b"com.apple.quarantine");
    }
}

#[test]
fn xattrs_match_rsync_locally_and_over_the_wire() {
    let tmp = tempfile::tempdir().unwrap();
    if skip_x(tmp.path()) {
        return;
    }
    for (i, how) in HOWS.iter().enumerate() {
        let cwd = tmp.path().join(format!("x{i}"));
        xattr_tree(&cwd.join("src"));
        scenario(&cwd, *how, &["-aiX"], "d", (10, 'x'), &xattr_mutate);
    }
}

/// `-XX` also copies rsync's own `rsync.%...` attributes (`user.rsync.%...` on Linux).
#[test]
fn double_x_copies_rsync_internal_attributes() {
    let tmp = tempfile::tempdir().unwrap();
    if skip_x(tmp.path()) {
        return;
    }
    let t = Tree::new(tmp.path().join("src"));
    t.file("f", "data");
    let internal: &[u8] = if cfg!(target_os = "linux") { b"user.rsync.%stat" } else { b"rsync.%stat" };
    set_x(&t.path("f"), internal, b"100644 0,0 0:0").unwrap();
    set_x(&t.path("f"), &nm("normal"), b"n").unwrap();
    t.settle_mtimes();
    for (flags, copied) in [("-aiX", false), ("-aiXX", true)] {
        let dr = format!("r{copied}");
        let df = format!("f{copied}");
        let out = both(tmp.path(), &[flags, "src/", "{dst}"], &dr, &df).unwrap();
        check(tmp.path(), &out, &dr, &df, flags);
        assert_eq!(get_x(&tmp.path().join(&df).join("f"), internal).is_some(), copied, "{flags}");
    }
}

/// Without `-X` nothing is copied, even from files that are cloned (APFS clones carry
/// their source's attributes), and existing destination attributes are left alone.
#[test]
fn without_x_attributes_are_neither_copied_nor_touched() {
    let tmp = tempfile::tempdir().unwrap();
    if skip_x(tmp.path()) {
        return;
    }
    for (i, how) in [How::Local, How::Push("ssh"), How::Pull("ssh")].iter().enumerate() {
        let cwd = tmp.path().join(format!("n{i}"));
        xattr_tree(&cwd.join("src"));
        let (dr, df) = ("d-r", "d-f");
        let first = sync(&cwd, *how, &["-ai"], dr, df);
        check(&cwd, &first, dr, df, &format!("{how:?} -ai"));
        let snap = xsnap(&cwd.join(df));
        assert!(snap.values().all(|(x, _)| x.is_empty()), "{how:?}: {snap:?}");
        // A local delta of the big file clones the old version: still nothing copied.
        let mut big = fs::read(cwd.join("src/big.bin")).unwrap();
        big[1000] ^= 1;
        fs::write(cwd.join("src/big.bin"), big).unwrap();
        for d in [dr, df] {
            set_x(&cwd.join(d).join("plain"), &nm("kept"), b"k").unwrap();
        }
        let second = sync(&cwd, *how, &["-ai", "--no-whole-file"], dr, df);
        check(&cwd, &second, dr, df, &format!("{how:?} -ai --no-whole-file"));
        assert_eq!(get_x(&cwd.join(df).join("plain"), &nm("kept")).as_deref(), Some(&b"k"[..]));
        assert!(get_x(&cwd.join(df).join("big.bin"), &nm("onbig")).is_none());
    }
}

// ---------------------------------------------------------------------------
// -A
// ---------------------------------------------------------------------------

/// Files and directories with ACLs (on directories also inherited/default ones), and a
/// FIFO with one where the system allows it.
fn acl_tree(root: &Path) {
    let t = Tree::new(root);
    t.file("f", "f\n").file("plain", "p\n").file("deny", "d\n").file("sub/g", "g\n").file("big.bin", bytes(700 << 10, 3)).dir("inherit");
    #[cfg(target_os = "macos")]
    {
        chmod_acl(&t.path("f"), &["+a", "user:nobody allow read,write"]);
        chmod_acl(&t.path("f"), &["+a", "group:everyone allow readattr"]);
        chmod_acl(&t.path("deny"), &["+a", "user:nobody deny write,append"]);
        chmod_acl(&t.path("sub"), &["+a", "user:nobody allow list,search"]);
        chmod_acl(&t.path("inherit"), &["+a", "group:staff allow list,add_file,file_inherit,directory_inherit"]);
        chmod_acl(&t.path("big.bin"), &["+a", "user:nobody allow read"]);
        t.fifo("pipe");
        chmod_acl(&t.path("pipe"), &["+a", "user:nobody allow read"]);
    }
    #[cfg(target_os = "linux")]
    {
        use tag::*;
        set_user_acl(&t.path("f"), 65534, 6, 6);
        set_user_acl(&t.path("deny"), 65534, 0, 4);
        set_user_acl(&t.path("big.bin"), 65534, 4, 4);
        let dir = posix(&[(USER_OBJ, 7, NONE), (USER, 5, 65534), (GROUP_OBJ, 5, NONE), (MASK, 5, NONE), (OTHER, 5, NONE)]);
        set_x(&t.path("sub"), b"system.posix_acl_access", &dir).unwrap();
        let def = posix(&[(USER_OBJ, 7, NONE), (USER, 5, 65534), (GROUP_OBJ, 5, NONE), (GROUP, 7, 65534), (MASK, 7, NONE), (OTHER, 0, NONE)]);
        set_x(&t.path("inherit"), b"system.posix_acl_default", &def).unwrap();
        t.fifo("pipe");
        set_user_acl(&t.path("pipe"), 65534, 4, 4);
    }
    t.settle_mtimes();
}

/// Only ACLs change: entries changed, added and removed, an inherited/default ACL
/// changed, an ACL that only the destination has.
fn acl_mutate(src: &Path, dsts: &[&Path]) {
    #[cfg(target_os = "macos")]
    {
        chmod_acl(&src.join("f"), &["-a", "group:everyone allow readattr"]);
        chmod_acl(&src.join("plain"), &["+a", "user:nobody allow read"]);
        chmod_acl(&src.join("deny"), &["-N"]);
        chmod_acl(&src.join("inherit"), &["+a", "user:nobody allow list"]);
        for d in dsts {
            chmod_acl(&d.join("sub/g"), &["+a", "group:everyone allow read"]);
        }
    }
    #[cfg(target_os = "linux")]
    {
        use tag::*;
        set_user_acl(&src.join("f"), 65534, 4, 6);
        set_user_acl(&src.join("plain"), 65534, 4, 4);
        remove_x(&src.join("deny"), b"system.posix_acl_access");
        let def = posix(&[(USER_OBJ, 7, NONE), (GROUP_OBJ, 5, NONE), (OTHER, 5, NONE)]);
        set_x(&src.join("inherit"), b"system.posix_acl_default", &def).unwrap();
        for d in dsts {
            set_user_acl(&d.join("sub/g"), 65534, 6, 6);
        }
    }
    let _ = (src, dsts);
}

#[test]
fn acls_match_rsync_locally_and_over_the_wire() {
    let tmp = tempfile::tempdir().unwrap();
    if skip_a(tmp.path()) {
        return;
    }
    for (i, how) in HOWS.iter().enumerate() {
        let cwd = tmp.path().join(format!("a{i}"));
        acl_tree(&cwd.join("src"));
        scenario(&cwd, *how, &["-aiA"], "d", (9, 'a'), &acl_mutate);
    }
}

// ---------------------------------------------------------------------------
// Together, and with --delete and -H
// ---------------------------------------------------------------------------

/// Item lines with the hard-link target dropped and the leader's line made neutral:
/// which name leads a group differs between rsync and barqsync (see m6_features).
fn without_leaders(items: Vec<String>) -> Vec<String> {
    let mut v: Vec<String> = items
        .into_iter()
        .map(|l| {
            let l = l.split(" => ").next().unwrap();
            ["<f+++++++++", ">f+++++++++", "hf+++++++++"].iter().fold(l.to_string(), |l, from| l.replacen(from, "?f+++++++++", 1))
        })
        .collect();
    v.sort();
    v
}

#[test]
fn xattrs_and_acls_with_delete_and_hard_links() {
    let tmp = tempfile::tempdir().unwrap();
    if skip_x(tmp.path()) || skip_a(tmp.path()) {
        return;
    }
    for (i, how) in [How::Local, How::Push("ssh"), How::Pull("quic")].iter().enumerate() {
        let cwd = tmp.path().join(format!("h{i}"));
        let t = Tree::new(cwd.join("src"));
        t.file("h1", "shared\n").file("single", "s\n").dir("sub").dir("gone");
        fs::hard_link(t.path("h1"), t.path("sub/h2")).unwrap();
        set_x(&t.path("h1"), &nm("linked"), &bytes(120, 12)).unwrap();
        set_x(&t.path("single"), &nm("one"), b"1").unwrap();
        #[cfg(target_os = "macos")]
        chmod_acl(&t.path("h1"), &["+a", "user:nobody allow read"]);
        #[cfg(target_os = "linux")]
        set_user_acl(&t.path("h1"), 65534, 4, 4);
        t.settle_mtimes();
        let flags = ["-aiXAH", "--delete"];
        let (dr, df) = ("d-r", "d-f");
        let first = sync(&cwd, *how, &flags, dr, df);
        same_code(&first.0, &first.1, &format!("{how:?} first"));
        let items = |o: &RunOut, d: &str| without_leaders(normalize(o.items(), d, "D"));
        assert_eq!(items(&first.0, dr), items(&first.1, df), "{how:?} first: {}", first.1.stderr);
        assert_same_tree(&snapshot(&cwd.join(dr), true), &snapshot(&cwd.join(df), true));
        assert_same_xsnap(&xsnap(&cwd.join(dr)), &xsnap(&cwd.join(df)), "first");
        // Attributes change on the linked file, files appear on the destinations only, a
        // source directory goes away.
        set_x(&t.path("h1"), &nm("linked"), &bytes(120, 21)).unwrap();
        fs::remove_dir(t.path("gone")).unwrap();
        for d in [dr, df] {
            fs::write(cwd.join(d).join("sub/extra"), "x").unwrap();
            set_x(&cwd.join(d).join("sub/extra"), &nm("e"), b"e").unwrap();
        }
        let update = sync(&cwd, *how, &flags, dr, df);
        check(&cwd, &update, dr, df, &format!("{how:?} update"));
        let ino = |d: &str, p: &str| std::os::unix::fs::MetadataExt::ino(&fs::metadata(cwd.join(d).join(p)).unwrap());
        assert_eq!(ino(df, "h1"), ino(df, "sub/h2"), "{how:?}: still one inode");
    }
}
