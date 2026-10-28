//! Checks on what the sending side of a remote transfer says, made before the receiver
//! acts on it. The sender is another machine: its listings decide where files are
//! written, so a name that is not one plain path component could reach outside the
//! destination, and file data that does not match what it announced must not become a
//! complete file.

use crate::checksum::Sum;
use crate::delta::Op;
use crate::entry::{DirEnt, Kind};
use crate::paths::join_rel;
use crate::proto::RootInfo;
use crate::xattrs::Xattr;
use std::collections::HashSet;
use std::os::unix::ffi::OsStrExt;

/// A name the receiver may create in a directory: one non-empty path component, not `.`
/// or `..`, with no `/` or NUL. Other bytes, not UTF-8 too, make a valid Unix name.
pub fn name_ok(name: &[u8]) -> bool {
    !name.is_empty() && name != b"." && name != b".." && !name.iter().any(|&b| b == b'/' || b == 0)
}

/// A directory's path below its transfer root: empty (the root itself), or valid names
/// joined by single slashes.
pub fn rel_ok(rel: &[u8]) -> bool {
    rel.is_empty() || rel.split(|&b| b == b'/').all(name_ok)
}

fn shown(name: &[u8]) -> String {
    format!("\"{}\"", String::from_utf8_lossy(name))
}

/// The roots the sender announced: at most one per `u16` root number; a `src/` root
/// comes with its directory's metadata and no items, any other names valid items.
pub fn roots(roots: &[RootInfo]) -> Result<(), String> {
    if roots.len() > usize::from(u16::MAX) + 1 {
        return Err(format!("{} transfer roots", roots.len()));
    }
    for (r, root) in roots.iter().enumerate() {
        match (root.contents, root.meta) {
            (true, Some(m)) if m.kind == Kind::Dir && root.items.is_empty() => {}
            (false, None) => entries(&root.items).map_err(|e| format!("root {r}: {e}"))?,
            _ => return Err(format!("malformed root {r}")),
        }
    }
    Ok(())
}

/// Entries asking for their destination's deletion (missing sources) come only with
/// `--delete-missing-args`.
pub fn missing<'e>(mut ents: impl Iterator<Item = &'e DirEnt>, opts: &crate::cli::Options) -> Result<(), String> {
    match ents.find(|e| e.missing) {
        Some(e) if opts.missing_args < 2 => Err(format!("deletion of {} asked for without --delete-missing-args", shown(e.name.as_bytes()))),
        _ => Ok(()),
    }
}

/// One listing's entries: valid names, each once.
fn entries(ents: &[DirEnt]) -> Result<(), String> {
    let mut seen = HashSet::with_capacity(ents.len());
    for e in ents {
        let name = e.name.as_bytes();
        // A missing source below a directory missing at the source is named by its path
        // from here (see `local::flags::missing_selected`).
        let ok = if e.missing { !name.is_empty() && rel_ok(name) } else { name_ok(name) };
        if !ok {
            return Err(format!("invalid file name {}", shown(name)));
        }
        if !seen.insert(name) {
            return Err(format!("file name {} listed twice", shown(name)));
        }
    }
    Ok(())
}

/// A listing's names and kinds.
type Names = Vec<(Vec<u8>, Kind)>;

/// The directory listings the receiver can still take: each root's own listing, and one
/// for each subdirectory of a listing already taken but a `-x` mount point (or, in a root
/// of named items, for each item that is a directory). Each arrives once, under a listing
/// id not used before.
pub struct Listings {
    /// `(root, rel)` of the listings that may still arrive.
    expected: HashSet<(u16, Vec<u8>)>,
    ids: HashSet<u32>,
    done: bool,
    /// Per root: its named items (`None` for a `src/` root), which its top listing repeats.
    items: Vec<Option<Names>>,
}

impl Listings {
    /// For roots that passed [`roots`].
    pub fn new(roots: &[RootInfo]) -> Listings {
        let mut expected = HashSet::new();
        let mut items = Vec::with_capacity(roots.len());
        for (r, root) in roots.iter().enumerate() {
            let r = r as u16;
            expected.insert((r, Vec::new()));
            if root.contents {
                items.push(None);
                continue;
            }
            for e in root.items.iter().filter(|e| e.meta.kind == Kind::Dir) {
                expected.insert((r, e.name.as_bytes().to_vec()));
            }
            items.push(Some(root.items.iter().map(|e| (e.name.as_bytes().to_vec(), e.meta.kind)).collect()));
        }
        Listings { expected, ids: HashSet::new(), done: false, items }
    }

    /// Take listing `dir`, of directory `rel` in `root`, or say what is wrong with it.
    /// `mounts`: its directories whose contents aren't sent (see `Msg::DirBatch`).
    pub fn take(&mut self, root: u16, dir: u32, rel: &[u8], ents: &[DirEnt], sums: &[Option<Sum>], mounts: &[u32]) -> Result<(), String> {
        if self.done {
            return Err("listing after scan completion".into());
        }
        let Some(items) = self.items.get(usize::from(root)) else {
            return Err(format!("listing for unknown root {root}"));
        };
        if !rel_ok(rel) {
            return Err(format!("invalid directory name {}", shown(rel)));
        }
        if !self.expected.remove(&(root, rel.to_vec())) {
            return Err(format!("unexpected listing of {}", shown(if rel.is_empty() { b"." } else { rel })));
        }
        if !self.ids.insert(dir) {
            return Err(format!("listing id {dir} used twice"));
        }
        if !sums.is_empty() && sums.len() != ents.len() {
            return Err(format!("{} checksums for {} entries", sums.len(), ents.len()));
        }
        entries(ents)?;
        if mounts.windows(2).any(|w| w[0] >= w[1]) || mounts.iter().any(|&i| ents.get(i as usize).is_none_or(|e| e.meta.kind != Kind::Dir)) {
            return Err("mount points out of order or not among the listing's directories".into());
        }
        if let Some(items) = items
            && rel.is_empty()
        {
            // Its subdirectories were expected from the start. A named item is a top
            // directory, which is never a mount point.
            let same = mounts.is_empty()
                && items.iter().map(|(n, k)| (n.as_slice(), *k)).eq(ents.iter().map(|e| (e.name.as_bytes(), e.meta.kind)));
            return if same { Ok(()) } else { Err("the listing of named items differs from the items".into()) };
        }
        for (i, e) in ents.iter().enumerate().filter(|(_, e)| e.meta.kind == Kind::Dir) {
            if mounts.binary_search(&(i as u32)).is_err() {
                self.expected.insert((root, join_rel(rel, &e.name)));
            }
        }
        Ok(())
    }

    /// Scan completion is a control-stream barrier, after every announced listing.
    pub fn finish(&mut self, dirs: u32) -> Result<(), String> {
        if self.done || self.ids.len() as u64 != u64::from(dirs) {
            return Err(format!("invalid scan completion: {dirs} listings announced, {} received", self.ids.len()));
        }
        self.done = true;
        Ok(())
    }

    pub fn finished(&self) -> bool {
        self.done
    }
}

/// A small-file pack: lengths that add up to the blob exactly, each file once, and
/// attribute values only for the pack's files, once each.
pub fn pack(files: &[(u64, u32)], blob: &[u8], xattrs: &[(u64, Vec<Xattr>)]) -> Result<(), String> {
    let mut ids = HashSet::with_capacity(files.len());
    let mut total = 0u64;
    for &(id, len) in files {
        if !ids.insert(id) {
            return Err(format!("file {id:#x} packed twice"));
        }
        total += u64::from(len);
    }
    if total != blob.len() as u64 {
        return Err(format!("{} bytes of data for {total} announced", blob.len()));
    }
    let mut with = HashSet::with_capacity(xattrs.len());
    if let Some((id, _)) = xattrs.iter().find(|(id, _)| !ids.contains(id) || !with.insert(*id)) {
        return Err(format!("stray attributes for file {id:#x}"));
    }
    Ok(())
}

/// A delta's copied ranges: copies only, in order and not overlapping, inside the new
/// file (`size` bytes) and inside the basis it is built from (`basis_size` bytes).
pub fn copies(ops: &[Op], size: u64, basis_size: u64) -> Result<(), String> {
    let mut end = 0;
    for op in ops {
        let Op::Copy { off, basis_off, len } = *op else {
            return Err("a literal range among the copies".into());
        };
        let (Some(to), Some(from_end)) = (off.checked_add(len), basis_off.checked_add(len)) else {
            return Err("a copied range overflows".into());
        };
        if off < end || to > size {
            return Err(format!("a copy to bytes {off}..{to} of a {size}-byte file"));
        }
        if from_end > basis_size {
            return Err(format!("a copy of bytes {basis_off}..{from_end} of a {basis_size}-byte basis"));
        }
        end = to;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::Meta;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    fn ent(name: &[u8], kind: Kind) -> DirEnt {
        let meta = Meta { kind, size: 0, mtime: 0, mtime_ns: 0, mode: 0o755, uid: 0, gid: 0, dev: 0, ino: 0, rdev: 0, nlink: 1, atime: 0, crtime: 0 };
        DirEnt::new(OsString::from_vec(name.to_vec()), meta, None)
    }

    #[test]
    fn names_are_single_components() {
        for good in [&b"a"[..], b"..a", b"a..", b"...", b".hidden", b"a b", b"\xff\xfe", b"\\", b"a\\..\\b"] {
            assert!(name_ok(good), "{good:?}");
        }
        for bad in [&b""[..], b".", b"..", b"/", b"a/b", b"/a", b"a/", b"../x", b"a\0b"] {
            assert!(!name_ok(bad), "{bad:?}");
        }
        assert!(rel_ok(b"") && rel_ok(b"a") && rel_ok(b"a/b/\xff"));
        for bad in [&b"/"[..], b"/a", b"a/", b"a//b", b"a/./b", b"a/../b", b"..", b"a\0"] {
            assert!(!rel_ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn listings_follow_accepted_directories() {
        let mut root = RootInfo { contents: true, meta: Some(ent(b"r", Kind::Dir).meta), items: vec![], sel: None, extra: None };
        roots(std::slice::from_ref(&root)).unwrap();
        let mut l = Listings::new(std::slice::from_ref(&root));
        assert!(l.take(1, 0, b"", &[], &[], &[]).is_err(), "unknown root");
        assert!(l.take(0, 0, b"d", &[], &[], &[]).is_err(), "no parent listed it");
        l.take(0, 0, b"", &[ent(b"d", Kind::Dir), ent(b"f", Kind::File)], &[], &[]).unwrap();
        assert!(l.take(0, 0, b"", &[], &[], &[]).is_err(), "root listed twice");
        assert!(l.take(0, 1, b"f", &[], &[], &[]).is_err(), "a file has no listing");
        assert!(l.take(0, 0, b"d", &[], &[], &[]).is_err(), "listing id reused");
        assert!(l.take(0, 1, b"d", &[ent(b"x", Kind::File), ent(b"x", Kind::Dir)], &[], &[]).is_err(), "duplicate names");
        let mut l = Listings::new(std::slice::from_ref(&root));
        assert!(l.take(0, 0, b"", &[ent(b"..", Kind::Dir)], &[], &[]).is_err());
        assert!(l.take(0, 1, b"", &[ent(b"f", Kind::File)], &[None, None], &[]).is_err(), "checksums don't match");
        root.items = vec![ent(b"x", Kind::File)];
        assert!(roots(std::slice::from_ref(&root)).is_err(), "src/ root with items");
        let items = RootInfo { contents: false, meta: None, items: vec![ent(b"d", Kind::Dir), ent(b"f", Kind::File)], sel: None, extra: None };
        let mut l = Listings::new(std::slice::from_ref(&items));
        l.take(0, 5, b"d", &[ent(b"s", Kind::Dir)], &[], &[]).unwrap();
        l.take(0, 6, b"d/s", &[], &[], &[]).unwrap();
        assert!(l.take(0, 7, b"", &[ent(b"d", Kind::Dir)], &[], &[]).is_err(), "items differ");
        let bad = RootInfo { items: vec![ent(b"a/b", Kind::File)], ..items };
        assert!(roots(&[bad]).is_err());
    }

    #[test]
    fn mount_points_get_no_listing() {
        let root = RootInfo { contents: true, meta: Some(ent(b"r", Kind::Dir).meta), items: vec![], sel: None, extra: None };
        let ents = [ent(b"a", Kind::Dir), ent(b"f", Kind::File), ent(b"m", Kind::Dir), ent(b"n", Kind::Dir)];
        for bad in [&[1][..], &[4], &[2, 2], &[3, 2]] {
            let mut l = Listings::new(std::slice::from_ref(&root));
            assert!(l.take(0, 0, b"", &ents, &[], bad).is_err(), "{bad:?}");
        }
        let mut l = Listings::new(std::slice::from_ref(&root));
        l.take(0, 0, b"", &ents, &[], &[2, 3]).unwrap();
        l.take(0, 1, b"a", &[], &[], &[]).unwrap();
        assert!(l.take(0, 2, b"m", &[], &[], &[]).is_err(), "a mount point has no listing");
        // A named item is a top directory, never a mount point.
        let items = RootInfo { contents: false, meta: None, items: vec![ent(b"d", Kind::Dir)], sel: None, extra: None };
        let mut l = Listings::new(std::slice::from_ref(&items));
        assert!(l.take(0, 0, b"", &[ent(b"d", Kind::Dir)], &[], &[0]).is_err());
    }

    #[test]
    fn packs_and_copies_must_add_up() {
        pack(&[(1, 2), (2, 0), (3, 1)], b"abc", &[(3, vec![])]).unwrap();
        assert!(pack(&[(1, 4)], b"X", &[]).is_err(), "short blob");
        assert!(pack(&[(1, 1)], b"XY", &[]).is_err(), "long blob");
        assert!(pack(&[(1, 1), (1, 1)], b"XY", &[]).is_err(), "same file twice");
        assert!(pack(&[(1, 1)], b"X", &[(2, vec![])]).is_err(), "attributes for another file");
        let c = |off, basis_off, len| Op::Copy { off, basis_off, len };
        copies(&[c(0, 10, 5), c(5, 0, 5), c(20, 0, 10)], 30, 15).unwrap();
        assert!(copies(&[c(0, 0, 5), c(4, 0, 5)], 30, 15).is_err(), "overlap");
        assert!(copies(&[c(10, 0, 5), c(0, 0, 5)], 30, 15).is_err(), "out of order");
        assert!(copies(&[c(0, 0, 31)], 30, 40).is_err(), "past the end of the file");
        assert!(copies(&[c(0, 10, 6)], 30, 15).is_err(), "past the end of the basis");
        assert!(copies(&[c(u64::MAX, 0, 2)], 30, 15).is_err(), "overflow");
        assert!(copies(&[Op::Literal { off: 0, len: 1 }], 30, 15).is_err());
    }
}
