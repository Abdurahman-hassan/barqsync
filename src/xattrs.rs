//! `-X/--xattrs`: extended attributes, with rsync's rules (xattrs.c) for which names are
//! copied and how they are compared.
//!
//! A sender lists an entry's attributes under their *wire names*; a receiver turns those
//! into the names it stores ([`Rules::to_local`]) and compares them with its own list:
//!
//! - Linux: the `system.` namespace is never copied. A non-root receiver only stores and
//!   compares `user.` names (non-root senders can't even list `trusted.`).
//! - macOS has one namespace: names go out as `user.NAME`; `rsync.NS` names (another
//!   system's namespace stored by a root receiver) go out as `NS`. A receiver strips
//!   `user.`, and only root keeps other names, as `rsync.NAME`.
//! - rsync's own `rsync.%...` attributes (`user.rsync.%...` on Linux) need `-XX`.
//!
//! Values over [`INLINE_MAX`] bytes travel as a length and checksum, like rsync's
//! abbreviated values; the full value is fetched only when the receiver lacks it.

use crate::checksum::Sum;
use crate::entry::Kind;
use crate::fsops::xattr;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

/// Longer values are listed as a length and checksum (rsync: `MAX_FULL_DATUM`).
pub const INLINE_MAX: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum XValue {
    Full(Vec<u8>),
    /// A value over [`INLINE_MAX`] bytes: its length and BLAKE3-128 checksum.
    Sum { len: u64, sum: Sum },
}

impl XValue {
    pub fn new(v: Vec<u8>, full: bool) -> XValue {
        if full || v.len() <= INLINE_MAX {
            return XValue::Full(v);
        }
        let mut sum = [0u8; 16];
        sum.copy_from_slice(&blake3::hash(&v).as_bytes()[..16]);
        XValue::Sum { len: v.len() as u64, sum }
    }

    /// The same value, whether held in full or as a checksum.
    pub fn same(&self, other: &XValue) -> bool {
        match (self, other) {
            (XValue::Full(a), XValue::Full(b)) => a == b,
            (XValue::Sum { .. }, XValue::Sum { .. }) => self == other,
            (XValue::Full(v), s @ XValue::Sum { .. }) | (s @ XValue::Sum { .. }, XValue::Full(v)) => {
                v.len() > INLINE_MAX && XValue::new(v.clone(), false) == *s
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Xattr {
    pub name: Vec<u8>,
    pub value: XValue,
}

/// Which names take part, decided per run.
#[derive(Debug, Clone, Copy)]
pub struct Rules {
    /// Number of `-X` given: two also copy rsync's own `rsync.%...` attributes.
    pub level: u8,
    /// Running as root (the receiver: may store other namespaces).
    pub am_root: bool,
}

#[cfg(target_os = "linux")]
const RSYNC_PREFIX: &[u8] = b"user.rsync.";
#[cfg(not(target_os = "linux"))]
const RSYNC_PREFIX: &[u8] = b"rsync.";
const USER_PREFIX: &[u8] = b"user.";

/// One of rsync's internal `rsync.%stat`-style attributes.
fn is_internal(name: &[u8]) -> bool {
    name.starts_with(RSYNC_PREFIX) && name.get(RSYNC_PREFIX.len()) == Some(&b'%')
}

impl Rules {
    /// The wire name of a name on the sender's disk, or `None` if it isn't sent.
    pub fn to_wire(&self, name: &[u8]) -> Option<Vec<u8>> {
        if self.level < 2 && is_internal(name) {
            return None;
        }
        if cfg!(target_os = "linux") {
            return (!name.starts_with(b"system.")).then(|| name.to_vec());
        }
        if name.starts_with(RSYNC_PREFIX) && !is_internal(name) {
            return Some(name[RSYNC_PREFIX.len()..].to_vec());
        }
        Some([USER_PREFIX, name].concat())
    }

    /// The name a receiver stores for a wire name, or `None` if it drops it.
    pub fn from_wire(&self, name: &[u8]) -> Option<Vec<u8>> {
        let local = if cfg!(target_os = "linux") {
            if !self.am_root && !name.starts_with(USER_PREFIX) {
                return None;
            }
            name.to_vec()
        } else if let Some(rest) = name.strip_prefix(USER_PREFIX) {
            rest.to_vec()
        } else if self.am_root {
            [RSYNC_PREFIX, name].concat()
        } else {
            return None;
        };
        (self.level >= 2 || !is_internal(&local)).then_some(local)
    }

    /// Whether a name on the receiver's own files is compared (and removed when the
    /// source lacks it).
    pub fn is_local(&self, name: &[u8]) -> bool {
        if cfg!(target_os = "linux") {
            if self.am_root { !name.starts_with(b"system.") } else { name.starts_with(USER_PREFIX) }
        } else {
            true
        }
    }

    /// A sender's list (wire names) as this receiver stores it, sorted by name.
    pub fn to_local(&self, list: Vec<Xattr>) -> Vec<Xattr> {
        let mut out: Vec<Xattr> =
            list.into_iter().filter_map(|x| Some(Xattr { name: self.from_wire(&x.name)?, value: x.value })).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

/// Does rsync keep attributes for this kind of entry (`get_xattr`)? macOS has none on
/// devices and special files; Linux keeps them where the entry is transferred at all.
pub fn kind_has_xattrs(kind: Kind) -> bool {
    matches!(kind, Kind::File | Kind::Dir | Kind::Symlink) || cfg!(target_os = "linux")
}

/// Read `path`'s attributes whose names pass `keep`, renamed by it, sorted. Values over
/// [`INLINE_MAX`] are summed unless `full`.
fn read_with(path: &Path, full: bool, keep: impl Fn(&[u8]) -> Option<Vec<u8>>) -> io::Result<Vec<Xattr>> {
    let _lane = xattr::lane();
    let mut out = Vec::new();
    for name in xattr::names(path)? {
        let Some(shown) = keep(&name) else { continue };
        // Removed since it was listed: as if it never was there.
        let Some(v) = xattr::get(path, &name)? else { continue };
        out.push(Xattr { name: shown, value: XValue::new(v, full) });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// The sender's list for `path`: wire names, values over [`INLINE_MAX`] summed unless `full`.
pub fn read_wire(path: &Path, r: &Rules, full: bool) -> io::Result<Vec<Xattr>> {
    read_with(path, full, |n| r.to_wire(n))
}

/// The receiver's list of its own `path`.
pub fn read_local(path: &Path, r: &Rules) -> io::Result<Vec<Xattr>> {
    read_with(path, false, |n| r.is_local(n).then(|| n.to_vec()))
}

/// Do two sorted lists hold the same names and values?
pub fn same(a: &[Xattr], b: &[Xattr]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.name == y.name && x.value.same(&y.value))
}

/// Making `have` into `want` needs a value that only the source has in full.
pub fn needs_full(want: &[Xattr], have: &[Xattr]) -> bool {
    want.iter().any(|w| {
        matches!(w.value, XValue::Sum { .. })
            && !have.iter().any(|h| h.name == w.name && h.value.same(&w.value))
    })
}

/// Give `path` exactly the attributes `want` (receiver names), where `have` is what it
/// has now. Summed values not already on `path` come from `full` (the complete list with
/// full values, see [`needs_full`]). Like rsync, the owner gets write permission for a
/// moment when the entry lacks it, and `user.` names on Linux symlinks (where the
/// kernel refuses them) are left alone. Returns whether anything changed.
pub fn apply(path: &Path, kind: Kind, want: &[Xattr], have: &[Xattr], full: Option<&[Xattr]>, r: &Rules) -> io::Result<bool> {
    // rsync's NO_SYMLINK_USER_XATTRS (Linux): other namespaces still go on symlinks.
    let settable = |name: &[u8]| !(cfg!(target_os = "linux") && kind == Kind::Symlink && name.starts_with(USER_PREFIX));
    let mut sets: Vec<(&[u8], &[u8])> = Vec::new();
    for w in want.iter().filter(|w| settable(&w.name)) {
        if have.iter().any(|h| h.name == w.name && h.value.same(&w.value)) {
            continue;
        }
        let value = match &w.value {
            XValue::Full(v) => v.as_slice(),
            XValue::Sum { .. } => match full.and_then(|f| f.iter().find(|f| f.name == w.name)) {
                Some(Xattr { value: XValue::Full(v), .. }) => v.as_slice(),
                _ => {
                    let shown = String::from_utf8_lossy(&w.name);
                    return Err(io::Error::other(format!("missing abbreviated xattr value, {shown}")));
                }
            },
        };
        sets.push((&w.name, value));
    }
    let removes: Vec<&[u8]> = have
        .iter()
        .filter(|h| r.is_local(&h.name) && settable(&h.name) && !want.iter().any(|w| w.name == h.name))
        .map(|h| h.name.as_slice())
        .collect();
    if sets.is_empty() && removes.is_empty() {
        return Ok(false);
    }
    let _lane = xattr::lane();
    let restore = if !r.am_root && kind != Kind::Symlink { add_write_perm(path)? } else { None };
    let res = (|| {
        for (name, value) in &sets {
            xattr::set(path, name, value).map_err(|e| named(e, "lsetxattr", path, name))?;
        }
        for name in &removes {
            xattr::remove(path, name).map_err(|e| named(e, "lremovexattr", path, name))?;
        }
        Ok(true)
    })();
    if let Some(mode) = restore {
        crate::fsops::chmod(path, mode)?;
    }
    res
}

fn named(e: io::Error, call: &str, path: &Path, name: &[u8]) -> io::Error {
    let msg = format!("{call}(\"{}\",\"{}\") failed: {e}", path.display(), String::from_utf8_lossy(name));
    io::Error::new(e.kind(), msg)
}

/// rsync's `set_xattr`: an entry the owner can't write gets `u+w` until its attributes
/// are set. Returns the mode to put back.
fn add_write_perm(path: &Path) -> io::Result<Option<u32>> {
    let c = crate::fsops::cstr(path)?;
    if unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0 {
        return Ok(None);
    }
    let mode = crate::fsops::lstat(path)?.mode;
    Ok(crate::fsops::chmod(path, mode | 0o200).ok().map(|()| mode))
}

/// Without `-X`: remove the attributes a copy-on-write clone brought along, so the file
/// ends up like rsync's freshly written one. Attributes the system won't let go of
/// (macOS `com.apple.provenance`) stay, as they would on a new file.
pub fn strip_clone(path: &Path) {
    if let Ok(names) = xattr::names(path) {
        for n in names {
            let _ = xattr::remove(path, &n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(level: u8, am_root: bool) -> Rules {
        Rules { level, am_root }
    }

    fn full(name: &str, v: &[u8]) -> Xattr {
        Xattr { name: name.as_bytes().to_vec(), value: XValue::new(v.to_vec(), false) }
    }

    /// Wire names, then the names a receiver stores, as measured with rsync 3.x.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_names_like_rsync() {
        let r = rules(1, false);
        assert_eq!(r.to_wire(b"com.apple.quarantine").unwrap(), b"user.com.apple.quarantine");
        assert_eq!(r.to_wire(b"rsync.trusted.x").unwrap(), b"trusted.x");
        assert_eq!(r.to_wire(b"rsync.%stat"), None);
        assert_eq!(rules(2, false).to_wire(b"rsync.%stat").unwrap(), b"user.rsync.%stat");
        assert_eq!(r.from_wire(b"user.com.apple.quarantine").unwrap(), b"com.apple.quarantine");
        assert_eq!(r.from_wire(b"trusted.x"), None, "only root keeps other namespaces");
        assert_eq!(rules(1, true).from_wire(b"trusted.x").unwrap(), b"rsync.trusted.x");
        assert_eq!(r.from_wire(b"user.rsync.%stat"), None);
        // Round trips as rsync's do: rsync.foo is dropped by a non-root receiver,
        // rsync.user.bar arrives as bar.
        let through = |n: &[u8]| r.to_wire(n).and_then(|w| r.from_wire(&w));
        assert_eq!(through(b"rsync.foo"), None);
        assert_eq!(through(b"rsync.user.bar").unwrap(), b"bar");
        assert!(r.is_local(b"anything"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_names_like_rsync() {
        let r = rules(1, false);
        assert_eq!(r.to_wire(b"system.posix_acl_access"), None);
        assert_eq!(r.to_wire(b"security.selinux").unwrap(), b"security.selinux");
        assert_eq!(r.to_wire(b"user.rsync.%stat"), None);
        assert_eq!(r.from_wire(b"security.selinux"), None);
        assert_eq!(rules(1, true).from_wire(b"trusted.x").unwrap(), b"trusted.x");
        assert!(r.is_local(b"user.x") && !r.is_local(b"trusted.x"));
        assert!(rules(1, true).is_local(b"trusted.x") && !rules(1, true).is_local(b"system.x"));
    }

    #[test]
    fn long_values_are_summed_and_compare_either_way() {
        let long = vec![b'a'; 100];
        let s = XValue::new(long.clone(), false);
        assert!(matches!(s, XValue::Sum { len: 100, .. }));
        assert!(s.same(&XValue::Full(long.clone())));
        assert!(XValue::Full(long.clone()).same(&s));
        let mut other = long.clone();
        other[50] = b'b';
        assert!(!s.same(&XValue::new(other, false)));
        assert_eq!(XValue::new(b"short".to_vec(), false), XValue::Full(b"short".to_vec()));
        let want = vec![Xattr { name: b"big".to_vec(), value: s.clone() }];
        assert!(needs_full(&want, &[]));
        assert!(!needs_full(&want, &[Xattr { name: b"big".to_vec(), value: XValue::Full(long) }]));
    }

    #[test]
    fn lists_compare_by_name_and_value() {
        let a = vec![full("a", b"1"), full("b", b"2")];
        assert!(same(&a, &a.clone()));
        assert!(!same(&a, &a[..1]));
        assert!(!same(&a, &[full("a", b"1"), full("b", b"3")]));
    }

    #[test]
    fn apply_sets_and_removes() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f");
        std::fs::write(&p, b"x").unwrap();
        let r = rules(1, crate::fsops::am_root());
        let pre = if cfg!(target_os = "linux") { "user." } else { "" };
        let n = |s: &str| format!("{pre}{s}");
        let probe = xattr::set(&p, n("probe").as_bytes(), b"1");
        if probe.as_ref().is_err_and(xattr::is_unsupported) {
            return;
        }
        probe.unwrap();
        let big = vec![b'z'; 500];
        // Sorted by name, as lists always are.
        let want = vec![Xattr { name: n("big").into_bytes(), value: XValue::new(big.clone(), false) }, full(&n("keep"), b"v")];
        let have = read_local(&p, &r).unwrap();
        // What the system put there (SELinux labels, macOS provenance) stays wanted.
        let theirs: Vec<Xattr> = have.iter().filter(|h| h.name != n("probe").as_bytes()).cloned().collect();
        let with_theirs = |mine: Vec<Xattr>| {
            let mut v: Vec<Xattr> = theirs.iter().cloned().chain(mine).collect();
            v.sort_by(|a, b| a.name.cmp(&b.name));
            v
        };
        let (want, mine_want) = (with_theirs(want.clone()), want);
        assert!(apply(&p, Kind::File, &want, &have, None, &r).is_err(), "a summed value must be supplied");
        let fulls = with_theirs(vec![Xattr { name: n("big").into_bytes(), value: XValue::Full(big.clone()) }, full(&n("keep"), b"v")]);
        // Read-only files work too (write permission is added for the moment).
        crate::fsops::chmod(&p, 0o444).unwrap();
        assert!(apply(&p, Kind::File, &want, &have, Some(&fulls), &r).unwrap());
        assert_eq!(crate::fsops::lstat(&p).unwrap().mode, 0o444);
        assert_eq!(xattr::get(&p, n("big").as_bytes()).unwrap(), Some(big));
        assert_eq!(xattr::get(&p, n("probe").as_bytes()).unwrap(), None, "extra names are removed");
        let now = read_local(&p, &r).unwrap();
        let mine: Vec<Xattr> = now.iter().filter(|x| !theirs.contains(x)).cloned().collect();
        assert!(same(&mine, &mine_want), "{mine:?}");
        assert!(!needs_full(&want, &now));
    }
}
